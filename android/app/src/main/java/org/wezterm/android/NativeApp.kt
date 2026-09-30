package org.wezterm.android

import android.content.Context
import org.json.JSONObject

/**
 * The single JNI surface of `libwezterm_android.so`.
 *
 * Symbol names are `Java_org_wezterm_android_NativeApp_<method>`; keep them in
 * sync with `wezterm-android/src/ffi.rs`.
 */
object NativeApp {
    init {
        System.loadLibrary("wezterm_android")
    }

    @JvmStatic
    private external fun nativeInitialize(
        filesDir: String,
        cacheDir: String,
        dpi: Int,
        verboseLogging: Boolean,
    ): String

    /** Debug builds only; the symbol is absent from release libraries. */
    @JvmStatic
    external fun nativeDiagnosticFault(kind: Int): String

    /** Initialize the native engine (idempotent) and parse its report. */
    fun initialize(context: Context): InitResponse {
        val app = context.applicationContext
        val json = nativeInitialize(
            app.filesDir.absolutePath,
            app.cacheDir.absolutePath,
            app.resources.displayMetrics.densityDpi,
            BuildConfig.DEBUG,
        )
        return InitResponse.parse(json)
    }
}

/** Mirror of the Rust `InitResponse` envelope. */
data class InitResponse(val initCalls: Int, val outcome: InitOutcome, val rawJson: String) {
    companion object {
        fun parse(json: String): InitResponse {
            val root = JSONObject(json)
            val outcome = root.getJSONObject("outcome")
            val parsed = when (val status = outcome.getString("status")) {
                "ready" -> InitOutcome.Ready(
                    engineInitializations = outcome.getInt("engine_initializations"),
                    arch = outcome.getString("arch"),
                    weztermVersion = outcome.getString("wezterm_version"),
                    codecVersion = outcome.getInt("codec_version"),
                    initDurationMs = outcome.getLong("init_duration_ms"),
                    defaultFont = outcome.getJSONObject("fonts").getJSONArray("default_font").let { arr ->
                        List(arr.length()) { arr.getString(it) }
                    },
                    rasterizedGlyphs = outcome.getJSONObject("shaping").getInt("rasterized_count"),
                    home = outcome.getJSONObject("dirs").getString("home"),
                )
                "failed" -> InitOutcome.Failed(
                    stage = outcome.getString("stage"),
                    message = outcome.getString("message"),
                )
                else -> error("unknown native status '$status'")
            }
            return InitResponse(root.getInt("init_calls"), parsed, json)
        }
    }
}

sealed interface InitOutcome {
    data class Ready(
        val engineInitializations: Int,
        val arch: String,
        val weztermVersion: String,
        val codecVersion: Int,
        val initDurationMs: Long,
        val defaultFont: List<String>,
        val rasterizedGlyphs: Int,
        val home: String,
    ) : InitOutcome

    data class Failed(val stage: String, val message: String) : InitOutcome
}
