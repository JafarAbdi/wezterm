//! JNI boundary for `org.wezterm.android.NativeApp`.
//!
//! Ownership and safety contract:
//!
//! * Every export takes an [`EnvUnowned`] and upgrades it with `with_env`,
//!   which attaches the frame and wraps the body in `catch_unwind`.  A Rust
//!   panic or error becomes a Java `RuntimeException` via
//!   [`ThrowRuntimeExAndDefault`]; nothing unwinds into the JVM.
//! * No `Env`, local reference or Java object escapes its export.  The
//!   engine state that persists ([`crate::initialize`]'s `OnceLock`) is
//!   plain Rust data.  The process environment is never mutated; sandbox
//!   paths reach `config` through `config::set_android_paths`.
//! * Symbol names must match the `external fun` declarations in
//!   `android/app/src/main/java/org/wezterm/android/NativeApp.kt`.

#![allow(unsafe_code)]

use crate::InitRequest;
use jni::EnvUnowned;
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JClass, JString};
use jni::sys::{JNI_TRUE, jboolean, jint};
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
            let verbose = verbose_logging == JNI_TRUE;
            install_logger(verbose);
            let request = InitRequest {
                files_dir: PathBuf::from(files_dir.to_string()),
                cache_dir: PathBuf::from(cache_dir.to_string()),
                dpi: u32::try_from(dpi).unwrap_or(0),
                verbose_logging: verbose,
            };
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

/// Error raised on purpose by [`Java_org_wezterm_android_NativeApp_nativeDiagnosticFault`].
#[cfg(debug_assertions)]
#[derive(Debug, thiserror::Error)]
enum DiagnosticFault {
    #[error("diagnostic error {0} requested from Java")]
    Requested(jint),
    #[error(transparent)]
    Jni(#[from] jni::errors::Error),
}

/// `NativeApp.nativeDiagnosticFault(kind)`: debug-only proof that Rust
/// failures are contained.  `kind == 0` panics, any other value returns an
/// error; both surface in Java as `RuntimeException`.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_wezterm_android_NativeApp_nativeDiagnosticFault<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    kind: jint,
) -> JString<'caller> {
    unowned_env
        .with_env(|_env| -> Result<JString<'caller>, DiagnosticFault> {
            if kind == 0 {
                panic!("diagnostic panic requested from Java");
            }
            Err(DiagnosticFault::Requested(kind))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}
