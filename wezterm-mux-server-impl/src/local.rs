use anyhow::{anyhow, Context as _};
use config::{create_user_owned_dirs, UnixDomain};
use promise::spawn::spawn_into_main_thread;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use wezterm_uds::{UnixListener, UnixStream};

pub struct LocalListener {
    listener: UnixListener,
    _socket_lock: Option<File>,
}

impl LocalListener {
    pub fn with_domain(unix_dom: &UnixDomain) -> anyhow::Result<Self> {
        Self::create(unix_dom, true)
    }

    /// Create a listener whose socket path is unique for this process.
    pub fn with_ephemeral_domain(unix_dom: &UnixDomain) -> anyhow::Result<Self> {
        Self::create(unix_dom, false)
    }

    fn create(unix_dom: &UnixDomain, lock_socket: bool) -> anyhow::Result<Self> {
        let (listener, socket_lock) = safely_create_sock_path(unix_dom, lock_socket)?;
        Ok(Self {
            listener,
            _socket_lock: socket_lock,
        })
    }

    pub fn run(&mut self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    spawn_into_main_thread(async move {
                        crate::dispatch::process(stream).await.map_err(|e| {
                            log::error!("{:#}", e);
                            e
                        })
                    })
                    .detach();
                }
                Err(err) => {
                    log::error!("accept failed: {}", err);
                    return;
                }
            }
        }
    }
}

/// Take care when setting up the listener socket;
/// we need to be sure that the directory that we create it in
/// is owned by the user and has appropriate file permissions
/// that prevent other users from manipulating its contents.
fn safely_create_sock_path(
    unix_dom: &UnixDomain,
    lock_socket: bool,
) -> anyhow::Result<(UnixListener, Option<File>)> {
    let sock_path = &unix_dom.socket_path();
    log::trace!("setting up {}", sock_path.display());

    let sock_dir = sock_path
        .parent()
        .ok_or_else(|| anyhow!("sock_path {} has no parent dir", sock_path.display()))?;

    create_user_owned_dirs(sock_dir)?;

    #[cfg(unix)]
    {
        use config::running_under_wsl;
        use std::os::unix::fs::PermissionsExt;

        if !running_under_wsl() && !unix_dom.skip_permissions_check {
            // Let's be sure that the ownership looks sane
            let meta = sock_dir.symlink_metadata()?;

            let permissions = meta.permissions();
            if (permissions.mode() & 0o22) != 0 {
                anyhow::bail!(
                    "The permissions for {} are insecure and currently \
                     allow other users to write to it (permissions={:?})",
                    sock_dir.display(),
                    permissions
                );
            }
        }
    }

    let socket_lock = if lock_socket {
        acquire_socket_lock(sock_path)?
    } else {
        None
    };
    let listener = bind_socket(sock_path)?;

    config::set_sticky_bit(sock_path);

    Ok((listener, socket_lock))
}

fn socket_lock_path(sock_path: &Path) -> PathBuf {
    let mut lock_path = sock_path.as_os_str().to_owned();
    lock_path.push(".lock");
    lock_path.into()
}

#[cfg(unix)]
fn acquire_socket_lock(sock_path: &Path) -> anyhow::Result<Option<File>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    if config::running_under_wsl() {
        // WSL1 can report a stale flock after reboot. The socket probe still
        // protects live listeners, but cannot serialize concurrent cleanup.
        return Ok(None);
    }

    let lock_path = socket_lock_path(sock_path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("Unable to open socket lock {}", lock_path.display()))?;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "Unable to lock {}; another process owns {}",
                lock_path.display(),
                sock_path.display()
            )
        });
    }

    config::set_sticky_bit(&lock_path);
    Ok(Some(file))
}

#[cfg(windows)]
fn acquire_socket_lock(sock_path: &Path) -> anyhow::Result<Option<File>> {
    use std::os::windows::fs::OpenOptionsExt;

    let lock_path = socket_lock_path(sock_path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "Unable to lock {}; another process owns {}",
                lock_path.display(),
                sock_path.display()
            )
        })?;

    Ok(Some(file))
}

fn bind_socket(sock_path: &Path) -> anyhow::Result<UnixListener> {
    let bind_error = match UnixListener::bind(sock_path) {
        Ok(listener) => return Ok(listener),
        Err(err) => err,
    };

    if bind_error.kind() != std::io::ErrorKind::AddrInUse {
        return Err(bind_error)
            .with_context(|| format!("Failed to bind to {}", sock_path.display()));
    }

    match UnixStream::connect(sock_path) {
        Ok(_) => anyhow::bail!(
            "Refusing to replace live mux server socket {}",
            sock_path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return UnixListener::bind(sock_path)
                .with_context(|| format!("Failed to bind to {}", sock_path.display()));
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Unable to determine whether {} is stale; refusing to remove it",
                    sock_path.display()
                )
            });
        }
    }

    ensure_socket_file(sock_path)?;
    std::fs::remove_file(sock_path)
        .with_context(|| format!("Unable to remove stale socket {}", sock_path.display()))?;
    UnixListener::bind(sock_path)
        .with_context(|| format!("Failed to bind to {}", sock_path.display()))
}

#[cfg(unix)]
fn ensure_socket_file(sock_path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    if sock_path.symlink_metadata()?.file_type().is_socket() {
        Ok(())
    } else {
        anyhow::bail!("Refusing to remove non-socket path {}", sock_path.display())
    }
}

#[cfg(windows)]
fn ensure_socket_file(_sock_path: &Path) -> anyhow::Result<()> {
    // Windows does not expose AF_UNIX socket paths through filesystem metadata.
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Arc, Barrier};

    fn domain(sock_path: &Path) -> UnixDomain {
        UnixDomain {
            socket_path: Some(sock_path.to_path_buf()),
            skip_permissions_check: true,
            ..Default::default()
        }
    }

    #[test]
    fn live_socket_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let sock_path = temp.path().join("sock");
        let first = UnixListener::bind(&sock_path).unwrap();
        let inode = sock_path.symlink_metadata().unwrap().ino();

        let error = LocalListener::with_domain(&domain(&sock_path))
            .err()
            .unwrap();

        assert!(error.to_string().contains("Refusing to replace live"));
        assert_eq!(sock_path.symlink_metadata().unwrap().ino(), inode);
        UnixStream::connect(&sock_path).unwrap();
        drop(first);
    }

    #[test]
    fn lock_blocks_a_second_starter_without_a_socket_path() {
        if config::running_under_wsl() {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let sock_path = temp.path().join("sock");
        let first = LocalListener::with_domain(&domain(&sock_path)).unwrap();
        std::fs::remove_file(&sock_path).unwrap();

        let error = LocalListener::with_domain(&domain(&sock_path))
            .err()
            .unwrap();

        assert!(error.to_string().contains("another process owns"));
        drop(first);
    }

    #[test]
    fn stale_socket_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let sock_path = temp.path().join("sock");
        let first = LocalListener::with_domain(&domain(&sock_path)).unwrap();
        drop(first);

        let second = LocalListener::with_domain(&domain(&sock_path)).unwrap();

        UnixStream::connect(&sock_path).unwrap();
        drop(second);
    }

    #[test]
    fn concurrent_bind_has_one_owner() {
        let temp = tempfile::tempdir().unwrap();
        let sock_path = Arc::new(temp.path().join("sock"));
        let start = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();

        for _ in 0..2 {
            let sock_path = Arc::clone(&sock_path);
            let start = Arc::clone(&start);
            let finish = Arc::clone(&finish);
            threads.push(std::thread::spawn(move || {
                start.wait();
                let listener = LocalListener::with_domain(&domain(&sock_path));
                let acquired = listener.is_ok();
                finish.wait();
                acquired
            }));
        }

        let owners = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|acquired| *acquired)
            .count();
        assert_eq!(owners, 1);
    }

    #[test]
    fn ephemeral_listener_does_not_create_a_lock_file() {
        let temp = tempfile::tempdir().unwrap();
        let sock_path = temp.path().join("sock");
        let listener = LocalListener::with_ephemeral_domain(&domain(&sock_path)).unwrap();

        assert!(!socket_lock_path(&sock_path).exists());
        drop(listener);
    }

    #[test]
    fn regular_file_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let sock_path = temp.path().join("sock");
        std::fs::write(&sock_path, "keep me").unwrap();

        assert!(LocalListener::with_domain(&domain(&sock_path)).is_err());

        assert_eq!(std::fs::read_to_string(sock_path).unwrap(), "keep me");
    }
}
