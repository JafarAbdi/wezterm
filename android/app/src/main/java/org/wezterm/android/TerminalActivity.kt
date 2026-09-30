package org.wezterm.android

import android.app.Activity
import android.os.Bundle
import android.os.SystemClock
import android.util.Log
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.ViewGroup
import android.widget.FrameLayout
import java.util.concurrent.atomic.AtomicLong

/**
 * Hosts the terminal `SurfaceView` and forwards its lifecycle to the GUI
 * thread as generation-tagged events.
 *
 * `surfaceDestroyed` blocks until the native side has released the surface,
 * as the platform contract requires; the GUI thread never calls back into
 * this thread, so the wait cannot deadlock.
 */
class TerminalActivity : Activity(), SurfaceHolder.Callback {
    private var generation = 0L

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val engine = NativeApp.startTerminal(this, intent.getStringExtra(EXTRA_CONFIG_OVERRIDES) ?: "")
        Log.i(TAG, "engine $engine")
        val view = SurfaceView(this)
        view.holder.addCallback(this)
        val container = FrameLayout(this)
        container.addView(view, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        container.setOnApplyWindowInsetsListener { _, insets ->
            @Suppress("DEPRECATION")
            view.layoutParams = FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT).apply {
                setMargins(
                    insets.systemWindowInsetLeft,
                    insets.systemWindowInsetTop,
                    insets.systemWindowInsetRight,
                    insets.systemWindowInsetBottom,
                )
            }
            insets
        }
        setContentView(container)
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        generation = SurfaceGenerations.next()
        val frame = holder.surfaceFrame
        NativeApp.nativeSurfaceCreated(generation, holder.surface, frame.width(), frame.height())
        Log.i(TAG, "surface created generation=$generation ${frame.width()}x${frame.height()}")
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        NativeApp.nativeSurfaceChanged(generation, width, height)
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
