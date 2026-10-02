package org.wezterm.android

import android.content.Intent
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.util.Log
import android.view.View
import androidx.test.core.app.ActivityScenario
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File

/** Device access shared by the lifecycle suites: real Activity, real input, native status waits. */
object TerminalHarness {
    const val TAG = "WezTermLifecycleTest"
    const val SHOTS = "/data/local/tmp/wezterm-lifecycle"
    const val TIMEOUT_MS = 30_000L

    /** How long the screen must stay unchanged before it counts as still. */
    const val SETTLE_WINDOW_MS = 1_000L

    val instrumentation get() = InstrumentationRegistry.getInstrumentation()

    fun shell(command: String): String {
        val fd = instrumentation.uiAutomation.executeShellCommand(command)
        return ParcelFileDescriptor.AutoCloseInputStream(fd).use { it.readBytes().toString(Charsets.UTF_8) }
    }

    fun launchIntent(): Intent {
        val overrides = InstrumentationRegistry.getArguments().getString("configOverrides") ?: ""
        return Intent(instrumentation.targetContext, TerminalActivity::class.java)
            .putExtra(TerminalActivity.EXTRA_CONFIG_OVERRIDES, overrides)
    }

    /** Launch with the diagnostic applet, the terminal content of the surface and lifecycle suites. */
    fun launch(): ActivityScenario<TerminalActivity> =
        ActivityScenario.launch(launchIntent().putExtra(TerminalActivity.EXTRA_DIAGNOSTIC_APPLET, true))

    /** Log the native status under `phase`; the host collects these lines as receipts. */
    fun receipt(phase: String, note: String = ""): SurfaceStatus {
        val status = NativeApp.surfaceStatus()
        Log.i(TAG, "receipt phase=$phase threads=${threads().size} $note ${status.rawJson}")
        return status
    }

    /**
     * Capture the screen once it is still: the view hierarchy idle and, on a
     * present surface, no frame for [SETTLE_WINDOW_MS] (glyph fallback
     * resolves on a worker thread and repaints).
     */
    fun screenshot(name: String, note: String = ""): SurfaceStatus {
        instrumentation.uiAutomation.waitForIdle(SETTLE_WINDOW_MS, TIMEOUT_MS)
        var status = NativeApp.surfaceStatus()
        while (status.state == "present" &&
            NativeApp.nativeAwaitSurfaceFrames(status.generation, status.framesPresented + 1, SETTLE_WINDOW_MS)
        ) {
            status = NativeApp.surfaceStatus()
        }
        shell("mkdir -p $SHOTS")
        shell("screencap -p $SHOTS/$name.png")
        return receipt(name, note)
    }

    /** Block on native status changes until `done` holds. */
    fun awaitStatus(what: String, done: (SurfaceStatus) -> Boolean): SurfaceStatus {
        val deadline = SystemClock.elapsedRealtime() + TIMEOUT_MS
        var status = NativeApp.surfaceStatus()
        while (!done(status)) {
            val left = deadline - SystemClock.elapsedRealtime()
            check(left > 0) { "$what not reached within ${TIMEOUT_MS}ms: ${status.rawJson}" }
            NativeApp.nativeAwaitSurfaceChange(status.revision, left)
            status = NativeApp.surfaceStatus()
        }
        return status
    }

    /** A frame presented after `since` on a present surface. */
    fun awaitFrameAfter(since: SurfaceStatus, what: String): SurfaceStatus =
        awaitStatus(what) { it.state == "present" && it.totalFramesPresented > since.totalFramesPresented }

    /**
     * Kernel names of the threads of this process, sorted. The framework's
     * binder pool is left out: the platform grows it on demand (observed:
     * `binder:<pid>_5` appearing between two cycles).
     */
    fun threads(): List<String> =
        File("/proc/self/task").listFiles().orEmpty().mapNotNull { task ->
            runCatching { File(task, "comm").readText().trim() }.getOrNull()
        }.filterNot { it.startsWith("binder:") }.sorted()

    /** Tap the centre of `view` with a real touch event. */
    fun tap(view: View) {
        val origin = IntArray(2)
        instrumentation.runOnMainSync { view.getLocationOnScreen(origin) }
        shell("input tap ${origin[0] + view.width / 2} ${origin[1] + view.height / 2}")
        instrumentation.waitForIdleSync()
    }
}
