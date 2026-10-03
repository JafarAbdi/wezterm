package org.wezterm.android

import android.app.ActivityManager
import android.app.AlertDialog
import android.os.Build
import android.os.Process
import android.os.SystemClock
import android.util.Log
import android.view.View
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TestName
import org.junit.runner.RunWith
import org.wezterm.android.SshMuxHarness.Companion.TAG
import org.wezterm.android.SshMuxHarness.Companion.fixture
import org.wezterm.android.SshMuxHarness.Companion.sshDir
import org.wezterm.android.TerminalHarness.SETTLE_WINDOW_MS
import org.wezterm.android.TerminalHarness.awaitStatus
import org.wezterm.android.TerminalHarness.instrumentation
import org.wezterm.android.TerminalHarness.launchIntent
import org.wezterm.android.TerminalHarness.shell
import java.io.File

/**
 * Cancellation, loss, reconnection and the process lifecycle of the
 * connection against the owned fixtures, through the real UI.
 *
 * Every method runs in its own process. The second force-stop method
 * finds the data the first one left; every other method starts with fresh
 * app data. The route to the fixture is the host's own address through
 * `lo`; these are unit and protocol fixture results, never private
 * Tailscale acceptance. Transport failures are injected into the app's
 * own socket only.
 */
@RunWith(AndroidJUnit4::class)
class ReconnectTest {
    @get:Rule
    val name = TestName()

    private lateinit var scenario: ActivityScenario<TerminalActivity>
    private lateinit var ui: SshMuxHarness

    private val relaunch get() = File(instrumentation.targetContext.filesDir, "relaunch-expected.json")

    @Before
    fun launchTheConnectionScreen() {
        if (!name.methodName.startsWith("forceStopWhileAttached2")) {
            sshDir().deleteRecursively()
            relaunch.delete()
            instrumentation.targetContext.deleteSharedPreferences(ConnectionPanel.PREFERENCES)
        }
        scenario = ActivityScenario.launch(launchIntent())
        ui = SshMuxHarness(scenario)
        awaitStatus("engine running") { it.engine == "running" }
        instrumentation.waitForIdleSync()
    }

    @After
    fun close() {
        scenario.close()
    }

    private fun heading(id: Int) = instrumentation.targetContext.getString(id)

    private fun knownHosts() = File(sshDir(), "known_hosts")

    private fun processCensus(): JSONObject {
        // Framework objects that hold descriptors (a dismissed dialog's input channel) close when collected.
        Runtime.getRuntime().gc()
        System.runFinalization()
        return JSONObject(checkNotNull(NativeApp.nativeDiagnosticConnection("census")) { "no census" })
    }

    private fun JSONObject.entries(key: String): Map<String, String> =
        getJSONObject(key).let { map -> map.keys().asSequence().associateWith { map.getString(it) } }

    /**
     * Threads (by id) and sockets (by inode) present in `after` but not in
     * `before`. Binder threads (`Binder:` on API 24, `binder:` on API 35)
     * are the framework's pool, which grows with IPC and never shrinks; they
     * serve no connection. A socket is
     * compared by what it is, not by its descriptor number.
     */
    private fun survivors(before: JSONObject, after: JSONObject): Pair<Map<String, String>, Set<String>> {
        val threads = after.entries("threads") - before.entries("threads").keys
        val sockets = after.entries("fds").values.filter { it.startsWith("socket:") }.toSet() -
            before.entries("fds").values.toSet()
        return threads.filterValues { !it.startsWith("binder:", ignoreCase = true) } to sockets
    }

    private fun receipt(phase: String, note: String) {
        Log.i(TAG, "receipt phase=$phase $note")
    }

    /** Log a census whole, its thread and descriptor maps included, in parts under logcat's entry limit. */
    private fun retain(label: String, census: JSONObject) {
        val parts = census.toString().chunked(3000)
        parts.forEachIndexed { index, part -> Log.i(TAG, "census $label part=${index + 1}/${parts.size} $part") }
    }

    /** Assert that nothing of the ended attempt remains: no thread, prompt or mux domain, so a new one can start. */
    private fun assertReleased(status: ConnectionStatus) {
        assertEquals("no thread of the attempt remains", 0, status.workers)
        assertEquals("no prompt remains", null, status.prompt)
        assertEquals("no domain is held", "none", status.domain)
        assertFalse("nothing is closing", status.closing)
        assertEquals("no mux domain remains", emptyList<String>(), domainNames(SshMuxHarness.census()))
        val census = processCensus()
        assertEquals("the census counts no worker", 0, census.getInt("workers"))
        assertFalse(
            "no connection UI thread remains",
            census.entries("threads").values.any { it.startsWith("wezterm-connect") },
        )
    }

    private fun tapCancel() = ui.tapVisible(ui.panel.cancel)

    /** The form fields that show the address, the user and laptop paths; for screenshots whose message is a fixed text. */
    private fun formFields(): Array<View> = ui.panel.let { arrayOf(it.host, it.user, it.remoteWezterm) }

    private fun awaitCancelled(attempt: Long): ConnectionStatus {
        val cancelled = ui.awaitConnection("cancelled", never = { it.phase == "attached" }) {
            it.attempt == attempt && it.phase == "cancelled"
        }
        ui.awaitMessage(R.string.connection_cancelled)
        assertReleased(cancelled)
        return cancelled
    }

    /** The laptop panes of the mirrored client panes: (window, tab, pane) by the laptop's ids. */
    private fun laptopIds(census: JSONObject): Set<Triple<Int, Int, Int>> {
        val panes = census.getJSONArray("panes")
        return List(panes.length()) { panes.getJSONObject(it) }
            .filter { it.getBoolean("client") }
            .map { Triple(it.getInt("remote_window"), it.getInt("remote_tab"), it.getInt("remote_pane")) }
            .toSet()
    }

    private fun localPanes(census: JSONObject): Set<Int> {
        val panes = census.getJSONArray("panes")
        return List(panes.length()) { panes.getJSONObject(it).getInt("pane") }.toSet()
    }

    private fun fixtureIds(key: String): Set<Triple<Int, Int, Int>> {
        val panes = JSONArray(fixture(key))
        return List(panes.length()) { panes.getJSONObject(it) }
            .map { Triple(it.getInt("window_id"), it.getInt("tab_id"), it.getInt("pane_id")) }
            .toSet()
    }

    private fun domains(census: JSONObject): List<JSONObject> =
        census.getJSONArray("domains").let { domains -> List(domains.length()) { domains.getJSONObject(it) } }

    private fun domainNames(census: JSONObject): List<String> = domains(census).map { it.getString("name") }

    private fun attachedDomains(census: JSONObject): List<String> =
        domains(census).filter { it.getBoolean("attached") }.map { it.getString("name") }

    /** Import the fixture key, trust the fixture host once and attach to `remote`. */
    private fun attach(remote: String, windows: Int): ConnectionStatus {
        assertEquals(heading(R.string.identity_imported), ui.pickIdentity("wezterm-fixture-key"))
        val attached = ui.connectTrusting(remote, "attach") { it.phase != "attaching" }
        assertEquals("attached: ${attached.failureKind}", "attached", attached.phase)
        awaitStatus("the laptop's windows shown") { it.windows.size == windows && it.state == "present" && it.framesPresented > 0 }
        return attached
    }

    /** The text of the bound window's active pane, viewport and scrollback. */
    private fun shownText(): String =
        NativeApp.nativeDiagnosticActivePane()?.let { json ->
            val lines = JSONObject(json).getJSONArray("lines")
            List(lines.length()) { lines.getString(it) }.joinToString("\n")
        } ?: ""

    private fun awaitShown(what: String, done: (String) -> Boolean): String {
        var last = ""
        return runCatching { ui.awaitUi(what) { _ -> shownText().also { last = it }.takeIf(done) } }
            .getOrElse { throw IllegalStateException("$it; the pane showed:\n$last") }
    }

    /** Tap Connect (or Reconnect) and wait for that attempt to attach without a prompt; returns it and the milliseconds to its first frame. */
    private fun reconnect(windows: Int): Pair<ConnectionStatus, Long> {
        val before = NativeApp.connectionStatus()
        val started = SystemClock.elapsedRealtime()
        ui.tapVisible(ui.panel.connect)
        val attached = ui.awaitConnection("reattached", never = { it.attempt > before.attempt && (it.prompt != null || it.phase == "failing" || it.phase == "failed") }) {
            it.attempt > before.attempt && it.phase == "attached"
        }
        awaitStatus("the laptop's windows shown again") { it.windows.size == windows && it.state == "present" && it.framesPresented > 0 }
        return attached to SystemClock.elapsedRealtime() - started
    }

    /** Tap the key row's connection key and confirm the disconnect. */
    private fun disconnectFromTheKeyRow() {
        ui.tap(ui.onActivity { activity -> (0 until activity.keys.childCount).map { activity.keys.getChildAt(it) }.single { it.contentDescription == heading(R.string.key_connection_description) } })
        val dialog = ui.awaitUi("the disconnect dialog") { activity -> activity.disconnectDialog?.takeIf { it.isShowing } }
        ui.tap(ui.onActivity { dialog.getButton(AlertDialog.BUTTON_POSITIVE) })
    }

    @Test
    fun aCancelDuringTheTcpConnectEndsEveryThreadOfTheAttempt() {
        val port = fixture("stall_port")
        var before = processCensus()
        for (cycle in 1..2) {
            val previous = NativeApp.connectionStatus()
            ui.connect(fixture("wezterm_populated"), port = port)
            val dialing = ui.awaitConnection("dialing the stalled listener") {
                it.attempt > previous.attempt && it.phase == "attaching" && it.progress.startsWith("Using libssh-rs to connect")
            }
            assertEquals("a running attempt has threads", true, dialing.workers > 0)
            if (cycle == 1) ui.screenshot("06-cancel-connect", *ui.privateFields())
            val cancelAt = SystemClock.elapsedRealtime()
            tapCancel()
            val cancelled = awaitCancelled(dialing.attempt)
            val census = ui.censusReceipt("cancel-connect-$cycle")
            ui.assertNothingAttached(census)
            val after = processCensus()
            val (threads, sockets) = survivors(before, after)
            receipt(
                "cancel-connect-$cycle",
                "attempt=${cancelled.attempt} cancel_to_released_ms=${SystemClock.elapsedRealtime() - cancelAt} " +
                    "threads_before=${before.getJSONObject("threads").length()} threads_after=${after.getJSONObject("threads").length()} " +
                    "fds_before=${before.getJSONObject("fds").length()} fds_after=${after.getJSONObject("fds").length()} " +
                    "new_threads=$threads new_sockets=$sockets workers=${after.getInt("workers")}",
            )
            if (cycle == 2) {
                assertEquals("no thread of the cancelled attempts survives", emptyMap<String, String>(), threads)
                assertEquals("no socket of the cancelled attempts survives", emptySet<String>(), sockets)
            }
            before = after
        }
    }

    @Test
    fun aCancelDuringHostVerificationEndsThePromptAndTrustsNothing() {
        val asked = ui.connectAndAwait(fixture("wezterm_populated"), "host trust prompt") { it.prompt is ConnectionPrompt.HostTrust }
        val prompt = asked.prompt as ConnectionPrompt.HostTrust
        ui.screenshot("06-cancel-host", *ui.privateFields(), ui.promptMessageView())
        ui.tap(ui.dialogButton(AlertDialog.BUTTON_NEUTRAL))
        val cancelled = awaitCancelled(asked.attempt)
        assertEquals("the prompt ended with the attempt", null, cancelled.prompt)
        ui.awaitUi("the prompt dialog gone") { activity -> true.takeIf { activity.connection.promptDialog?.isShowing != true } }
        assertFalse("a late trust is refused", NativeApp.nativeAnswerHostTrust(asked.attempt, prompt.id, true))
        assertFalse("nothing was trusted", knownHosts().exists() && knownHosts().length() > 0)
        ui.assertNothingAttached(ui.censusReceipt("cancel-host"))
        receipt("cancel-host", "attempt=${cancelled.attempt} prompt=${prompt.id} late_answer_accepted=false known_hosts_bytes=${if (knownHosts().exists()) knownHosts().length() else 0}")
    }

    @Test
    fun aCancelDuringAuthenticationWipesTheSecretPromptAndAttachesNothing() {
        val asked = ui.connectTrusting(fixture("wezterm_populated"), "password prompt") { it.prompt is ConnectionPrompt.Secret }
        val prompt = asked.prompt as ConnectionPrompt.Secret
        ui.dialogButton(AlertDialog.BUTTON_POSITIVE)
        val input = checkNotNull(ui.onActivity { it.connection.promptInput })
        ui.type(input, "not-sent-secret")
        ui.screenshot("06-cancel-auth", *ui.privateFields(), ui.promptMessageView(), input)
        ui.tap(ui.dialogButton(AlertDialog.BUTTON_NEUTRAL))
        val cancelled = awaitCancelled(asked.attempt)
        assertEquals("the typed secret was wiped", "", ui.onActivity { input.text.toString() })
        assertEquals("no prompt remains", null, cancelled.prompt)
        assertFalse("a late secret is refused", NativeApp.nativeAnswerText(asked.attempt, prompt.id, "late"))
        val census = ui.censusReceipt("cancel-auth")
        ui.assertNothingAttached(census)
        val change = NativeApp.nativeAwaitConnectionChange(cancelled.revision, 3 * SETTLE_WINDOW_MS)
        assertEquals("the cancelled attempt changes nothing later", cancelled.revision, change)
        assertEquals("and attaches nothing later", emptyList<String>(), attachedDomains(ui.censusReceipt("cancel-auth-later")))
        receipt("cancel-auth", "attempt=${cancelled.attempt} prompt=${prompt.id} late_answer_accepted=false later_changes=0")
    }

    private fun cancelledNegotiation(remote: String, progress: String, shot: String) {
        assertEquals(heading(R.string.identity_imported), ui.pickIdentity("wezterm-fixture-key"))
        val stalled = ui.connectTrusting(remote, "stalled at '$progress'") { it.phase == "attaching" && it.progress.startsWith(progress) }
        ui.screenshot(shot, *formFields())
        tapCancel()
        val cancelled = awaitCancelled(stalled.attempt)
        val census = ui.censusReceipt(shot)
        ui.assertNothingAttached(census)
        assertEquals("no pane mapping was published", emptySet<Int>(), localPanes(census))
        receipt(shot, "attempt=${cancelled.attempt} stalled_at='$progress'")
    }

    @Test
    fun aCancelDuringTheVersionCheckPublishesNothing() {
        cancelledNegotiation(fixture("wezterm_stall_version"), "Checking server version", "06-cancel-version")
    }

    @Test
    fun aCancelDuringThePaneListPublishesNoPaneMapping() {
        cancelledNegotiation(fixture("wezterm_stall_list"), "Version check OK!  Requesting pane list", "06-cancel-list")
    }

    @Test
    fun aLostConnectionIsDisconnectedAndAnExplicitReconnectShowsTheSamePanesWithoutReplay() {
        val first = attach(fixture("wezterm_reconnect"), windows = 1)
        val laptop = fixtureIds("reconnect_panes")
        val attachedCensus = ui.censusReceipt("attached")
        assertEquals("the laptop's panes, by the laptop's ids", laptop, laptopIds(attachedCensus))
        assertTrue(NativeApp.nativeInputCommit(0, -1, 0, "ab", 0))
        awaitShown("the laptop capture of 'ab'") { it.contains(" 61 62") }

        val before = NativeApp.surfaceStatus().input
        assertTrue(NativeApp.nativeDiagnosticGui("hold-input"))
        assertTrue("input queued before the loss", NativeApp.nativeInputCommit(0, -1, 0, "QZ", 0))
        assertEquals("true", NativeApp.nativeDiagnosticConnection("interrupt-transport"))
        val lost = ui.awaitConnection("disconnected", never = { it.attempt != first.attempt }) { it.phase == "disconnected" }
        assertEquals("the connection was lost, not ended by the user", "lost", lost.cause)
        assertReleased(lost)
        awaitStatus("the laptop's windows gone from the phone") { it.windows.isEmpty() }
        assertTrue("the queued input was held", NativeApp.nativeDiagnosticGui("release-input"))
        assertTrue("input while disconnected", NativeApp.nativeInputCommit(0, -1, 0, "QZ", 0))
        val dropped = awaitStatus("both inputs dropped") { it.input.dropped >= before.dropped + 2 }.input
        assertEquals("both inputs were dropped, nothing delivered", before.copy(dropped = before.dropped + 2), dropped)
        ui.awaitMessage(R.string.connection_lost)
        assertEquals(heading(R.string.connect_reconnect), ui.onActivity { it.connection.connect.text.toString() })
        ui.screenshot("06-disconnected", *formFields())
        val lostCensus = ui.censusReceipt("lost")
        ui.assertNothingAttached(lostCensus)

        val (second, toPaneMs) = reconnect(windows = 1)
        val reattached = ui.censusReceipt("reattached")
        assertEquals("the same laptop panes are shown again", laptop, laptopIds(reattached))
        assertEquals("through a fresh mapping", emptySet<Int>(), localPanes(reattached) intersect localPanes(attachedCensus))
        assertEquals("only the new domain is registered", listOf("laptop-${second.attempt}"), domainNames(reattached))
        val shown = awaitShown("the capture pane again") { it.contains(" 61 62") }
        assertFalse("nothing typed during the loss reached the laptop", shown.contains(" 51 5a"))
        TerminalHarness.screenshot("06-reconnect")
        shell("cp ${TerminalHarness.SHOTS}/06-reconnect.png ${SshMuxHarness.SHOTS}/06-reconnect.png")
        assertTrue(NativeApp.nativeInputCommit(0, -1, 0, "c", 0))
        val typed = awaitShown("typing through the new connection") { it.contains(" 63") }
        assertFalse("still nothing replayed", typed.contains(" 51 5a"))
        receipt(
            "reconnect",
            "first=${first.attempt} second=${second.attempt} reconnect_to_pane_ms=$toPaneMs replayed_inputs=0 " +
                "dropped=${dropped.dropped - before.dropped} prompts_during_reconnect=0",
        )

        disconnectFromTheKeyRow()
        val ended = ui.awaitConnection("disconnected by the user") { it.attempt == second.attempt && it.phase == "disconnected" }
        assertEquals("user", ended.cause)
        assertReleased(ended)
        ui.awaitMessage(R.string.connection_disconnected)
        ui.assertNothingAttached(ui.censusReceipt("user-disconnected"))
        receipt("user-disconnect", "attempt=${ended.attempt} cause=${ended.cause}")
    }

    @Test
    fun forceStopWhileAttached1AttachesAndRecordsTheLaptopPanes() {
        val attached = attach(fixture("wezterm_reconnect"), windows = 1)
        val ids = laptopIds(ui.censusReceipt("before-force-stop"))
        assertEquals(fixtureIds("reconnect_panes"), ids)
        assertTrue(NativeApp.nativeInputCommit(0, -1, 0, "fs", 0))
        awaitShown("the laptop capture of 'fs'") { it.contains(" 66 73") }
        relaunch.writeText(
            JSONObject()
                .put("pid", Process.myPid())
                .put("attempt", attached.attempt)
                .put("panes", JSONArray(ids.map { JSONArray(listOf(it.first, it.second, it.third)) }))
                .toString(),
        )
        receipt("force-stop-attached", "pid=${Process.myPid()} attempt=${attached.attempt} phase=${NativeApp.connectionStatus().phase}")
    }

    @Test
    fun forceStopWhileAttached2RelaunchAttachesTheSamePanesAgain() {
        val expected = JSONObject(relaunch.readText())
        val previousPid = expected.getInt("pid")
        assertTrue("a new process", previousPid != Process.myPid())
        val exit = if (Build.VERSION.SDK_INT >= 30) {
            instrumentation.targetContext.getSystemService(ActivityManager::class.java)
                .getHistoricalProcessExitReasons(instrumentation.targetContext.packageName, previousPid, 1)
                .firstOrNull()?.let { "reason=${it.reason} description=${it.description}" } ?: "none"
        } else {
            "unavailable-before-api-30"
        }
        receipt("relaunch", "previous_pid=$previousPid pid=${Process.myPid()} previous_exit=$exit")
        val idle = NativeApp.connectionStatus()
        assertEquals("a relaunch attaches nothing by itself", "idle", idle.phase)
        assertTrue("the identity survived", idle.identity)
        assertTrue("the host trust survived", knownHosts().length() > 0)
        ui.assertNothingAttached(ui.censusReceipt("relaunched"))
        val panes = expected.getJSONArray("panes")
        val laptop = List(panes.length()) { panes.getJSONArray(it).let { p -> Triple(p.getInt(0), p.getInt(1), p.getInt(2)) } }.toSet()

        ui.awaitUi("the saved profile") { activity -> true.takeIf { activity.connection.host.text.isNotEmpty() } }
        val (attached, toPaneMs) = reconnect(windows = 1)
        assertEquals("the laptop sessions survived the force-stop", laptop, laptopIds(ui.censusReceipt("relaunch-attached")))
        awaitShown("what was typed before the force-stop, still on the laptop") { it.contains(" 66 73") }
        TerminalHarness.screenshot("06-relaunch")
        shell("cp ${TerminalHarness.SHOTS}/06-relaunch.png ${SshMuxHarness.SHOTS}/06-relaunch.png")
        receipt("relaunch-attached", "attempt=${attached.attempt} reconnect_to_pane_ms=$toPaneMs")
        relaunch.delete()
    }

    @Test
    fun closingTheLastLaptopPaneLeavesAnEmptyConnectionThatReconnectsNothing() {
        val attached = attach(fixture("wezterm_lastpane"), windows = 1)
        assertEquals(fixtureIds("lastpane_panes"), laptopIds(ui.censusReceipt("lastpane-attached")))
        awaitShown("the laptop shell prompt") { text -> text.lines().any { it == "$" } }
        assertTrue(NativeApp.nativeInputCommit(0, -1, 0, "lastpane-cli kill-pane --pane-id \"\$WEZTERM_PANE\"\n", 0))
        val empty = ui.awaitConnection("the empty laptop", never = { it.attempt != attached.attempt || it.phase != "attached" }) {
            it.windows == 0
        }
        awaitStatus("no window on the phone") { it.windows.isEmpty() }
        ui.awaitMessage(R.string.connection_empty)
        ui.screenshot("06-exit", *formFields())
        val settled = NativeApp.nativeAwaitConnectionChange(empty.revision, 3 * SETTLE_WINDOW_MS)
        assertEquals("nothing reconnects or spawns after the deliberate exit", empty.revision, settled)
        val census = ui.censusReceipt("lastpane-empty")
        assertEquals("the connection stays attached and empty", listOf("laptop-${attached.attempt}"), attachedDomains(census))
        assertEquals("its domain stays registered", listOf("laptop-${attached.attempt}"), domainNames(census))
        assertEquals("no pane was spawned in its place", emptySet<Int>(), localPanes(census))
        assertEquals("the engine keeps running", "running", NativeApp.surfaceStatus().engine)
        receipt("lastpane-exit", "attempt=${attached.attempt} auto_reconnects=0 spawned=0 settle_ms=${3 * SETTLE_WINDOW_MS}")

        ui.tapVisible(ui.panel.disconnect)
        val ended = ui.awaitConnection("disconnected from the empty laptop") { it.phase == "disconnected" }
        assertEquals("user", ended.cause)
        assertReleased(ended)
        ui.awaitMessage(R.string.connection_disconnected)
    }

    @Test
    fun networkLossSurfaceLossAndALateAnswerAreRefusedWithoutDeadlockOrKill() {
        assertEquals(heading(R.string.identity_imported), ui.pickIdentity("wezterm-fixture-key"))
        val asked = ui.connectAndAwait(fixture("wezterm_reconnect"), "host trust prompt") { it.prompt is ConnectionPrompt.HostTrust }
        val prompt = asked.prompt as ConnectionPrompt.HostTrust
        val shown = NativeApp.surfaceStatus()
        shell("input keyevent KEYCODE_HOME")
        scenarioStopped()
        assertEquals("true", NativeApp.nativeDiagnosticConnection("interrupt-transport"))
        assertTrue("the answer arrives after the network loss", NativeApp.nativeAnswerHostTrust(asked.attempt, prompt.id, true))
        val failed = ui.awaitConnection("the attempt ended and its threads with it", never = { it.phase == "attached" }) {
            it.attempt == asked.attempt && it.phase == "failed" && !it.closing
        }
        assertReleased(failed)
        assertFalse("a second late answer is refused", NativeApp.nativeAnswerHostTrust(asked.attempt, prompt.id, true))
        assertFalse("a trust answered over a lost transport is not saved", knownHosts().exists() && knownHosts().length() > 0)
        receipt("race-prompt", "attempt=${asked.attempt} failure=${failed.failureKind} known_hosts_bytes=0 generation_before=${shown.generation}")

        reopen(shown)
        ui.awaitUi("the failure shown") { activity -> true.takeIf { activity.connection.view.visibility == View.VISIBLE } }
        ui.screenshot("06-race", *ui.privateFields())

        val attached = ui.connectTrusting(fixture("wezterm_reconnect"), "attach") { it.phase != "attaching" }
        assertEquals("attached: ${attached.failureKind}", "attached", attached.phase)
        awaitStatus("the laptop window shown") { it.windows.size == 1 && it.state == "present" && it.framesPresented > 0 }
        val laptop = laptopIds(ui.censusReceipt("race-attached"))
        assertEquals(fixtureIds("reconnect_panes"), laptop)
        val front = NativeApp.surfaceStatus()
        shell("input keyevent KEYCODE_BACK")
        scenarioStopped()
        assertEquals("true", NativeApp.nativeDiagnosticConnection("interrupt-transport"))
        val lost = ui.awaitConnection("lost while in the background") { it.attempt == attached.attempt && it.phase == "disconnected" }
        assertEquals("lost", lost.cause)
        assertReleased(lost)
        reopen(front)
        ui.awaitMessage(R.string.connection_lost)
        val (again, toPaneMs) = reconnect(windows = 1)
        assertEquals("no pane of the laptop was killed", laptop, laptopIds(ui.censusReceipt("race-reattached")))
        val surface = NativeApp.surfaceStatus()
        receipt(
            "race-attached",
            "attempt=${again.attempt} reconnect_to_pane_ms=$toPaneMs stale_events=${surface.staleEvents} live_leases=${surface.liveLeases}",
        )
    }

    /**
     * Three rounds of a connect cancelled while dialing and an attach the user
     * disconnects. After every completed close no worker, prompt or mux domain
     * remains, and after the third round no thread or socket remains that the
     * first round's close did not leave. Every attach shows the laptop's same
     * panes through a fresh domain.
     */
    @Test
    fun repeatedCancelsDisconnectsAndReconnectsLeaveNothingBehind() {
        assertEquals(heading(R.string.identity_imported), ui.pickIdentity("wezterm-fixture-key"))
        val laptop = fixtureIds("reconnect_panes")
        var first: JSONObject? = null
        for (round in 1..3) {
            retain("round-$round-before", processCensus())
            val previous = NativeApp.connectionStatus()
            ui.connect(fixture("wezterm_reconnect"), port = fixture("stall_port"))
            val dialing = ui.awaitConnection("round $round dialing the stalled listener") {
                it.attempt > previous.attempt && it.phase == "attaching" && it.progress.startsWith("Using libssh-rs to connect")
            }
            tapCancel()
            val cancelled = awaitCancelled(dialing.attempt)
            ui.assertNothingAttached(ui.censusReceipt("cycle-$round-cancelled"))

            val attached = if (round == 1) {
                ui.connectTrusting(fixture("wezterm_reconnect"), "round 1 attach") { it.phase != "attaching" }
            } else {
                ui.connect(fixture("wezterm_reconnect"))
                ui.awaitConnection("round $round attach", never = { it.attempt > cancelled.attempt && (it.prompt != null || it.phase == "failing" || it.phase == "failed") }) {
                    it.attempt > cancelled.attempt && it.phase == "attached"
                }
            }
            assertEquals("attached: ${attached.failureKind}", "attached", attached.phase)
            awaitStatus("round $round: the laptop's window shown") { it.windows.size == 1 && it.state == "present" && it.framesPresented > 0 }
            val census = ui.censusReceipt("cycle-$round-attached")
            assertEquals("round $round: the laptop's same panes", laptop, laptopIds(census))
            assertEquals("round $round: only this attempt's domain", listOf("laptop-${attached.attempt}"), domainNames(census))

            disconnectFromTheKeyRow()
            val ended = ui.awaitConnection("round $round disconnected") { it.attempt == attached.attempt && it.phase == "disconnected" }
            assertEquals("user", ended.cause)
            assertReleased(ended)
            ui.assertNothingAttached(ui.censusReceipt("cycle-$round-disconnected"))

            val after = processCensus()
            retain("round-$round-after", after)
            val (threads, sockets) = survivors(first ?: after, after)
            receipt(
                "cycle-$round",
                "cancelled=${cancelled.attempt} attached=${attached.attempt} workers=${ended.workers} domain=${ended.domain} " +
                    "prompts=0 mux_domains=0 threads=${after.getJSONObject("threads").length()} fds=${after.getJSONObject("fds").length()} " +
                    "new_threads=$threads new_sockets=$sockets",
            )
            if (round == 3) {
                assertEquals("no thread survives the rounds", emptyMap<String, String>(), threads)
                assertEquals("no socket survives the rounds", emptySet<String>(), sockets)
            }
            if (first == null) first = after
        }
    }

    /**
     * The GUI engine fails (a debug panic in its surface-lost handler) while
     * an attempt waits for the laptop's version answer with its domain
     * registered. The stage-03 failure parks the GUI thread with its tasks,
     * the attach among them; the attempt's threads still end, its connection
     * UI thread too, and its domain is reported stranded, not gone.
     */
    @Test
    fun anEngineFailureDuringAnAttemptEndsItsThreadsAndStrandsItsDomain() {
        assertEquals(heading(R.string.identity_imported), ui.pickIdentity("wezterm-fixture-key"))
        val stalled = ui.connectTrusting(fixture("wezterm_stall_version"), "stalled at the version check") {
            it.phase == "attaching" && it.progress.startsWith("Checking server version")
        }
        assertEquals("the attempt holds its domain", "registered", stalled.domain)
        assertTrue("the attempt has threads", stalled.workers > 0)
        val (ended, generation) = failTheEngineAndAwaitTheThreads(stalled.attempt)
        receipt(
            "engine-failure",
            "attempt=${ended.attempt} failure=${ended.failureKind} workers=${ended.workers} domain=${ended.domain} " +
                "connect_ui_threads=0 workers_before=${stalled.workers} generation=$generation",
        )
    }

    /**
     * The GUI engine fails while the attempt's connection UI holds an input
     * request it has not registered as a prompt: a debug hold makes its
     * consumer take the password request and then stop on the engine's
     * end, as when its select takes the end while that request is queued.
     * Nobody answers the dropped request, yet the session thread blocked on
     * it resumes, so every thread of the attempt still ends.
     */
    @Test
    fun anEngineFailureWithAnUnregisteredInputRequestStillEndsEveryThread() {
        assertEquals("true", NativeApp.nativeDiagnosticConnection("hold-next-input"))
        val held = ui.connectTrusting(fixture("wezterm_populated"), "the password request held") {
            it.phase == "attaching" && it.progress == HELD_INPUT
        }
        assertEquals("the request never became a prompt", null, held.prompt)
        assertEquals("the attempt holds its domain", "registered", held.domain)
        assertTrue("the attempt has threads", held.workers > 0)
        val (ended, generation) = failTheEngineAndAwaitTheThreads(held.attempt)
        receipt(
            "engine-failure-held-input",
            "attempt=${ended.attempt} failure=${ended.failureKind} workers=${ended.workers} domain=${ended.domain} " +
                "connect_ui_threads=0 prompts=0 workers_before=${held.workers} generation=$generation",
        )
    }

    /**
     * Fail the GUI engine with a debug panic in its surface-lost handler,
     * which the stage-03 failure answers by parking the GUI thread with its
     * tasks, and wait until `attempt` failed with every thread ended and
     * its domain stranded. Returns that status and the failed surface's
     * generation.
     */
    private fun failTheEngineAndAwaitTheThreads(attempt: Long): Pair<ConnectionStatus, Long> {
        // A window with a presented surface, so that a surface-lost handler exists to fail.
        assertTrue(NativeApp.nativeDiagnosticGui("open-window"))
        val shown = awaitStatus("a frame on a present surface") { it.state == "present" && it.framesPresented >= 1 }
        assertTrue(NativeApp.nativeDiagnosticGui("panic-in-surface-lost"))
        scenario.moveToState(Lifecycle.State.CREATED)
        val failed = awaitStatus("engine failed and native window released") { it.engine == "failed" && it.liveLeases == 0 }
        assertTrue(failed.engineMessage, failed.engineMessage.contains("injected SurfaceLost handler panic"))
        val ended = ui.awaitConnection("the attempt failed and its threads ended") {
            it.attempt == attempt && it.phase == "failed" && !it.closing
        }
        assertEquals("engine_ended", ended.failureKind)
        assertEquals("no thread of the attempt remains", 0, ended.workers)
        assertEquals("no prompt remains", null, ended.prompt)
        assertEquals("the domain is unreachable, not counted as gone", "stranded", ended.domain)
        val census = processCensus()
        assertEquals("the census counts no worker", 0, census.getInt("workers"))
        val connectUi = census.entries("threads").values.filter { it.startsWith("wezterm-connect") }
        assertEquals("no connection UI thread remains", emptyList<String>(), connectUi)
        return ended to shown.generation
    }

    /** The Activity left the screen and its surface was released. */
    private fun scenarioStopped() {
        awaitStatus("the surface released") { it.state == "absent" && it.liveLeases == 0 }
    }

    /** Bring the app back as the launcher does and wait for a newer surface than `before`. */
    private fun reopen(before: SurfaceStatus) {
        val launch = instrumentation.targetContext.packageManager.getLaunchIntentForPackage(instrumentation.targetContext.packageName)!!
        instrumentation.targetContext.startActivity(launch)
        awaitStatus("shown again") { it.state == "present" && it.generation > before.generation }
        // Until its window has focus the system may still show the task's snapshot from before.
        ui.awaitUi("the reopened window focused") { activity -> true.takeIf { activity.hasWindowFocus() } }
        instrumentation.waitForIdleSync()
    }

    private companion object {
        /** The progress of an attempt whose input request is held: `HELD_INPUT` in `sshmux.rs`. */
        const val HELD_INPUT = "debug: an input request is held until the engine ends"
    }
}
