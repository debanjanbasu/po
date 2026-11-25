package com.debanjanbasu.po

import android.app.Activity
import android.content.pm.PackageManager
import android.util.Log
import com.google.android.gms.common.ConnectionResult
import com.google.android.gms.common.GoogleApiAvailability
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import java.util.concurrent.CountDownLatch
import java.util.concurrent.atomic.AtomicBoolean

class QRScannerPlugin(private val activity: Activity) {

    private companion object {
        const val TAG = "po.QRScannerPlugin"

        // The Google Code Scanner is a single system UI: launching a second
        // scan while one is on screen would stack scanners. Static because
        // the FFI layer constructs a fresh plugin instance per call.
        val scanInFlight = AtomicBoolean(false)
    }

    fun canScan(): String =
        (
            activity.packageManager.hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY) &&
                // A camera is not enough: the scanner here is the Play-services
                // "code scanner" module, so without Play services `startScan()`
                // fails and the scan reports an empty result, which the UI reads
                // as a cancel — a dead button. Report unavailable instead so the
                // caller hides it.
                GoogleApiAvailability.getInstance()
                    .isGooglePlayServicesAvailable(activity) == ConnectionResult.SUCCESS
        ).toString()

    // Launches the Google Code Scanner (system UI) and blocks until it
    // reaches a terminal state. Returns the decoded string, or "" on
    // cancel/failure.
    //
    // No arbitrary timeout: the scanner's Task is guaranteed to complete
    // (success, cancel, or failure all count), and a timeout that fired while
    // the system UI was still on screen would let a second scan overlap it.
    fun scan(): String {
        if (!scanInFlight.compareAndSet(false, true)) {
            Log.w(TAG, "scan requested while another scan is active; ignoring")
            return ""
        }

        val latch = CountDownLatch(1)
        var result = ""

        activity.runOnUiThread {
            try {
                val options = GmsBarcodeScannerOptions.Builder()
                    .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
                    .build()
                GmsBarcodeScanning.getClient(activity, options)
                    .startScan()
                    .addOnSuccessListener { barcode -> result = barcode.rawValue.orEmpty() }
                    .addOnFailureListener { e -> Log.w(TAG, "QR scan failed", e) }
                    .addOnCompleteListener {
                        // Fires exactly once for success, failure, AND cancel —
                        // the single cleanup point.
                        scanInFlight.set(false)
                        latch.countDown()
                    }
            } catch (e: Exception) {
                // getClient/startScan can throw synchronously (e.g. Play
                // services unavailable); never leave the caller blocked.
                Log.w(TAG, "failed to launch QR scanner", e)
                scanInFlight.set(false)
                latch.countDown()
            }
        }

        try {
            latch.await()
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
            return ""
        }
        return result
    }
}
