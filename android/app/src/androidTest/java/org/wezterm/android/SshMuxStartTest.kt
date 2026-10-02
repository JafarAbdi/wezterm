package org.wezterm.android

import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.wezterm.android.SshMuxHarness.Companion.fixture
import org.wezterm.android.TerminalHarness.awaitStatus
import org.wezterm.android.TerminalHarness.instrumentation
import org.wezterm.android.TerminalHarness.launchIntent

/**
 * Connect before the GUI engine runs. The profile names the owned fixture,
 * so an attempt this test fails to prevent reaches only the fixture.
 */
@RunWith(AndroidJUnit4::class)
class SshMuxStartTest {
    @Test
    fun connectWhileTheEngineStartsIsRefusedAndStartsNoAttempt() {
        val overrides = InstrumentationRegistry.getArguments().getString("configOverrides") ?: ""
        var engine = ""
        instrumentation.runOnMainSync {
            engine = NativeApp.startTerminal(instrumentation.targetContext, overrides, diagnosticApplet = false)
        }
        val outcome = NativeApp.connect(Profile(fixture("host"), fixture("port"), fixture("user"), fixture("wezterm_populated")))
        val early = NativeApp.connectionStatus()
        assertEquals("the engine was starting when Connect arrived", "starting", JSONObject(engine).getString("status"))
        assertEquals(
            ConnectOutcome.Refused("starting", "the terminal engine is still starting"),
            outcome,
        )
        assertEquals("no attempt exists", "idle" to 0L, early.phase to early.attempt)
        assertFalse("not ready while starting", early.ready)

        awaitStatus("engine running") { it.engine == "running" }
        val running = NativeApp.connectionStatus()
        assertEquals("still no attempt", "idle" to 0L, running.phase to running.attempt)
        assertTrue("ready once running", running.ready)
        ActivityScenario.launch<TerminalActivity>(launchIntent()).use { scenario ->
            var enabled = false
            scenario.onActivity { enabled = it.connection.connect.isEnabled }
            assertTrue("Connect is enabled once the engine runs", enabled)
        }
    }
}
