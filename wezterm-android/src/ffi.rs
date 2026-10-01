//! JNI boundary for `org.wezterm.android.NativeApp`.
//!
//! Ownership and safety contract:
//!
//! * Every export takes an [`EnvUnowned`] and upgrades it with `with_env`,
//!   which attaches the frame and wraps the body in `catch_unwind`.  A Rust
//!   panic or error becomes a Java `RuntimeException` via
//!   [`ThrowRuntimeExAndDefault`]; nothing unwinds into the JVM.
//! * No `Env`, local reference or Java object escapes its export.  The
//!   only native resource taken from a Java object is the `ANativeWindow`
//!   behind a `Surface`, acquired here on the calling Java thread and moved
//!   into a [`NativeWindowLease`] that the GUI thread owns.  The GUI thread
//!   never calls back into Java, so a Java thread blocked in
//!   `nativeSurfaceDestroyed` cannot be waited on by the GUI thread.  What
//!   the GUI thread needs from Java (clipboard, selector refresh) a Java
//!   thread fetches by blocking in `nativeNextRequest`.
//! * The process environment is never mutated; sandbox paths reach `config`
//!   through `config::set_android_paths`.
//! * Symbol names must match the `external fun` declarations in
//!   `android/app/src/main/java/org/wezterm/android/NativeApp.kt`.
//!
//! [`NativeWindowLease`]: window::os::android::NativeWindowLease

#![allow(unsafe_code)]

use crate::InitRequest;
use crate::terminal::{self, RetireOutcome, StartRequest, SurfaceBridgeError};
use jni::EnvUnowned;
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jint, jlong};
use ndk::native_window::NativeWindow;
use std::path::PathBuf;
use std::sync::Once;

static LOGGER: Once = Once::new();

fn install_logger(verbose: bool) {
    LOGGER.call_once(|| {
        let level = if verbose {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        };
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(level)
                .with_tag("wezterm"),
        );
    });
}

/// Failures an export reports to Java as `RuntimeException`.
#[derive(Debug, thiserror::Error)]
enum Fault {
    #[error(transparent)]
    Surface(#[from] SurfaceBridgeError),
    #[error(transparent)]
    NotAccepting(#[from] crate::engine::NotAccepting),
    #[error("{0}")]
    Overrides(String),
    #[error(transparent)]
    Jni(#[from] jni::errors::Error),
}

fn init_request(
    files_dir: &JString<'_>,
    cache_dir: &JString<'_>,
    dpi: jint,
    verbose_logging: jboolean,
) -> InitRequest {
    let verbose = verbose_logging == JNI_TRUE;
    install_logger(verbose);
    InitRequest {
        files_dir: PathBuf::from(files_dir.to_string()),
        cache_dir: PathBuf::from(cache_dir.to_string()),
        dpi: u32::try_from(dpi).unwrap_or(0),
        verbose_logging: verbose,
    }
}

/// `NativeApp.nativeInitialize(filesDir, cacheDir, dpi, verboseLogging)`.
///
/// Returns the JSON [`crate::InitResponse`].  Idempotent: later calls return
/// the same outcome with an incremented `init_calls`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeInitialize<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    files_dir: JString<'caller>,
    cache_dir: JString<'caller>,
    dpi: jint,
    verbose_logging: jboolean,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            let request = init_request(&files_dir, &cache_dir, dpi, verbose_logging);
            let response = crate::initialize(&request);
            log::info!(
                "nativeInitialize call {} -> {}",
                response.init_calls,
                match response.outcome {
                    crate::InitOutcome::Ready(_) => "ready",
                    crate::InitOutcome::Failed { .. } => "failed",
                }
            );
            JString::from_str(env, response.to_json())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeTerminalStart(filesDir, cacheDir, dpi, verboseLogging,
/// diagnosticApplet, configOverrides)`.
///
/// Starts the GUI thread once per process and returns the JSON
/// [`terminal::EngineState`] observed right after.  Surface callbacks may
/// follow immediately; they queue until the engine accepts work.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeTerminalStart<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    files_dir: JString<'caller>,
    cache_dir: JString<'caller>,
    dpi: jint,
    verbose_logging: jboolean,
    diagnostic_applet: jboolean,
    config_overrides: JString<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> Result<_, Fault> {
            let init = init_request(&files_dir, &cache_dir, dpi, verbose_logging);
            let config_overrides = crate::parse_config_overrides(&config_overrides.to_string())
                .map_err(Fault::Overrides)?;
            let state = terminal::start(StartRequest {
                init,
                diagnostic_applet: diagnostic_applet == JNI_TRUE,
                config_overrides,
            });
            log::info!("nativeTerminalStart -> {state:?}");
            Ok(JString::from_str(
                env,
                serde_json::to_string(&state).expect("EngineState serializes"),
            )?)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeSurfaceCreated(generation, surface, width, height)`.
///
/// Acquires the `ANativeWindow` behind `surface` and hands it to the GUI
/// thread.  Throws when the generation is invalid, the surface has no
/// native window, or the engine is not running.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeSurfaceCreated<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    generation: jlong,
    surface: JObject<'caller>,
    width: jint,
    height: jint,
) {
    unowned_env
        .with_env(|env| -> Result<(), Fault> {
            let window = if surface.is_null() {
                None
            } else {
                // SAFETY: `env` is the live JNIEnv of this JNI call and
                // `surface` is the non-null `android.view.Surface` local
                // reference Java passed; both outlive this call.  jni 0.22
                // (jni-sys 0.4) and ndk 0.9 (jni-sys 0.3) declare the same C
                // `JNIEnv`/`jobject` layouts, so the pointer casts preserve
                // the types.  `from_surface` returns an acquired reference
                // that `NativeWindow` releases on drop.
                unsafe { NativeWindow::from_surface(env.get_raw().cast(), surface.as_raw().cast()) }
            };
            terminal::surface_created(generation, window, width, height)?;
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeSurfaceChanged(generation, width, height)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeSurfaceChanged<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    generation: jlong,
    width: jint,
    height: jint,
) {
    unowned_env
        .with_env(|_env| -> Result<(), Fault> {
            terminal::surface_changed(generation, width, height)?;
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeSurfaceDestroyed(generation)`: blocks until the GUI
/// thread acknowledged the retirement; false when the engine dropped it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeSurfaceDestroyed<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    generation: jlong,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> Result<jboolean, Fault> {
            Ok(match terminal::surface_destroyed(generation)? {
                RetireOutcome::Acknowledged => JNI_TRUE,
                RetireOutcome::Dropped => JNI_FALSE,
            })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeSelectWindow(id)`: bind logical window `id` to the
/// surface.  Throws when the engine is not accepting events.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeSelectWindow<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jlong,
) {
    unowned_env
        .with_env(|_env| -> Result<(), Fault> { Ok(terminal::select_window(id)?) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeNextRequest()`: block until the GUI thread asks the
/// platform for something and return the JSON
/// [`window::os::android::PlatformRequest`]; null once the engine ended.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeNextRequest<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            match terminal::next_request() {
                Some(request) => JString::from_str(
                    env,
                    serde_json::to_string(&request).expect("PlatformRequest serializes"),
                ),
                None => Ok(JString::null()),
            }
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeClipboardText(request, text)`: the answer to a
/// `clipboard_get` request; a null `text` means the read was refused.
/// Throws when the engine is not accepting events.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeClipboardText<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request: jlong,
    text: JString<'caller>,
) {
    unowned_env
        .with_env(|_env| -> Result<(), Fault> {
            let text = (!text.is_null()).then(|| text.to_string());
            Ok(terminal::clipboard_text(request, text)?)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeAwaitSurfaceChange(since, timeoutMs)`: block until the
/// status revision differs from `since`; returns the current revision.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeAwaitSurfaceChange<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    since: jlong,
    timeout_ms: jlong,
) -> jlong {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jlong> {
            Ok(terminal::await_change(since, timeout_ms))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeSurfaceStatus()`: JSON [`terminal::Status`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeSurfaceStatus<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            JString::from_str(
                env,
                serde_json::to_string(&terminal::status()).expect("Status serializes"),
            )
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeAwaitSurfaceFrames(generation, minFrames, timeoutMs)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeAwaitSurfaceFrames<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    generation: jlong,
    min_frames: jlong,
    timeout_ms: jlong,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(
                if terminal::await_frames(generation, min_frames, timeout_ms) {
                    JNI_TRUE
                } else {
                    JNI_FALSE
                },
            )
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeAwaitSurfaceState(state, generation, timeoutMs)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeAwaitSurfaceState<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    state: JString<'caller>,
    generation: jlong,
    timeout_ms: jlong,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(
                if terminal::await_state(&state.to_string(), generation, timeout_ms) {
                    JNI_TRUE
                } else {
                    JNI_FALSE
                },
            )
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeAwaitRenderFailures(stage, minFailures, timeoutMs)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeAwaitRenderFailures<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    stage: jint,
    min_failures: jlong,
    timeout_ms: jlong,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(
                if terminal::await_render_failures(stage, min_failures, timeout_ms) {
                    JNI_TRUE
                } else {
                    JNI_FALSE
                },
            )
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Error raised on purpose by [`Java_org_wezterm_android_NativeApp_nativeDiagnosticFault`].
#[cfg(debug_assertions)]
#[derive(Debug, thiserror::Error)]
enum DiagnosticFault {
    #[error("diagnostic error {0} requested from Java")]
    Requested(jint),
    #[error(transparent)]
    Jni(#[from] jni::errors::Error),
}

/// `NativeApp.nativeDiagnosticFault(kind)`: debug-only fault injection.
/// `kind == 0` panics and `kind >= 4` returns an error, both surfacing in
/// Java as `RuntimeException`; `2` and `3` arm a one-shot GPU creation or
/// draw failure on the GUI thread and return the armed stage.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeDiagnosticFault<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    kind: jint,
) -> JString<'caller> {
    use wezterm_gui::renderfault::{RenderStage, arm};
    unowned_env
        .with_env(|env| -> Result<JString<'caller>, DiagnosticFault> {
            if kind == 0 {
                panic!("diagnostic panic requested from Java");
            }
            let stage = RenderStage::from_code(kind).ok_or(DiagnosticFault::Requested(kind))?;
            arm(stage);
            log::warn!("armed a {stage:?} failure at Java's request");
            Ok(JString::from_str(env, format!("{stage:?}"))?)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeDiagnosticGui(command)`: debug-only.  Runs
/// `open-window`, `paste`, `panic-on-queued-destroy` or
/// `panic-with-clipboard-read` on the GUI thread, or arms
/// `panic-in-surface-lost`; false when the command is unknown or no GUI
/// thread accepts work.  `panic-with-clipboard-read` blocks until the read
/// resolves and is also false when the read did not fail.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeDiagnosticGui<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    command: JString<'caller>,
) -> jboolean {
    use terminal::DiagnosticCommand;
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            let accepted =
                DiagnosticCommand::parse(&command.to_string()).is_some_and(terminal::diagnostic);
            Ok(if accepted { JNI_TRUE } else { JNI_FALSE })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}
