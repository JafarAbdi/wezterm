//! App-private directory layout under Android's `filesDir` and `cacheDir`.

#![forbid(unsafe_code)]

use config::AndroidPaths;
use std::fs::DirBuilder;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Why the layout could not be established.
#[derive(Debug, Error)]
pub enum DirsError {
    /// Android handed over a relative or empty path.
    #[error("{role} directory must be an absolute path, got {path:?}")]
    NotAbsolute {
        /// `files` or `cache`.
        role: &'static str,
        /// The offending value.
        path: PathBuf,
    },
    /// The sandbox refused directory creation.
    #[error("failed to create {path}: {source}")]
    Create {
        /// Directory being created.
        path: PathBuf,
        /// OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Lay out the private directories and create them, mode `0700` where the
/// platform has modes.  Idempotent.
pub fn create_app_dirs(files_dir: &Path, cache_dir: &Path) -> Result<AndroidPaths, DirsError> {
    for (role, path) in [("files", files_dir), ("cache", cache_dir)] {
        if !path.is_absolute() {
            return Err(DirsError::NotAbsolute {
                role,
                path: path.to_path_buf(),
            });
        }
    }
    let paths = AndroidPaths {
        home: files_dir.join("home"),
        config: files_dir.join("config"),
        data: files_dir.join("data"),
        cache: cache_dir.join("wezterm"),
        runtime: files_dir.join("runtime"),
    };
    for path in [
        &paths.home,
        &paths.config,
        &paths.data,
        &paths.cache,
        &paths.runtime,
    ] {
        private_dir_builder()
            .create(path)
            .map_err(|source| DirsError::Create {
                path: path.clone(),
                source,
            })?;
    }
    Ok(paths)
}

fn private_dir_builder() -> DirBuilder {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_paths() {
        let err = create_app_dirs(Path::new("files"), Path::new("/tmp")).unwrap_err();
        assert!(matches!(err, DirsError::NotAbsolute { role: "files", .. }));
    }

    #[test]
    fn creates_private_layout_idempotently() {
        let root = tempfile::tempdir().unwrap();
        let files = root.path().join("files");
        let cache = root.path().join("cache");
        let first = create_app_dirs(&files, &cache).unwrap();
        let second = create_app_dirs(&files, &cache).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first,
            AndroidPaths {
                home: files.join("home"),
                config: files.join("config"),
                data: files.join("data"),
                cache: cache.join("wezterm"),
                runtime: files.join("runtime"),
            }
        );
        for path in [
            &first.home,
            &first.config,
            &first.data,
            &first.cache,
            &first.runtime,
        ] {
            let metadata = std::fs::metadata(path).unwrap();
            assert!(metadata.is_dir(), "{}", path.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    metadata.permissions().mode() & 0o777,
                    0o700,
                    "{}",
                    path.display()
                );
            }
        }
    }
}
