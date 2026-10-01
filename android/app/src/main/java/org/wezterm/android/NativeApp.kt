package org.wezterm.android

import android.content.Context
import android.view.Surface
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

    @JvmStatic
    private external fun nativeTerminalStart(
        filesDir: String,
        cacheDir: String,
        dpi: Int,
        verboseLogging: Boolean,
        diagnosticApplet: Boolean,
        configOverrides: String,
    ): String

    /** Hands the surface's native window to the GUI thread. Throws when the engine is not running. */
    @JvmStatic
    external fun nativeSurfaceCreated(generation: Long, surface: Surface, width: Int, height: Int)

    @JvmStatic
    external fun nativeSurfaceChanged(generation: Long, width: Int, height: Int)

    /**
     * Blocks until the GUI thread released the native window. False when the
     * engine ended instead: its shutdown has finished, but it did not confirm
     * the release.
     */
    @JvmStatic
    external fun nativeSurfaceDestroyed(generation: Long): Boolean

    /** Binds logical window `id` to the surface. Throws when the engine is not accepting events. */
    @JvmStatic
    external fun nativeSelectWindow(id: Long)

    /** Blocks until the GUI thread asks for something; null once the engine ended. See [PlatformRequests]. */
    @JvmStatic
    external fun nativeNextRequest(): String?

    /** Answers a `clipboard_get` request; null `text` means the read was refused. Throws when the engine ended. */
    @JvmStatic
    external fun nativeClipboardText(request: Long, text: String?)

    /** Blocks until the status revision differs from `since`, or `timeoutMs` elapsed; returns the current revision. */
    @JvmStatic
    external fun nativeAwaitSurfaceChange(since: Long, timeoutMs: Long): Long

    @JvmStatic
    private external fun nativeSurfaceStatus(): String

    /** Blocks until `minFrames` frames were presented on `generation`, or `timeoutMs` elapsed. */
    @JvmStatic
    external fun nativeAwaitSurfaceFrames(generation: Long, minFrames: Long, timeoutMs: Long): Boolean

    /** Blocks until the surface slot is in `state` (`generation <= 0` accepts any), or `timeoutMs` elapsed. */
    @JvmStatic
    external fun nativeAwaitSurfaceState(state: String, generation: Long, timeoutMs: Long): Boolean

    /** Blocks until `minFailures` GPU failures of `stage` were recorded, or `timeoutMs` elapsed. */
    @JvmStatic
    external fun nativeAwaitRenderFailures(stage: Int, minFailures: Long, timeoutMs: Long): Boolean

    /**
     * Debug builds only; the symbol is absent from release libraries.
     * `0` panics and any kind that is not a render stage throws;
     * [STAGE_GPU_CREATION] and [STAGE_DRAW] arm a one-shot GPU failure on
     * the GUI thread.
     */
    @JvmStatic
    external fun nativeDiagnosticFault(kind: Int): String

    /**
     * Debug builds only. Runs `open-window`, `paste`,
     * `panic-on-queued-destroy` or `panic-with-clipboard-read` on the GUI
     * thread, or arms `panic-in-surface-lost`; false when the command is
     * unknown or no GUI thread accepts work. `panic-with-clipboard-read`
     * blocks until the read resolves and is also false when it did not fail.
     */
    @JvmStatic
    external fun nativeDiagnosticGui(command: String): Boolean

    /** Render stage codes shared by [nativeDiagnosticFault] and [nativeAwaitRenderFailures]. */
    const val STAGE_GPU_CREATION = 2
    const val STAGE_DRAW = 3

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

    /**
     * Start the GUI thread once per process and return its state as JSON.
     * Debug builds open the diagnostic applet; `configOverrides` holds
     * `key=value` lines and is honoured by debug builds only.
     */
    fun startTerminal(context: Context, configOverrides: String): String {
        val app = context.applicationContext
        PlatformRequests.start(app)
        return nativeTerminalStart(
            app.filesDir.absolutePath,
            app.cacheDir.absolutePath,
            app.resources.displayMetrics.densityDpi,
            BuildConfig.DEBUG,
            BuildConfig.DEBUG,
            if (BuildConfig.DEBUG) configOverrides else "",
        )
    }

    fun surfaceStatus(): SurfaceStatus = SurfaceStatus.parse(nativeSurfaceStatus())
}

/** Mirror of the Rust `terminal::Status`. */
data class SurfaceStatus(
    val engine: String,
    /** Why the engine failed; empty otherwise. */
    val engineMessage: String,
    val revision: Long,
    val state: String,
    val generation: Long,
    val width: Int,
    val height: Int,
    val framesPresented: Long,
    val totalFramesPresented: Long,
    val staleEvents: Long,
    val retireAcks: Long,
    val liveLeases: Int,
    val boundWindow: Long?,
    /** Every logical window, bound or surfaceless. */
    val windows: List<WindowSummary>,
    val closedWindows: Long,
    val clipboardRequests: Long,
    val clipboardResponses: Long,
    val loopWakeups: Long,
    val gpuCreationFailures: Long,
    val drawFailures: Long,
    val rawJson: String,
) {
    companion object {
        fun parse(json: String): SurfaceStatus {
            val root = JSONObject(json)
            val surface = root.getJSONObject("surface")
            val render = root.getJSONObject("render")
            val engine = root.getJSONObject("engine")
            val windows = surface.getJSONArray("windows")
            return SurfaceStatus(
                engine = engine.getString("status"),
                engineMessage = engine.optString("message"),
                revision = surface.getLong("revision"),
                state = surface.getString("state"),
                generation = if (surface.isNull("generation")) 0 else surface.getLong("generation"),
                width = surface.getInt("width"),
                height = surface.getInt("height"),
                framesPresented = surface.getLong("frames_presented"),
                totalFramesPresented = surface.getLong("total_frames_presented"),
                staleEvents = surface.getLong("stale_events"),
                retireAcks = surface.getLong("retire_acks"),
                liveLeases = surface.getInt("live_leases"),
                boundWindow = if (surface.isNull("bound_window")) null else surface.getLong("bound_window"),
                windows = List(windows.length()) {
                    val window = windows.getJSONObject(it)
                    WindowSummary(window.getLong("id"), window.getString("title"))
                },
                closedWindows = surface.getLong("closed_windows"),
                clipboardRequests = surface.getLong("clipboard_requests"),
                clipboardResponses = surface.getLong("clipboard_responses"),
                loopWakeups = surface.getLong("loop_wakeups"),
                gpuCreationFailures = render.getLong("gpu_creation"),
                drawFailures = render.getLong("draw"),
                rawJson = json,
            )
        }
    }
}

/** One logical window as the native side lists it. */
data class WindowSummary(val id: Long, val title: String)

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
