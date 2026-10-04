package org.wezterm.android

import android.content.ComponentName
import android.content.Intent
import android.graphics.Rect
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.provider.Settings
import android.util.Base64
import android.view.Choreographer
import android.view.InputDevice
import android.view.MotionEvent
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.accessibility.AccessibilityNodeInfo
import android.view.inputmethod.EditorInfo
import org.json.JSONArray
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.Properties
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Signed-release protocol fixture, not private-route or physical-phone acceptance. */
@RunWith(AndroidJUnit4::class)
class ReleaseSshMuxTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val automation = instrumentation.uiAutomation
    private val context = instrumentation.targetContext
    private val nativeClass = Class.forName("org.wezterm.android.NativeApp")
    // Same empirical test deadline as the existing TerminalHarness; driven by UI/vsync events.
    private val timeoutMs = 30000L
    private val directory = "/data/local/tmp/wezterm-sshmux"
    private val fixture = Properties().apply { load(shell("cat $directory/fixture.properties").reader()) }

    private fun shell(command: String): String = automation.executeShellCommand(command).use {
        java.io.FileInputStream(it.fileDescriptor).bufferedReader().readText()
    }

    private fun status(method: String): JSONObject = JSONObject(
        nativeClass.getDeclaredMethod(method).apply { isAccessible = true }.invoke(null) as String,
    )
    private fun connection() = status("nativeConnectionStatus")
    private fun surface() = status("nativeSurfaceStatus").getJSONObject("surface")

    private fun find(root: AccessibilityNodeInfo?, label: String, clickable: Boolean = false, exact: Boolean = false, className: String? = null): AccessibilityNodeInfo? {
        if (root == null) return null
        val text = root.text?.toString()
        if (root.isVisibleToUser && (!clickable || root.isClickable) && (className == null || root.className == className) &&
            ((if (exact) text?.equals(label, true) == true else text?.contains(label, true) == true) || root.contentDescription?.toString() == label)) return root
        for (index in 0 until root.childCount) find(root.getChild(index), label, clickable, exact, className)?.let { return it }
        return null
    }
    private fun node(label: String) = checkNotNull(find(automation.rootInActiveWindow, label)) { "Visible node missing: $label" }
    private fun action(label: String, className: String = "android.widget.Button") =
        find(automation.rootInActiveWindow, label, clickable = true, exact = true, className = className)
    private fun click(label: String, className: String = "android.widget.Button") {
        val target = checkNotNull(action(label, className)) { "Exact $className action missing: $label" }
        assertTrue(target.isEnabled)
        assertTrue(target.performAction(AccessibilityNodeInfo.ACTION_CLICK))
        instrumentation.waitForIdleSync()
    }
    private fun setText(label: String, text: String) {
        assertTrue(node(label).performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text)
        }))
    }

    /** Observe production state on display events; no wait-only JNI or timed sleeps. */
    private fun await(what: String, ready: () -> Boolean) {
        val done = CountDownLatch(1)
        val deadline = SystemClock.elapsedRealtime() + timeoutMs
        var failure: Throwable? = null
        val callback = object : Choreographer.FrameCallback {
            override fun doFrame(frameTimeNanos: Long) {
                try {
                    check(connection().getString("phase") != "failed") { "Connection failed: ${connection().optJSONObject("failure")}" }
                    val engine = status("nativeSurfaceStatus").getJSONObject("engine")
                    check(engine.getString("status") != "failed") { "Production engine failed: $engine" }
                    if (ready()) done.countDown()
                    else if (SystemClock.elapsedRealtime() >= deadline) {
                        failure = AssertionError("Timed out: $what")
                        done.countDown()
                    } else Choreographer.getInstance().postFrameCallback(this)
                } catch (error: Throwable) {
                    failure = error
                    done.countDown()
                }
            }
        }
        instrumentation.runOnMainSync { Choreographer.getInstance().postFrameCallback(callback) }
        try {
            assertTrue("Timed out: $what", done.await(timeoutMs, TimeUnit.MILLISECONDS))
            failure?.let { throw it }
        } catch (error: Throwable) {
            android.util.Log.i("WezTermSshMuxTest", "failed $what connection=${connection()} surface=${status("nativeSurfaceStatus")}")
            screenshot("failed")
            throw error
        } finally {
            instrumentation.runOnMainSync { Choreographer.getInstance().removeFrameCallback(callback) }
        }
        instrumentation.waitForIdleSync()
    }

    private fun fingerTap(at: Rect) {
        val down = SystemClock.uptimeMillis()
        val pointer = arrayOf(MotionEvent.PointerProperties().apply { id = 0; toolType = MotionEvent.TOOL_TYPE_FINGER })
        val coordinates = arrayOf(MotionEvent.PointerCoords().apply {
            x = at.exactCenterX(); y = at.exactCenterY(); pressure = 1f; size = 1f
        })
        for (action in listOf(MotionEvent.ACTION_DOWN, MotionEvent.ACTION_UP)) {
            val event = MotionEvent.obtain(down, SystemClock.uptimeMillis(), action, 1, pointer, coordinates,
                0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0)
            assertTrue("Picker ${MotionEvent.actionToString(action)} not delivered at $at", automation.injectInputEvent(event, true))
            event.recycle()
        }
    }

    private fun importIdentity() {
        click("Import SSH key…")
        val device = Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME) ?: Build.MODEL
        var rootsOpened = false
        var internalHidden = false
        var internalShown = false
        var settled: Pair<String, Rect>? = null
        val deadline = SystemClock.elapsedRealtime() + timeoutMs
        while (!connection().getBoolean("identity")) {
            check(SystemClock.elapsedRealtime() < deadline) { "Picker/import did not finish" }
            automation.waitForIdle(300, 5000)
            val root = automation.rootInActiveWindow ?: continue
            if (root.packageName == context.packageName) continue
            check(root.packageName.toString().endsWith(".documentsui")) { "Unexpected picker package" }
            val file = find(root, "wezterm-fixture-key", exact = true)
            val roots = find(root, "Show roots", exact = true)
            val deviceRoot = find(root, device, exact = true)
            if (rootsOpened && !internalShown && deviceRoot == null && find(root, "Open from") != null) internalHidden = true
            val showInternal = find(root, "Show internal storage")
            val step = file ?: find(root, "wezterm-fixture", exact = true) ?: find(root, "Download", exact = true)
                ?: deviceRoot?.takeIf { rootsOpened } ?: showInternal
                ?: find(root, "More options")?.takeIf { internalHidden && !internalShown } ?: roots ?: continue
            val at = (step.text ?: step.contentDescription).toString() to Rect().also { step.getBoundsInScreen(it) }
            val window = Rect().also { root.getBoundsInScreen(it) }
            if (!window.contains(at.second.centerX(), at.second.centerY())) {
                step.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_SHOW_ON_SCREEN.id)
                settled = null
                continue
            }
            if (at != settled) { settled = at; continue }
            settled = null
            android.util.Log.i("WezTermSshMuxTest", "picker step ${at.first} bounds=${at.second} window=${root.windowId}")
            fingerTap(at.second)
            if (step === roots) rootsOpened = true
            if (step === showInternal) internalShown = true
            if (file != null) break
        }
        await("import result") { connection().getBoolean("identity") && find(automation.rootInActiveWindow, "SSH key imported.") != null }
    }

    private fun screenshot(name: String) {
        shell("mkdir -p $directory/release-shots")
        shell("screencap -p $directory/release-shots/$name.png")
    }

    private fun hostProof(name: String) {
        // The owned laptop verifier checks canonical pane IDs, literal execution and
        // no replay before acknowledging. This is a public fixture file, not JNI.
        val before = surface().getLong("total_frames_presented")
        await("owned laptop $name proof") { shell("cat $directory/host-$name").trim() == "verified" }
        if (name == "selected" || name == "reconnected") {
            // The host prints a literal note in the selected shell, so stale frames
            // from a previous binding cannot stand in for the laptop update.
            await("selected laptop update presented") { surface().getLong("total_frames_presented") > before }
        }
    }

    private fun selectShell() {
        val panes = JSONArray(fixture.getProperty("input_panes"))
        val shellPane = (0 until panes.length()).map { panes.getJSONObject(it) }.single { it.getLong("pane_id") == 0L }
        val title = shellPane.getString("title")
        check(title.startsWith("ANDROID07 SHELL ")) { "Owned shell needs a unique controlled title" }
        await("owned shell window title") {
            val windows = surface().getJSONArray("windows")
            (0 until windows.length()).count { windows.getJSONObject(it).getString("title") == title } == 1
        }
        val current = surface()
        val windows = current.getJSONArray("windows")
        val index = (0 until windows.length()).single { windows.getJSONObject(it).getLong("id") == current.getLong("bound_window") }
        click("Window ${index + 1} of ${windows.length()}", "android.widget.TextView")
        click(title, "android.widget.CheckedTextView")
        await("selected canonical shell window") {
            val selected = surface()
            val listed = selected.getJSONArray("windows")
            (0 until listed.length()).any {
                val window = listed.getJSONObject(it)
                window.getLong("id") == selected.getLong("bound_window") && window.getString("title") == title
            } && selected.getLong("input_pane") >= 0 && selected.getLong("frames_presented") > 0
        }
    }

    private fun receipt(name: String) {
        // Production counters only; omit endpoint, user, prompt and pane titles.
        val current = surface()
        val connection = connection()
        val counters = JSONObject().put("phase", connection.getString("phase"))
            .put("attempt", connection.optLong("attempt")).put("domain", connection.getString("domain"))
            .put("workers", connection.getInt("workers")).put("windows", current.getJSONArray("windows").length())
            .put("frames", current.getLong("total_frames_presented")).put("input", current.getJSONObject("input"))
            .put("bound_window", current.opt("bound_window")).put("input_pane", current.opt("input_pane"))
            .put("input_generation", current.opt("input_generation"))
        android.util.Log.i("WezTermSshMuxTest", "release receipt $name $counters")
    }

    @Test
    fun productionPickerTrustAttachInputDisconnectAndReconnect() {
        assertEquals("self-tailscale-address-via-lo", fixture.getProperty("route"))
        val activity = instrumentation.startActivitySync(Intent().setComponent(
            ComponentName(context.packageName, "org.wezterm.android.TerminalActivity"),
        ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        await("production profile") { connection().getBoolean("ready") && action("Connect")?.isEnabled == true }
        assertTrue("Use a clean owned app install, never operator state", !connection().getBoolean("identity"))
        importIdentity()
        setText("Laptop Tailscale address", fixture.getProperty("host"))
        setText("SSH port", fixture.getProperty("port"))
        setText("SSH user", fixture.getProperty("user"))
        setText("wezterm path", fixture.getProperty("wezterm_input"))
        click("Connect")
        await("own trust prompt") { connection().optJSONObject("prompt")?.optString("kind") == "host_trust" && find(automation.rootInActiveWindow, "Trust") != null }
        val digest = Base64.decode(fixture.getProperty("host_fingerprint").removePrefix("SHA256:"), Base64.NO_PADDING)
        val fingerprint = digest.joinToString(":") { "%02x".format(it) }
        assertEquals(fingerprint, connection().getJSONObject("prompt").getString("fingerprint"))
        assertTrue(node(fingerprint).isVisibleToUser)
        android.util.Log.i("WezTermSshMuxTest", "verified visible fixture fingerprint $fingerprint")
        click("Trust")
        await("attached rendered pane") { connection().getString("phase") == "attached" && surface().getLong("frames_presented") > 0 }
        val firstAttempt = connection().getLong("attempt")
        receipt("attached")
        selectShell()
        receipt("selected")
        hostProof("selected")
        assertTrue(surface().getInt("cell_width") > 0)
        assertTrue(surface().getInt("cell_height") > 0)
        screenshot("laptop-update")

        fun terminal(root: View): SurfaceView? {
            if (root is SurfaceView) return root
            if (root is ViewGroup) for (index in 0 until root.childCount) terminal(root.getChildAt(index))?.let { return it }
            return null
        }
        val before = surface().getLong("total_frames_presented")
        instrumentation.runOnMainSync {
            val view = checkNotNull(terminal(activity.window.decorView))
            assertTrue(view.requestFocus())
            val input = checkNotNull(view.onCreateInputConnection(EditorInfo()))
            assertTrue(input.commitText("printf 'ANDROID07_RELEASE_INPUT\\n' | tee -a ~/release-input.receipt\n", 1))
            input.closeConnection()
        }
        await("input repaint") { surface().getLong("total_frames_presented") > before && surface().getJSONObject("input").getLong("commits") > 0 }
        receipt("input")
        hostProof("input")
        screenshot("input")
        click("Connection: disconnect from the laptop")
        click("Disconnect")
        await("explicit disconnect") { connection().getString("phase") == "disconnected" && !connection().getBoolean("closing") && action("Reconnect")?.isEnabled == true }
        assertEquals(0, connection().getInt("workers"))
        assertEquals("none", connection().getString("domain"))
        assertTrue(connection().isNull("prompt"))
        assertEquals(0, surface().getJSONArray("windows").length())
        val dropped = surface().getJSONObject("input").getLong("dropped")
        nativeClass.getDeclaredMethod("nativeInputCommit", Int::class.javaPrimitiveType, Long::class.javaPrimitiveType,
            Long::class.javaPrimitiveType, String::class.java, Int::class.javaPrimitiveType)
            .apply { isAccessible = true }.invoke(null, 0, -1L, 0L, "ANDROID07_NO_REPLAY", 0)
        await("disconnected input drop") { surface().getJSONObject("input").getLong("dropped") > dropped }
        receipt("disconnected")
        hostProof("disconnected")
        click("Reconnect")
        await("explicit reattach") { connection().getString("phase") == "attached" && connection().getLong("attempt") > firstAttempt && surface().getLong("frames_presented") > 0 }
        assertEquals(firstAttempt + 1, connection().getLong("attempt"))
        selectShell()
        receipt("reconnected")
        hostProof("reconnected")
        screenshot("reconnected")
        click("Connection: disconnect from the laptop")
        click("Disconnect")
        await("final disconnect") { connection().getString("phase") == "disconnected" && !connection().getBoolean("closing") }
        assertEquals(0, connection().getInt("workers"))
        assertEquals("none", connection().getString("domain"))
        assertTrue(connection().isNull("prompt"))
        assertEquals(0, surface().getJSONArray("windows").length())
        receipt("finished")
        hostProof("finished")
    }
}
