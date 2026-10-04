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

use crate::input::{Erase, Gesture, Input, InputTarget};
use crate::profile::ProfileFields;
use crate::terminal::{self, RetireOutcome, StartRequest, SurfaceBridgeError};
use crate::{InitRequest, sshmux};
use jni::EnvUnowned;
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{JNI_FALSE, JNI_TRUE, jboolean, jfloat, jint, jlong};
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
                // The shaper's debug records hold the text it shapes: the
                // user's composition and the terminal's screen.
                .with_filter(
                    android_logger::FilterBuilder::new()
                        .filter_level(level)
                        .filter_module("wezterm_font::shaper", log::LevelFilter::Info)
                        .build(),
                )
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
    #[cfg(debug_assertions)]
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
    let verbose = cfg!(debug_assertions) && verbose_logging == JNI_TRUE;
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
            #[cfg(debug_assertions)]
            let config_overrides = crate::parse_config_overrides(&config_overrides.to_string())
                .map_err(Fault::Overrides)?;
            #[cfg(not(debug_assertions))]
            let _ = (diagnostic_applet, config_overrides);
            let state = terminal::start(StartRequest {
                init,
                #[cfg(debug_assertions)]
                diagnostic_applet: diagnostic_applet == JNI_TRUE,
                #[cfg(debug_assertions)]
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

fn deliver(input: Option<Input>) -> jboolean {
    if input.is_some_and(terminal::input) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

/// `NativeApp.nativeInputPreedit(text)`: the IME's composing text.  This
/// and the other `nativeInput*` exports return false when the engine does
/// not run; the input is then dropped.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeInputPreedit<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    text: JString<'caller>,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(deliver(Some(Input::Preedit(text.to_string()))))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeInputCommit(erase, pane, generation, text, meta)`:
/// the IME's committed text changed; `erase` characters typed under the
/// input target `pane` (negative for none) and `generation`.  A negative
/// `erase` or `generation` is a Java caller bug and delivers nothing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeInputCommit<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    erase: jint,
    pane: jlong,
    generation: jlong,
    text: JString<'caller>,
    meta: jint,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            let text = text.to_string();
            let commit = u32::try_from(erase)
                .ok()
                .zip(u64::try_from(generation).ok())
                .map(|(erase, generation)| Input::Commit {
                    erase: Erase::new(
                        erase,
                        InputTarget {
                            pane: usize::try_from(pane).ok(),
                            generation,
                        },
                    ),
                    text,
                    meta,
                });
            Ok(deliver(commit))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeIsPasteKey(code, unicode, meta)`: whether the key
/// table makes this key press a paste.  Answered on the caller's thread;
/// the GUI thread takes no part.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeIsPasteKey<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    code: jint,
    unicode: jint,
    meta: jint,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            let pastes = crate::input::pressed_key(code, unicode as u32, meta)
                .is_some_and(|key| wezterm_gui::android::is_paste_key(&key));
            Ok(if pastes { JNI_TRUE } else { JNI_FALSE })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeInputKey(code, unicode, meta)`: a key press.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeInputKey<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    code: jint,
    unicode: jint,
    meta: jint,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(deliver(Some(Input::Key {
                code,
                unicode: unicode as u32,
                meta,
            })))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeInputPaste(text)`: paste the clipboard text Kotlin read.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeInputPaste<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    text: JString<'caller>,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(deliver(Some(Input::Paste(text.to_string()))))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeInputTouch(gesture, x, y, from, to)`: a touch gesture;
/// false also for an unknown gesture code.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeInputTouch<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    gesture: jint,
    x: jfloat,
    y: jfloat,
    from: jfloat,
    to: jfloat,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            let gesture = Gesture::from_code(gesture, from, to);
            Ok(deliver(gesture.map(|gesture| Input::Touch {
                x,
                y,
                gesture,
            })))
        })
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
#[cfg(debug_assertions)]
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
#[cfg(debug_assertions)]
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
#[cfg(debug_assertions)]
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
#[cfg(debug_assertions)]
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

/// `NativeApp.nativeConnect(host, port, user, remoteWezterm)`: validate the
/// profile and start attaching to the laptop mux.  Returns JSON:
/// `{"status":"started","attempt":n}`,
/// `{"status":"invalid_profile","field":…,"message":…}`, or
/// `{"status":"busy"|"unavailable"|"storage"|"starting","message":…}`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeConnect<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    host: JString<'caller>,
    port: JString<'caller>,
    user: JString<'caller>,
    remote_wezterm: JString<'caller>,
) -> JString<'caller> {
    use crate::connection::Refused;
    use crate::sshmux::ConnectRefused;
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            let fields = ProfileFields {
                host: host.to_string(),
                port: port.to_string(),
                user: user.to_string(),
                remote_wezterm: remote_wezterm.to_string(),
            };
            let outcome = match sshmux::connect(&fields) {
                Ok(attempt) => serde_json::json!({"status": "started", "attempt": attempt}),
                Err(ConnectRefused::Profile(err)) => serde_json::json!({
                    "status": "invalid_profile",
                    "field": err.field(),
                    "message": err.to_string(),
                }),
                Err(err @ ConnectRefused::Refused(Refused::Busy)) => {
                    serde_json::json!({"status": "busy", "message": err.to_string()})
                }
                Err(err @ ConnectRefused::Refused(Refused::Uncancellable(_))) => {
                    serde_json::json!({"status": "unavailable", "message": err.to_string()})
                }
                Err(err @ ConnectRefused::Storage) => {
                    serde_json::json!({"status": "storage", "message": err.to_string()})
                }
                Err(err @ ConnectRefused::Starting) => {
                    serde_json::json!({"status": "starting", "message": err.to_string()})
                }
            };
            log::info!("nativeConnect -> {}", outcome["status"]);
            JString::from_str(env, outcome.to_string())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeConnectionStatus()`: JSON [`sshmux::Status`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeConnectionStatus<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            JString::from_str(
                env,
                serde_json::to_string(&sshmux::status()).expect("Status serializes"),
            )
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeAwaitConnectionChange(since, timeoutMs)`: block until
/// the connection revision differs from `since`; returns the current one.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeAwaitConnectionChange<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    since: jlong,
    timeout_ms: jlong,
) -> jlong {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jlong> {
            Ok(sshmux::await_change(since, timeout_ms))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeCancelConnect(attempt)`: cancel that attempt while it
/// attaches; false when it is not attaching or already published its panes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeCancelConnect<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    attempt: jlong,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(accepted(
                sshmux::cancel(u64::try_from(attempt).unwrap_or(0)),
                "cancel",
            ))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeDisconnect(attempt)`: disconnect that attached attempt;
/// false when it is not attached.  The laptop's panes keep running.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeDisconnect<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    attempt: jlong,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            Ok(accepted(
                sshmux::disconnect(u64::try_from(attempt).unwrap_or(0)),
                "disconnect",
            ))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

fn accepted(outcome: Result<(), impl std::fmt::Display>, what: &str) -> jboolean {
    match outcome {
        Ok(()) => JNI_TRUE,
        Err(err) => {
            log::warn!("{what} refused: {err}");
            JNI_FALSE
        }
    }
}

/// `NativeApp.nativeAnswerHostTrust(attempt, prompt, trust)`: false when
/// that prompt is not pending (already answered, or its attempt ended).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeAnswerHostTrust<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    attempt: jlong,
    prompt: jlong,
    trust: jboolean,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            let outcome = sshmux::answer_host_trust(
                u64::try_from(attempt).unwrap_or(0),
                u64::try_from(prompt).unwrap_or(0),
                trust == JNI_TRUE,
            );
            Ok(accepted(outcome, "host trust answer"))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeAnswerText(attempt, prompt, text)`: answer a secret or
/// text prompt; a null `text` cancels it.  False when that prompt is not
/// pending.  The text is never logged.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeAnswerText<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    attempt: jlong,
    prompt: jlong,
    text: JString<'caller>,
) -> jboolean {
    unowned_env
        .with_env(|_env| -> jni::errors::Result<jboolean> {
            let text = (!text.is_null()).then(|| text.to_string());
            let outcome = sshmux::answer_text(
                u64::try_from(attempt).unwrap_or(0),
                u64::try_from(prompt).unwrap_or(0),
                text,
            );
            Ok(accepted(outcome, "prompt answer"))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeImportIdentity(key)`: store the private key the user
/// picked as the app's identity.  Returns `""` on success, otherwise
/// `<code>: <message>` with the code of [`crate::sshstore::ImportError`].
/// The bytes are never logged.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeImportIdentity<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    key: JByteArray<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            let bytes = env.convert_byte_array(&key)?;
            let outcome = match sshmux::import_identity(&bytes) {
                Ok(()) => String::new(),
                Err(err) => format!("{}: {err}", err.code()),
            };
            log::info!(
                "nativeImportIdentity -> {}",
                if outcome.is_empty() {
                    "imported"
                } else {
                    "refused"
                }
            );
            JString::from_str(env, outcome)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeDiagnosticActivePane()`: debug-only JSON
/// [`sshmux::ActivePane`] of the bound window, taken on the GUI thread;
/// null when no window is bound or no GUI thread answers.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeDiagnosticActivePane<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            match terminal::diagnostic_active_pane() {
                Some(pane) => JString::from_str(env, pane),
                None => Ok(JString::null()),
            }
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeDiagnosticHeldObservations()`: debug-only JSON list of
/// the mux notifications whose input-target observation
/// `hold-target-observations` holds; null when nothing is held.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeDiagnosticHeldObservations<
    'caller,
>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            match terminal::diagnostic_held_observations() {
                Some(held) => JString::from_str(env, held),
                None => Ok(JString::null()),
            }
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeDiagnosticMux()`: debug-only JSON
/// [`sshmux::MuxCensus`] taken on the GUI thread; null when no GUI thread
/// answers.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeDiagnosticMux<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            match terminal::diagnostic_mux() {
                Some(census) => JString::from_str(env, census),
                None => Ok(JString::null()),
            }
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `NativeApp.nativeDiagnosticConnection(command)`: debug-only.
/// `interrupt-transport` shuts the latest attempt's transport down as a
/// failing network would and returns `true`, or `false` when its threads
/// already ended; `hold-next-input` arms [`sshmux::hold_next_input`] and
/// returns `true`; `census` returns JSON [`sshmux::ProcessCensus`].  Null
/// for an unknown command.  Runs on the calling thread.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeDiagnosticConnection<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    command: JString<'caller>,
) -> JString<'caller> {
    unowned_env
        .with_env(|env| -> jni::errors::Result<_> {
            let reply = match command.to_string().as_str() {
                "interrupt-transport" => sshmux::interrupt_transport().to_string(),
                "hold-next-input" => {
                    sshmux::hold_next_input();
                    "true".to_string()
                }
                "census" => serde_json::to_string(&sshmux::process_census())
                    .expect("ProcessCensus serializes"),
                _ => return Ok(JString::null()),
            };
            JString::from_str(env, reply)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Error raised on purpose by [`Java_org_wezterm_android_NativeApp_nativeDiagnosticFault`].
#[cfg(all(debug_assertions, not(doc)))]
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
#[cfg(all(debug_assertions, not(doc)))]
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
/// `open-window`, `paste`, `panic-on-queued-destroy`,
/// `panic-with-clipboard-read`, `snapshot-bound-tab` or
/// `apply-bound-tab-snapshot` on the GUI thread, arms
/// `panic-in-surface-lost`, or holds and releases input-target
/// observations (`hold-target-observations`,
/// `release-target-observations`); false when the command is unknown or no
/// GUI thread accepts work.  `panic-with-clipboard-read` blocks until the read
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
