package com.debanjanbasu.po

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.os.Handler
import android.os.Looper
import android.util.Base64
import android.util.Log
import androidx.core.content.FileProvider
import org.json.JSONException
import org.json.JSONObject
import java.io.File
import java.io.IOException
import java.nio.file.Files
import java.util.concurrent.ExecutionException
import java.util.concurrent.FutureTask
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException

class SharePlugin(private val activity: Activity) {
    fun shareText(input: String): String = resultOf("Share text") {
        if (input.startsWith("{\"text\":")) {
            val payload = JSONObject(input)
            val text = payload.getString("text")
            val image = payload.optString("image", "")
            if (image.isEmpty()) {
                sharePlainText(text)
            } else {
                shareCachedFile(
                    cacheFile("oo-qr.webp", Base64.decode(image, Base64.DEFAULT)),
                    "image/webp",
                    text,
                )
            }
        } else {
            sharePlainText(input)
        }
    }

    fun shareFile(input: String): String = resultOf("Share file") {
        val payload = JSONObject(input)
        val filename = payload.getString("filename")
        val mime = payload.optString("mime", "application/octet-stream")
        require(mime.matches(Regex("[A-Za-z0-9.+-]+/[A-Za-z0-9.+-]+"))) {
            "Invalid share MIME type"
        }
        shareCachedFile(
            cacheFile(filename, Base64.decode(payload.getString("data"), Base64.DEFAULT)),
            mime,
        )
    }

    fun copyText(text: String): String = onMainThread("Copy text") {
        val clipboard = activity.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
        if (clipboard == null) {
            "unsupported"
        } else {
            clipboard.setPrimaryClip(ClipData.newPlainText("po", text))
            "true"
        }
    }

    private fun sharePlainText(text: String): String {
        val intent = Intent(Intent.ACTION_SEND).apply {
            type = "text/plain"
            putExtra(Intent.EXTRA_TEXT, text)
        }
        return presentChooser(intent)
    }

    private fun cacheFile(filename: String, bytes: ByteArray): File {
        require(
            filename.isNotBlank() &&
                filename != "." &&
                filename != ".." &&
                !filename.contains('/') &&
                !filename.contains('\\') &&
                filename.none { it.code < 32 || it.code == 127 }
        ) { "Share filename must be a single non-empty path component" }
        val root = File(activity.cacheDir, "shared")
        if (!root.isDirectory && !root.mkdirs()) {
            throw IOException("Unable to create share cache")
        }
        val directory = Files.createTempDirectory(root.toPath(), "share-").toFile()
        val file = File(directory, filename)
        var written = false
        try {
            file.writeBytes(bytes)
            written = true
            return file
        } finally {
            if (!written) {
                discardCachedFile(file)
            }
        }
    }

    private fun shareCachedFile(file: File, mime: String, text: String? = null): String {
        var presented = false
        try {
            val uri = FileProvider.getUriForFile(
                activity,
                "${activity.packageName}.fileprovider",
                file,
            )
            val intent = Intent(Intent.ACTION_SEND).apply {
                type = mime
                putExtra(Intent.EXTRA_STREAM, uri)
                putExtra(Intent.EXTRA_TITLE, file.name)
                if (text != null) {
                    putExtra(Intent.EXTRA_TEXT, text)
                }
                clipData = ClipData.newUri(activity.contentResolver, file.name, uri)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            val outcome = presentChooser(intent)
            presented = outcome == "true"
            return outcome
        } finally {
            // Keep successful handoffs readable while the receiving app opens
            // them. Android owns eventual eviction from the private cache.
            if (!presented) {
                discardCachedFile(file)
            }
        }
    }

    private fun discardCachedFile(file: File) {
        if (file.exists() && !file.delete()) {
            Log.w("SharePlugin", "Failed to remove an unused share file")
        }
        val directory = file.parentFile
        if (directory != null && directory.exists() && !directory.delete()) {
            Log.w("SharePlugin", "Failed to remove an unused share directory")
        }
    }

    private fun presentChooser(intent: Intent): String = onMainThread("Present share dialog") {
        if (activity.isFinishing || activity.isDestroyed) {
            "failed"
        } else {
            val chooser = Intent.createChooser(intent, null).apply {
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            activity.startActivity(chooser)
            "true"
        }
    }

    private fun onMainThread(operation: String, action: () -> String): String {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            return resultOf(operation, action)
        }
        val task = FutureTask { resultOf(operation, action) }
        val handler = Handler(Looper.getMainLooper())
        if (!handler.post(task)) {
            Log.w("SharePlugin", "$operation could not reach the main thread")
            return "failed"
        }
        return try {
            task.get(5, TimeUnit.SECONDS)
        } catch (error: TimeoutException) {
            task.cancel(false)
            handler.removeCallbacks(task)
            Log.w("SharePlugin", "$operation timed out", error)
            "failed"
        } catch (error: InterruptedException) {
            task.cancel(false)
            handler.removeCallbacks(task)
            Thread.currentThread().interrupt()
            Log.w("SharePlugin", "$operation was interrupted", error)
            "failed"
        } catch (error: ExecutionException) {
            val cause = error.cause
            if (cause is Error) {
                throw cause
            }
            Log.w("SharePlugin", "$operation failed on the main thread", error)
            "failed"
        }
    }

    private fun resultOf(operation: String, action: () -> String): String = try {
        action()
    } catch (error: ActivityNotFoundException) {
        Log.w("SharePlugin", "$operation is unavailable", error)
        "unsupported"
    } catch (error: SecurityException) {
        Log.w("SharePlugin", "$operation was denied", error)
        "denied"
    } catch (error: JSONException) {
        Log.w("SharePlugin", "$operation payload is invalid", error)
        "failed"
    } catch (error: IOException) {
        Log.w("SharePlugin", "$operation file I/O failed", error)
        "failed"
    } catch (error: IllegalArgumentException) {
        Log.w("SharePlugin", "$operation arguments are invalid", error)
        "failed"
    } catch (error: IllegalStateException) {
        Log.w("SharePlugin", "$operation is unavailable in the current state", error)
        "failed"
    }
}
