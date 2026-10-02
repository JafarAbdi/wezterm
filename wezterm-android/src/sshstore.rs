//! App-private SSH files: host trust and the imported identity.
//!
//! Both live in one `0700` directory under `filesDir`, which the manifest
//! excludes from backup and device transfer.  The identity arrives as the
//! bytes of a document the user picked; it is validated, written to a
//! `0600` temporary file in the same directory, synced and renamed, so the
//! path SSH reads holds either the previous identity or the complete new
//! one.  Key bytes are never logged.

#![forbid(unsafe_code)]

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// OpenSSH refuses key files above 1 MiB (`MAX_KEY_FILE_SIZE`, authfile.c).
const MAX_KEY_FILE_SIZE: usize = 1024 * 1024;

/// Why a picked document was not imported.
#[derive(Debug, Error)]
pub enum ImportError {
    /// The document is empty.
    #[error("the file is empty")]
    Empty,
    /// The document is larger than any private key file.
    #[error("the file is larger than a private key (1 MiB)")]
    TooLarge,
    /// The document has no PEM or OpenSSH private key block.
    #[error("the file is not an OpenSSH or PEM private key")]
    NotAPrivateKey,
    /// The private directory refused the write.
    #[error("could not store the key: {0}")]
    Io(#[from] std::io::Error),
}

impl ImportError {
    /// Stable name for the platform.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLarge => "too_large",
            Self::NotAPrivateKey => "not_a_private_key",
            Self::Io(_) => "io",
        }
    }
}

/// The directory that holds `known_hosts` and `identity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshStore {
    dir: PathBuf,
}

fn is_private_key(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        text.lines().any(|line| {
            let line = line.trim_end();
            line.starts_with("-----BEGIN ") && line.ends_with("PRIVATE KEY-----")
        })
    })
}

fn private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

impl SshStore {
    /// Create `<files_dir>/ssh`, mode `0700`.  Idempotent.
    pub fn create(files_dir: &Path) -> std::io::Result<Self> {
        let dir = files_dir.join("ssh");
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir)?;
        Ok(Self { dir })
    }

    /// The only host-trust file SSH reads and writes.
    pub fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }

    /// Where the imported identity lives.
    pub fn identity_path(&self) -> PathBuf {
        self.dir.join("identity")
    }

    /// The identity, when one was imported.
    pub fn identity(&self) -> Option<PathBuf> {
        Some(self.identity_path()).filter(|path| path.is_file())
    }

    /// Replace the identity with the private key in `bytes`.
    pub fn import_identity(&self, bytes: &[u8]) -> Result<(), ImportError> {
        if bytes.is_empty() {
            return Err(ImportError::Empty);
        }
        if bytes.len() > MAX_KEY_FILE_SIZE {
            return Err(ImportError::TooLarge);
        }
        if !is_private_key(bytes) {
            return Err(ImportError::NotAPrivateKey);
        }
        let staged = self.dir.join("identity.importing");
        match std::fs::remove_file(&staged) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        let written = private_file(&staged).and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&staged, self.identity_path())?;
            File::open(&self.dir)?.sync_all()
        });
        if written.is_err() {
            std::fs::remove_file(&staged).ok();
        }
        Ok(written?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n";

    fn store() -> (tempfile::TempDir, SshStore) {
        let root = tempfile::tempdir().unwrap();
        let store = SshStore::create(root.path()).unwrap();
        (root, store)
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn layout_is_private_and_starts_without_an_identity() {
        let (root, store) = store();
        assert_eq!(store, SshStore::create(root.path()).unwrap());
        assert_eq!(store.known_hosts(), root.path().join("ssh/known_hosts"));
        assert_eq!(store.identity(), None);
        #[cfg(unix)]
        assert_eq!(mode(&root.path().join("ssh")), 0o700);
    }

    #[test]
    fn imports_a_private_key_as_a_private_file() {
        let (root, store) = store();
        store.import_identity(KEY).unwrap();
        let identity = root.path().join("ssh/identity");
        assert_eq!(store.identity(), Some(identity.clone()));
        assert_eq!(std::fs::read(&identity).unwrap(), KEY);
        #[cfg(unix)]
        assert_eq!(mode(&identity), 0o600);
        let names: Vec<_> = std::fs::read_dir(root.path().join("ssh"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["identity"]);
    }

    #[test]
    fn a_refused_import_keeps_the_previous_identity() {
        let (_root, store) = store();
        store.import_identity(KEY).unwrap();
        let refused = [
            (&b""[..], "empty"),
            (
                &b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 me@laptop\n"[..],
                "not_a_private_key",
            ),
            (
                &b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"[..],
                "not_a_private_key",
            ),
            (
                &b"\xff\xfe-----BEGIN OPENSSH PRIVATE KEY-----\n"[..],
                "not_a_private_key",
            ),
            (&vec![b'A'; MAX_KEY_FILE_SIZE + 1][..], "too_large"),
        ];
        for (bytes, code) in refused {
            assert_eq!(store.import_identity(bytes).unwrap_err().code(), code);
            assert_eq!(std::fs::read(store.identity_path()).unwrap(), KEY);
        }
    }

    #[test]
    fn a_new_import_replaces_the_identity_and_a_stale_staging_file() {
        let (root, store) = store();
        store.import_identity(KEY).unwrap();
        std::fs::write(root.path().join("ssh/identity.importing"), b"partial").unwrap();
        let rsa = b"-----BEGIN RSA PRIVATE KEY-----\r\nMIIE\r\n-----END RSA PRIVATE KEY-----\r\n";
        store.import_identity(rsa).unwrap();
        assert_eq!(std::fs::read(store.identity_path()).unwrap(), rsa);
        assert!(!root.path().join("ssh/identity.importing").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_no_staging_file_and_no_identity() {
        use std::os::unix::fs::PermissionsExt;
        let (root, store) = store();
        let dir = root.path().join("ssh");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let err = store.import_identity(KEY).unwrap_err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(err.code(), "io");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}
