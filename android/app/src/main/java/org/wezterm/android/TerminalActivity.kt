package org.wezterm.android

import android.app.Activity
import android.app.AlertDialog
import android.graphics.Color
import android.os.Bundle
import android.os.SystemClock
import android.util.Log
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.TextView
import java.util.concurrent.atomic.AtomicLong

/**
 * Hosts the terminal `SurfaceView` and forwards its lifecycle to the GUI
 * thread as generation-tagged events.
 *
 * `surfaceDestroyed` blocks until the native side has released the surface,
 * as the platform contract requires; the GUI thread never calls back into
 * this thread, so the wait cannot deadlock, and an engine that ends
 * resolves the wait from its shutdown.
 *
 * Leaving this Activity (Back, Home, rotation, finish) only takes the
 * surface away. Logical windows and their panes live in the native engine
 * for the life of the process; nothing here closes one. The selector shows
 * the native window list as it is at that moment and keeps no copy.
 */
class TerminalActivity : Activity(), SurfaceHolder.Callback, PlatformRequests.Listener {
    private var generation = 0L
    internal lateinit var surfaceView: SurfaceView
    internal lateinit var selector: TextView
    internal lateinit var engineBanner: TextView
    internal var selectorDialog: AlertDialog? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val engine = NativeApp.startTerminal(this, intent.getStringExtra(EXTRA_CONFIG_OVERRIDES) ?: "")
        Log.i(TAG, "engine $engine")
        surfaceView = SurfaceView(this)
        surfaceView.holder.addCallback(this)
        selector = overlayText().apply {
            visibility = View.GONE
            setOnClickListener { showSelector() }
        }
        engineBanner = overlayText().apply { visibility = View.GONE }
        val container = FrameLayout(this)
        val fill = ViewGroup.LayoutParams.MATCH_PARENT
        val wrap = ViewGroup.LayoutParams.WRAP_CONTENT
        container.addView(surfaceView, FrameLayout.LayoutParams(fill, fill))
        container.addView(selector, FrameLayout.LayoutParams(wrap, wrap, Gravity.TOP or Gravity.END))
        container.addView(engineBanner, FrameLayout.LayoutParams(fill, wrap, Gravity.BOTTOM))
        container.setOnApplyWindowInsetsListener { _, insets ->
            @Suppress("DEPRECATION")
            container.setPadding(
                insets.systemWindowInsetLeft,
                insets.systemWindowInsetTop,
                insets.systemWindowInsetRight,
                insets.systemWindowInsetBottom,
            )
            insets
        }
        setContentView(container)
    }

    private fun overlayText() = TextView(this).apply {
        setTextColor(Color.WHITE)
        setBackgroundColor(Color.argb(0xC0, 0x30, 0x30, 0x30))
        setPadding(24, 16, 24, 16)
    }

    override fun onStart() {
        super.onStart()
        PlatformRequests.listener = this
        onWindowsChanged()
        showEngineEnd()
    }

    override fun onStop() {
        PlatformRequests.listener = null
        selectorDialog?.dismiss()
        super.onStop()
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        moveTaskToBack(true)
    }

    override fun onWindowsChanged() {
        val status = NativeApp.surfaceStatus()
        val bound = status.windows.indexOfFirst { it.id == status.boundWindow }
        selector.visibility = if (status.windows.size > 1) View.VISIBLE else View.GONE
        selector.text = getString(R.string.window_selector, bound + 1, status.windows.size)
    }

    override fun onEngineEnded() = showEngineEnd()

    private fun showEngineEnd() {
        val status = NativeApp.surfaceStatus()
        if (status.engine != "failed" && status.engine != "stopped") return
        engineBanner.text = getString(R.string.engine_ended, status.engine, status.engineMessage)
        engineBanner.visibility = View.VISIBLE
    }

    private fun showSelector() {
        val status = NativeApp.surfaceStatus()
        val windows = status.windows
        val labels = windows.map { it.title.ifEmpty { getString(R.string.window_untitled, it.id) } }.toTypedArray()
        selectorDialog = AlertDialog.Builder(this)
            .setTitle(R.string.window_selector_title)
            .setSingleChoiceItems(labels, windows.indexOfFirst { it.id == status.boundWindow }) { dialog, which ->
                forward("select window") { NativeApp.nativeSelectWindow(windows[which].id) }
                dialog.dismiss()
            }
            .show()
    }

    private fun forward(what: String, event: () -> Unit) {
        try {
            event()
        } catch (e: RuntimeException) {
            Log.e(TAG, "$what refused: ${e.message}")
            showEngineEnd()
        }
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        generation = SurfaceGenerations.next()
        val frame = holder.surfaceFrame
        forward("surface created") { NativeApp.nativeSurfaceCreated(generation, holder.surface, frame.width(), frame.height()) }
        Log.i(TAG, "surface created generation=$generation ${frame.width()}x${frame.height()}")
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        forward("surface changed") { NativeApp.nativeSurfaceChanged(generation, width, height) }
        Log.i(TAG, "surface changed generation=$generation ${width}x$height format=$format")
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        val started = SystemClock.elapsedRealtime()
        val released = NativeApp.nativeSurfaceDestroyed(generation)
        val waitMs = SystemClock.elapsedRealtime() - started
        Log.i(TAG, "surface destroyed generation=$generation released=$released waitMs=$waitMs")
    }

    companion object {
        const val TAG = "WezTermSurface"

        /** Intent extra with `key=value` config override lines; debug builds only. */
        const val EXTRA_CONFIG_OVERRIDES = "org.wezterm.android.CONFIG_OVERRIDES"
    }
}

/** Process-wide, strictly increasing surface generations. */
object SurfaceGenerations {
    private val next = AtomicLong(1)

    fun next(): Long = next.getAndIncrement()
}
