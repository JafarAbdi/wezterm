//! App-private paths published before any Android path static is read.
//! The JVM owns the process environment, which is read concurrently.

use std::path::PathBuf;
#[cfg(target_os = "android")]
use std::sync::OnceLock;

/// Directories inside the app sandbox. Every path is absolute.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AndroidPaths {
    /// [`crate::HOME_DIR`]: `~/.wezterm.lua`, `~/.wezterm` and SSH lookups.
    pub home: PathBuf,
    /// The only entry of [`crate::CONFIG_DIRS`].
    pub config: PathBuf,
    /// [`crate::DATA_DIR`].
    pub data: PathBuf,
    /// [`crate::CACHE_DIR`].
    pub cache: PathBuf,
    /// [`crate::RUNTIME_DIR`]: sockets, locks and the pki directory.
    pub runtime: PathBuf,
}

#[cfg(target_os = "android")]
static ANDROID_PATHS: OnceLock<AndroidPaths> = OnceLock::new();

/// Publish paths before any path static is read; a second call returns the
/// rejected value. This never mutates the process environment.
#[cfg(target_os = "android")]
pub fn set_android_paths(paths: AndroidPaths) -> Result<(), AndroidPaths> {
    ANDROID_PATHS.set(paths)
}

#[cfg(target_os = "android")]
pub(crate) fn android_paths() -> &'static AndroidPaths {
    ANDROID_PATHS
        .get()
        .expect("config::set_android_paths must be called before any path is resolved")
}
