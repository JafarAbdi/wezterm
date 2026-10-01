//! Native engine entry for the WezTerm Android app.
//!
//! [`initialize`] publishes app-private paths to `config`, initializes the
//! configuration system and the real font engine, then reports what it
//! found.  No terminal, window, mux domain or local process is created.
//!
//! Unsafe code is confined to the JNI boundary in [`ffi`].  Every other
//! module forbids it.

#![deny(unsafe_code)]

mod dirs;
pub mod engine;
#[cfg(target_os = "android")]
pub mod ffi;
mod probe;
#[cfg(target_os = "android")]
pub mod terminal;

pub use config::AndroidPaths;
pub use dirs::{DirsError, create_app_dirs};
pub use probe::{FontReport, GpuAdapter, NativeVersions, ShapingProbe};

/// Parse debug config overrides: one `key=value` per line, values being
/// Lua expressions.  Blank lines are skipped.
pub fn parse_config_overrides(text: &str) -> Result<Vec<(String, String)>, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once('=')
                .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
                .filter(|(key, _)| !key.is_empty())
                .ok_or_else(|| format!("config override {line:?} is not key=value"))
        })
        .collect()
}

use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Once, OnceLock};
use std::time::Instant;

/// What the Java side hands over on first use.
#[derive(Debug, Clone)]
pub struct InitRequest {
    /// `Context.getFilesDir()`.
    pub files_dir: PathBuf,
    /// `Context.getCacheDir()`.
    pub cache_dir: PathBuf,
    /// `DisplayMetrics.densityDpi` of the display that will host terminals.
    pub dpi: u32,
    /// Emit debug-level logs to logcat.
    pub verbose_logging: bool,
}

/// The initialization step that produced a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InitStage {
    /// Creating the app-private directories and publishing them to `config`.
    Dirs,
    /// `config::common_init` without any user `wezterm.lua`.
    Config,
    /// Native library probes (OpenSSL, zstd, libgit2, Lua, libssh, libssh2, codec, wgpu).
    NativeVersions,
    /// Font enumeration through the real WezTerm font engine.
    Fonts,
    /// Shaping and rasterizing sample text through the GUI glyph cache.
    Shaping,
}

/// Result of the single engine initialization in this process.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InitOutcome {
    /// The closure loaded and every probe passed.
    Ready(Box<InitReport>),
    /// A probe failed; `stage` says which one.
    Failed {
        /// The step that failed.
        stage: InitStage,
        /// Human-readable error chain.
        message: String,
    },
}

/// Diagnostic facts gathered by a successful [`initialize`].
#[derive(Debug, Clone, Serialize)]
pub struct InitReport {
    /// Always 1: the engine initializes once per process (`OnceLock`).
    pub engine_initializations: u32,
    /// Rust `target_arch` of the loaded library.
    pub arch: &'static str,
    /// Rust target triple compiled into `config`.
    pub target_triple: &'static str,
    /// WezTerm version string.
    pub wezterm_version: &'static str,
    /// Mux wire protocol version this build speaks.
    pub codec_version: usize,
    /// Wall-clock initialization time.
    pub init_duration_ms: u128,
    /// Thread that ran initialization.
    pub thread: String,
    /// The app-private directory layout `config` resolves paths from.
    pub dirs: AndroidPaths,
    /// Native library versions reached through the linked closure.
    pub natives: NativeVersions,
    /// Bundled fonts as the font engine resolved them.
    pub fonts: FontReport,
    /// Sample text shaped and rasterized through the GUI glyph cache.
    pub shaping: ShapingProbe,
}

/// Envelope returned on every call so callers can observe idempotence:
/// `init_calls` grows, `outcome` never changes.
#[derive(Debug, Serialize)]
pub struct InitResponse<'a> {
    /// Number of `initialize` calls in this process, including this one.
    pub init_calls: u32,
    /// The process-wide outcome.
    pub outcome: &'a InitOutcome,
}

impl InitResponse<'_> {
    /// JSON for the Kotlin side.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("InitResponse serializes")
    }
}

static ENGINE: OnceLock<InitOutcome> = OnceLock::new();
static INIT_CALLS: AtomicU32 = AtomicU32::new(0);
static VERSION_INFO: Once = Once::new();

/// Initialize the native engine once per process and return its outcome.
pub fn initialize(request: &InitRequest) -> InitResponse<'static> {
    let init_calls = INIT_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
    let outcome = ENGINE.get_or_init(|| run_once(request));
    InitResponse {
        init_calls,
        outcome,
    }
}

fn run_once(request: &InitRequest) -> InitOutcome {
    let started = Instant::now();
    match run_stages(request) {
        Ok((dirs, natives, fonts, shaping)) => InitOutcome::Ready(Box::new(InitReport {
            engine_initializations: 1,
            arch: std::env::consts::ARCH,
            target_triple: config::wezterm_target_triple(),
            wezterm_version: config::wezterm_version(),
            codec_version: codec::CODEC_VERSION,
            init_duration_ms: started.elapsed().as_millis(),
            thread: format!("{:?}", std::thread::current().id()),
            dirs,
            natives,
            fonts,
            shaping,
        })),
        Err((stage, message)) => {
            log::error!("native initialization failed at {stage:?}: {message}");
            InitOutcome::Failed { stage, message }
        }
    }
}

type StageError = (InitStage, String);

fn run_stages(
    request: &InitRequest,
) -> Result<(AndroidPaths, NativeVersions, FontReport, ShapingProbe), StageError> {
    let dirs = create_app_dirs(&request.files_dir, &request.cache_dir)
        .map_err(|e| (InitStage::Dirs, e.to_string()))?;
    #[cfg(target_os = "android")]
    config::set_android_paths(dirs.clone()).map_err(|_| {
        (
            InitStage::Dirs,
            "config paths were already published".to_string(),
        )
    })?;

    VERSION_INFO.call_once(|| {
        config::assign_version_info(
            wezterm_version::wezterm_version(),
            wezterm_version::wezterm_target_triple(),
        )
    });
    config::designate_this_as_the_main_thread();
    config::common_init(None, &[], true).map_err(|e| (InitStage::Config, format!("{e:#}")))?;
    let config = config::configuration();
    log::info!(
        "config initialized; font_locator={:?} home={}",
        config.font_locator,
        config::HOME_DIR.display()
    );

    let natives =
        probe::native_versions(&dirs).map_err(|e| (InitStage::NativeVersions, format!("{e:#}")))?;
    let fonts =
        probe::fonts(&config, request.dpi).map_err(|e| (InitStage::Fonts, format!("{e:#}")))?;
    let shaping = probe::shape_and_rasterize(&config, request.dpi, "Wez ⇒ 🚀")
        .map_err(|e| (InitStage::Shaping, format!("{e:#}")))?;
    Ok((dirs, natives, fonts, shaping))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_outcome_serializes_with_stage_tag() {
        let outcome = InitOutcome::Failed {
            stage: InitStage::Fonts,
            message: "no fonts".into(),
        };
        let json = InitResponse {
            init_calls: 2,
            outcome: &outcome,
        }
        .to_json();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["init_calls"], 2);
        assert_eq!(value["outcome"]["status"], "failed");
        assert_eq!(value["outcome"]["stage"], "fonts");
        assert_eq!(value["outcome"]["message"], "no fonts");
    }

    #[test]
    fn config_overrides_parse_key_value_lines() {
        assert_eq!(
            parse_config_overrides(" cursor_blink_rate = 0 \n\nfont_size=9.5\n").unwrap(),
            vec![
                ("cursor_blink_rate".to_string(), "0".to_string()),
                ("font_size".to_string(), "9.5".to_string()),
            ]
        );
        assert_eq!(parse_config_overrides("").unwrap(), vec![]);
        assert!(parse_config_overrides("novalue").is_err());
        assert!(parse_config_overrides("=1").is_err());
    }
}
