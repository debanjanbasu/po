package dev.dioxus.main

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.view.WindowInsets
import android.view.WindowInsetsController

// Dioxus's generated `WryActivity` in `package dev.dioxus.main` references `BuildConfig.DEBUG` unqualified.
typealias BuildConfig = com.debanjanbasu.po.BuildConfig
class MainActivity : WryActivity() {
    companion object {
        private var veilidAndroidInitialized = false

        /** Intent-extra prefix for the harness seam values. */
        private const val SEAM_PREFIX = "po_e2e_"

        /**
         * Runtime permission Veilid needs for peer discovery on the LAN.
         * `reportLocalNetworkAccess` hands the answer to
         * `src/platform/native_bridge.rs`.
         */
        private const val LOCAL_NETWORK_PERMISSION = Manifest.permission.ACCESS_LOCAL_NETWORK
    }

    // JNI bridge into `src/platform/native_bridge.rs`: Veilid needs the Android
    // application context seeded before Rust starts `api_startup()`, and
    // activity/deep-link callbacks nudge the Rust runtime directly.
    private external fun initVeilidAndroid(ctx: Context)
    private external fun notifyAppActivityChanged()
    private external fun notifyAppBackgrounded()
    private external fun cachePendingDeepLink(intent: Intent?)
    private external fun cacheE2eSeams(keys: Array<String>, values: Array<String>)
    private external fun reportLocalNetworkAccess(granted: Boolean)

    /** Whether this process has already shown the local-network prompt. */
    private var localNetworkPrompted = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        applyDistractionFreeChrome()
        if (!veilidAndroidInitialized) {
            initVeilidAndroid(applicationContext)
            veilidAndroidInitialized = true
        }
        // Only cache the launch intent on a fresh start. On recreation
        // (rotation, process death) the same intent is redelivered and would
        // re-add the deep link a second time.
        if (savedInstanceState == null) {
            cachePendingDeepLink(intent)
            cacheE2eSeamsFromIntent(intent)
        }
        notifyAppActivityChanged()
    }

    /**
     * Distraction-free by default: the app draws edge to edge — its viewport
     * meta carries `viewport-fit=cover` and every strip respects the safe-area
     * insets — so the system bars only repeat space the app has already left
     * clear. Hiding *both* of them is the Android half of the same decision
     * the iOS bundle makes with `UIStatusBarHidden` plus the home indicator,
     * and it is what a foldable's inner and cover displays both inherit.
     *
     * `BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE` keeps them reachable with an
     * edge swipe, which is what the platform expects of a full-screen app —
     * the bars come back on demand instead of being unreachable.
     */
    private fun applyDistractionFreeChrome() {
        window.setDecorFitsSystemWindows(false)
        window.insetsController?.let { controller ->
            controller.hide(WindowInsets.Type.systemBars())
            controller.systemBarsBehavior =
                WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    /**
     * The bars are transient by design, so anything that hands the window
     * focus back — the swipe that revealed them, the notification shade, a
     * fold or unfold that moves the app between displays — gets them hidden
     * again. Doing this only in `onCreate` would leave the first reveal in
     * place for the rest of the session.
     */
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            applyDistractionFreeChrome()
        }
    }

    /**
     * Hands the harness seam values to Rust.
     *
     * `am start` cannot set environment variables for the app it starts, so
     * the harness carries them as intent extras named `po_e2e_<KEY>` — e.g.
     * `po_e2e_PO_E2E=1` for the marker switch. The `po_e2e_` prefix is
     * stripped before the key reaches Rust, so the Rust side sees the same
     * names the environment uses.
     */
    private fun cacheE2eSeamsFromIntent(intent: Intent?) {
        val keys = ArrayList<String>()
        val values = ArrayList<String>()
        val extras = intent?.extras
        if (extras != null) {
            for (name in extras.keySet()) {
                if (!name.startsWith(SEAM_PREFIX)) continue
                val value = extras.getString(name) ?: continue
                keys.add(name.removePrefix(SEAM_PREFIX))
                values.add(value)
            }
        }
        cacheE2eSeams(keys.toTypedArray(), values.toTypedArray())
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        cachePendingDeepLink(intent)
        notifyAppActivityChanged()
    }

    override fun onResume() {
        super.onResume()
        notifyAppActivityChanged()
        syncLocalNetworkAccess()
    }

    /**
     * Reports the local-network permission state to Rust, asking for it at most
     * once per process.
     *
     * `ACCESS_LOCAL_NETWORK` is a runtime permission on API 36+ and Veilid needs
     * it for peer discovery on the LAN. The answer has to reach Rust, which
     * otherwise keeps attempting discovery against a denied permission and fails
     * with no cause shown. The request goes through the launcher the
     * `WryActivity` superclass registers (its `requestPermissions(Array,
     * listener)`) instead of the raw `Activity.requestPermissions`, whose result
     * would land in `onRequestPermissionsResult` — a callback nothing in this app
     * implements.
     *
     * Re-read on every resume: the user can grant or revoke the permission in
     * Settings while the app is backgrounded, and Android itself stops showing
     * the dialog after two denials, so prompting again would nag without effect.
     */
    private fun syncLocalNetworkAccess() {
        if (checkSelfPermission(LOCAL_NETWORK_PERMISSION) == PackageManager.PERMISSION_GRANTED) {
            reportLocalNetworkAccess(true)
            return
        }
        if (localNetworkPrompted) {
            reportLocalNetworkAccess(false)
            return
        }
        localNetworkPrompted = true
        requestPermissions(arrayOf(LOCAL_NETWORK_PERMISSION)) { granted ->
            reportLocalNetworkAccess(granted == true)
        }
    }

    override fun onStop() {
        super.onStop()
        // Kick the Rust sync queue so pending writes flush during the
        // post-onStop grace window before the process is frozen.
        notifyAppBackgrounded()
    }
}
