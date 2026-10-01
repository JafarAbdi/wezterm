package org.wezterm.android

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import org.json.JSONObject
import kotlin.concurrent.thread

/**
 * Carries what the GUI thread asks of the platform to the UI thread.
 *
 * The GUI thread never calls into Java. One daemon thread blocks in
 * `nativeNextRequest` and posts each request to the UI thread, so a UI
 * thread that is waiting for the GUI thread (`surfaceDestroyed`) is never
 * waited on in return: its requests simply run after it returns.
 */
object PlatformRequests {
    /** UI-thread observer; the visible [TerminalActivity] registers itself. */
    interface Listener {
        fun onWindowsChanged()

        fun onEngineEnded()
    }

    private const val TAG = "WezTermRequests"
    private val main = Handler(Looper.getMainLooper())
    private var started = false

    /** Read and written on the UI thread only. */
    var listener: Listener? = null

    @Synchronized
    fun start(app: Context) {
        if (started) return
        started = true
        thread(name = "wezterm-requests", isDaemon = true) {
            while (true) {
                val request = NativeApp.nativeNextRequest() ?: break
                main.post { handle(app, JSONObject(request)) }
            }
            main.post { listener?.onEngineEnded() }
        }
    }

    private fun handle(app: Context, request: JSONObject) {
        val clipboard = app.getSystemService(ClipboardManager::class.java)
        when (val type = request.getString("type")) {
            "clipboard_get" -> {
                val id = request.getLong("request")
                // Null while the app has no input focus (Android 10+) or the clip holds no text.
                val text = clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(app)?.toString()
                try {
                    NativeApp.nativeClipboardText(id, text)
                } catch (e: RuntimeException) {
                    Log.w(TAG, "clipboard answer $id not delivered: ${e.message}")
                }
            }
            "clipboard_set" -> clipboard.setPrimaryClip(ClipData.newPlainText("WezTerm", request.getString("text")))
            "windows_changed" -> listener?.onWindowsChanged()
            else -> Log.e(TAG, "unknown platform request '$type'")
        }
    }
}
