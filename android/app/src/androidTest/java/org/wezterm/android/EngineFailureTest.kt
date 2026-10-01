package org.wezterm.android

import android.view.View
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.wezterm.android.TerminalHarness.awaitStatus
import org.wezterm.android.TerminalHarness.instrumentation
import org.wezterm.android.TerminalHarness.launch
import org.wezterm.android.TerminalHarness.launchIntent
import org.wezterm.android.TerminalHarness.receipt
import org.wezterm.android.TerminalHarness.screenshot

/**
 * Suite `lifecycle` in ci/android.sh. Every test ends the engine, so
 * ci/android.sh runs each in a process of its own. The failures are debug
 * panics on the GUI thread (in a task, or in the bound window's
 * `SurfaceLost` handler) and a config override the bootstrap rejects. The
 * engine's shutdown must resolve whoever waits on it, report a surface
 * release only when it happened, and never close a logical window.
 */
@RunWith(AndroidJUnit4::class)
class EngineFailureTest {
    private fun presentWithFrame(): SurfaceStatus =
        awaitStatus("a frame on a present surface") { it.state == "present" && it.framesPresented >= 1 }

    private fun assertUiThreadRuns() {
        var uiRan = false
        instrumentation.runOnMainSync { uiRan = true }
        assertTrue("the UI thread still runs tasks", uiRan)
    }

    private fun assertFailureBanner(scenario: ActivityScenario<TerminalActivity>) {
        var banner = ""
        var bannerShown = false
        scenario.onActivity { activity ->
            bannerShown = activity.engineBanner.visibility == View.VISIBLE
            banner = activity.engineBanner.text.toString()
        }
        assertTrue("the Activity shows the failure instead of crashing", bannerShown)
        assertTrue(banner, banner.contains("failed"))
    }

    @Test
    fun guiThreadPanicWithAQueuedDestroyReleasesTheSurfaceAndTheUiThread() {
        val scenario = launch()
        try {
            val shown = presentWithFrame()
            assertTrue(NativeApp.nativeDiagnosticGui("open-window"))
            val two = awaitStatus("second window listed") { it.windows.size == 2 }
            assertEquals(1, two.liveLeases)
            screenshot("03-failure-before")

            // The GUI thread now waits inside a task; the destroy queues behind it and the task panics.
            assertTrue(NativeApp.nativeDiagnosticGui("panic-on-queued-destroy"))
            scenario.moveToState(Lifecycle.State.CREATED)

            val failed = awaitStatus("engine failed and surface retired") { it.engine == "failed" && it.state == "absent" }
            assertTrue(failed.engineMessage, failed.engineMessage.contains("diagnostic GUI-thread panic with a surface destroy queued"))
            assertEquals("the native window was released", 0, failed.liveLeases)
            assertEquals("logical windows are still listed", two.windows.map { it.id }, failed.windows.map { it.id })
            assertEquals("no window was closed", 0, failed.closedWindows)
            assertEquals(shown.boundWindow, failed.boundWindow)
            receipt("03-failure-retired")
            assertUiThreadRuns()

            scenario.moveToState(Lifecycle.State.RESUMED)
            assertFailureBanner(scenario)
            val refused = NativeApp.surfaceStatus()
            assertEquals("a failed engine takes no surface", 0, refused.liveLeases)
            assertEquals("absent", refused.state)
            assertFalse("a failed engine accepts no diagnostic", NativeApp.nativeDiagnosticGui("open-window"))
            screenshot("03-failure")
        } finally {
            scenario.close()
        }
        val closed = NativeApp.surfaceStatus()
        assertEquals(0, closed.liveLeases)
        assertEquals(2, closed.windows.size)
        receipt("03-failure-closed")
    }

    @Test
    fun guiThreadPanicRightAfterAClipboardReadStartedFailsThatRead() {
        val scenario = launch()
        try {
            val shown = presentWithFrame()
            assertEquals("the first read of the process", 0, shown.clipboardRequests)

            // The read starts through the bound window on the GUI thread, which
            // panics in the same task. The call returns once the read's future
            // resolved on a thread of its own.
            assertTrue(
                "the read's future resolved with an error",
                NativeApp.nativeDiagnosticGui("panic-with-clipboard-read"),
            )
            val failed = NativeApp.surfaceStatus()
            assertEquals("failed", failed.engine)
            assertTrue(failed.engineMessage, failed.engineMessage.contains("diagnostic GUI-thread panic with a clipboard read pending"))
            assertEquals("the platform never answered", 0, failed.clipboardResponses)
            receipt("03-clipboard-failure")

            // The platform's answer, had it come now, is refused.
            assertThrows(RuntimeException::class.java) { NativeApp.nativeClipboardText(1, "late") }
            assertEquals(0, NativeApp.surfaceStatus().clipboardResponses)

            // The UI thread destroys the surface of a failed engine and comes back.
            scenario.moveToState(Lifecycle.State.CREATED)
            assertUiThreadRuns()
            val retired = awaitStatus("surface retired") { it.state == "absent" && it.liveLeases == 0 }
            assertEquals(shown.windows.map { it.id }, retired.windows.map { it.id })
            assertEquals(0, retired.closedWindows)
            scenario.moveToState(Lifecycle.State.RESUMED)
            assertFailureBanner(scenario)
        } finally {
            scenario.close()
        }
    }

    @Test
    fun bootstrapFailureBeforeAConnectionExistsReleasesTheRequestThread() {
        val rejected = launchIntent().putExtra(TerminalActivity.EXTRA_CONFIG_OVERRIDES, "android_03_no_such_option=1")
        val scenario = ActivityScenario.launch<TerminalActivity>(rejected)
        try {
            // The Kotlin thread blocked in nativeNextRequest ends when the engine does.
            val requests = Thread.getAllStackTraces().keys.firstOrNull { it.name == "wezterm-requests" }
            requests?.join(TerminalHarness.TIMEOUT_MS)
            assertFalse("wezterm-requests left nativeNextRequest", requests?.isAlive ?: false)
            assertNull("the request queue is closed", NativeApp.nativeNextRequest())

            val failed = awaitStatus("bootstrap failed and queued surface released") {
                it.engine == "failed" && it.liveLeases == 0
            }
            receipt("03-bootstrap-failure")
            assertEquals("failed", failed.engine)
            assertTrue(failed.rawJson, failed.rawJson.contains("\"stage\":\"config_overrides\""))
            assertEquals("no logical window was created", 0, failed.windows.size)
            assertEquals("the queued surface was released", 0, failed.liveLeases)
            assertUiThreadRuns()
            assertFailureBanner(scenario)
        } finally {
            scenario.close()
        }
    }

    @Test
    fun surfaceLostHandlerPanicStillReleasesTheNativeWindowInTheShutdown() {
        val scenario = launch()
        try {
            val shown = presentWithFrame()
            assertEquals(1, shown.liveLeases)
            assertTrue(NativeApp.nativeDiagnosticGui("panic-in-surface-lost"))

            // surfaceDestroyed: the slot turns absent, then the window's handler
            // panics with its GPU state and lease clone intact.
            scenario.moveToState(Lifecycle.State.CREATED)
            val failed = awaitStatus("engine failed and native window released") { it.engine == "failed" && it.liveLeases == 0 }
            assertTrue(failed.engineMessage, failed.engineMessage.contains("injected SurfaceLost handler panic"))
            assertEquals("absent", failed.state)
            assertEquals(shown.windows.map { it.id }, failed.windows.map { it.id })
            assertEquals(0, failed.closedWindows)
            receipt("03-surface-lost-panic")
            assertUiThreadRuns()
        } finally {
            scenario.close()
        }
    }

    @Test
    fun surfaceLostHandlerPanicInTheShutdownReportsTheSurfaceAsNotReleased() {
        val scenario = launch()
        try {
            val shown = presentWithFrame()
            assertTrue(NativeApp.nativeDiagnosticGui("panic-in-surface-lost"))
            assertTrue(NativeApp.nativeDiagnosticGui("panic-on-queued-destroy"))

            // The destroy queues behind the waiting GUI task. That task panics,
            // and the shutdown's own SurfaceLost dispatch panics next.
            assertFalse(
                "a destroy the shutdown could not honour is not acknowledged",
                NativeApp.nativeSurfaceDestroyed(shown.generation),
            )
            val failed = receipt("03-retire-panic")
            assertEquals("failed", failed.engine)
            assertTrue(failed.engineMessage, failed.engineMessage.contains("diagnostic GUI-thread panic with a surface destroy queued"))
            assertEquals("the window kept its GPU state and its lease", 1, failed.liveLeases)
            assertEquals(0, failed.closedWindows)

            // The Activity's own surfaceDestroyed returns too.
            scenario.moveToState(Lifecycle.State.CREATED)
            assertUiThreadRuns()
        } finally {
            scenario.close()
        }
    }
}
