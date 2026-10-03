package org.wezterm.android

import android.content.pm.ApplicationInfo
import android.os.SystemClock
import android.system.Os
import android.util.Log
import android.view.View
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.json.JSONArray
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.wezterm.android.SshMuxHarness.Companion.TAG
import org.wezterm.android.SshMuxHarness.Companion.childProcesses
import org.wezterm.android.SshMuxHarness.Companion.fixture
import org.wezterm.android.SshMuxHarness.Companion.sha256
import org.wezterm.android.SshMuxHarness.Companion.sshDir
import org.wezterm.android.TerminalHarness.awaitStatus
import org.wezterm.android.TerminalHarness.instrumentation
import org.wezterm.android.TerminalHarness.launchIntent
import org.wezterm.android.TerminalHarness.shell
import java.io.ByteArrayInputStream
import java.io.File
import java.io.IOException
import java.io.InputStream

/**
 * The connection screen against the owned SSH and mux fixtures: native
 * SSH, native host-key verification, the document picker and the
 * existing SSHMUX client, observed through the real UI.
 *
 * The route to the fixture is the host's own address through `lo`; these
 * are unit and protocol fixture results, never private Tailscale
 * acceptance. Every method runs in a fresh process with fresh app data.
 */
@RunWith(AndroidJUnit4::class)
class SshMuxTest {
    private lateinit var scenario: ActivityScenario<TerminalActivity>
    private lateinit var ui: SshMuxHarness

    @Before
    fun launchTheConnectionScreen() {
        // An install that kept data from an earlier run must not lend this one trust, a key or a profile.
        sshDir().deleteRecursively()
        instrumentation.targetContext.deleteSharedPreferences(ConnectionPanel.PREFERENCES)
        scenario = ActivityScenario.launch(launchIntent())
        ui = SshMuxHarness(scenario)
        awaitStatus("engine running") { it.engine == "running" }
        instrumentation.waitForIdleSync()
    }

    @After
    fun close() {
        scenario.close()
    }

    private fun knownHosts() = File(sshDir(), "known_hosts")

    private fun mode(file: File) = Os.stat(file.path).st_mode and 0x1ff

    private fun heading(id: Int) = instrumentation.targetContext.getString(id)

    /** A picker document as the device stores it; the fixture's documents are ASCII. API 24 has no `sha256sum`. */
    private fun pickedDocument(name: String) = shell("cat /sdcard/Download/${SshMuxHarness.PICKER_DIR}/$name").toByteArray()

    private fun importFixtureKey(name: String) {
        assertEquals(heading(R.string.identity_imported), ui.pickIdentity(name))
        assertTrue("the status reports the identity", NativeApp.connectionStatus().identity)
    }

    @Test
    fun aNormalLaunchShowsTheConnectionScreenAndRefusesAddressesOutsideTheTailnet() {
        val idle = NativeApp.connectionStatus()
        assertEquals("idle", idle.phase)
        assertFalse("no identity before an import", idle.identity)
        assertEquals("the connection screen is shown", View.VISIBLE, ui.onActivity { it.connection.view.visibility })
        val census = ui.censusReceipt("launch")
        assertEquals("no domain exists before a connection", 0, census.getJSONArray("domains").length())
        ui.assertNothingAttached(census)

        val info = instrumentation.targetContext.applicationInfo
        assertEquals("backup is disabled", 0, info.flags and ApplicationInfo.FLAG_ALLOW_BACKUP)
        val rules = instrumentation.targetContext.resources.getXml(R.xml.data_extraction_rules)
        var excludes = 0
        while (rules.next() != org.xmlpull.v1.XmlPullParser.END_DOCUMENT) {
            if (rules.eventType == org.xmlpull.v1.XmlPullParser.START_TAG && rules.name == "exclude") excludes++
        }
        assertEquals("cloud backup and device transfer exclude all five domains", 10, excludes)

        // The emulator's alias of the host loopback, a LAN address and a public one.
        for (outside in listOf("10.0.2.2", "192.168.1.10", "8.8.8.8")) {
            ui.connect(fixture("wezterm_populated"), host = outside)
            instrumentation.waitForIdleSync()
            val error = ui.onActivity { it.connection.host.error?.toString() }
            assertEquals(
                "the address is not a Tailscale address (100.64.0.0/10 or fd7a:115c:a1e0::/48)",
                error,
            )
            assertEquals("no attempt started for $outside", idle.revision, NativeApp.connectionStatus().revision)
        }
        ui.screenshot("04-profile-refused", ui.panel.user, ui.panel.remoteWezterm)
        ui.assertNothingAttached(ui.censusReceipt("refused"))
    }

    @Test
    fun theKeyReadStopsAtItsLimitAndRefusesAZeroRead() {
        val bytes = "0123456789".toByteArray()
        assertEquals("0123", String(readAtMost(ByteArrayInputStream(bytes), 4)))
        assertEquals("0123456789", String(readAtMost(ByteArrayInputStream(bytes), 11)))
        val trickle = object : InputStream() {
            private val source = ByteArrayInputStream(bytes)

            override fun read() = source.read()

            override fun read(buffer: ByteArray, offset: Int, length: Int) = source.read(buffer, offset, minOf(length, 3))
        }
        assertEquals("short reads are joined", "012345678", String(readAtMost(trickle, 9)))
        val stalled = object : InputStream() {
            override fun read() = 0

            override fun read(buffer: ByteArray, offset: Int, length: Int) = 0
        }
        assertEquals("the document returned no bytes", assertThrows(IOException::class.java) { readAtMost(stalled, 4) }.message)
    }

    @Test
    fun anUnknownHostIsRejectedThenTrustedOnceAndWrongPasswordsFailClosed() {
        val populated = fixture("wezterm_populated")

        val asked = ui.connectAndAwait(populated, "host trust prompt") { it.prompt is ConnectionPrompt.HostTrust }
        ui.screenshot("04-host-reject", *ui.privateFields(), ui.promptMessageView())
        ui.answerHostTrust(asked, trust = false)
        val rejected = ui.awaitConnection("rejection") { it.phase == "failed" }
        assertEquals("host_key_rejected", rejected.failureKind)
        assertFalse("rejecting adds no trust", knownHosts().exists())
        ui.awaitMessage(R.string.failure_host_key_rejected)
        ui.assertNothingAttached(ui.censusReceipt("host-rejected"))

        val askedAgain = ui.connectAndAwait(populated, "second host trust prompt") { it.prompt is ConnectionPrompt.HostTrust }
        assertTrue("a new attempt asks with a new prompt", askedAgain.prompt!!.id > asked.prompt!!.id)
        ui.screenshot("04-host-accept", *ui.privateFields(), ui.promptMessageView())
        val trusted = ui.answerHostTrust(askedAgain, trust = true)
        val password = ui.awaitConnection("password prompt") { it.prompt is ConnectionPrompt.Secret }
        assertFalse(
            "a second answer to the same prompt is refused",
            NativeApp.nativeAnswerHostTrust(askedAgain.attempt, trusted.id, false),
        )
        assertFalse(
            "an answer to the rejected attempt's prompt is refused",
            NativeApp.nativeAnswerHostTrust(asked.attempt, asked.prompt!!.id, true),
        )
        val trustFile = knownHosts().readText()
        assertEquals("one host is trusted", 1, trustFile.lines().count { it.isNotBlank() })
        assertTrue("for the fixture endpoint", trustFile.startsWith("[${fixture("host")}]:${fixture("port")} ssh-ed25519 "))
        assertEquals("known_hosts is private", "600", Integer.toOctalString(mode(knownHosts())))
        assertEquals("the ssh directory is private", "700", Integer.toOctalString(mode(sshDir())))

        ui.answerSecret(password, "not-the-password")
        val denied = ui.awaitConnection("authentication failure") { it.phase == "failed" }
        assertEquals("authentication", denied.failureKind)
        ui.awaitMessage(R.string.failure_authentication)
        ui.screenshot("04-auth-fail", *ui.privateFields())
        ui.assertNothingAttached(ui.censusReceipt("auth-failed"))

        val verified = ui.connectAndAwait(populated, "a prompt of the third attempt") { it.prompt != null || it.phase != "attaching" }
        assertTrue("the trusted host is verified without asking: ${verified.prompt?.javaClass?.simpleName}", verified.prompt is ConnectionPrompt.Secret)
        ui.answerSecret(verified, null)
        val cancelled = ui.awaitConnection("cancellation") { it.phase == "failed" }
        assertEquals("authentication_cancelled", cancelled.failureKind)
        assertTrue("trust is unchanged", trustFile == knownHosts().readText())
        ui.assertNothingAttached(ui.censusReceipt("auth-cancelled"))
    }

    @Test
    fun aPendingPromptSurvivesLeavingTheActivityAndIsAnsweredOnce() {
        val asked = ui.connectAndAwait(fixture("wezterm_populated"), "host trust prompt") { it.prompt is ConnectionPrompt.HostTrust }
        val refusedWhileBusy = NativeApp.connect(Profile(fixture("host"), fixture("port"), fixture("user"), ""))
        assertEquals("one connect operation at a time", "busy", (refusedWhileBusy as ConnectOutcome.Refused).status)

        scenario.moveToState(Lifecycle.State.CREATED)
        assertNull("the dialog left with the Activity", ui.onActivity { it.connection.promptDialog })
        val away = NativeApp.connectionStatus()
        assertTrue("the prompt is still pending", asked.prompt == away.prompt)
        assertEquals("nothing answered it", asked.revision, away.revision)

        scenario.moveToState(Lifecycle.State.RESUMED)
        instrumentation.waitForIdleSync()
        assertTrue("the dialog is back", ui.onActivity { it.connection.promptDialog?.isShowing == true })
        scenario.recreate()
        instrumentation.waitForIdleSync()
        assertTrue("a recreated Activity shows it again", ui.onActivity { it.connection.promptDialog?.isShowing == true })
        assertTrue("still the same prompt", asked.prompt == NativeApp.connectionStatus().prompt)

        ui.answerHostTrust(NativeApp.connectionStatus(), trust = true)
        val password = ui.awaitConnection("password prompt") { it.prompt is ConnectionPrompt.Secret }
        assertEquals("the same attempt went on", asked.attempt, password.attempt)
        ui.answerSecret(password, null)
        ui.awaitConnection("cancellation") { it.phase == "failed" && it.failureKind == "authentication_cancelled" }
    }

    @Test
    fun aChangedHostKeyFailsClosedAndKeepsTheTrustedKey() {
        val populated = fixture("wezterm_populated")
        val password = ui.connectTrusting(populated, "password prompt") { it.prompt is ConnectionPrompt.Secret }
        ui.answerSecret(password, null)
        ui.awaitConnection("cancellation") { it.phase == "failed" }
        val trusted = knownHosts().readBytes()

        // The trusted key now differs from the one the server presents,
        // which is what a changed server key looks like to the client.
        val entry = String(trusted).trim().split(' ')
        val changed = "${entry[0]} ${fixture("other_host_key")}\n".toByteArray()
        knownHosts().writeBytes(changed)

        val before = ui.connect(populated)
        val failed = ui.awaitConnection(
            "host key failure",
            never = { it.attempt > before.attempt && it.prompt != null },
        ) { it.attempt > before.attempt && it.phase == "failed" }
        assertEquals("host_key_changed", failed.failureKind)
        ui.awaitMessage(R.string.failure_host_key_changed)
        assertTrue("the trusted key was not overwritten", changed.contentEquals(knownHosts().readBytes()))
        ui.screenshot("04-host-changed", *ui.privateFields())
        ui.assertNothingAttached(ui.censusReceipt("host-changed"))

        // The trusted key is RSA; the server presents Ed25519.
        val otherType = "${entry[0]} ${fixture("other_type_host_key")}\n".toByteArray()
        assertTrue("the other-type key is RSA", String(otherType).contains(" ssh-rsa "))
        knownHosts().writeBytes(otherType)
        val typeBefore = ui.connect(populated)
        val typeFailed = ui.awaitConnection(
            "host key type failure",
            never = { it.attempt > typeBefore.attempt && it.prompt != null },
        ) { it.attempt > typeBefore.attempt && it.phase == "failed" }
        assertEquals("host_key_changed", typeFailed.failureKind)
        ui.awaitMessage(R.string.failure_host_key_changed)
        assertTrue("the trusted RSA key was not overwritten", otherType.contentEquals(knownHosts().readBytes()))
        ui.screenshot("04-host-type-changed", *ui.privateFields())
        ui.assertNothingAttached(ui.censusReceipt("host-type-changed"))

        // Negative control: with the key the server presents trusted again,
        // the same connection passes host verification without asking.
        knownHosts().writeBytes(trusted)
        val control = ui.connectAndAwait(populated, "control prompt") { it.prompt != null || it.phase != "attaching" }
        assertTrue("host verification passed: ${control.failureKind}", control.prompt is ConnectionPrompt.Secret)
        ui.answerSecret(control, null)
        ui.awaitConnection("cancellation") { it.phase == "failed" }
    }

    @Test
    fun anImportedIdentityAttachesToTheExistingPanesAndStartsNothing() {
        val refused = ui.pickIdentity("wezterm-fixture-not-a-key")
        assertEquals(instrumentation.targetContext.getString(R.string.identity_refused, "the file is not an OpenSSH or PEM private key"), refused)
        assertFalse("a refused document imports nothing", File(sshDir(), "identity").exists())
        assertEquals(
            instrumentation.targetContext.getString(R.string.identity_refused, "the file is larger than a private key (1 MiB)"),
            ui.pickIdentity("wezterm-fixture-oversized"),
        )
        assertFalse("a document one byte over the limit imports nothing", File(sshDir(), "identity").exists())
        importFixtureKey("wezterm-fixture-limit")
        assertEquals(
            "a document of exactly the limit is stored whole",
            sha256(pickedDocument("wezterm-fixture-limit")),
            sha256(File(sshDir(), "identity").readBytes()),
        )

        importFixtureKey("wezterm-fixture-key")
        val identity = File(sshDir(), "identity")
        assertEquals("the identity is private", "600", Integer.toOctalString(mode(identity)))
        assertEquals(
            "the stored identity is the picked document",
            sha256(pickedDocument("wezterm-fixture-key")),
            sha256(identity.readBytes()),
        )
        assertEquals(listOf("identity"), sshDir().list()!!.sorted())
        ui.screenshot("04-identity", *ui.privateFields())

        val started = SystemClock.elapsedRealtime()
        val attached = ui.connectTrusting(fixture("wezterm_populated"), "attach") { it.phase != "attaching" }
        assertEquals("attached without a credential prompt: ${attached.failureKind}", "attached", attached.phase)
        val shown = awaitStatus("the laptop's two windows") { it.windows.size == 2 && it.state == "present" && it.framesPresented > 0 }
        Log.i(TAG, "receipt phase=attach attach_to_first_frame_ms=${SystemClock.elapsedRealtime() - started} attempts=${attached.attempt}")
        assertEquals(2, NativeApp.connectionStatus().windows)
        ui.awaitUi("the connection screen gone") { activity -> true.takeIf { activity.connection.view.visibility == View.GONE } }
        val settled = TerminalHarness.screenshot("04-attach")
        shell("mkdir -p ${SshMuxHarness.SHOTS}")
        shell("cp ${TerminalHarness.SHOTS}/04-attach.png ${SshMuxHarness.SHOTS}/04-attach.png")
        assertFalse(
            "an attached, idle pane is not redrawn",
            NativeApp.nativeAwaitSurfaceFrames(settled.generation, settled.framesPresented + 1, 3 * TerminalHarness.SETTLE_WINDOW_MS),
        )
        Log.i(TAG, "receipt phase=idle idle_redraws=0 over_ms=${3 * TerminalHarness.SETTLE_WINDOW_MS}")

        val census = ui.censusReceipt("attached")
        val expected = JSONArray(fixture("populated_panes"))
        val laptopPanes = List(expected.length()) { expected.getJSONObject(it) }
            .map { Triple(it.getInt("window_id"), it.getInt("tab_id"), it.getInt("pane_id")) }.toSet()
        val panes = census.getJSONArray("panes")
        val mirrored = List(panes.length()) { panes.getJSONObject(it) }
        assertTrue("every pane is a client pane", mirrored.all { it.getBoolean("client") })
        assertEquals(
            "the mirrored panes are exactly the laptop's, by the laptop's ids",
            laptopPanes,
            mirrored.map { Triple(it.getInt("remote_window"), it.getInt("remote_tab"), it.getInt("remote_pane")) }.toSet(),
        )
        assertEquals("no pane is mirrored twice", mirrored.size, mirrored.map { it.getInt("pane") }.toSet().size)
        val domains = census.getJSONArray("domains")
        assertEquals("one domain exists", 1, domains.length())
        assertTrue("and it is the attached SSHMUX client", domains.getJSONObject(0).let { it.getBoolean("client") && it.getBoolean("attached") })
        assertEquals("the app started no process", emptyList<String>(), childProcesses())

        // The selector lists the laptop's windows; choosing one binds it and keeps both.
        val other = shown.windows.first { it.id != shown.boundWindow }
        ui.tap(ui.awaitUi("the window selector") { activity -> activity.selector.takeIf { it.visibility == View.VISIBLE } })
        ui.tap(
            ui.awaitUi("the selector's row of the other window") { activity ->
                activity.selectorDialog?.takeIf { it.isShowing }?.listView?.getChildAt(shown.windows.indexOf(other))
            },
        )
        val rebound = awaitStatus("the other window bound") { it.boundWindow == other.id && it.framesPresented > 0 }
        assertEquals(2, rebound.windows.size)
        TerminalHarness.screenshot("04-windows")
        shell("cp ${TerminalHarness.SHOTS}/04-windows.png ${SshMuxHarness.SHOTS}/04-windows.png")
        assertEquals("selecting duplicates and kills nothing", census.getJSONArray("panes").toString(), ui.censusReceipt("selected").getJSONArray("panes").toString())

        // Leaving and returning takes only the surface.
        scenario.moveToState(Lifecycle.State.CREATED)
        awaitStatus("surface retired") { it.state == "absent" && it.liveLeases == 0 }
        assertEquals("attached", NativeApp.connectionStatus().phase)
        scenario.moveToState(Lifecycle.State.RESUMED)
        awaitStatus("surface back") { it.state == "present" && it.framesPresented > 0 }
        assertEquals("the same panes after the surface came back", census.getJSONArray("panes").toString(), ui.censusReceipt("resumed").getJSONArray("panes").toString())
        assertEquals("still the first successful attempt", attached.attempt, NativeApp.connectionStatus().attempt)
    }

    @Test
    fun anEncryptedIdentityAsksForItsPassphraseThroughTheSecretPrompt() {
        importFixtureKey("wezterm-fixture-key-encrypted")
        val cancelledPrompt = ui.connectTrusting(fixture("wezterm_populated"), "passphrase prompt") { it.prompt != null || it.phase != "attaching" }
        assertTrue("the key asks for its passphrase", cancelledPrompt.prompt is ConnectionPrompt.Secret)
        ui.answerSecret(cancelledPrompt, null)
        val cancelled = ui.awaitConnection("passphrase cancellation") { it.phase == "failed" }
        assertEquals("authentication_cancelled", cancelled.failureKind)
        assertFalse("a cancelled passphrase cannot be replayed", NativeApp.nativeAnswerText(cancelledPrompt.attempt, cancelledPrompt.prompt!!.id, "late"))
        ui.assertNothingAttached(ui.censusReceipt("passphrase-cancelled"))

        val passphrase = ui.connectAndAwait(fixture("wezterm_populated"), "fresh passphrase prompt") { it.prompt != null || it.phase != "attaching" }
        assertTrue("the key asks for its passphrase: ${passphrase.failureKind}", passphrase.prompt is ConnectionPrompt.Secret)
        ui.screenshot("04-passphrase", *ui.privateFields(), ui.promptMessageView())
        ui.answerSecret(passphrase, fixture("passphrase"))
        val attached = ui.awaitConnection("attach") { it.prompt?.id != passphrase.prompt!!.id && (it.prompt != null || it.phase != "attaching") }
        assertEquals("the passphrase unlocked the key: ${attached.failureKind} ${attached.prompt?.javaClass?.simpleName}", "attached", attached.phase)
        assertEquals(2, attached.windows)
    }

    @Test
    fun anUnauthorizedIdentityFailsAuthenticationAndAttachesNothing() {
        importFixtureKey("wezterm-fixture-key-unauthorized")
        val password = ui.connectTrusting(fixture("wezterm_populated"), "password prompt") { it.prompt != null || it.phase != "attaching" }
        assertTrue("the server refused the key and asks for a password: ${password.phase}", password.prompt is ConnectionPrompt.Secret)
        ui.answerSecret(password, "not-the-password")
        val denied = ui.awaitConnection("authentication failure") { it.phase == "failed" }
        assertEquals("authentication", denied.failureKind)
        ui.assertNothingAttached(ui.censusReceipt("unauthorized"))
    }

    @Test
    fun aMissingMuxServerIsAVisibleFailureAndNothingIsStarted() {
        importFixtureKey("wezterm-fixture-key")
        val failed = ui.connectTrusting(fixture("wezterm_missing"), "the missing-server failure") { it.phase !in listOf("attaching", "failing") }
        assertEquals("failed", failed.phase)
        assertEquals("server_unavailable", failed.failureKind)
        ui.awaitMessage(R.string.failure_server_unavailable)
        ui.screenshot("04-missing-server", *ui.privateFields())
        ui.assertNothingAttached(ui.censusReceipt("missing-server"))
    }

    @Test
    fun aCodecMismatchIsADistinctFailureAndAttachesNoPanes() {
        importFixtureKey("wezterm-fixture-key")
        val failed = ui.connectTrusting(fixture("wezterm_mismatch"), "the version failure") { it.phase !in listOf("attaching", "failing") }
        assertEquals("failed", failed.phase)
        assertEquals("incompatible_version", failed.failureKind)
        ui.awaitMessage(R.string.failure_incompatible_version)
        assertTrue("the failure names the server's codec", failed.failureMessage.contains("fixture-codec-mismatch (codec version 46)"))
        ui.screenshot("04-version", *ui.privateFields())
        ui.assertNothingAttached(ui.censusReceipt("codec-mismatch"))
    }

    @Test
    fun anEmptyMuxServerShowsTheEmptyStateAndSpawnsNothing() {
        assertEquals("the fixture has a listening empty server", "available", fixture("empty"))
        importFixtureKey("wezterm-fixture-key")
        val attached = ui.connectTrusting(fixture("wezterm_empty"), "attach") { it.phase != "attaching" }
        assertEquals("attached: ${attached.failureKind}", "attached", attached.phase)
        assertEquals("the laptop has no windows", 0, attached.windows)
        ui.awaitMessage(R.string.connection_empty)
        assertEquals("the connection screen stays", View.VISIBLE, ui.onActivity { it.connection.view.visibility })
        ui.screenshot("04-empty")

        val census = ui.censusReceipt("empty")
        assertEquals("no pane was created", 0, census.getJSONArray("panes").length())
        assertEquals("no logical window exists", 0, NativeApp.surfaceStatus().windows.size)
        assertEquals("the app started no process", emptyList<String>(), childProcesses())
        val domains = census.getJSONArray("domains")
        assertEquals(1, domains.length())
        assertTrue(domains.getJSONObject(0).let { it.getBoolean("client") && it.getBoolean("attached") })
        assertEquals("still attached and still empty", "attached" to 0, NativeApp.connectionStatus().let { it.phase to it.windows })
    }
}
