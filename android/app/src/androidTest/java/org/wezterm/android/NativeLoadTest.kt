package org.wezterm.android

import android.os.Build
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/** Suite `native-load` in ci/android.sh. */
@RunWith(AndroidJUnit4::class)
class NativeLoadTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext

    @Test
    fun closureLoadsAndInitializesOnce() {
        val first = NativeApp.initialize(context)
        val second = NativeApp.initialize(context)

        val outcome = first.outcome
        assertTrue("expected ready, got $outcome", outcome is InitOutcome.Ready)
        outcome as InitOutcome.Ready
        assertEquals(1, outcome.engineInitializations)
        assertEquals(first.initCalls + 1, second.initCalls)
        assertEquals(outcome, second.outcome)

        val expectedArch = when (Build.SUPPORTED_ABIS.first()) {
            "arm64-v8a" -> "aarch64"
            "x86_64" -> "x86_64"
            else -> error("unsupported ABI ${Build.SUPPORTED_ABIS.first()}")
        }
        assertEquals(expectedArch, outcome.arch)
        assertTrue("codec version", outcome.codecVersion > 0)
        assertTrue("default font resolved: ${outcome.defaultFont}", outcome.defaultFont.isNotEmpty())
        assertTrue("glyphs rasterized", outcome.rasterizedGlyphs > 0)
        assertTrue("home is app-private", outcome.home.startsWith(context.applicationContext.filesDir.absolutePath))
    }

    @Test
    fun rustPanicIsContainedAsRuntimeException() {
        val panic = assertThrows(RuntimeException::class.java) { NativeApp.nativeDiagnosticFault(0) }
        assertTrue(panic.message ?: "", panic.message!!.contains("diagnostic panic requested"))

        val error = assertThrows(RuntimeException::class.java) { NativeApp.nativeDiagnosticFault(7) }
        assertTrue(error.message ?: "", error.message!!.contains("diagnostic error 7"))
    }
}
