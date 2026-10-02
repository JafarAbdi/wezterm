package org.wezterm.android

import android.app.AlertDialog
import android.graphics.Rect
import android.os.Build
import android.os.SystemClock
import android.provider.Settings
import android.util.Base64
import android.util.Log
import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import android.view.accessibility.AccessibilityNodeInfo
import android.view.inputmethod.InputMethodManager
import android.widget.EditText
import androidx.test.core.app.ActivityScenario
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.wezterm.android.TerminalHarness.TIMEOUT_MS
import org.wezterm.android.TerminalHarness.instrumentation
import org.wezterm.android.TerminalHarness.shell
import java.io.File
import java.security.MessageDigest
import java.util.Properties

/**
 * Drives the connection screen of the real Activity against the owned
 * fixture `ci/android-sshmux-fixture.sh` started: real touches and key
 * events, the real dialogs and the system document picker.
 *
 * The fixture's address and user are read from the device and never
 * logged; receipts carry ids, kinds and counts only.
 */
class SshMuxHarness(private val scenario: ActivityScenario<TerminalActivity>) {
    companion object {
        const val TAG = "WezTermSshMuxTest"
        const val DEVICE_DIR = "/data/local/tmp/wezterm-sshmux"
        const val SHOTS = "$DEVICE_DIR/shots"

        /** Where the runner puts the fixture keys for the document picker. */
        const val PICKER_DIR = "wezterm-fixture"

        val fixture: Properties by lazy {
            Properties().apply { load(shell("cat $DEVICE_DIR/fixture.properties").reader()) }
                .also { check(it.getProperty("host") != null) { "no fixture.properties on the device; run ci/android-sshmux-fixture.sh up" } }
        }

        fun fixture(key: String): String = checkNotNull(fixture.getProperty(key)) { "fixture.properties lacks $key" }

        /** The fixture host key fingerprint as libssh prints it: SHA-256, hex, colon separated. */
        fun fixtureFingerprintHex(): String {
            val digest = Base64.decode(fixture("host_fingerprint").removePrefix("SHA256:"), Base64.NO_PADDING)
            return digest.joinToString(":") { "%02x".format(it) }
        }

        fun sha256(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }

        fun sshDir() = File(instrumentation.targetContext.filesDir, "ssh")

        /** Direct children of every thread of this process: a local shell or helper would be one. */
        fun childProcesses(): List<String> =
            File("/proc/self/task").listFiles().orEmpty().flatMap { task ->
                runCatching { File(task, "children").readText().trim().split(' ').filter { it.isNotEmpty() } }.getOrDefault(emptyList())
            }

        fun census(): JSONObject = JSONObject(checkNotNull(NativeApp.nativeDiagnosticMux()) { "no GUI thread answered the census" })
    }

    fun <T> onActivity(read: (TerminalActivity) -> T): T {
        var value: Result<T>? = null
        scenario.onActivity { value = runCatching { read(it) } }
        return value!!.getOrThrow()
    }

    val panel get() = onActivity { it.connection }

    /** Block on native connection changes until `done` holds; fails if `never` holds first. */
    fun awaitConnection(
        what: String,
        never: (ConnectionStatus) -> Boolean = { false },
        done: (ConnectionStatus) -> Boolean,
    ): ConnectionStatus {
        val deadline = SystemClock.elapsedRealtime() + TIMEOUT_MS
        var status = NativeApp.connectionStatus()
        while (!done(status)) {
            check(!never(status)) { "$what: reached a forbidden state: phase=${status.phase} prompt=${status.prompt?.javaClass?.simpleName} failure=${status.failureKind}" }
            val left = deadline - SystemClock.elapsedRealtime()
            if (left <= 0) {
                shell("mkdir -p $SHOTS")
                shell("screencap -p $SHOTS/timeout.png")
                error("$what not reached within ${TIMEOUT_MS}ms: phase=${status.phase} prompt=${status.prompt?.javaClass?.simpleName} failure=${status.failureKind}")
            }
            NativeApp.nativeAwaitConnectionChange(status.revision, left)
            status = NativeApp.connectionStatus()
        }
        instrumentation.waitForIdleSync()
        return status
    }

    private fun screenRect(view: View): Rect {
        val origin = IntArray(2)
        instrumentation.runOnMainSync { view.getLocationOnScreen(origin) }
        return Rect(origin[0], origin[1], origin[0] + view.width, origin[1] + view.height)
    }

    /** Tap `view` with a real touch once it can take input. */
    fun tap(view: View) {
        awaitAtRest(view)
        val at = screenRect(view)
        shell("input tap ${at.centerX()} ${at.centerY()}")
        instrumentation.waitForIdleSync()
    }

    /** Bring `view` on screen and tap it. */
    fun tapVisible(view: View) {
        instrumentation.runOnMainSync { view.requestRectangleOnScreen(Rect(0, 0, view.width, view.height), true) }
        tap(view)
    }

    /**
     * Wait until `view` can take input: its window has input focus (a
     * closing picker or dialog still holds it for a moment) and it stopped
     * moving (the soft keyboard and dialogs slide windows around).
     */
    private fun awaitAtRest(view: View) {
        val deadline = SystemClock.elapsedRealtime() + TIMEOUT_MS
        var at = screenRect(view)
        while (true) {
            runCatching { instrumentation.uiAutomation.waitForIdle(300, 5_000) }
            val now = screenRect(view)
            if (now == at && onActivity { view.hasWindowFocus() }) return
            check(SystemClock.elapsedRealtime() < deadline) { "the view never came to rest in a focused window" }
            at = now
        }
    }

    /**
     * Focus `field` with a real tap and type `text` as key events. The soft
     * keyboard a tap opens reorders keys that arrive while it starts, so
     * typing waits until the field stopped moving. What was typed is
     * private: a mismatch reports lengths, never the text.
     */
    fun type(field: EditText, text: String) {
        tapVisible(field)
        check(onActivity { field.isFocused }) { "the field did not take focus" }
        awaitAtRest(field)
        instrumentation.runOnMainSync { field.text.clear() }
        // One key event at a time, each delivered before the next: `adb
        // shell input text` drops and reorders keys of long strings.
        instrumentation.sendStringSync(text)
        instrumentation.waitForIdleSync()
        val arrived = onActivity { field.text.toString() }
        check(arrived == text) { "typed text did not arrive intact: ${arrived.length} of ${text.length} characters, same length differs=${arrived.length == text.length}" }
    }

    /**
     * Close the soft keyboard. An injected Escape would do it while the
     * keyboard is up, but API 24 turns an unhandled Escape into Back.
     */
    private fun hideKeyboard() {
        onActivity { activity ->
            activity.getSystemService(InputMethodManager::class.java)
                .hideSoftInputFromWindow(activity.window.decorView.windowToken, 0)
        }
        instrumentation.waitForIdleSync()
    }

    /** Fill the form with the fixture endpoint and `remoteWezterm`, then tap Connect. */
    fun connect(remoteWezterm: String, host: String = fixture("host")): ConnectionStatus {
        val before = NativeApp.connectionStatus()
        val panel = panel
        type(panel.host, host)
        type(panel.port, fixture("port"))
        type(panel.user, fixture("user"))
        type(panel.remoteWezterm, remoteWezterm)
        hideKeyboard()
        tapVisible(panel.connect)
        return before
    }

    /** Connect and wait for the attempt that tap started. */
    fun connectAndAwait(remoteWezterm: String, what: String, done: (ConnectionStatus) -> Boolean): ConnectionStatus {
        val before = connect(remoteWezterm)
        return awaitConnection(what) { it.attempt > before.attempt && done(it) }
    }

    /**
     * What the UI shows once it shows it. The native status changes before
     * the UI thread hears of it, so wait for the UI to catch up.
     */
    fun <T : Any> awaitUi(what: String, read: (TerminalActivity) -> T?): T {
        val deadline = SystemClock.elapsedRealtime() + TIMEOUT_MS
        while (true) {
            onActivity(read)?.let { return it }
            check(SystemClock.elapsedRealtime() < deadline) { "the UI never showed $what" }
            runCatching { instrumentation.uiAutomation.waitForIdle(100, 1_000) }
        }
    }

    /** Wait until the connection screen's message starts with the string `heading`. */
    fun awaitMessage(heading: Int) {
        val text = instrumentation.targetContext.getString(heading)
        awaitUi("the message '$text'") { activity -> true.takeIf { activity.connection.message.text.startsWith(text) } }
    }

    private fun <T : Any> fromDialog(what: String, read: (AlertDialog) -> T?): T =
        awaitUi("the prompt dialog with $what") { activity -> activity.connection.promptDialog?.takeIf { it.isShowing }?.let(read) }

    fun dialogButton(which: Int): View = fromDialog("its buttons") { it.getButton(which) }

    /** Answer the host-trust dialog with a real tap and return the prompt it showed. */
    fun answerHostTrust(status: ConnectionStatus, trust: Boolean): ConnectionPrompt.HostTrust {
        val prompt = status.prompt as ConnectionPrompt.HostTrust
        assertTrue("the dialog shows the fixture address", prompt.remoteAddress == "${fixture("host")}:${fixture("port")}")
        assertEquals("the dialog shows the fixture host key", fixtureFingerprintHex(), prompt.fingerprint)
        tap(dialogButton(if (trust) AlertDialog.BUTTON_POSITIVE else AlertDialog.BUTTON_NEGATIVE))
        return prompt
    }

    /** Type `secret` into the secret dialog and confirm, or cancel it when `secret` is null. */
    fun answerSecret(status: ConnectionStatus, secret: String?) {
        assertTrue("a secret prompt is pending, not ${status.prompt?.javaClass?.simpleName}", status.prompt is ConnectionPrompt.Secret)
        if (secret == null) {
            tap(dialogButton(AlertDialog.BUTTON_NEGATIVE))
            return
        }
        dialogButton(AlertDialog.BUTTON_POSITIVE)
        type(checkNotNull(onActivity { it.connection.promptInput }), secret)
        tap(dialogButton(AlertDialog.BUTTON_POSITIVE))
    }

    /** Connect, trust the fixture host through the dialog, and wait for what follows the trust decision. */
    fun connectTrusting(remoteWezterm: String, what: String, done: (ConnectionStatus) -> Boolean): ConnectionStatus {
        val asked = connectAndAwait(remoteWezterm, "host trust prompt") { it.prompt is ConnectionPrompt.HostTrust }
        val prompt = answerHostTrust(asked, trust = true)
        return awaitConnection(what) { it.prompt?.id != prompt.id && done(it) }
    }

    private fun findNode(root: AccessibilityNodeInfo?, label: String): AccessibilityNodeInfo? {
        if (root == null) return null
        if (root.text?.toString() == label || root.contentDescription?.toString() == label) return root
        for (i in 0 until root.childCount) findNode(root.getChild(i), label)?.let { return it }
        return null
    }

    /**
     * A finger tap as the touchscreen delivers it. The API 24 document
     * picker ignores the taps `input tap` injects on its list items.
     */
    private fun fingerTap(at: Rect) {
        val down = SystemClock.uptimeMillis()
        val pointer = arrayOf(
            MotionEvent.PointerProperties().apply {
                id = 0
                toolType = MotionEvent.TOOL_TYPE_FINGER
            },
        )
        val coords = arrayOf(
            MotionEvent.PointerCoords().apply {
                x = at.exactCenterX()
                y = at.exactCenterY()
                pressure = 1f
                size = 1f
            },
        )
        for (action in listOf(MotionEvent.ACTION_DOWN, MotionEvent.ACTION_UP)) {
            val event = MotionEvent.obtain(down, SystemClock.uptimeMillis(), action, 1, pointer, coords, 0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0)
            check(instrumentation.uiAutomation.injectInputEvent(event, true)) { "the ${MotionEvent.actionToString(action)} of a tap was not delivered" }
            event.recycle()
        }
    }

    /**
     * Tap "Import SSH key…" and pick `name` in the system document picker:
     * from wherever it opens, through the device's storage root and
     * `Download/$PICKER_DIR`. A picker that lists no device storage root
     * (API 24 hides it) is told to show it through its overflow menu.
     * Returns the message the app shows afterwards.
     */
    fun pickIdentity(name: String): String {
        hideKeyboard()
        tapVisible(panel.importIdentity)
        val automation = instrumentation.uiAutomation
        val device = Settings.Global.getString(instrumentation.targetContext.contentResolver, Settings.Global.DEVICE_NAME) ?: Build.MODEL
        val own = instrumentation.targetContext.packageName
        val deadline = SystemClock.elapsedRealtime() + TIMEOUT_MS
        var rootsOpened = false
        var internalHidden = false
        var internalShown = false
        // The step seen in the previous pass: a node is tapped only once it
        // was found at the same place twice, because the picker's lists and
        // drawer slide in after their window appears.
        var settled: Pair<String, Rect>? = null
        while (true) {
            check(SystemClock.elapsedRealtime() < deadline) { "the document picker never offered $name" }
            runCatching { automation.waitForIdle(300, 5_000) }
            val root = automation.rootInActiveWindow ?: continue
            if (root.packageName == own) continue
            val file = findNode(root, name)
            // Inside the fixture directory only the file itself is a step forward.
            if (file == null && findNode(root, "Files in $PICKER_DIR") != null) continue
            val roots = findNode(root, "Show roots")
            val deviceRoot = findNode(root, device)
            if (rootsOpened && !internalShown && deviceRoot == null && findNode(root, "Open from") != null) internalHidden = true
            val showInternal = findNode(root, "Show internal storage")
            val step = file
                ?: findNode(root, PICKER_DIR)
                ?: findNode(root, "Download")
                ?: deviceRoot?.takeIf { rootsOpened }
                ?: showInternal
                ?: findNode(root, "More options")?.takeIf { internalHidden && !internalShown }
                ?: roots
                ?: continue
            val at = (step.text ?: step.contentDescription).toString() to Rect().also { step.getBoundsInScreen(it) }
            val window = Rect().also { root.getBoundsInScreen(it) }
            if (!window.contains(at.second.centerX(), at.second.centerY())) {
                // A list row partly below the window: scroll it in, then look again.
                step.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_SHOW_ON_SCREEN.id)
                settled = null
                continue
            }
            if (at != settled) {
                settled = at
                continue
            }
            settled = null
            fingerTap(at.second)
            if (step === roots) rootsOpened = true
            if (step === showInternal) internalShown = true
            if (file != null) break
        }
        Log.i(TAG, "receipt picker package=com.android.documentsui picked=$name")
        // The app cleared its import outcome when the picker opened; the
        // import runs on its own thread and reports on the UI thread.
        while (true) {
            check(SystemClock.elapsedRealtime() < deadline) { "the import of $name never reported" }
            val outcome = onActivity { it.connection.importOutcome.text.toString() }
            if (outcome.isNotEmpty()) return outcome
            runCatching { automation.waitForIdle(100, 1_000) }
        }
    }

    /** Screenshot `name`; `private` views are logged as rectangles the host blacks out in the redacted copy. */
    fun screenshot(name: String, vararg private: View) {
        instrumentation.waitForIdleSync()
        shell("mkdir -p $SHOTS")
        shell("screencap -p $SHOTS/$name.png")
        for (view in private) {
            val origin = IntArray(2)
            instrumentation.runOnMainSync { view.getLocationOnScreen(origin) }
            Log.i(TAG, "redact $name ${origin[0]},${origin[1]} ${origin[0] + view.width},${origin[1] + view.height}")
        }
    }

    /** The views of the form that show the address, the user and laptop paths. */
    fun privateFields(): Array<View> = panel.let { arrayOf(it.host, it.user, it.remoteWezterm, it.message) }

    fun promptMessageView(): View = fromDialog("its message") { it.findViewById<View>(android.R.id.message) }

    /** Log the mux census under `phase` and return it. Ids and domain names only. */
    fun censusReceipt(phase: String): JSONObject {
        val census = census()
        Log.i(TAG, "receipt phase=$phase children=${childProcesses().size} census=$census")
        return census
    }

    fun assertNothingAttached(census: JSONObject) {
        assertEquals("no pane exists", 0, census.getJSONArray("panes").length())
        val domains = census.getJSONArray("domains")
        for (i in 0 until domains.length()) {
            val domain = domains.getJSONObject(i)
            assertTrue("only SSHMUX client domains exist: $domain", domain.getBoolean("client"))
            assertEquals("no domain is attached: $domain", false, domain.getBoolean("attached"))
        }
        assertEquals("the app started no process", emptyList<String>(), childProcesses())
        assertEquals("no logical window exists", 0, NativeApp.surfaceStatus().windows.size)
    }
}
