package org.wezterm.android

import android.graphics.Bitmap
import android.graphics.Color
import android.os.Process
import android.util.Log
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.wezterm.android.SshMuxHarness.Companion.TAG
import org.wezterm.android.SshMuxHarness.Companion.fixture
import org.wezterm.android.SshMuxHarness.Companion.sshDir
import org.json.JSONObject
import org.wezterm.android.TerminalHarness.SETTLE_WINDOW_MS
import org.wezterm.android.TerminalHarness.TIMEOUT_MS
import org.wezterm.android.TerminalHarness.awaitStatus
import org.wezterm.android.TerminalHarness.instrumentation
import org.wezterm.android.TerminalHarness.launchIntent
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.math.abs

/**
 * Font failures while the laptop's screen is shaped are logged without
 * the text being shaped, at their own level, in a default debug build.
 * The fixture's `lastpane` bash shows only ASCII until the test has it
 * print one character with `printf` octal escapes, so the character never
 * passes through the phone's input. The fonts come from debug config
 * overrides: JetBrains Mono, then a copy of a system font from a font
 * directory of this test.
 *
 * Each method runs in its own process; the route is the host's own
 * address through `lo`: protocol fixture results, never private
 * Tailscale acceptance.
 */
@RunWith(AndroidJUnit4::class)
class FontLogTest {
    private lateinit var scenario: ActivityScenario<TerminalActivity>
    private lateinit var ui: SshMuxHarness

    private val fonts get() = File(instrumentation.targetContext.filesDir, "fontlog-fonts")
    private val fallback get() = File(fonts, "DroidSansMono.ttf")

    @Before
    fun attachWithATestFallbackFont() {
        sshDir().deleteRecursively()
        instrumentation.targetContext.deleteSharedPreferences(ConnectionPanel.PREFERENCES)
        fonts.deleteRecursively()
        check(fonts.mkdirs())
        File("/system/fonts/DroidSansMono.ttf").copyTo(fallback)
        val intent = launchIntent()
        val overrides = listOf(
            intent.getStringExtra(TerminalActivity.EXTRA_CONFIG_OVERRIDES).orEmpty(),
            "font_dirs={'${fonts.path}'}",
            "font=wezterm.font_with_fallback({'JetBrains Mono','Droid Sans Mono'})",
        ).filter { it.isNotEmpty() }.joinToString("\n")
        scenario = ActivityScenario.launch(intent.putExtra(TerminalActivity.EXTRA_CONFIG_OVERRIDES, overrides))
        ui = SshMuxHarness(scenario)
        awaitStatus("engine running") { it.engine == "running" }
        instrumentation.waitForIdleSync()
        assertEquals(heading(R.string.identity_imported), ui.pickIdentity("wezterm-fixture-key"))
        val attached = ui.connectTrusting(fixture("wezterm_lastpane"), "attach") { it.phase != "attaching" }
        assertEquals("attached: ${attached.failureKind}", "attached", attached.phase)
        awaitStatus("the laptop's window shown") { it.windows.size == 1 && it.state == "present" && it.framesPresented > 0 }
        awaitShown("the laptop shell prompt") { text -> text.lines().any { it == "$" } }
        // A character the first font lacks, shown when the window first
        // paints, would load the fallback font before the test removes it.
        check(shownText(viewport = true).all { it.code < 0x80 }) { "the laptop shell shows non-ASCII text of an earlier run; bring the fixture up again" }
    }

    @After
    fun close() {
        scenario.close()
        fonts.deleteRecursively()
    }

    /**
     * The fallback font's file is gone when the shaper first needs it, so
     * loading it fails: the wide character is drawn as a placeholder that
     * keeps its two cells, the letter after it stays in the third, the GUI
     * keeps running, and the failure is logged with the text's size, not
     * the text.
     */
    @Test
    fun aClusterNoFallbackFontCanLoadIsLoggedWithoutItsText() {
        check(fallback.delete())
        val needle = "\u4E2D"
        printOnTheLaptop("\\344\\270\\255X", "${needle}X")
        val record = awaitNativeLog("no fallback font could shape|load_fallback")
        val after = NativeApp.surfaceStatus()
        val accepted = NativeApp.nativeInputCommit(0, -1, 0, "\n", 0)
        val next = awaitStatus("a frame drawn after the failure, or the engine's end") {
            it.engine != "running" || it.totalFramesPresented > after.totalFramesPresented
        }
        assertEquals("the GUI engine survives the failed fallback", "running" to true, next.engine to accepted)
        val inked = inkedCells("${needle}X", 4)
        ui.screenshot("fontlog-placeholder")
        assertEquals("cells of the printed row holding ink", listOf(0, 2), inked)
        assertNoNeedle("fontlog-cluster", needle, "u{4e2d}")
        assertEquals("wezterm_font::shaper::harfbuzz: no fallback font could shape 3 bytes of text; showing placeholders", record)
        clearTheLaptopShell()
    }

    /**
     * No font covers U+10FFFD: the warning names how many codepoints were
     * missing, not which.
     */
    @Test
    fun codepointsNoFontCoversAreLoggedWithoutTheirValues() {
        val needle = "\uDBFF\uDFFD"
        printOnTheLaptop("\\364\\217\\277\\275", needle)
        val record = awaitNativeLog("No fonts contain glyphs for these codepoints")
        assertNoNeedle("fontlog-codepoints", needle, "10fffd")
        assertEquals("wezterm_font: No fonts contain glyphs for these codepoints: <1 codepoints>.", record)
        clearTheLaptopShell()
    }

    private fun heading(id: Int) = instrumentation.targetContext.getString(id)

    /** Have the laptop shell print `octal` (printf escapes) and wait until the pane holds `shown`. */
    private fun printOnTheLaptop(octal: String, shown: String) {
        check(NativeApp.nativeInputCommit(0, -1, 0, "printf '$octal\\n'\n", 0))
        awaitShown("the printed character") { shown in it }
    }

    /** Clear the laptop shell's screen and scrollback, so the next method attaches to ASCII only. */
    private fun clearTheLaptopShell() {
        check(NativeApp.nativeInputCommit(0, -1, 0, "clear; printf '\\033[3J'\n", 0))
        awaitShown("the cleared laptop shell") { shownText(viewport = true).all { it.code < 0x80 } }
    }

    /** The bound pane's text: some scrollback rows and the viewport, or the viewport alone. */
    private fun shownText(viewport: Boolean = false): String =
        NativeApp.nativeDiagnosticActivePane()?.let { json ->
            val root = org.json.JSONObject(json)
            val lines = root.getJSONArray("lines")
            val first = if (viewport) root.getInt("viewport_start") else 0
            (first until lines.length()).joinToString("\n") { lines.getString(it) }
        } ?: ""

    /**
     * Which of the first `count` cells of the viewport row showing `text`
     * hold ink in a screenshot of a settled frame; the row's last cell is
     * the background. The cells are placed from the cursor cell that frame
     * painted, and the pane is kept only if no frame followed its read.
     */
    private fun inkedCells(text: String, count: Int): List<Int> {
        while (true) {
            var status = NativeApp.surfaceStatus()
            while (NativeApp.nativeAwaitSurfaceFrames(status.generation, status.framesPresented + 1, SETTLE_WINDOW_MS)) {
                status = NativeApp.surfaceStatus()
            }
            val pane = JSONObject(checkNotNull(NativeApp.nativeDiagnosticActivePane()) { "the bound window shows no pane" })
            val shot = instrumentation.uiAutomation.takeScreenshot().let { raw ->
                raw.copy(Bitmap.Config.ARGB_8888, false).also { raw.recycle() }
            }
            if (NativeApp.nativeAwaitSurfaceFrames(status.generation, status.framesPresented + 1, SETTLE_WINDOW_MS)) {
                shot.recycle()
                continue
            }
            val lines = pane.getJSONArray("lines")
            val first = pane.getInt("viewport_start")
            val row = (first until lines.length()).single { lines.getString(it) == text } - first
            val origin = ui.onActivity { activity -> IntArray(2).also { activity.surfaceView.getLocationOnScreen(it) } }
            val (w, h) = status.cellWidth to status.cellHeight
            val left = origin[0] + status.cursorX - pane.getInt("cursor_col") * w
            val top = origin[1] + status.cursorY + (row - pane.getInt("cursor_row")) * h
            val background = shot.getPixel(left + (pane.getInt("cols") - 1) * w + w / 2, top + h / 2)
            val ink = (0 until count).map { cell ->
                (left + cell * w + 1 until left + (cell + 1) * w - 1).sumOf { x ->
                    (top + 1 until top + h - 1).count { y -> differs(shot.getPixel(x, y), background) }
                }
            }
            shot.recycle()
            Log.i(TAG, "receipt phase=fontlog-cells cell=${w}x$h row=$row ink=$ink")
            return ink.indices.filter { ink[it] > 0 }
        }
    }

    private fun differs(a: Int, b: Int): Boolean =
        listOf(Color::red, Color::green, Color::blue).any { channel -> abs(channel(a) - channel(b)) > INK_CONTRAST }

    /** Wait for the pane text to satisfy `done`; the failure never quotes the text. */
    private fun awaitShown(what: String, done: (String) -> Boolean) {
        ui.awaitUi(what) { _ -> true.takeIf { done(shownText()) } }
    }

    private fun logcat(vararg args: String): String {
        val process = ProcessBuilder(listOf("logcat", "--pid=${Process.myPid()}", "-v", "raw") + args)
            .redirectErrorStream(true)
            .start()
        // `Process.waitFor` with a timeout needs API 26. The output is read
        // as it comes, so a long dump never fills the pipe.
        val ended = CountDownLatch(1)
        var output = ""
        val waiter = Thread {
            // A destroyed logcat interrupts the read; the timeout reports it.
            runCatching { output = process.inputStream.bufferedReader().readText() }
            process.waitFor()
            ended.countDown()
        }.apply { start() }
        try {
            check(ended.await(TIMEOUT_MS, TimeUnit.MILLISECONDS)) { "logcat ${args.joinToString(" ")} did not end within ${TIMEOUT_MS}ms" }
            return output
        } finally {
            process.destroy()
            waiter.join()
        }
    }

    /** Block until this process logs a native line matching `regex`, and return that line. */
    private fun awaitNativeLog(regex: String): String =
        logcat("-e", regex, "-m", "1", "-s", "wezterm:V").lines().single { Regex(regex).containsMatchIn(it) }

    /** Nothing this process logged holds `needle`, raw or as a hex escape. */
    private fun assertNoNeedle(phase: String, needle: String, hex: String) {
        val lines = logcat("-d").lines()
        val holding = lines.count { needle in it || it.contains(hex, ignoreCase = true) }
        Log.i(TAG, "receipt phase=$phase scanned_lines=${lines.size} needle_lines=$holding")
        assertEquals("lines of this process holding the shaped text", 0, holding)
    }

    private companion object {
        /** A channel difference that counts as ink: a quarter of the channel's range. */
        const val INK_CONTRAST = 64
    }
}
