package org.wezterm.android

import android.app.UiAutomation
import android.content.ClipData
import android.content.ClipboardManager
import android.os.SystemClock
import android.util.Log
import android.view.View
import androidx.lifecycle.Lifecycle
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.runner.lifecycle.ActivityLifecycleCallback
import androidx.test.runner.lifecycle.ActivityLifecycleMonitorRegistry
import androidx.test.runner.lifecycle.Stage
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.FixMethodOrder
import org.junit.Test
import org.junit.runner.RunWith
import org.junit.runners.MethodSorters
import org.wezterm.android.TerminalHarness.awaitFrameAfter
import org.wezterm.android.TerminalHarness.awaitStatus
import org.wezterm.android.TerminalHarness.instrumentation
import org.wezterm.android.TerminalHarness.launch
import org.wezterm.android.TerminalHarness.receipt
import org.wezterm.android.TerminalHarness.screenshot
import org.wezterm.android.TerminalHarness.shell
import org.wezterm.android.TerminalHarness.tap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

/**
 * Suite `lifecycle` in ci/android.sh, first class. One engine and its
 * logical windows live across every test of the process, as they do across
 * Activity instances; the tests run in name order and each drives the real
 * `TerminalActivity`.
 */
@RunWith(AndroidJUnit4::class)
@FixMethodOrder(MethodSorters.NAME_ASCENDING)
class LifecycleTest {
    @After
    fun unfreezeRotation() {
        instrumentation.uiAutomation.setRotation(UiAutomation.ROTATION_UNFREEZE)
    }

    private fun presentWithFrame(): SurfaceStatus =
        awaitStatus("a frame on a present surface") { it.state == "present" && it.framesPresented >= 1 }

    /** The surface is gone and its native window released. */
    private fun awaitRetired(): SurfaceStatus =
        awaitStatus("surface retired") { it.state == "absent" && it.liveLeases == 0 }

    private fun assertWindowsKept(before: SurfaceStatus, after: SurfaceStatus) {
        assertEquals("logical windows", before.windows.map { it.id }, after.windows.map { it.id })
        assertEquals("bound window", before.boundWindow, after.boundWindow)
        assertEquals("no window was closed", 0, after.closedWindows)
        assertEquals("running", after.engine)
    }

    @Test
    fun t1_rotationWhileTheEngineInitializesPaintsOnlyTheCurrentGeneration() {
        var atRotation: SurfaceStatus? = null
        val rotated = CountDownLatch(1)
        val rotateOnCreate = ActivityLifecycleCallback { activity, stage ->
            if (activity is TerminalActivity && stage == Stage.CREATED && atRotation == null) {
                atRotation = NativeApp.surfaceStatus()
                thread {
                    instrumentation.uiAutomation.setRotation(UiAutomation.ROTATION_FREEZE_90)
                    rotated.countDown()
                }
            }
        }
        val registry = ActivityLifecycleMonitorRegistry.getInstance()
        registry.addLifecycleCallback(rotateOnCreate)
        val scenario = launch()
        try {
            registry.removeLifecycleCallback(rotateOnCreate)
            assertTrue("rotation was requested", rotated.await(TerminalHarness.TIMEOUT_MS, TimeUnit.MILLISECONDS))
            val requested = atRotation!!
            Log.i(TerminalHarness.TAG, "receipt phase=03-init-rotate-requested ${requested.rawJson}")
            assertEquals("rotation was requested before the engine ran", "starting", requested.engine)
            assertEquals("rotation was requested before any frame", 0, requested.totalFramesPresented)

            val landscape = awaitStatus("a frame on the landscape surface") {
                it.state == "present" && it.width > it.height && it.framesPresented >= 1
            }
            assertEquals("running", landscape.engine)
            assertEquals("one native window is held", 1, landscape.liveLeases)
            assertEquals("no callback was applied to a retired generation", 0, landscape.staleEvents)
            assertEquals("one logical window", 1, landscape.windows.size)
            assertEquals(landscape.windows.single().id, landscape.boundWindow)
            screenshot("03-init-rotate")
        } finally {
            scenario.close()
        }
        val closed = awaitRetired()
        assertEquals("the window outlives its Activity", 1, closed.windows.size)
    }

    @Test
    fun t2_backgroundingWithAFramePendingRetiresItAndResumesTheSameWindow() {
        val scenario = launch()
        try {
            val shown = presentWithFrame()
            // A size change queues a repaint; the surface goes away right behind it.
            NativeApp.nativeSurfaceChanged(shown.generation, shown.width, shown.height - 1)
            scenario.moveToState(Lifecycle.State.CREATED)
            val retired = awaitRetired()
            assertEquals(shown.retireAcks + 1, retired.retireAcks)
            assertWindowsKept(shown, retired)
            receipt("03-pending-retired", "frames_between=${retired.totalFramesPresented - shown.totalFramesPresented}")

            scenario.moveToState(Lifecycle.State.RESUMED)
            val resumed = presentWithFrame()
            assertEquals(shown.generation + 1, resumed.generation)
            assertEquals(1, resumed.liveLeases)
            assertWindowsKept(shown, resumed)
            screenshot("03-pending")
        } finally {
            scenario.close()
        }
    }

    @Test
    fun t3_duplicateAndLateSurfaceEventsChangeNothing() {
        val scenario = launch()
        try {
            val first = presentWithFrame()
            scenario.moveToState(Lifecycle.State.CREATED)
            val retired = awaitRetired()

            assertTrue("a second destroy is acknowledged at once", NativeApp.nativeSurfaceDestroyed(first.generation))
            assertTrue("a third destroy is acknowledged at once", NativeApp.nativeSurfaceDestroyed(first.generation))
            val duplicate = awaitStatus("both duplicates counted") { it.staleEvents == retired.staleEvents + 2 }
            assertEquals("a retirement is acknowledged once", retired.retireAcks, duplicate.retireAcks)
            assertEquals(0, duplicate.liveLeases)
            assertEquals("absent", duplicate.state)
            assertWindowsKept(first, duplicate)

            scenario.moveToState(Lifecycle.State.RESUMED)
            val resumed = presentWithFrame()
            screenshot("03-duplicate")

            // The creation of the retired generation arrives late, with a live Surface.
            scenario.onActivity { activity ->
                NativeApp.nativeSurfaceCreated(first.generation, activity.surfaceView.holder.surface, resumed.width, resumed.height)
            }
            NativeApp.nativeSurfaceChanged(first.generation, 7, 7)
            assertTrue("a late destroy is acknowledged at once", NativeApp.nativeSurfaceDestroyed(first.generation))
            val late = awaitStatus("late events counted and the late lease released") {
                it.staleEvents == duplicate.staleEvents + 3 && it.liveLeases == 1
            }
            assertEquals("the live generation is untouched", resumed.generation, late.generation)
            assertEquals("present", late.state)
            assertEquals(resumed.width, late.width)
            assertEquals(resumed.height, late.height)
            assertEquals(resumed.retireAcks, late.retireAcks)
            assertWindowsKept(first, late)

            NativeApp.nativeSurfaceChanged(resumed.generation, resumed.width, resumed.height - 1)
            awaitFrameAfter(late, "the live generation still paints")
            NativeApp.nativeSurfaceChanged(resumed.generation, resumed.width, resumed.height)
            awaitStatus("original size painted") { it.height == resumed.height && it.totalFramesPresented > late.totalFramesPresented + 1 }
            screenshot("03-late")
        } finally {
            scenario.close()
        }
    }

    @Test
    fun t4_clipboardReadRacingSurfaceDestructionCompletesWithoutACircularWait() {
        val scenario = launch()
        try {
            val shown = presentWithFrame()
            scenario.onActivity { activity ->
                activity.getSystemService(ClipboardManager::class.java)
                    .setPrimaryClip(ClipData.newPlainText("test", "paste-03"))
            }
            assertTrue(NativeApp.nativeDiagnosticGui("paste"))
            val pasted = awaitStatus("clipboard read answered") { it.clipboardResponses == shown.clipboardResponses + 1 }
            awaitFrameAfter(pasted, "pasted text painted")
            screenshot("03-clipboard-paste")

            // On the UI thread: start a read, wait until the GUI thread has asked
            // for the clipboard, then destroy the surface. The answer can only be
            // produced by this thread after surfaceDestroyed returns.
            var destroyMs = -1L
            lateinit var atDestroy: SurfaceStatus
            lateinit var afterDestroy: SurfaceStatus
            scenario.onActivity { activity ->
                NativeApp.nativeDiagnosticGui("paste")
                atDestroy = awaitStatus("GUI thread asked for the clipboard") { it.clipboardRequests == pasted.clipboardRequests + 1 }
                val started = SystemClock.elapsedRealtime()
                activity.surfaceView.visibility = View.GONE
                destroyMs = SystemClock.elapsedRealtime() - started
                afterDestroy = NativeApp.surfaceStatus()
            }
            assertEquals("the read was unanswered when the surface was destroyed", pasted.clipboardResponses, atDestroy.clipboardResponses)
            assertEquals("surfaceDestroyed ran on the UI thread before the answer could", "absent", afterDestroy.state)
            assertEquals("the read was still unanswered when surfaceDestroyed returned", pasted.clipboardResponses, afterDestroy.clipboardResponses)
            assertEquals("the native window was released when surfaceDestroyed returned", 0, afterDestroy.liveLeases)
            val raced = awaitStatus("clipboard read answered") { it.clipboardResponses == it.clipboardRequests }
            assertEquals("absent", raced.state)
            assertEquals(pasted.retireAcks + 1, raced.retireAcks)
            assertEquals(0, raced.liveLeases)
            assertEquals(shown.clipboardRequests + 2, raced.clipboardRequests)
            assertWindowsKept(shown, raced)
            receipt("03-clipboard-race-retired", "destroy_ms=$destroyMs")

            scenario.onActivity { activity -> activity.surfaceView.visibility = View.VISIBLE }
            val back = presentWithFrame()
            assertEquals(shown.generation + 1, back.generation)
            screenshot("03-clipboard-race")
        } finally {
            scenario.close()
        }
    }

    @Test
    fun t5_backLeavesWindowsAliveAndReopenShowsTheSameWindow() {
        val scenario = launch()
        try {
            val shown = presentWithFrame()
            shell("input keyevent KEYCODE_BACK")
            val behind = awaitRetired()
            assertWindowsKept(shown, behind)
            screenshot("03-back-away")

            val reopen = instrumentation.targetContext.packageManager
                .getLaunchIntentForPackage(instrumentation.targetContext.packageName)!!
            instrumentation.targetContext.startActivity(reopen)
            val reopened = presentWithFrame()
            assertEquals(1, reopened.liveLeases)
            assertWindowsKept(shown, reopened)
            screenshot("03-back")
        } finally {
            scenario.close()
        }
    }

    @Test
    fun t6_selectorBindsAnotherWindowAndKeepsEveryWindow() {
        val scenario = launch()
        try {
            val one = presentWithFrame()
            val first = one.boundWindow!!
            assertTrue(NativeApp.nativeDiagnosticGui("open-window"))
            val two = awaitStatus("second window listed with a title") {
                it.windows.size == 2 && it.windows.all { window -> window.title.isNotEmpty() }
            }
            val second = two.windows.map { it.id }.single { it != first }
            assertEquals("a new window does not take the surface", first, two.boundWindow)
            assertEquals("diagnostic 1", two.windows.first { it.id == first }.title)
            instrumentation.waitForIdleSync()
            screenshot("03-windows-first")

            fun selectThroughTheUi(index: Int, shot: String) {
                lateinit var activity: TerminalActivity
                scenario.onActivity { activity = it }
                assertEquals("the selector is offered", View.VISIBLE, activity.selector.visibility)
                tap(activity.selector)
                val row = activity.selectorDialog?.takeIf { it.isShowing }?.listView?.getChildAt(index)
                assertNotNull("the selector dialog lists window $index", row)
                screenshot(shot)
                tap(row!!)
            }

            selectThroughTheUi(1, "03-windows-selector")
            val onSecond = awaitStatus("second window bound and painted") {
                it.boundWindow == second && it.totalFramesPresented > two.totalFramesPresented
            }
            assertEquals("the surface is the same", two.generation, onSecond.generation)
            awaitStatus("both windows titled") { it.windows.map { window -> window.title } == listOf("diagnostic 1", "diagnostic 2") }
            assertEquals(1, onSecond.liveLeases)
            assertEquals(listOf(first, second), onSecond.windows.map { it.id })
            assertEquals(0, onSecond.closedWindows)
            screenshot("03-windows")

            // Duplicate and unknown selections are ignored.
            NativeApp.nativeSelectWindow(second)
            NativeApp.nativeSelectWindow(9_999)
            scenario.moveToState(Lifecycle.State.CREATED)
            val away = awaitRetired()
            assertEquals("the selection survives surface loss", second, away.boundWindow)
            scenario.moveToState(Lifecycle.State.RESUMED)
            val resumed = presentWithFrame()
            assertEquals(second, resumed.boundWindow)
            assertEquals(2, resumed.windows.size)

            selectThroughTheUi(0, "03-windows-selector-back")
            val onFirst = awaitStatus("first window bound and painted") {
                it.boundWindow == first && it.totalFramesPresented > resumed.totalFramesPresented
            }
            assertEquals(listOf(first, second), onFirst.windows.map { it.id })
            assertEquals(0, onFirst.closedWindows)
            assertEquals(1, onFirst.liveLeases)
            screenshot("03-windows-back-to-first")
        } finally {
            scenario.close()
        }
    }

    @Test
    fun t7_idleEngineDoesNotRedrawAndLifecycleCyclesRetainNothing() {
        val scenario = launch()
        try {
            val generation = presentWithFrame().generation
            // The idle measurement starts once the screen is still.
            val idleStart = screenshot("03-idle-start")
            assertFalse(
                "an idle window presents no frame",
                NativeApp.nativeAwaitSurfaceFrames(generation, idleStart.framesPresented + 1, IDLE_WINDOW_MS),
            )
            val idleEnd = NativeApp.surfaceStatus()
            assertEquals(idleStart.totalFramesPresented, idleEnd.totalFramesPresented)
            screenshot("03-idle", "idle_ms=$IDLE_WINDOW_MS frames=0 loop_wakeups=${idleEnd.loopWakeups - idleStart.loopWakeups}")

            var baseline: SurfaceStatus? = null
            var baselineThreads = emptyList<String>()
            repeat(CYCLES) { cycle ->
                scenario.moveToState(Lifecycle.State.CREATED)
                awaitRetired()
                val resumeStarted = SystemClock.elapsedRealtime()
                scenario.moveToState(Lifecycle.State.RESUMED)
                val shown = presentWithFrame()
                val resumeMs = SystemClock.elapsedRealtime() - resumeStarted
                receipt("03-cycle-$cycle", "resume_to_frame_ms=$resumeMs")
                assertEquals("cycle $cycle holds one native window", 1, shown.liveLeases)
                if (baseline == null) {
                    baseline = shown
                    baselineThreads = TerminalHarness.threads()
                } else {
                    assertEquals("cycle $cycle logical windows", baseline!!.windows, shown.windows)
                    assertEquals("cycle $cycle bound window", baseline!!.boundWindow, shown.boundWindow)
                    val threads = TerminalHarness.threads()
                    Log.i(TerminalHarness.TAG, "cycle $cycle threads gained=${threads - baselineThreads} lost=${baselineThreads - threads}")
                    assertEquals("cycle $cycle threads", baselineThreads, threads)
                }
            }
            assertEquals(0, NativeApp.surfaceStatus().closedWindows)
        } finally {
            scenario.close()
        }
        val closed = awaitRetired()
        assertEquals(2, closed.windows.size)
        receipt("03-closed")
    }

    companion object {
        /** The idle measurement itself; not a synchronisation delay. */
        const val IDLE_WINDOW_MS = 5_000L
        const val CYCLES = 5
    }
}
