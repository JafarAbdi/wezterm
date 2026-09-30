package org.wezterm.android

import android.app.UiAutomation
import android.content.Intent
import android.os.ParcelFileDescriptor
import android.util.Log
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Suite `surface` in ci/android.sh: real `TermWindow` frames on the
 * `SurfaceView`, stale-generation rejection, zero-size waits, a real resize,
 * and an ordered retire/resume of one logical window.
 */
@RunWith(AndroidJUnit4::class)
class SurfaceTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()

    private fun shell(command: String): String {
        val fd = instrumentation.uiAutomation.executeShellCommand(command)
        return ParcelFileDescriptor.AutoCloseInputStream(fd).use { it.readBytes().toString(Charsets.UTF_8) }
    }

    private fun receipt(phase: String): SurfaceStatus {
        val status = NativeApp.surfaceStatus()
        Log.i(TAG, "receipt phase=$phase ${status.rawJson}")
        return status
    }

    private fun screenshot(name: String): SurfaceStatus {
        shell("screencap -p $SHOTS/$name.png")
        return receipt(name)
    }

    private fun awaitState(state: String, generation: Long = 0): SurfaceStatus {
        assertTrue(
            "surface did not reach $state (generation $generation) within ${TIMEOUT_MS}ms: ${NativeApp.surfaceStatus().rawJson}",
            NativeApp.nativeAwaitSurfaceState(state, generation, TIMEOUT_MS),
        )
        return NativeApp.surfaceStatus()
    }

    private fun awaitFrames(generation: Long, minFrames: Long): SurfaceStatus {
        assertTrue(
            "fewer than $minFrames frames on generation $generation within ${TIMEOUT_MS}ms: ${NativeApp.surfaceStatus().rawJson}",
            NativeApp.nativeAwaitSurfaceFrames(generation, minFrames, TIMEOUT_MS),
        )
        return NativeApp.surfaceStatus()
    }

    private fun awaitRenderFailures(stage: Int, minFailures: Long) {
        assertTrue(
            "fewer than $minFailures failures of stage $stage within ${TIMEOUT_MS}ms: ${NativeApp.surfaceStatus().rawJson}",
            NativeApp.nativeAwaitRenderFailures(stage, minFailures, TIMEOUT_MS),
        )
    }

    /** Wait frame by frame until the presented geometry satisfies [orientation]. */
    private fun awaitOrientation(generation: Long, landscape: Boolean): SurfaceStatus {
        var status = NativeApp.surfaceStatus()
        var frames = status.framesPresented
        repeat(MAX_ROTATION_FRAMES) {
            if (status.state == "present" && (status.width > status.height) == landscape) return status
            status = awaitFrames(generation, frames + 1)
            frames = status.framesPresented
        }
        error("surface never presented landscape=$landscape: ${status.rawJson}")
    }

    @Test
    fun rendersRetiresAndResumesOneLogicalWindow() {
        shell("mkdir -p $SHOTS")
        val overrides = InstrumentationRegistry.getArguments().getString("configOverrides") ?: ""
        Log.i(TAG, "launching with config overrides: ${overrides.ifEmpty { "(none)" }}")
        val intent = Intent(instrumentation.targetContext, TerminalActivity::class.java)
            .putExtra(TerminalActivity.EXTRA_CONFIG_OVERRIDES, overrides)
        val scenario = ActivityScenario.launch<TerminalActivity>(intent)
        try {
            val first = awaitState("present")
            val gen1 = first.generation
            val text = awaitFrames(gen1, 1)
            assertEquals("running", text.engine)
            assertEquals(1, text.liveLeases)
            assertTrue("drawable size ${text.width}x${text.height}", text.width > 0 && text.height > 0)
            assertNotNull("a logical window is bound", text.boundWindow)
            screenshot("02-text")

            val stale = gen1 + 1000
            NativeApp.nativeSurfaceChanged(stale, 7, 7)
            assertTrue("stale destroy is acknowledged at once", NativeApp.nativeSurfaceDestroyed(stale))
            val afterStale = screenshot("02-stale")
            assertEquals(gen1, afterStale.generation)
            assertEquals("present", afterStale.state)
            assertEquals(text.width, afterStale.width)
            assertEquals(text.height, afterStale.height)
            assertEquals(2, afterStale.staleEvents)
            assertEquals(1, afterStale.liveLeases)

            NativeApp.nativeSurfaceChanged(gen1, 0, 0)
            val unsized = awaitState("unsized", gen1)
            val framesAtZero = unsized.framesPresented
            assertFalse(
                "no frame may be presented while the surface has no size",
                NativeApp.nativeAwaitSurfaceFrames(gen1, framesAtZero + 1, ABSENCE_WINDOW_MS),
            )
            assertEquals(1, unsized.liveLeases)
            screenshot("02-zero")
            NativeApp.nativeSurfaceChanged(gen1, text.width, text.height)
            awaitState("present", gen1)
            awaitFrames(gen1, framesAtZero + 1)

            instrumentation.uiAutomation.setRotation(UiAutomation.ROTATION_FREEZE_90)
            val landscape = awaitOrientation(gen1, landscape = true)
            assertEquals("resize keeps the generation", gen1, landscape.generation)
            screenshot("02-resize")
            instrumentation.uiAutomation.setRotation(UiAutomation.ROTATION_FREEZE_0)
            awaitOrientation(gen1, landscape = false)

            scenario.moveToState(Lifecycle.State.CREATED)
            val absent = awaitState("absent")
            assertEquals("every lease released before the surface returned", 0, absent.liveLeases)
            assertEquals(1, absent.retireAcks)
            assertEquals("logical window survives surface loss", text.boundWindow, absent.boundWindow)
            receipt("02-retire")

            scenario.moveToState(Lifecycle.State.RESUMED)
            val resumed = awaitState("present")
            assertEquals(gen1 + 1, resumed.generation)
            val resumedFrames = awaitFrames(resumed.generation, 1)
            assertEquals("same logical window is bound", text.boundWindow, resumedFrames.boundWindow)
            assertEquals(1, resumedFrames.liveLeases)
            screenshot("02-resume")

            NativeApp.nativeDiagnosticFault(NativeApp.STAGE_GPU_CREATION)
            scenario.moveToState(Lifecycle.State.CREATED)
            assertEquals(2, awaitState("absent").retireAcks)
            scenario.moveToState(Lifecycle.State.RESUMED)
            val surfaceless = awaitState("present")
            assertEquals(gen1 + 2, surfaceless.generation)
            awaitRenderFailures(NativeApp.STAGE_GPU_CREATION, 1)
            val gpuError = screenshot("02-gpu-error")
            assertEquals("engine survives GPU creation failure", "running", gpuError.engine)
            assertEquals("logical window survives GPU creation failure", text.boundWindow, gpuError.boundWindow)
            assertEquals("no frame after GPU creation failed", 0, gpuError.framesPresented)
            assertEquals(1, gpuError.liveLeases)

            scenario.moveToState(Lifecycle.State.CREATED)
            val retiredSurfaceless = awaitState("absent")
            assertEquals("a surface without GPU state retires cleanly", 0, retiredSurfaceless.liveLeases)
            assertEquals(3, retiredSurfaceless.retireAcks)
            assertEquals(resumedFrames.totalFramesPresented, retiredSurfaceless.totalFramesPresented)
            scenario.moveToState(Lifecycle.State.RESUMED)
            val recovered = awaitState("present")
            assertEquals(gen1 + 3, recovered.generation)
            val recoveredFrames = awaitFrames(recovered.generation, 1)
            assertEquals("same logical window renders again", text.boundWindow, recoveredFrames.boundWindow)
            screenshot("02-gpu-recovered")

            NativeApp.nativeDiagnosticFault(NativeApp.STAGE_DRAW)
            NativeApp.nativeSurfaceChanged(recovered.generation, recoveredFrames.width, recoveredFrames.height - 1)
            awaitRenderFailures(NativeApp.STAGE_DRAW, 1)
            val drawError = receipt("02-draw-error")
            assertEquals("engine survives a draw failure", "running", drawError.engine)
            assertEquals("logical window survives a draw failure", text.boundWindow, drawError.boundWindow)
            assertEquals("present", drawError.state)
            NativeApp.nativeSurfaceChanged(recovered.generation, recoveredFrames.width, recoveredFrames.height)
            val drawRecovered = awaitFrames(recovered.generation, drawError.framesPresented + 1)
            assertEquals(1, drawRecovered.drawFailures)
            assertEquals(1, drawRecovered.gpuCreationFailures)
            screenshot("02-draw-recovered")
        } finally {
            instrumentation.uiAutomation.setRotation(UiAutomation.ROTATION_UNFREEZE)
            scenario.close()
        }
        val closed = awaitState("absent")
        assertEquals(0, closed.liveLeases)
        assertEquals(4, closed.retireAcks)
        receipt("02-closed")
    }

    companion object {
        const val TAG = "WezTermSurfaceTest"
        const val SHOTS = "/data/local/tmp/wezterm-surface"
        const val TIMEOUT_MS = 30_000L

        /** Bound for asserting that no frame arrives; the only negative wait in the suite. */
        const val ABSENCE_WINDOW_MS = 1_000L
        const val MAX_ROTATION_FRAMES = 20
    }
}
