package com.debanjanbasu.po

import android.app.Activity
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

class BiometricPlugin(private val activity: Activity) {
    companion object {
        private val authInFlight = AtomicBoolean(false)
    }


    fun canCheck(): String =
        (BiometricManager.from(activity).canAuthenticate(
            BiometricManager.Authenticators.BIOMETRIC_STRONG,
        ) == BiometricManager.BIOMETRIC_SUCCESS).toString()

    // Prompts for biometric authentication. Blocks the calling thread until
    // the user responds. Returns "true" on success, "false" otherwise.
    fun authenticate(reason: String): String {
        val fragmentActivity = activity as? FragmentActivity
            ?: return "false"
        if (!authInFlight.compareAndSet(false, true)) {
            return "false"
        }
        try {
            val latch = CountDownLatch(1)
            val success = AtomicBoolean(false)
            val promptRef = AtomicReference<BiometricPrompt?>(null)
            val executor = ContextCompat.getMainExecutor(activity)
            val callback = object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                    success.set(true)
                    latch.countDown()
                }
                override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                    latch.countDown()
                }
                override fun onAuthenticationFailed() {
                    // Individual attempt failed (e.g. finger not recognised).
                    // BiometricPrompt keeps the dialog open for retries, so we
                    // do NOT count down here — wait for success or final error.
                }
            }

            activity.runOnUiThread {
                if (fragmentActivity.isFinishing || fragmentActivity.isDestroyed) {
                    latch.countDown()
                    return@runOnUiThread
                }
                try {
                    val prompt = BiometricPrompt(fragmentActivity, executor, callback)
                    promptRef.set(prompt)
                    val info = BiometricPrompt.PromptInfo.Builder()
                        .setTitle("Authenticate")
                        .setSubtitle(reason)
                        .setNegativeButtonText("Cancel")
                        .setAllowedAuthenticators(BiometricManager.Authenticators.BIOMETRIC_STRONG)
                        .build()
                    prompt.authenticate(info)
                } catch (_: Exception) {
                    latch.countDown()
                }
            }

            val completed = try {
                latch.await(60, TimeUnit.SECONDS)
            } catch (_: InterruptedException) {
                Thread.currentThread().interrupt()
                false
            }
            if (!completed) {
                activity.runOnUiThread { promptRef.get()?.cancelAuthentication() }
            }
            return if (completed && success.get()) "true" else "false"
        } finally {
            authInFlight.set(false)
        }
    }
}
