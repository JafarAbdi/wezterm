package org.wezterm.android

import android.app.Activity
import android.graphics.Typeface
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.util.TypedValue
import android.widget.ScrollView
import android.widget.TextView
import java.util.concurrent.Executors

/** Initializes the native engine off the UI thread and shows its report. */
class DiagnosticActivity : Activity() {
    private lateinit var text: TextView
    private val worker = Executors.newSingleThreadExecutor()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        text = TextView(this).apply {
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
            setTextIsSelectable(true)
            setPadding(24, 24, 24, 24)
            text = "loading libwezterm_android.so …"
        }
        setContentView(ScrollView(this).apply { addView(text) })
        worker.execute { runDiagnostic() }
    }

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }

    private fun runDiagnostic() {
        val startedNs = System.nanoTime()
        val report = try {
            val response = NativeApp.initialize(this)
            val elapsedMs = (System.nanoTime() - startedNs) / 1_000_000
            val header = when (val outcome = response.outcome) {
                is InitOutcome.Ready -> buildString {
                    appendLine("native-load status=ready")
                    appendLine("abi=${Build.SUPPORTED_ABIS.first()} arch=${outcome.arch} api=${Build.VERSION.SDK_INT}")
                    appendLine("wezterm=${outcome.weztermVersion} codec=${outcome.codecVersion}")
                    appendLine("engine_initializations=${outcome.engineInitializations} init_calls=${response.initCalls}")
                    appendLine("native_init_ms=${outcome.initDurationMs} jni_roundtrip_ms=$elapsedMs")
                    appendLine("rasterized_glyphs=${outcome.rasterizedGlyphs} default_font=${outcome.defaultFont.firstOrNull()}")
                    appendLine("home=${outcome.home}")
                }
                is InitOutcome.Failed ->
                    "native-load status=failed stage=${outcome.stage}\n${outcome.message}\n"
            }
            Log.i(TAG, header.lineSequence().first())
            header + "\n" + response.rawJson
        } catch (e: RuntimeException) {
            Log.e(TAG, "native-load status=exception", e)
            "native-load status=exception\n${e}"
        }
        runOnUiThread { text.text = report }
    }

    companion object {
        const val TAG = "WezTermDiag"
    }
}
