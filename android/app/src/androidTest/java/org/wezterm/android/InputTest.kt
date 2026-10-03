package org.wezterm.android

import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.accessibilityservice.AccessibilityServiceInfo
import android.content.pm.ActivityInfo
import android.graphics.Rect
import android.os.Build
import android.os.SystemClock
import android.util.Log
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.WindowInsets
import android.view.WindowInsetsAnimation
import android.view.accessibility.AccessibilityNodeInfo
import android.view.accessibility.AccessibilityWindowInfo
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.FixMethodOrder
import org.junit.Test
import org.junit.runner.RunWith
import org.junit.runners.MethodSorters
import org.wezterm.android.SshMuxHarness.Companion.childProcesses
import org.wezterm.android.SshMuxHarness.Companion.fixture
import org.wezterm.android.SshMuxHarness.Companion.sshDir
import org.wezterm.android.TerminalHarness.SETTLE_WINDOW_MS
import org.wezterm.android.TerminalHarness.TIMEOUT_MS
import org.wezterm.android.TerminalHarness.awaitStatus
import org.wezterm.android.TerminalHarness.instrumentation
import org.wezterm.android.TerminalHarness.launchIntent
import org.wezterm.android.TerminalHarness.shell
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Phone input into the laptop panes of the owned `input` mux fixture: a
 * bash shell in one window, in another a capture that prints the hex of
 * every byte it receives (bracketed paste enabled), and in a third two
 * stacked captures: the top one has the laptop focus the bottom one once
 * it read "abc", the bottom one has it focus the top one and back once it
 * read "aba". The shell's `laptop-resize` resizes its pane from the
 * laptop, as a laptop GUI does. Input goes in
 * through the real Activity: injected touches and key events, the key
 * row, and the view's own `InputConnection` called as an IME calls it.
 * Results are what the laptop produced, read back from the pane this
 * phone mirrors, compared with literal expectations.
 *
 * The methods share one process and one connection, in name order. The
 * route is the host's own address through `lo`: protocol fixture results,
 * never private Tailscale acceptance.
 */
@RunWith(AndroidJUnit4::class)
@FixMethodOrder(MethodSorters.NAME_ASCENDING)
class InputTest {
    companion object {
        const val TAG = "WezTermInputTest"
        const val SHOTS = "${SshMuxHarness.DEVICE_DIR}/input-shots"
        private var attached = false

        /** Makes this run's capture markers unlike any an earlier run left on the laptop. */
        private val run = SystemClock.elapsedRealtime() % 100_000
    }

    /** A capture marker for `lane`, unique to this run. */
    private fun mark(lane: String) = "@$lane$run@"


    private lateinit var scenario: ActivityScenario<TerminalActivity>
    private lateinit var ui: SshMuxHarness

    private val laptopPanes get() = JSONArray(fixture("input_panes")).let { panes -> List(panes.length()) { panes.getJSONObject(it) } }

    /** The laptop's pane id in window `window` of the input fixture, which has one pane. */
    private fun laptopPane(window: Int): Int = laptopPanes.single { it.getInt("window_id") == window }.getInt("pane_id")

    /** The laptop window holding laptop pane `pane`. */
    private fun laptopWindow(pane: Int): Int = laptopPanes.single { it.getInt("pane_id") == pane }.getInt("window_id")

    private val shellPane get() = laptopPane(0)
    private val capturePane get() = laptopPane(1)

    /** The two captures of the third window: the laptop moves its focus from the top to the bottom one. */
    private val focusFrom get() = fixture("input_focus_from").toInt()
    private val focusTo get() = fixture("input_focus_to").toInt()

    @Before
    fun attachOnce() {
        if (!attached) {
            sshDir().deleteRecursively()
            instrumentation.targetContext.deleteSharedPreferences(ConnectionPanel.PREFERENCES)
        }
        scenario = ActivityScenario.launch(launchIntent())
        ui = SshMuxHarness(scenario)
        awaitStatus("engine running") { it.engine == "running" }
        instrumentation.waitForIdleSync()
        if (!attached) attach()
        awaitStatus("a pane shown") { it.state == "present" && it.framesPresented > 0 && it.cellHeight > 0 }
    }

    @After
    fun close() {
        ui.hideKeyboard()
        scenario.close()
    }

    private fun attach() {
        assertFalse("no IME before a laptop pane is shown", ui.onActivity { it.surfaceView.onCheckIsTextEditor() })
        val before = NativeApp.surfaceStatus().input
        assertTrue("the engine accepts input", NativeApp.nativeInputCommit(0, -1, 0, "x", 0))
        val dropped = awaitStatus("input without a window dropped") { it.input.dropped > before.dropped }.input
        assertEquals("input without a window reaches nothing", before.copy(dropped = before.dropped + 1), dropped)

        assertEquals(instrumentation.targetContext.getString(R.string.identity_imported), ui.pickIdentity("wezterm-fixture-key"))
        val status = ui.connectTrusting(fixture("wezterm_input"), "attach") { it.phase != "attaching" }
        assertEquals("attached: ${status.failureKind}", "attached", status.phase)
        awaitStatus("the laptop's three windows") { it.windows.size == 3 && it.state == "present" && it.framesPresented > 0 }
        ui.awaitUi("the terminal and its key row") { activity -> true.takeIf { activity.keys.visibility == View.VISIBLE } }
        assertTrue("a shown pane takes IME input", ui.onActivity { it.surfaceView.onCheckIsTextEditor() })
        attached = true
    }

    // ---- observation ----

    /**
     * The bound pane as this phone mirrors it: `lines` are some scrollback
     * rows, then the viewport from `viewportStart`; a `wrapped` row
     * continues on the next.
     */
    data class Pane(
        val local: Int,
        val remote: Int,
        val rows: Int,
        val cols: Int,
        val cursorRow: Int,
        val cursorCol: Int,
        val lines: List<String>,
        val viewportStart: Int,
        val wrapped: List<Boolean>,
    ) {
        /** Viewport row `index`. */
        fun row(index: Int): String? = lines.getOrNull(viewportStart + index)

        /** The text of the viewport with wrapped rows joined. */
        val logical: List<String>
            get() = lines.indices.fold(mutableListOf("")) { acc, i ->
                acc[acc.size - 1] = acc.last() + lines[i]
                if (!wrapped[i]) acc.add("")
                acc
            }.dropLast(1)

        /** The capture pane's bytes, as hex tokens. */
        val bytes get() = logical.joinToString(" ").split(' ').filter { it.isNotEmpty() }
    }

    private fun activePane(): Pane? = NativeApp.nativeDiagnosticActivePane()?.let { json ->
        val root = JSONObject(json)
        val lines = root.getJSONArray("lines")
        val wrapped = root.getJSONArray("wrapped")
        Pane(
            local = root.getInt("pane"),
            remote = root.getInt("remote_pane"),
            rows = root.getInt("rows"),
            cols = root.getInt("cols"),
            cursorRow = root.getInt("cursor_row"),
            cursorCol = root.getInt("cursor_col"),
            lines = List(lines.length()) { lines.getString(it) },
            viewportStart = root.getInt("viewport_start"),
            wrapped = List(wrapped.length()) { wrapped.getBoolean(it) },
        )
    }

    /** Block on surface status changes (frames, input, cursor) until the bound pane satisfies `done`. */
    private fun awaitPane(what: String, done: (Pane) -> Boolean): Pane {
        val deadline = SystemClock.elapsedRealtime() + TIMEOUT_MS
        var status = NativeApp.surfaceStatus()
        var pane: Pane? = null
        while (true) {
            pane = activePane() ?: pane
            if (pane != null && done(pane)) return pane
            val left = deadline - SystemClock.elapsedRealtime()
            check(left > 0) { "$what not seen within ${TIMEOUT_MS}ms; pane ${pane?.remote} ${pane?.cols}x${pane?.rows} cursor ${pane?.cursorRow},${pane?.cursorCol}: ${pane?.logical?.takeLast(8)}" }
            NativeApp.nativeAwaitSurfaceChange(status.revision, left)
            status = NativeApp.surfaceStatus()
        }
    }

    private fun hex(text: String) = text.toByteArray(Charsets.UTF_8).map { "%02x".format(it) }

    /**
     * The surface status once the GUI thread applied every input queued so
     * far: the active-pane query runs as a GUI-thread task queued behind it.
     */
    private fun drained(): SurfaceStatus {
        activePane()
        return NativeApp.surfaceStatus()
    }

    /** The capture's bytes after the last `marker`, once they end with `end` (a carriage return). */
    private fun captured(marker: String, end: List<String> = listOf("0d")): List<String> {
        val start = hex(marker)
        fun after(pane: Pane): List<String>? {
            val bytes = pane.bytes
            val at = bytes.lastIndexOfSublist(start).takeIf { it >= 0 } ?: return null
            return bytes.drop(at + start.size).takeIf { it.takeLast(end.size) == end }
        }
        return after(awaitPane("capture after $marker") { it.remote == capturePane && after(it) != null })!!
    }

    /** Wait for a shell line that is exactly `line`. */
    private fun awaitShellLine(line: String): Pane = awaitPane("shell line '$line'") { it.remote == shellPane && line in it.logical }

    /** Wait for the shell's prompt, with `line` printed right above it. */
    private fun awaitPrompt(line: String): Pane = awaitPane("'$line' above the prompt") {
        it.remote == shellPane && it.row(it.cursorRow) == "\$" && it.cursorCol == 2 && it.row(it.cursorRow - 1) == line
    }

    /**
     * Wait until no frame was presented for [SETTLE_WINDOW_MS]: the window
     * painted what the pane holds, so the cursor cell it reported is current.
     */
    private fun settle(): SurfaceStatus {
        var status = NativeApp.surfaceStatus()
        while (NativeApp.nativeAwaitSurfaceFrames(status.generation, status.framesPresented + 1, SETTLE_WINDOW_MS)) {
            status = NativeApp.surfaceStatus()
        }
        return NativeApp.surfaceStatus()
    }

    private fun receipt(phase: String, note: String = "") {
        val status = NativeApp.surfaceStatus()
        Log.i(TAG, "receipt phase=$phase children=${childProcesses().size} input=${status.input} surface=${status.width}x${status.height} cell=${status.cellWidth}x${status.cellHeight} $note")
    }

    private fun screenshot(name: String) {
        TerminalHarness.screenshot(name)
        shell("mkdir -p $SHOTS")
        shell("cp ${TerminalHarness.SHOTS}/$name.png $SHOTS/$name.png")
    }

    // ---- input, the way the platform delivers it ----

    /**
     * The view's input connection, called on the UI thread as an IME calls
     * it. It is created once the framework finished starting input on the
     * focused terminal, so it is the newest connection, the one the
     * terminal takes edits from; every call checks that it still is.
     */
    private inner class Ime {
        private val view: TerminalView
        private val connection: InputConnection

        init {
            ui.onActivity { it.surfaceView.requestFocus() }
            awaitKeyFocus()
            instrumentation.waitForIdleSync()
            view = ui.onActivity { it.surfaceView }
            connection = ui.onActivity { it.surfaceView.onCreateInputConnection(EditorInfo())!! }
        }

        fun <T> call(action: (InputConnection) -> T): T {
            var result: Result<T>? = null
            instrumentation.runOnMainSync {
                result = runCatching {
                    check(view.isCurrent(connection as TerminalInputConnection)) { "the framework replaced the test's input connection" }
                    action(connection)
                }
            }
            instrumentation.waitForIdleSync()
            return result!!.getOrThrow()
        }

        /** An edit that changes what the laptop holds; wait until the GUI thread applied it. */
        fun applied(edit: (InputConnection) -> Boolean) {
            val before = NativeApp.surfaceStatus().input
            assertTrue(call(edit))
            awaitStatus("edit applied") { it.input.commits > before.commits }
        }

        fun textBefore() = call { it.getTextBeforeCursor(256, 0).toString() }

        /** Commit `text` and wait until the GUI thread applied it. */
        fun commit(text: String) = applied { it.commitText(text, 1) }

        fun compose(text: String) = assertTrue(call { it.setComposingText(text, 1) })

        /** Enter as the soft keyboard sends it, a key event through the connection; wait until it was applied. */
        fun enter() {
            awaitKeyFocus()
            val before = NativeApp.surfaceStatus().input
            // `BaseInputConnection.sendKeyEvent` reports false even when it dispatched the key.
            call { it.sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER)) }
            call { it.sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER)) }
            awaitStatus("Enter applied") { it.input.keys > before.keys }
        }
    }

    /**
     * Wait until key events can reach the terminal: the window has input
     * focus (the platform drops keys sent while it has none, as during a
     * rotation) and the terminal is its focused view.
     */
    private fun awaitKeyFocus() {
        ui.awaitUi("the terminal focused in a focused window") { activity ->
            true.takeIf { activity.hasWindowFocus() && activity.currentFocus === activity.surfaceView }
        }
    }

    /** A key typed on a hardware keyboard: down and up through the window's input pipeline. */
    private fun hardwareKey(code: Int, meta: Int = 0) {
        awaitKeyFocus()
        val now = SystemClock.uptimeMillis()
        for (action in listOf(KeyEvent.ACTION_DOWN, KeyEvent.ACTION_UP)) {
            val event = KeyEvent(now, SystemClock.uptimeMillis(), action, code, 0, meta, -1, 0, 0, InputDevice.SOURCE_KEYBOARD)
            instrumentation.sendKeySync(event)
        }
    }

    /**
     * Hardware key presses dispatched through the Activity, as a keyboard's
     * events reach it, all in one UI-thread turn: nothing the UI thread
     * would run in between (a clipboard answer, a target change) runs.
     */
    private fun hardwareKeysInOneTurn(vararg keys: Pair<Int, Int>) {
        awaitKeyFocus()
        scenario.onActivity { activity ->
            val now = SystemClock.uptimeMillis()
            for ((code, meta) in keys) {
                for (action in listOf(KeyEvent.ACTION_DOWN, KeyEvent.ACTION_UP)) {
                    val event = KeyEvent(now, now, action, code, 0, meta, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD)
                    check(activity.dispatchKeyEvent(event)) { "the terminal did not take key $code" }
                }
            }
        }
    }

    private fun surfaceOrigin(): IntArray = ui.onActivity { activity -> IntArray(2).also { activity.surfaceView.getLocationOnScreen(it) } }

    /** A finger on the screen; `x`, `y` are surface pixels. */
    private inner class Finger(x: Float, y: Float) {
        private val origin = surfaceOrigin()
        private val down = SystemClock.uptimeMillis()

        init {
            inject(MotionEvent.ACTION_DOWN, x, y)
        }

        fun inject(action: Int, x: Float, y: Float) {
            val event = MotionEvent.obtain(down, SystemClock.uptimeMillis(), action, origin[0] + x, origin[1] + y, 0)
            event.source = InputDevice.SOURCE_TOUCHSCREEN
            check(instrumentation.uiAutomation.injectInputEvent(event, true)) { "${MotionEvent.actionToString(action)} not delivered" }
            event.recycle()
        }

        fun move(x: Float, y: Float) = inject(MotionEvent.ACTION_MOVE, x, y)

        fun up(x: Float, y: Float) {
            inject(MotionEvent.ACTION_UP, x, y)
            instrumentation.waitForIdleSync()
        }
    }

    private fun touchesDelivered(since: Long) = awaitStatus("touch delivered") { it.input.touches > since }

    /** Centre of viewport cell `row`, `col` of the bound pane, from the cursor cell its settled window painted. */
    private fun cellCentre(pane: Pane, row: Int, col: Int): Pair<Float, Float> {
        val s = settle()
        val left = s.cursorX - pane.cursorCol * s.cellWidth
        val top = s.cursorY - pane.cursorRow * s.cellHeight
        return (left + (col + 0.5f) * s.cellWidth) to (top + (row + 0.5f) * s.cellHeight)
    }

    private fun tapCell(pane: Pane, row: Int, col: Int) {
        val (x, y) = cellCentre(pane, row, col)
        val before = NativeApp.surfaceStatus().input.touches
        Finger(x, y).up(x, y)
        touchesDelivered(before)
    }

    private fun tapKey(label: Int) {
        val text = instrumentation.targetContext.getString(label)
        val key = ui.onActivity { activity -> (0 until activity.keys.childCount).map { activity.keys.getChildAt(it) as android.widget.Button }.single { it.text == text } }
        ui.tap(key)
    }

    /** Hide the soft keyboard and wait until the surface fills the space it left. */
    private fun hideKeyboardAndAwaitSurface(): SurfaceStatus {
        ui.hideKeyboard()
        val height = ui.onActivity { it.surfaceView.height }
        return awaitStatus("the surface without the keyboard") { it.state == "present" && it.height == ui.onActivity { a -> a.surfaceView.height } && it.height >= height }
    }

    /** Show the laptop window holding `laptopPane` through the selector, with real taps on its next entries. */
    private fun bind(laptopPane: Int): Pane {
        val window = laptopWindow(laptopPane)
        bindShowing("laptop window $window") { shown -> laptopPanes.any { it.getInt("pane_id") == shown.remote && it.getInt("window_id") == window } }
        return awaitPane("laptop pane $laptopPane bound") { it.remote == laptopPane }
    }

    /** Show, through the selector, with real taps on its next entries, the window whose shown pane `shows`. */
    private fun bindShowing(what: String, shows: (Pane) -> Boolean): Pane {
        repeat(NativeApp.surfaceStatus().windows.size) {
            val shown = awaitPane("a laptop pane shown") { true }
            if (shows(shown)) return shown
            ui.tap(ui.onActivity { it.selector })
            val next = ui.awaitUi("the selector dialog") { activity ->
                val dialog = activity.selectorDialog?.takeIf { it.isShowing } ?: return@awaitUi null
                val s = NativeApp.surfaceStatus()
                dialog.listView.getChildAt((s.windows.indexOfFirst { it.id == s.boundWindow } + 1) % s.windows.size)
            }
            ui.tap(next)
            awaitPane("the next window bound") { it.local != shown.local }
        }
        error("no window shows $what")
    }

    /**
     * The installed soft keyboard's key labelled with one of `labels`, as
     * the keyboard itself describes it to accessibility (Gboard as views,
     * LatinIME as virtual nodes): its screen bounds.
     */
    private fun softKey(labels: Set<String>): Rect {
        val automation = instrumentation.uiAutomation
        automation.serviceInfo = automation.serviceInfo.apply { flags = flags or AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS }
        fun find(node: AccessibilityNodeInfo): AccessibilityNodeInfo? {
            if ((node.contentDescription ?: node.text)?.toString() in labels) return node
            return (0 until node.childCount).firstNotNullOfOrNull { i -> node.getChild(i)?.let(::find) }
        }
        val deadline = SystemClock.elapsedRealtime() + TIMEOUT_MS
        while (true) {
            val window = automation.windows.firstOrNull { it.type == AccessibilityWindowInfo.TYPE_INPUT_METHOD }
            val keyboard = window?.root
            keyboard?.let(::find)?.let { key ->
                val bounds = Rect().also(key::getBoundsInScreen)
                Log.i(TAG, "receipt soft-key labels=$labels node=${key.className} bounds=$bounds keyboard=${Rect().also(window::getBoundsInScreen)}")
                return bounds
            }
            check(SystemClock.elapsedRealtime() < deadline) {
                fun shown(node: AccessibilityNodeInfo): List<String> =
                    listOfNotNull((node.contentDescription ?: node.text)?.toString()) +
                        (0 until node.childCount).flatMap { i -> node.getChild(i)?.let(::shown).orEmpty() }
                "the soft keyboard has no key labelled $labels; its window ${if (keyboard == null) "is not listed" else "offers ${shown(keyboard)}"}"
            }
            runCatching { automation.waitForIdle(100, 1_000) }
        }
    }

    /**
     * Open the soft keyboard with a tap on the terminal and wait until it is
     * in place: on API 30 and later the keyboard slides in under the app's
     * insets animation, and touches during it land where it is, not where
     * its keys will be.
     */
    private fun showSoftKeyboard() {
        val placed = CountDownLatch(1)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            ui.onActivity { activity ->
                activity.window.decorView.setWindowInsetsAnimationCallback(object : WindowInsetsAnimation.Callback(DISPATCH_MODE_CONTINUE_ON_SUBTREE) {
                    override fun onProgress(insets: WindowInsets, running: List<WindowInsetsAnimation>) = insets

                    override fun onEnd(animation: WindowInsetsAnimation) {
                        if (animation.typeMask and WindowInsets.Type.ime() != 0) placed.countDown()
                    }
                })
            }
        }
        val (x, y) = ui.onActivity { it.surfaceView.width / 2f to it.surfaceView.height / 4f }
        Finger(x, y).up(x, y)
        ui.awaitUi("the soft keyboard shown") { activity -> true.takeIf { !ui.keyboardHidden(activity) } }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            assertTrue("the soft keyboard's animation ended", placed.await(TIMEOUT_MS, TimeUnit.MILLISECONDS))
            ui.onActivity { it.window.decorView.setWindowInsetsAnimationCallback(null) }
        }
    }

    /** Tap the soft keyboard's key with a real touch and wait until the terminal took what the keyboard did. */
    private fun tapSoftKey(vararg labels: String) {
        val key = softKey(labels.toSet())
        fun applied(s: SurfaceStatus) = s.input.preedits + s.input.commits + s.input.keys
        val before = applied(NativeApp.surfaceStatus())
        val down = SystemClock.uptimeMillis()
        for (action in listOf(MotionEvent.ACTION_DOWN, MotionEvent.ACTION_UP)) {
            val event = MotionEvent.obtain(down, SystemClock.uptimeMillis(), action, key.exactCenterX(), key.exactCenterY(), 0)
            event.source = InputDevice.SOURCE_TOUCHSCREEN
            check(instrumentation.uiAutomation.injectInputEvent(event, true)) { "the ${labels.first()} key's touch was not delivered" }
            event.recycle()
        }
        try {
            awaitStatus("the ${labels.first()} key applied") { applied(it) > before }
        } catch (e: IllegalStateException) {
            screenshot("soft-key-not-applied")
            throw e
        }
    }

    // ---- the lanes ----

    /**
     * The installed soft keyboard (Gboard on API 35, LatinIME on API 24)
     * typing through real touches on its keys: the terminal takes them
     * through the connection the framework gave that keyboard. This runs
     * first, before the other lanes' own connections. Instrumentation
     * touches, not a person typing.
     */
    @Test
    fun t00_theInstalledSoftKeyboardTypesACommandWithRealKeyTaps() {
        bind(shellPane)
        showSoftKeyboard()
        val before = NativeApp.surfaceStatus().input
        // Each letter once: LatinIME keeps a pressed key's preview, labelled
        // like the key, in its accessibility tree above the keys.
        for (letter in "echo") tapSoftKey("$letter")
        tapSoftKey("Space", "space")
        for (letter in "jawk") tapSoftKey("$letter")
        tapSoftKey("Delete", "delete")
        tapSoftKey("Enter", "enter", "Return", "return")
        val pane = awaitPrompt("jaw")
        assertEquals("the command line is what the keys typed, once", "\$ echo jaw", pane.logical[pane.logical.lastIndexOf("jaw") - 1])
        screenshot("05-soft-keyboard")
        val after = NativeApp.surfaceStatus().input
        receipt("soft-keyboard", "preedits=${after.preedits - before.preedits} commits=${after.commits - before.commits} keys=${after.keys - before.keys}")
    }


    @Test
    fun t01_aCommandTypedOnThePhoneRunsOnTheLaptopAndShowsInThatPane() {
        bind(shellPane)
        val (x, y) = ui.onActivity { it.surfaceView.width / 2f to it.surfaceView.height / 2f }
        Finger(x, y).up(x, y)
        ui.awaitUi("the soft keyboard") { activity -> true.takeIf { activity.surfaceView.isFocused } }
        val before = NativeApp.surfaceStatus().input
        val ime = Ime()
        ime.commit("echo PHONE_\$((6*7))")
        ime.enter()
        val pane = awaitPrompt("PHONE_42")
        val output = pane.logical.lastIndexOf("PHONE_42")
        assertEquals("the command line is the literal commit, once", "\$ echo PHONE_\$((6*7))", pane.logical[output - 1])
        val after = NativeApp.surfaceStatus().input
        assertEquals("one commit", before.commits + 1, after.commits)
        assertEquals("one key", before.keys + 1, after.keys)
        assertEquals("the app runs no process of its own", emptyList<String>(), childProcesses())
        screenshot("05-command")
        receipt("command", "laptop_pane=${pane.remote}")
    }

    @Test
    fun t02_composingTextStaysLocalAndItsCommitArrivesOnce() {
        bind(capturePane)
        val ime = Ime()
        ime.commit(mark("c"))
        val before = NativeApp.surfaceStatus()
        ime.compose("n")
        ime.compose("ni")
        ime.compose("nihao")
        val composing = awaitStatus("the preedit drawn") { it.input.preedits >= before.input.preedits + 3 && it.totalFramesPresented > before.totalFramesPresented }
        screenshot("05-composition")
        assertEquals("composing sends nothing", before.input.commits, composing.input.commits)
        ime.commit("你好")
        ime.compose("e")
        ime.commit("é")
        ime.enter()
        assertEquals(hex("你好é") + "0d", captured(mark("c")))
        receipt("composition")
    }

    @Test
    fun t03_deletionAroundSurrogatesAndCombiningMarksMatchesTheirRanges() {
        bind(capturePane)
        val ime = Ime()
        ime.commit(mark("d"))
        val before = NativeApp.surfaceStatus().input
        ime.commit("ab😀")
        ime.applied { it.deleteSurroundingText(2, 0) }
        assertTrue("one emoji is two UTF-16 units", ime.textBefore().endsWith("ab"))
        ime.commit("😀")
        assertFalse("half a surrogate pair is refused", ime.call { it.deleteSurroundingText(1, 0) })
        assertTrue("and nothing changed", ime.textBefore().endsWith("ab😀"))
        ime.applied { it.deleteSurroundingTextInCodePoints(1, 0) }
        ime.commit("e\u0301")
        ime.applied { it.deleteSurroundingText(1, 0) }
        assertTrue("the accent alone was deleted", ime.textBefore().endsWith("abe"))

        // Composing text, and text a narrowed composition left after it, are on the phone only.
        val local = NativeApp.surfaceStatus().input
        ime.compose("hello")
        val start = ime.textBefore().length - "hello".length
        assertTrue(ime.call { it.setComposingRegion(start, start + 3) })
        assertTrue(ime.call { it.setSelection(start + 3, start + 3) })
        assertTrue(ime.call { it.deleteSurroundingText(0, 2) })
        assertEquals("the local text after the narrowed composition is gone", "abehel", ime.textBefore().takeLast(6))
        assertEquals("composing and local deletion sent nothing", local.commits, drained().input.commits)
        ime.applied { it.finishComposingText() }

        // The IME moves its cursor into sent text and deletes forward: the laptop retypes what follows.
        assertTrue(ime.call { it.setSelection(start + 1, start + 1) })
        ime.applied { it.deleteSurroundingText(0, 1) }
        assertEquals("abehl", ime.call { it.getTextBeforeCursor(4, 0).toString() + it.getTextAfterCursor(4, 0) })
        ime.enter()
        val backspace = "7f"
        assertEquals(
            hex("ab😀") + backspace + hex("😀") + backspace + hex("e\u0301") + backspace + hex("e") + hex("hel") +
                backspace + backspace + hex("l") + "0d",
            captured(mark("d")),
        )
        val after = NativeApp.surfaceStatus().input
        assertEquals("one RPC batch per edit that changed the laptop", before.commits + 8, after.commits)
        assertEquals("Enter", before.keys + 1, after.keys)

        // The laptop's line editor erases a letter with its accent as one character.
        bind(shellPane)
        val shell = Ime()
        shell.commit("echo ab😀")
        shell.applied { it.deleteSurroundingText(2, 0) }
        shell.commit(" xe\u0301")
        shell.applied { it.deleteSurroundingText(1, 0) }
        shell.enter()
        awaitPrompt("ab xe")
        screenshot("05-delete")
        receipt("delete")
    }

    @Test
    fun t04_controlAltEscapeArrowsAndAHardwareKeyboardSendTheirSequencesOnce() {
        bind(capturePane)
        val (x, y) = ui.onActivity { it.surfaceView.width / 2f to it.surfaceView.height / 4f }
        Finger(x, y).up(x, y)
        ui.awaitUi("the soft keyboard shown") { activity -> true.takeIf { !ui.keyboardHidden(activity) } }
        val ime = Ime()
        ime.commit(mark("k"))
        val before = NativeApp.surfaceStatus().input
        // A shown soft keyboard does not take the hardware Escape.
        hardwareKey(KeyEvent.KEYCODE_ESCAPE)
        hideKeyboardAndAwaitSurface()
        val ctrl = KeyEvent.META_CTRL_ON or KeyEvent.META_CTRL_LEFT_ON
        val alt = KeyEvent.META_ALT_ON or KeyEvent.META_ALT_LEFT_ON
        val altGr = KeyEvent.META_ALT_ON or KeyEvent.META_ALT_RIGHT_ON
        hardwareKey(KeyEvent.KEYCODE_C, ctrl)
        hardwareKey(KeyEvent.KEYCODE_X, alt)
        hardwareKey(KeyEvent.KEYCODE_ESCAPE)
        for (arrow in listOf(KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_DPAD_DOWN, KeyEvent.KEYCODE_DPAD_RIGHT, KeyEvent.KEYCODE_DPAD_LEFT)) hardwareKey(arrow)
        tapKey(R.string.key_escape)
        tapKey(R.string.key_ctrl)
        ime.commit("d")
        tapKey(R.string.key_alt)
        hardwareKey(KeyEvent.KEYCODE_B)
        tapKey(R.string.key_tab)
        tapKey(R.string.key_left)
        // The instrumentation keyboard's layout (Virtual.kcm) gives alt+C 'ç' and alt+E a dead acute.
        hardwareKey(KeyEvent.KEYCODE_C, altGr)
        hardwareKey(KeyEvent.KEYCODE_C, alt)
        hardwareKey(KeyEvent.KEYCODE_E, altGr)
        hardwareKey(KeyEvent.KEYCODE_E)
        hardwareKey(KeyEvent.KEYCODE_E, altGr)
        hardwareKey(KeyEvent.KEYCODE_X)
        hardwareKey(KeyEvent.KEYCODE_ENTER)
        assertEquals(
            listOf("1b", "03") + hex("\u001bx") + "1b" + hex("\u001b[A\u001b[B\u001b[C\u001b[D") + "1b" + "04" + hex("\u001bb") + "09" +
                hex("\u001b[D") + hex("ç") + hex("\u001bc") + hex("é") + hex("´x") + "0d",
            captured(mark("k")),
        )
        val after = NativeApp.surfaceStatus().input
        assertEquals("each of the 19 characters and keys arrived once, as a key or as text", 19L, after.keys - before.keys + after.commits - before.commits)
        assertEquals("the armed modifiers were used up", 0, ui.onActivity { it.surfaceView.armedMeta })
        screenshot("05-keys")
        receipt("keys")
    }

    @Test
    fun t05_pastePreservesTheTextAndTheServersBracketedPaste() {
        bind(capturePane)
        val text = "line one\nline two ü\n\tend"
        scenario.onActivity { activity ->
            activity.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("test", text))
        }
        val ime = Ime()
        ime.commit(mark("p"))
        val before = NativeApp.surfaceStatus()
        // The IME pastes and commits a line break in one UI-thread turn, before any clipboard answer could run.
        assertTrue(ime.call { it.performContextMenuAction(android.R.id.paste) && it.commitText("\n", 1) })
        val bracketed = hex("\u001b[200~") + hex(text) + hex("\u001b[201~")
        assertEquals(bracketed + "0d", captured(mark("p")))

        ime.commit(mark("q"))
        tapKey(R.string.key_paste)
        ime.enter()
        assertEquals(bracketed + "0d", captured(mark("q")))

        // The key table's paste keys on a hardware keyboard, each followed by Enter in the same UI-thread turn.
        val shift = KeyEvent.META_SHIFT_ON or KeyEvent.META_SHIFT_LEFT_ON
        val ctrlShift = shift or KeyEvent.META_CTRL_ON or KeyEvent.META_CTRL_LEFT_ON
        ime.commit(mark("h"))
        hardwareKeysInOneTurn(KeyEvent.KEYCODE_V to ctrlShift, KeyEvent.KEYCODE_ENTER to 0)
        assertEquals(bracketed + "0d", captured(mark("h")))
        ime.commit(mark("i"))
        hardwareKeysInOneTurn(KeyEvent.KEYCODE_INSERT to shift, KeyEvent.KEYCODE_ENTER to 0)
        assertEquals(bracketed + "0d", captured(mark("i")))
        val after = NativeApp.surfaceStatus()
        assertEquals("four pastes", before.input.pastes + 4, after.input.pastes)
        val classifyNs = ui.onActivity {
            val started = System.nanoTime()
            repeat(20) { NativeApp.nativeIsPasteKey(KeyEvent.KEYCODE_A, 'a'.code, 0) }
            (System.nanoTime() - started) / 20
        }
        Log.i(TAG, "receipt perf-paste-key is_paste_key_us=${classifyNs / 1000} (UI thread, per key press)")
        assertEquals("the phone's pastes carry the text they read; no clipboard request went out", before.clipboardRequests, after.clipboardRequests)
        screenshot("05-paste")
        receipt("paste")
    }

    @Test
    fun t06_touchSelectionCopiesTheLiteralTextAndScrollingTypesNothing() {
        bind(shellPane)
        val ime = Ime()
        ime.commit("echo SELECT_ME")
        ime.enter()
        val pane = awaitPrompt("SELECT_ME")
        val clipboard = ui.onActivity { it.getSystemService(ClipboardManager::class.java) }
        val copied = CountDownLatch(1)
        val listener = ClipboardManager.OnPrimaryClipChangedListener { copied.countDown() }
        scenario.onActivity { clipboard.addPrimaryClipChangedListener(listener) }
        val before = NativeApp.surfaceStatus().input
        // A mouse selection starts at the cell boundary nearest the press, so press in the first cell's left half.
        val cellWidth = NativeApp.surfaceStatus().cellWidth
        val (x0, y) = cellCentre(pane, pane.cursorRow - 1, 0).let { (x, y) -> x - cellWidth / 4f to y }
        val (x1, _) = cellCentre(pane, pane.cursorRow - 1, "SELECT_ME".length - 1)
        val finger = Finger(x0, y)
        touchesDelivered(before.touches) // the long press started the selection
        finger.move((x0 + x1) / 2, y)
        finger.move(x1, y)
        finger.up(x1, y)
        assertTrue("the selection reached the clipboard", copied.await(TIMEOUT_MS, TimeUnit.MILLISECONDS))
        scenario.onActivity { clipboard.removePrimaryClipChangedListener(listener) }
        assertEquals("SELECT_ME", ui.onActivity { clipboard.primaryClip!!.getItemAt(0).text.toString() })
        screenshot("05-selection")

        val census = SshMuxHarness.census().toString()
        val scrolled = NativeApp.surfaceStatus().input
        val cell = NativeApp.surfaceStatus().cellHeight.toFloat()
        val (sx, sy) = ui.onActivity { it.surfaceView.width / 2f to it.surfaceView.height / 3f }
        val drag = Finger(sx, sy)
        for (step in 1..6) drag.move(sx, sy + step * cell)
        drag.up(sx, sy + 6 * cell)
        val after = awaitStatus("the drag delivered") { it.input.touches > scrolled.touches }.input
        assertEquals("scrolling typed nothing", scrolled.copy(touches = after.touches), after)
        assertEquals("scrolling left the shell as it was", pane.logical, activePane()!!.logical)
        assertEquals("no pane closed or opened", census, SshMuxHarness.census().toString())
        screenshot("05-scrolled")
        receipt("selection")
    }

    /** The pixels `pane`'s cells leave unused right and below on `s`'s surface, from the first cell's top left. */
    private fun unused(pane: Pane, s: SurfaceStatus): Pair<Int, Int> {
        val left = s.cursorX - pane.cursorCol * s.cellWidth
        val top = s.cursorY - pane.cursorRow * s.cellHeight
        return (s.width - left - pane.cols * s.cellWidth) to (s.height - top - pane.rows * s.cellHeight)
    }

    /** Whether `pane`'s rows and columns fit `s`'s surface with less than two cells unused each way. */
    private fun fits(pane: Pane, s: SurfaceStatus): Boolean = unused(pane, s).let { (x, y) ->
        x >= 0 && x < 2 * s.cellWidth && y >= 0 && y < 2 * s.cellHeight
    }

    private fun assertGeometry(state: String, pane: Pane) {
        val s = settle()
        val (unusedX, unusedY) = unused(pane, s)
        Log.i(TAG, "receipt geometry state=$state surface=${s.width}x${s.height} cell=${s.cellWidth}x${s.cellHeight} cells=${pane.cols}x${pane.rows} unused=${unusedX}x$unusedY")
        assertTrue("$state: columns fit and use the width ($unusedX px left)", unusedX >= 0 && unusedX < 2 * s.cellWidth)
        assertTrue("$state: rows fit and use the height ($unusedY px left)", unusedY >= 0 && unusedY < 2 * s.cellHeight)
    }

    private fun laptopSize(ime: Ime): Pane {
        awaitPane("the pane resized to the surface") { it.remote == shellPane && fits(it, NativeApp.surfaceStatus()) }
        ime.commit("stty size\n")
        return awaitPane("stty size") { it.remote == shellPane && it.row(it.cursorRow) == "\$" && it.cursorCol == 2 && it.row(it.cursorRow - 1) == "${it.rows} ${it.cols}" }
    }

    @Test
    fun t07_keyboardInsetsAndRotationResizeTheSharedPaneToTheUsableSurface() {
        bind(shellPane)
        val ime = Ime()
        hideKeyboardAndAwaitSurface()
        val full = laptopSize(ime)
        assertGeometry("no keyboard", full)
        val fullSurface = NativeApp.surfaceStatus()

        val (x, y) = ui.onActivity { it.surfaceView.width / 2f to it.surfaceView.height / 4f }
        Finger(x, y).up(x, y)
        awaitStatus("the surface above the keyboard") { it.state == "present" && it.height < fullSurface.height }
        val withKeyboard = laptopSize(Ime())
        assertTrue("the keyboard takes rows: ${full.rows} -> ${withKeyboard.rows}", withKeyboard.rows < full.rows)
        assertEquals(full.cols, withKeyboard.cols)
        assertGeometry("keyboard", withKeyboard)
        screenshot("05-insets")

        hideKeyboardAndAwaitSurface()
        scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE }
        awaitStatus("landscape surface") { it.state == "present" && it.width > it.height }
        val landscape = laptopSize(Ime())
        assertGeometry("landscape", landscape)
        screenshot("05-insets-landscape")
        scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT }
        awaitStatus("portrait surface") { it.state == "present" && it.width == fullSurface.width }
        val portrait = hideKeyboardAndAwaitSurface()
        assertEquals("the keyboardless portrait surface again", fullSurface.width to fullSurface.height, portrait.width to portrait.height)
        val restored = laptopSize(Ime())
        assertEquals("the same size again", full.rows to full.cols, restored.rows to restored.cols)
        receipt("insets", "full=${full.cols}x${full.rows} keyboard=${withKeyboard.cols}x${withKeyboard.rows} landscape=${landscape.cols}x${landscape.rows}")
    }

    @Test
    fun t08_aLaptopEditorTakesTextKeysAndMouseClicks() {
        bind(shellPane)
        val ime = Ime()
        // Short enough for vim's status line on a phone-wide pane.
        val file = "v$run.txt"
        ime.commit("vim -u NONE -N -c 'set mouse=a' ~/$file")
        ime.enter()
        awaitPane("vim") { it.remote == shellPane && it.lines.any { line -> file in line && "[New]" in line } && it.row(0) == "" }
        ime.commit("i")
        ime.commit("hello vim")
        tapKey(R.string.key_escape)
        val typed = awaitPane("the text, in normal mode") { it.row(0) == "hello vim" && it.cursorRow == 0 && it.cursorCol == 8 }
        screenshot("05-editor")
        tapCell(typed, 0, 0)
        awaitPane("the click moved the cursor") { it.cursorRow == 0 && it.cursorCol == 0 }
        tapKey(R.string.key_right)
        awaitPane("right arrow") { it.cursorCol == 1 }
        tapKey(R.string.key_left)
        awaitPane("left arrow") { it.cursorCol == 0 }
        ime.commit("x")
        awaitPane("the edit") { it.row(0) == "ello vim" }
        ime.commit(":wq")
        ime.enter()
        awaitPane("the shell again") { it.row(it.cursorRow) == "\$" && it.cursorCol == 2 }
        ime.commit("cat ~/$file")
        ime.enter()
        awaitPrompt("ello vim")
        receipt("editor")
    }

    @Test
    fun t09_onlyTheSelectedLaptopPaneReceivesInputAcrossSwitches() {
        bind(capturePane)
        val ime = Ime()
        ime.compose("ghost")
        // The selector takes the window's focus; the framework then ends the IME's composition.
        ui.tap(ui.onActivity { it.selector })
        ui.awaitUi("the selector dialog with the window focus") { activity ->
            activity.selectorDialog?.takeIf { it.isShowing && !activity.hasWindowFocus() }
        }
        assertTrue(ime.call { it.finishComposingText() })
        ui.onActivity { it.selectorDialog!!.dismiss() }
        bind(shellPane)
        val shellIme = Ime()
        shellIme.commit("echo TARGET_SHELL")
        shellIme.enter()
        val shell = awaitPrompt("TARGET_SHELL")
        bind(capturePane)
        val target = Ime()
        target.commit(mark("t"))
        target.enter()
        assertEquals(listOf("0d"), captured(mark("t")))
        val capture = activePane()!!
        screenshot("05-target")
        assertFalse("the dropped composition reached no pane", capture.bytes.joinToString("").contains(hex("ghost").joinToString("")))
        assertFalse("the shell's command did not reach the capture", capture.bytes.joinToString("").contains(hex("TARGET_SHELL").joinToString("")))
        assertFalse("the capture's marker did not reach the shell", shell.logical.any { mark("t") in it || "ghost" in it })
        receipt("target", "shell=${shell.remote} capture=${capture.remote}")
    }

    @Test
    fun t10_backAndBackgroundReplayNothingAndCloseNoPane() {
        bind(capturePane)
        val census = SshMuxHarness.census().toString()
        val ime = Ime()
        ime.compose("pending")
        // Back first closes a shown soft keyboard; this Back leaves the app.
        hideKeyboardAndAwaitSurface()
        shell("input keyevent KEYCODE_BACK")
        awaitStatus("the surface retired") { it.state == "absent" && it.liveLeases == 0 }
        // The framework ends the composition of a window that went away.
        assertTrue(ime.call { it.finishComposingText() })
        val reopen = instrumentation.targetContext.packageManager.getLaunchIntentForPackage(instrumentation.targetContext.packageName)!!
        instrumentation.targetContext.startActivity(reopen)
        awaitStatus("shown again") { it.state == "present" && it.framesPresented > 0 }
        instrumentation.waitForIdleSync()
        val again = Ime()
        again.commit(mark("b"))
        again.enter()
        assertEquals("nothing composed before Back was sent", listOf("0d"), captured(mark("b")))
        assertEquals("Back closed no pane", census, SshMuxHarness.census().toString())
        assertEquals("attached", NativeApp.connectionStatus().phase)
        assertEquals(0L, NativeApp.surfaceStatus().closedWindows)
        receipt("background")
    }

    @Test
    fun t11_commitsAreSentOnceWithMeasuredLatencyAndNoIdleRedraw() {
        bind(capturePane)
        val ime = Ime()
        ime.commit(mark("m"))
        val samples = listOf("a", "b", "c", "d", "e", "日本", "語の", "ü", "😀", "z")
        var expected = emptyList<String>()
        for ((i, text) in samples.withIndex()) {
            val before = NativeApp.surfaceStatus()
            val started = SystemClock.elapsedRealtime()
            ime.commit(text)
            val dispatched = awaitStatus("commit $i applied") { it.input.commits > before.input.commits }
            val dispatchMs = SystemClock.elapsedRealtime() - started
            expected = expected + hex(text)
            val want = expected
            awaitPane("commit $i on the laptop") { pane ->
                val bytes = pane.bytes
                val start = hex(mark("m"))
                val at = bytes.lastIndexOfSublist(start)
                at >= 0 && bytes.drop(at + start.size) == want
            }
            val visibleMs = SystemClock.elapsedRealtime() - started
            Log.i(TAG, "receipt perf i=$i kind=${if (text.codePointCount(0, text.length) == 1) "char" else "text"} bytes=${hex(text).size} dispatch_ms=$dispatchMs commit_to_visible_ms=$visibleMs commits=${dispatched.input.commits - before.input.commits}")
        }
        ime.enter()
        assertEquals(expected + "0d", captured(mark("m")))
        val settled = TerminalHarness.screenshot("05-perf-settled")
        assertFalse(
            "a drained, idle pane is not redrawn",
            NativeApp.nativeAwaitSurfaceFrames(settled.generation, settled.framesPresented + 1, 3 * SETTLE_WINDOW_MS),
        )
        receipt("perf", "idle_redraws=0 over_ms=${3 * SETTLE_WINDOW_MS}")
    }

    /**
     * The IME re-marks text the laptop already holds as composing and
     * changes it: the laptop keeps its text until the composition is
     * committed, then takes the change once; a composition the user
     * abandons (restored, or ended by lost focus) changes nothing there.
     */
    @Test
    fun t12_recomposingSentTextChangesTheLaptopOnlyWhenItCommits() {
        bind(capturePane)
        val ime = Ime()
        ime.commit(mark("r"))
        ime.commit("abc")
        val start = ime.textBefore().length - 3
        var applied = NativeApp.surfaceStatus().input.commits
        fun sentNothing(what: String) = assertEquals(what, applied, drained().input.commits)
        fun sentOnce(edit: (InputConnection) -> Boolean) {
            ime.applied(edit)
            applied += 1
            assertEquals("one edit, one commit", applied, drained().input.commits)
        }

        assertTrue(ime.call { it.setComposingRegion(start, start + 3) && it.setComposingText("abd", 1) })
        assertEquals("the IME's text is its draft", "abd", ime.textBefore().takeLast(3))
        sentNothing("recomposing abc as abd sent nothing")
        assertTrue(ime.call { it.setComposingText("abc", 1) && it.finishComposingText() })
        sentNothing("restoring abc and finishing sent nothing")

        assertTrue(ime.call { it.setComposingRegion(start, start + 3) && it.setComposingText("abdoce", 1) })
        sentNothing("recomposing abc as abdoce sent nothing")
        sentOnce { it.finishComposingText() }
        assertTrue(ime.call { it.setComposingRegion(start + 3, start + 6) && it.setComposingText("o", 1) })
        sentNothing("recomposing oce as o sent nothing")
        sentOnce { it.commitText("ok", 1) }
        assertEquals("abdok", ime.textBefore().takeLast(5))

        assertTrue(ime.call { it.setComposingRegion(start, start + 5) && it.setComposingText("zzz", 1) })
        ui.tap(ui.onActivity { it.selector })
        ui.awaitUi("the selector dialog with the window focus") { activity ->
            activity.selectorDialog?.takeIf { it.isShowing && !activity.hasWindowFocus() }
        }
        assertTrue(ime.call { it.finishComposingText() })
        ui.onActivity { it.selectorDialog!!.dismiss() }
        sentNothing("a recomposition dropped with the focus sent nothing")
        val again = Ime()
        again.enter()
        assertEquals(hex("abc") + "7f" + hex("doce") + "7f" + "7f" + hex("k") + "0d", captured(mark("r")))
        screenshot("05-recompose")
        receipt("recompose")
    }

    /**
     * The laptop moves its focus to another pane of the shown window while
     * the IME still holds text it typed into the first: an IME edit that
     * would erase that text is refused, not delivered to the pane that has
     * the focus now, and the next commit reaches that pane alone.
     */
    @Test
    fun t13_aRewriteForAPaneTheLaptopFocusedAwayFromIsRefused() {
        val from = bind(focusFrom)
        settle()
        val ime = Ime()
        ime.commit(mark("f"))
        sentSettled(focusFrom, mark("f"))
        val before = NativeApp.surfaceStatus()
        assertEquals("the published target is the shown pane", from.local.toLong(), before.inputTarget.pane)
        receipt("laptop-focus")
        // One UI-thread turn: the view cannot learn of the laptop's focus change before the IME's rewrite.
        val rewritten = ime.call { connection ->
            check(connection.commitText("abc", 1))
            awaitPane("the laptop's focus on its other pane, mirrored") { it.remote == focusTo }
            val end = connection.getTextBeforeCursor(256, 0)!!.length
            connection.setSelection(end - 3, end) && connection.commitText("X", 1)
        }
        assertTrue(rewritten)
        val refused = awaitStatus("the rewrite refused") { it.input.refused > before.input.refused }
        assertEquals("abc went out, the rewrite did not", before.input.commits + 1, refused.input.commits)
        val to = awaitPane("the other pane shown") { it.remote == focusTo }
        ui.awaitUi("the view took the new target") { activity -> true.takeIf { activity.surfaceView.inputTarget?.pane == to.local.toLong() } }
        assertEquals("the IME's record of abc is gone", "", ime.textBefore())
        // The laptop leaves this pane's cursor on its banner's row; the echo of five characters covers all of it.
        ime.commit("focus")
        ime.enter()
        val shown = awaitPane("the commit on the focused pane") { it.remote == focusTo && it.bytes.lastOrNull() == "0d" }
        val banner = setOf("CAPTURE", "READY")
        assertEquals("the focused pane got the commit and Enter, nothing of the rewrite", hex("focus") + "0d", shown.bytes.filterNot { it in banner })
        screenshot("05-target-focus")
        receipt("target-focus", "from=${from.remote} to=${to.remote} target=${NativeApp.surfaceStatus().inputTarget}")
    }

    /** Block on surface changes until the held observations satisfy `done`. */
    private fun awaitHeld(what: String, done: (List<String>) -> Boolean): List<String> {
        val deadline = SystemClock.elapsedRealtime() + TIMEOUT_MS
        while (true) {
            val status = NativeApp.surfaceStatus()
            val held = NativeApp.nativeDiagnosticHeldObservations()?.let { json -> JSONArray(json).let { a -> List(a.length()) { a.getString(it) } } }.orEmpty()
            if (done(held)) return held
            val left = deadline - SystemClock.elapsedRealtime()
            check(left > 0) { "$what not seen within ${TIMEOUT_MS}ms; held $held" }
            NativeApp.nativeAwaitSurfaceChange(status.revision, left)
        }
    }

    /**
     * The laptop moves its focus away from the pane the IME typed into and
     * back before the GUI thread observed either move, as when the
     * laptop's two notifications arrive together: the observations they
     * schedule are held (debug hook). A rewrite of the typed text is
     * refused all the same, because the laptop may have changed that pane
     * meanwhile, and the next commit reaches the pane alone.
     */
    @Test
    fun t14_aRewriteAfterTheLaptopFocusedAwayAndBackIsRefused() {
        val pane = bind(focusTo)
        settle()
        val ime = Ime()
        ime.commit(mark("g"))
        sentSettled(focusTo, mark("g"))
        val before = NativeApp.surfaceStatus()
        receipt("laptop-focus")
        assertTrue(NativeApp.nativeDiagnosticGui("hold-target-observations"))
        val held = try {
            ime.commit("aba")
            val held = awaitHeld("the laptop's focus away and back, applied by the phone's mux") { held ->
                val focused = held.filter { it.startsWith("PaneFocused(") }
                focused.size >= 2 && focused.last() == "PaneFocused(${pane.local})" && focused.dropLast(1).any { it != focused.last() }
            }
            assertEquals("the view has not learnt of it", before.inputTarget, ui.onActivity { it.surfaceView.inputTarget })
            val rewritten = ime.call { connection ->
                val end = connection.getTextBeforeCursor(256, 0)!!.length
                connection.setSelection(end - 3, end) && connection.commitText("X", 1)
            }
            assertTrue(rewritten)
            val refused = awaitStatus("the rewrite refused") { it.input.refused > before.input.refused }
            assertEquals("aba went out, the rewrite did not", before.input.commits + 1, refused.input.commits)
            held
        } finally {
            NativeApp.nativeDiagnosticGui("release-target-observations")
        }
        ui.awaitUi("the view took the new target") { activity -> true.takeIf { activity.surfaceView.inputTarget?.let { it.generation > before.inputTarget.generation && it.pane == pane.local.toLong() } == true } }
        assertEquals("the IME's record of aba is gone", "", ime.textBefore())
        ime.commit("next")
        ime.enter()
        val shown = awaitPane("the commit on the pane") { it.remote == focusTo && it.bytes.lastOrNull() == "0d" }
        val bytes = shown.bytes.let { it.drop(it.lastIndexOfSublist(hex(mark("g"))) + hex(mark("g")).size) }
        assertEquals("aba, then the next commit and Enter: nothing of the rewrite", hex("aba") + hex("next") + "0d", bytes)
        screenshot("05-target-away-and-back")
        receipt("target-away-and-back", "pane=${pane.remote} held=${held.joinToString("|")} target=${NativeApp.surfaceStatus().inputTarget}")
    }

    /**
     * A resync applies a pane tree the laptop took before this phone's last
     * resize (the client's `ListPanes` answer that crossed a rotation):
     * replayed here through `Tab::sync_with_pane_tree` from a snapshot kept
     * before rotating (debug hook). The laptop pane takes the stale size,
     * then the shown window fits it to the usable surface again, and once
     * fitted nothing more is sent or drawn.
     */
    @Test
    fun t15_aStalePaneTreeAfterRotationIsFittedToTheSurfaceAgain() {
        bind(shellPane)
        val ime = Ime()
        hideKeyboardAndAwaitSurface()
        val portrait = laptopSize(ime)
        assertTrue(NativeApp.nativeDiagnosticGui("snapshot-bound-tab"))
        drained()
        scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE }
        awaitStatus("landscape surface") { it.state == "present" && it.width > it.height }
        val landscape = laptopSize(Ime())
        assertGeometry("landscape", landscape)
        assertTrue("landscape differs: ${portrait.cols}x${portrait.rows} -> ${landscape.cols}x${landscape.rows}", portrait.rows to portrait.cols != landscape.rows to landscape.cols)

        receipt("stale-tree-apply")
        assertTrue(NativeApp.nativeDiagnosticGui("apply-bound-tab-snapshot"))
        val fitted = laptopSize(Ime())
        assertEquals("the laptop pane has the landscape cells again", landscape.rows to landscape.cols, fitted.rows to fitted.cols)
        assertGeometry("landscape after a stale pane tree", fitted)
        val settled = TerminalHarness.screenshot("05-stale-tree-fitted")
        receipt("stale-tree-idle")
        assertFalse(
            "a fitted, idle pane is not redrawn",
            NativeApp.nativeAwaitSurfaceFrames(settled.generation, settled.framesPresented + 1, 3 * SETTLE_WINDOW_MS),
        )
        receipt("stale-tree-fitted", "portrait=${portrait.cols}x${portrait.rows} landscape=${landscape.cols}x${landscape.rows} idle_redraws=0 over_ms=${3 * SETTLE_WINDOW_MS}")
        scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT }
        awaitStatus("portrait surface") { it.state == "present" && it.width < it.height }
    }

    /** What the laptop's `stty size` prints in the shell now, whatever size this phone gave the pane. */
    private fun laptopStty(ime: Ime, marker: String): String {
        ime.commit("echo $marker; stty size")
        ime.enter()
        val pane = awaitPane("stty size after $marker") {
            it.remote == shellPane && it.row(it.cursorRow) == "\$" && it.cursorCol == 2 && it.row(it.cursorRow - 2) == marker
        }
        return pane.row(pane.cursorRow - 1)!!
    }

    /**
     * The laptop resizes the shell pane while this phone shows nothing
     * (Home; the engine and the connection stay): the laptop's size
     * stands, though the phone's mux learns of it, until the phone shows
     * the pane again at the same surface size and fits it to that surface.
     * The phone must decide while it shows nothing: the fit the laptop's
     * resize asks for is held (debug hook) until the surface is gone, and
     * the fit the surface's return asks for until the laptop printed its
     * size again. The laptop's resize is a laptop client of its own server
     * (`laptop-resize`), as a laptop GUI resizing its window.
     */
    @Test
    fun t16_aLaptopResizeStandsWhileThePhoneShowsNothingAndFitsWhenItShowsAgain() {
        bind(shellPane)
        hideKeyboardAndAwaitSurface()
        scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE }
        awaitStatus("landscape surface") { it.state == "present" && it.width > it.height }
        val shown = laptopSize(Ime())
        assertGeometry("landscape", shown)
        val before = NativeApp.surfaceStatus()
        val census = SshMuxHarness.census().toString()
        val connection = NativeApp.connectionStatus()
        val laptop = "10 40"
        assertTrue(NativeApp.nativeDiagnosticGui("hold-fits"))
        var refitHeld = false
        try {
            val ime = Ime()
            ime.commit("laptop-resize $laptop")
            ime.enter()
            awaitPrompt(laptop)
            awaitPane("the laptop's size in the phone's mux") { it.rows == 10 && it.cols == 40 }
            receipt("laptop-resized")
            shell("input keyevent KEYCODE_HOME")
            awaitStatus("the surface retired") { it.state == "absent" && it.liveLeases == 0 }
            assertTrue("the laptop's resize asked the phone to fit", NativeApp.nativeDiagnosticGui("release-fits"))
            val decided = activePane()!!
            receipt("background-fit", "phone=${decided.cols}x${decided.rows}")
            assertTrue(NativeApp.nativeDiagnosticGui("hold-fits"))
            val reopen = instrumentation.targetContext.packageManager.getLaunchIntentForPackage(instrumentation.targetContext.packageName)!!
            instrumentation.targetContext.startActivity(reopen)
            val back = awaitStatus("shown again") { it.state == "present" && it.generation > before.generation && it.framesPresented > 0 }
            instrumentation.waitForIdleSync()
            assertEquals("the same surface size", before.width to before.height, back.width to back.height)
            assertEquals("the same window", before.boundWindow, back.boundWindow)
            assertEquals("the laptop's size stood while the phone showed nothing", laptop, laptopStty(Ime(), mark("r")))
            receipt("resumed-held")
        } finally {
            refitHeld = NativeApp.nativeDiagnosticGui("release-fits")
        }
        assertEquals("shown again, the phone's cells", "${shown.rows} ${shown.cols}", laptopStty(Ime(), mark("f")))
        assertTrue("the surface's return asked for the fit", refitHeld)
        val fitted = activePane()!!
        assertGeometry("landscape, shown again", fitted)
        assertEquals("no pane closed or opened", census, SshMuxHarness.census().toString())
        val after = NativeApp.connectionStatus()
        assertEquals("the same connection", connection.phase to connection.attempt, after.phase to after.attempt)
        val settled = TerminalHarness.screenshot("05-background-resize-fitted")
        receipt("background-resize-idle")
        assertFalse(
            "a fitted, idle pane is not redrawn",
            NativeApp.nativeAwaitSurfaceFrames(settled.generation, settled.framesPresented + 1, 3 * SETTLE_WINDOW_MS),
        )
        receipt("background-resize-fitted", "landscape=${shown.cols}x${shown.rows} laptop=40x10 idle_redraws=0 over_ms=${3 * SETTLE_WINDOW_MS}")
        scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT }
        awaitStatus("portrait surface") { it.state == "present" && it.width < it.height }
    }

    /** Wait for the soft keyboard a tap on the terminal opens, close it, and wait until the window drew the full surface. */
    private fun closeTheOpenedKeyboard() {
        ui.awaitUi("the soft keyboard a tap opened") { activity -> true.takeIf { !ui.keyboardHidden(activity) } }
        hideKeyboardAndAwaitSurface()
        settle()
    }

    /** A tap at view point `x`, `y` as the touchscreen delivers it to the terminal, on the UI thread. */
    private fun TerminalView.tap(x: Float, y: Float) {
        val down = SystemClock.uptimeMillis()
        for (action in listOf(MotionEvent.ACTION_DOWN, MotionEvent.ACTION_UP)) {
            val event = MotionEvent.obtain(down, down, action, x, y, 0)
            event.source = InputDevice.SOURCE_TOUCHSCREEN
            check(dispatchTouchEvent(event)) { "the terminal did not take ${MotionEvent.actionToString(action)}" }
            event.recycle()
        }
    }

    /**
     * Wait for the capture in laptop pane `pane` to show `marker`: a focus
     * this window's first paint advised went out before the marker's keys,
     * and the laptop's answer came back before its echo.
     */
    private fun sentSettled(pane: Int, marker: String) {
        awaitPane("the echo of $marker in laptop pane $pane") { it.remote == pane && it.bytes.containsSublist(hex(marker)) }
    }

    /** Commit `marker` and Enter into `pane`, the bound window's focused laptop pane, and wait for its capture's echo. */
    private fun echoed(pane: Int, marker: String): Pane {
        val ime = Ime()
        ime.commit(marker)
        ime.enter()
        return awaitPane("the echo of $marker in laptop pane $pane") { it.remote == pane && it.bytes.containsSublist(hex(marker) + "0d") }
    }

    /**
     * Taps on the top then the bottom pane of the laptop's split window,
     * in one UI-thread turn and applied in one GUI-thread task (inputs
     * held, debug hook): the phone sends both focus changes before the
     * laptop announced either, and the laptop then announces each twice.
     * The phone adopts the announcements without sending them back, so the
     * laptop's focus stays on the bottom pane and nothing more is sent: one
     * commit's echo later the focus has settled, another one later it has
     * not moved, and the commits reach the bottom pane alone. A tap first
     * gives the bottom pane the focus, whatever an earlier lane left, and
     * opens the soft keyboard. The held taps are made once the surface fits
     * above it and settled: a tap asks for the keyboard, and the hold lets
     * surface events through, so a keyboard opened by the held taps would
     * resize the window before they apply. The keyboard is closed, and the
     * window settled, before the round trips.
     */
    @Test
    fun t17_twoQuickPaneFocusChangesAreSentOnceAndNotEchoed() {
        bindShowing("the laptop's split window") { it.remote == focusFrom || it.remote == focusTo }
        val full = hideKeyboardAndAwaitSurface()
        val (x, y) = ui.onActivity { it.surfaceView.width / 2f to it.surfaceView.height * 3 / 4f }
        Finger(x, y).up(x, y)
        val shown = awaitPane("the bottom pane focused") { it.remote == focusTo }
        ui.awaitUi("the soft keyboard the tap opened") { activity -> true.takeIf { !ui.keyboardHidden(activity) } }
        awaitStatus("the surface above the keyboard") {
            it.state == "present" && it.height < full.height && it.height == ui.onActivity { a -> a.surfaceView.height }
        }
        settle()
        echoed(focusTo, mark("e"))
        val before = NativeApp.surfaceStatus()
        receipt("focus-taps", "pane=${shown.remote}")
        assertTrue(NativeApp.nativeDiagnosticGui("hold-input"))
        try {
            scenario.onActivity { activity ->
                val view = activity.surfaceView
                view.tap(view.width / 2f, view.height / 4f)
                view.tap(view.width / 2f, view.height * 3 / 4f)
            }
        } finally {
            assertTrue("the taps were held", NativeApp.nativeDiagnosticGui("release-input"))
        }
        val tapped = awaitStatus("both taps applied") { it.input.touches >= before.input.touches + 2 }
        assertEquals("two taps", before.input.touches + 2, tapped.input.touches)
        closeTheOpenedKeyboard()
        echoed(focusTo, mark("o"))
        val settled = drained().inputTarget
        assertEquals("the bottom pane has the focus", shown.local.toLong(), settled.pane)
        echoed(focusTo, mark("n"))
        assertEquals("the focus did not move over a round trip", settled, drained().inputTarget)
        screenshot("05-focus-taps")
        val idle = settle()
        receipt("focus-settled", "target=$settled")
        assertFalse(
            "a settled focus redraws nothing",
            NativeApp.nativeAwaitSurfaceFrames(idle.generation, idle.framesPresented + 1, 3 * SETTLE_WINDOW_MS),
        )
        receipt("focus-idle", "idle_redraws=0 over_ms=${3 * SETTLE_WINDOW_MS}")
    }

    /** What `laptop-cli list` gives as laptop pane `pane`'s size now (`<cols>x<rows>`), typed in the shell. */
    private fun laptopListedSize(ime: Ime, pane: Int, marker: String): String {
        ime.commit("echo $marker; laptop-cli list | awk '\$3 == $pane {print \$5}'")
        ime.enter()
        val shown = awaitPane("the laptop's list after $marker") {
            it.remote == shellPane && it.row(it.cursorRow) == "\$" && it.cursorCol == 2 && it.row(it.cursorRow - 2) == marker
        }
        return shown.row(shown.cursorRow - 1)!!
    }

    /**
     * The laptop closes the window this phone shows, after it resized the
     * pane of this phone's first window while another window showed: the
     * phone shows its first window again, at the surface size it had, and
     * fits that pane to it. The laptop first closes its split window, so
     * every window left holds one pane, which the laptop resizes as a laptop
     * GUI resizes a one-pane tab. The laptop opens and closes windows with
     * its own commands (`laptop-cli`, `exit`); no other pane changes.
     */
    @Test
    fun t18_theWindowShownAfterTheLaptopClosedTheShownOneFitsItsPanes() {
        bind(shellPane)
        val split = Ime()
        for (pane in listOf(focusFrom, focusTo)) {
            split.commit("laptop-cli kill-pane --pane-id $pane")
            split.enter()
        }
        awaitStatus("the laptop's split window closed") { it.windows.size == 2 }
        val first = NativeApp.surfaceStatus().windows.minOf { it.id }
        val pane = bindShowing("this phone's first window") { NativeApp.surfaceStatus().boundWindow == first }.remote
        hideKeyboardAndAwaitSurface()
        val fitted = awaitPane("the first window's pane fitted") { it.remote == pane && fits(it, NativeApp.surfaceStatus()) }
        assertGeometry("the first window", fitted)
        val before = NativeApp.surfaceStatus()
        val census = sortedCensus()
        val connection = NativeApp.connectionStatus()
        bind(shellPane)
        val ime = Ime()
        ime.commit("laptop-cli spawn --new-window")
        ime.enter()
        val opened = awaitPane("the laptop window's pane id") {
            it.remote == shellPane && it.row(it.cursorRow) == "\$" && it.row(it.cursorRow - 1)?.toIntOrNull() != null
        }
        val window = opened.row(opened.cursorRow - 1)!!.toInt()
        awaitStatus("the laptop's new window") { it.windows.size == 3 }
        bindShowing("the laptop's new window") { it.remote == window }
        val other = Ime()
        other.commit("WEZTERM_PANE=$pane laptop-resize 10 40")
        other.enter()
        awaitPane("the laptop's resize done") { it.remote == window && it.row(it.cursorRow) == "\$" && it.row(it.cursorRow - 1)?.matches(Regex("\\d+ \\d+")) == true }
        receipt("window-closing", "first=$pane window=$window from=$focusFrom to=$focusTo")
        other.commit("exit")
        other.enter()
        val shownAgain = awaitStatus("this phone's first window shown again") { it.windows.size == 2 && it.boundWindow == first && it.state == "present" }
        hideKeyboardAndAwaitSurface()
        assertEquals("the same surface size", before.width to before.height, NativeApp.surfaceStatus().let { it.width to it.height })
        receipt("window-closed", "bound=${shownAgain.boundWindow}")
        if (pane != shellPane) bind(shellPane)
        assertEquals("the first window's pane on the laptop, shown again", "${fitted.cols}x${fitted.rows}", laptopListedSize(Ime(), pane, mark("w")))
        assertEquals("no other pane closed or opened", census, sortedCensus())
        val after = NativeApp.connectionStatus()
        assertEquals("the same connection", connection.phase to connection.attempt, after.phase to after.attempt)
        screenshot("05-rebound-fitted")
        val idle = settle()
        receipt("rebound-idle")
        assertFalse(
            "a fitted, idle pane is not redrawn",
            NativeApp.nativeAwaitSurfaceFrames(idle.generation, idle.framesPresented + 1, 3 * SETTLE_WINDOW_MS),
        )
        receipt("rebound-fitted", "first=$pane cells=${fitted.cols}x${fitted.rows} laptop=40x10 idle_redraws=0 over_ms=${3 * SETTLE_WINDOW_MS}")
    }

    @Test
    fun t19_aLaptopOsc52CopyReachesThePhoneClipboard() {
        bind(shellPane)
        val clipboard = ui.onActivity { it.getSystemService(ClipboardManager::class.java) }
        val copied = CountDownLatch(1)
        val listener = ClipboardManager.OnPrimaryClipChangedListener { copied.countDown() }
        scenario.onActivity { clipboard.addPrimaryClipChangedListener(listener) }
        val before = NativeApp.surfaceStatus()
        // The laptop assembles the copied literal, so no typed or shown text holds it.
        val ime = Ime()
        ime.commit("printf '\\e]52;c;%s\\a' \$(printf 'OSC52_%s_%s' CLIP \$((6*7))$run | base64); echo OSC52_SENT")
        ime.enter()
        awaitPrompt("OSC52_SENT")
        assertTrue("the laptop's copy reached the clipboard", copied.await(TIMEOUT_MS, TimeUnit.MILLISECONDS))
        scenario.onActivity { clipboard.removePrimaryClipChangedListener(listener) }
        assertEquals("OSC52_CLIP_42$run", ui.onActivity { clipboard.primaryClip!!.getItemAt(0).text.toString() })
        assertEquals("a copy asks the platform for no read", before.clipboardRequests, NativeApp.surfaceStatus().clipboardRequests)
        screenshot("05-osc52")
        receipt("osc52", "run=$run bytes=${"OSC52_CLIP_42$run".length}")
    }

    /** The mux census, its panes in pane-id order: closing a window reorders the mux's list of them. */
    private fun sortedCensus(): String = SshMuxHarness.census().let { census ->
        val panes = census.getJSONArray("panes").let { a -> List(a.length()) { a.getJSONObject(it) } }
        census.put("panes", JSONArray(panes.sortedBy { it.getInt("pane") })).toString()
    }

    private fun List<String>.containsSublist(sub: List<String>) = (0..size - sub.size).any { subList(it, it + sub.size) == sub }

    private fun List<String>.lastIndexOfSublist(sub: List<String>) = (size - sub.size downTo 0).firstOrNull { subList(it, it + sub.size) == sub } ?: -1
}
