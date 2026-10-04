package org.wezterm.android

import android.content.ComponentName
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class NativeLoadReleaseTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext
    private val nativeClass = Class.forName("org.wezterm.android.NativeApp")

    private fun status(method: String): JSONObject = JSONObject(
        nativeClass.getDeclaredMethod(method).apply { isAccessible = true }.invoke(null) as String,
    )

    @Test
    fun closureLoadsAndInitializesOnce() {
        val initialize = nativeClass.getDeclaredMethod(
            "nativeInitialize", String::class.java, String::class.java,
            Int::class.javaPrimitiveType, Boolean::class.javaPrimitiveType,
        ).apply { isAccessible = true }
        fun initializeOnce() = JSONObject(initialize.invoke(
            null, context.filesDir.absolutePath, context.cacheDir.absolutePath,
            context.resources.displayMetrics.densityDpi, false,
        ) as String)
        val first = initializeOnce()
        val second = initializeOnce()
        val ready = first.getJSONObject("outcome")
        assertEquals("ready", ready.getString("status"))
        assertEquals(1, ready.getInt("engine_initializations"))
        assertEquals(first.getInt("init_calls") + 1, second.getInt("init_calls"))
        assertEquals(ready.toString(), second.getJSONObject("outcome").toString())
        assertTrue(ready.getJSONObject("shaping").getInt("rasterized_count") > 0)
        assertTrue(ready.getJSONObject("fonts").getJSONArray("default_font").length() > 0)
        assertEquals(if (Build.SUPPORTED_ABIS.first() == "arm64-v8a") "aarch64" else "x86_64", ready.getString("arch"))
        assertFalse(context.applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE != 0)
    }

    private fun node(text: String): AccessibilityNodeInfo {
        fun find(current: AccessibilityNodeInfo): AccessibilityNodeInfo? {
            if (current.isVisibleToUser && current.text?.toString()?.contains(text, ignoreCase = true) == true) return current
            for (index in 0 until current.childCount) {
                current.getChild(index)?.let { child -> find(child)?.let { return it } }
            }
            return null
        }
        return checkNotNull(find(instrumentation.uiAutomation.rootInActiveWindow)) { "Visible node missing: $text" }
    }

    private fun setText(hint: String, text: String) {
        val arguments = Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text)
        }
        assertTrue(node(hint).performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, arguments))
        instrumentation.waitForIdleSync()
    }

    @Test
    fun publicLaunchRefusesForeignAddressesAndOpensTheDocumentPicker() {
        val forbidden = Intent().setComponent(ComponentName(context.packageName, "org.wezterm.android.DiagnosticActivity"))
        assertEquals(null, context.packageManager.resolveActivity(forbidden, PackageManager.MATCH_DEFAULT_ONLY))
        val launch = Intent().setComponent(ComponentName(context.packageName, "org.wezterm.android.TerminalActivity"))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            .putExtra("org.wezterm.android.DIAGNOSTIC_APPLET", true)
            .putExtra("org.wezterm.android.CONFIG_OVERRIDES", "check_for_updates=true")
        instrumentation.uiAutomation.executeAndWaitForEvent(
            { context.startActivity(launch) },
            { status("nativeConnectionStatus").getBoolean("ready") },
            30000,
        )
        instrumentation.waitForIdleSync()
        assertTrue(node("Connect").isEnabled)
        setText("Laptop Tailscale address", "192.0.2.1")
        setText("SSH user", "not-a-real-user")
        assertTrue(node("Connect").performAction(AccessibilityNodeInfo.ACTION_CLICK))
        instrumentation.waitForIdleSync()
        assertTrue(node("192.0.2.1").error != null)
        val connection = status("nativeConnectionStatus")
        assertEquals("idle", connection.getString("phase"))
        assertEquals(0, connection.getInt("workers"))
        assertEquals("none", connection.getString("domain"))
        assertEquals(0, status("nativeSurfaceStatus").getJSONObject("surface").getJSONArray("windows").length())
        instrumentation.uiAutomation.executeAndWaitForEvent(
            { assertTrue(node("Import SSH key").performAction(AccessibilityNodeInfo.ACTION_CLICK)) },
            {
                it.packageName?.toString()?.endsWith(".documentsui") == true &&
                    instrumentation.uiAutomation.rootInActiveWindow?.packageName?.toString()?.endsWith(".documentsui") == true
            },
            30000,
        )
    }
}
