use crate::sshd::*;
use assert_fs::prelude::*;
use rstest::*;
use socket2::{Domain, Socket, Type};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::{channel, Receiver};
use wezterm_ssh::{Cancel, Config, ConfigMap, Session, SessionEvent};

fn config(port: u16, dir: &std::path::Path, backend: &str) -> ConfigMap {
    let mut config = Config::new().for_host("localhost");
    for (key, value) in [
        ("port", port.to_string()),
        ("addressfamily", "inet".to_string()),
        ("wezterm_ssh_backend", backend.to_string()),
        ("wezterm_ssh_config_dir", dir.display().to_string()),
        ("user", whoami::username()),
        ("identitiesonly", "yes".to_string()),
        ("identityfile", dir.join("id_rsa").display().to_string()),
        (
            "userknownhostsfile",
            dir.join("known_hosts").display().to_string(),
        ),
    ] {
        config.insert(key.to_string(), value);
    }
    config
}

fn cancel() -> (Cancel, Receiver<()>) {
    let (released, on_release) = channel();
    let cancel = Cancel::new(move || {
        released.send(()).ok();
    })
    .unwrap();
    (cancel, on_release)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    HostVerify,
    Authenticate,
}

/// Run a session that `cancel` cancels at the first `at` event.  A host
/// trust prompt is then answered as a user who changed their mind too
/// late would; a credential prompt is dropped unanswered, as a connection
/// UI ends a cancelled prompt.  Returns every later event but banners;
/// waits until the session thread released its worker.
fn cancelled_at(
    config: ConfigMap,
    at: At,
    cancel: &Cancel,
    released: &Receiver<()>,
) -> Vec<String> {
    let (session, events) =
        Session::connect_cancellable(wezterm_ssh::ResolvedSshRoute::direct(config), cancel)
            .unwrap();
    let mut seen = vec![];
    smol::block_on(async {
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::Banner(_) => {}
                SessionEvent::HostVerify(verify) => {
                    if at == At::HostVerify {
                        assert!(cancel.cancel());
                    }
                    verify.answer(true).await.ok();
                }
                SessionEvent::Authenticate(auth) => {
                    assert!(at == At::Authenticate && cancel.cancel());
                    drop(auth);
                }
                SessionEvent::Error(err) if cancel.is_shut() => seen.push(format!("Error {err}")),
                other => assert!(!cancel.is_shut(), "after the cancel: {:?}", other),
            }
        }
    });
    drop(session);
    released
        .recv()
        .expect("the session thread released its worker");
    assert_eq!((cancel.workers(), cancel.sockets()), (0, 0));
    seen
}

fn host_trust_cancelled(backend: &str) -> Vec<String> {
    let sshd = Sshd::spawn(Default::default()).unwrap();
    sshd.tmp.child("known_hosts").touch().unwrap();
    let (cancel, released) = cancel();
    let seen = cancelled_at(
        config(sshd.port, sshd.tmp.path(), backend),
        At::HostVerify,
        &cancel,
        &released,
    );
    assert_eq!(
        std::fs::read_to_string(sshd.tmp.child("known_hosts").path()).unwrap(),
        "",
        "a trust answer after the cancel adds nothing"
    );
    seen
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_a_trust_answer_after_a_cancel_is_refused() {
    if !sshd_available() {
        return;
    }
    assert_eq!(
        host_trust_cancelled("libssh"),
        ["Error connecting final SSH target localhost: the connection was cancelled"]
    );
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_a_trust_answer_after_a_cancel_is_refused() {
    if !sshd_available() {
        return;
    }
    assert_eq!(
        host_trust_cancelled("ssh2"),
        ["Error connecting final SSH target localhost: \
          host verification: the connection was cancelled"]
    );
}

fn authentication_cancelled(backend: &str) -> Vec<String> {
    let mut sshd_config = SshdConfig::default();
    sshd_config.set_authentication_methods(vec!["password".to_string()]);
    let sshd = Sshd::spawn(sshd_config).unwrap();
    sshd.tmp.child("known_hosts").touch().unwrap();
    let (trust, trust_released) = cancel();
    let trusting = Session::connect_cancellable(
        wezterm_ssh::ResolvedSshRoute::direct(config(sshd.port, sshd.tmp.path(), backend)),
        &trust,
    )
    .unwrap();
    smol::block_on(async {
        while let Ok(event) = trusting.1.recv().await {
            match event {
                SessionEvent::HostVerify(verify) => verify.answer(true).await.unwrap(),
                SessionEvent::Authenticate(_) => break,
                _ => {}
            }
        }
    });
    trust.shutdown();
    drop(trusting);
    trust_released.recv().unwrap();

    let (cancel, released) = cancel();
    let seen = cancelled_at(
        config(sshd.port, sshd.tmp.path(), backend),
        At::Authenticate,
        &cancel,
        &released,
    );
    let log = std::fs::read_to_string(sshd.tmp.child("sshd.log").path()).unwrap();
    assert!(
        !log.contains("password for"),
        "no password was tried: {}",
        log
    );
    seen
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_a_cancelled_password_prompt_ends_the_session() {
    if !sshd_available() {
        return;
    }
    assert_eq!(
        authentication_cancelled("libssh"),
        ["Error connecting final SSH target localhost: \
          waiting for authentication answers from user: \
          receiving from an empty and closed channel"]
    );
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_a_cancelled_password_prompt_ends_the_session() {
    if !sshd_available() {
        return;
    }
    assert_eq!(
        authentication_cancelled("ssh2"),
        [
            "Error connecting final SSH target localhost: authentication: \
          waiting for authentication answers from user: \
          receiving from an empty and closed channel"
        ]
    );
}

/// Sockets of this process in SYN_SENT towards `port` on 127.0.0.1.
#[cfg(target_os = "linux")]
fn connecting_to(port: u16) -> usize {
    let remote = format!("0100007F:{port:04X}");
    std::fs::read_to_string("/proc/net/tcp")
        .unwrap()
        .lines()
        .skip(1)
        .filter(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields[2] == remote && fields[3] == "02"
        })
        .count()
}

/// A connect to a listener whose accept queue is full stays in progress
/// (the kernel drops the SYNs) until it is cancelled.
#[cfg(target_os = "linux")]
fn connect_cancelled(backend: &str) -> Vec<String> {
    let listener = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    listener
        .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
        .unwrap();
    listener.listen(0).unwrap();
    let listener: TcpListener = listener.into();
    let port = listener.local_addr().unwrap().port();
    let queued = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();

    let dir = assert_fs::TempDir::new().unwrap();
    let (cancel, released) = cancel();
    let (session, events) = Session::connect_cancellable(
        wezterm_ssh::ResolvedSshRoute::direct(config(port, dir.path(), backend)),
        &cancel,
    )
    .unwrap();
    while connecting_to(port) == 0 {
        assert!(cancel.workers() == 1, "the session thread is still dialing");
    }
    assert!(cancel.cancel());
    let mut seen = vec![];
    smol::block_on(async {
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::Banner(_) => {}
                SessionEvent::Error(err) => {
                    seen.push(format!("Error {err}").replace(&port.to_string(), "PORT"))
                }
                other => panic!("{:?}", other),
            }
        }
    });
    drop(session);
    released.recv().unwrap();
    assert_eq!((cancel.workers(), cancel.sockets()), (0, 0));
    assert_eq!(connecting_to(port), 0, "the cancelled connect is gone");
    listener.set_nonblocking(true).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    assert_eq!(
        accepted.peer_addr().unwrap(),
        queued.local_addr().unwrap(),
        "only the connection that filled the queue was accepted"
    );
    assert!(listener.accept().is_err());
    seen
}

#[rstest]
#[cfg(target_os = "linux")]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_a_cancel_aborts_a_connect_in_progress() {
    assert_eq!(
        connect_cancelled("libssh"),
        ["Error connecting final SSH target localhost: \
          Connecting to localhost:PORT (127.0.0.1:PORT): the connection was cancelled"]
    );
}

#[rstest]
#[cfg(target_os = "linux")]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_a_cancel_aborts_a_connect_in_progress() {
    assert_eq!(
        connect_cancelled("ssh2"),
        ["Error connecting final SSH target localhost: \
          Connecting to localhost:PORT (127.0.0.1:PORT): the connection was cancelled"]
    );
}

/// A shutdown after authentication ends the session and the channel it
/// serves without waiting on the network.
fn established_session_shut_down(backend: &str) -> (Vec<String>, usize) {
    let sshd = Sshd::spawn(Default::default()).unwrap();
    sshd.tmp.child("known_hosts").touch().unwrap();
    let (cancel, released) = cancel();
    let (session, events) = Session::connect_cancellable(
        wezterm_ssh::ResolvedSshRoute::direct(config(sshd.port, sshd.tmp.path(), backend)),
        &cancel,
    )
    .unwrap();
    smol::block_on(async {
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::HostVerify(verify) => verify.answer(true).await.unwrap(),
                SessionEvent::Authenticated => break,
                SessionEvent::Error(err) => panic!("{}", err),
                _ => {}
            }
        }
    });
    let mut exec = smol::block_on(session.exec("cat", None)).unwrap();
    assert!(cancel.commit());
    assert_eq!(cancel.sockets(), 1);
    cancel.shutdown();
    let mut seen = vec![];
    smol::block_on(async {
        while let Ok(event) = events.recv().await {
            if let SessionEvent::Error(err) = event {
                seen.push(format!("Error {err}"));
            }
        }
    });
    let mut out = vec![];
    let read = std::io::Read::read_to_end(&mut exec.stdout, &mut out).unwrap_or(usize::MAX);
    drop(session);
    released.recv().unwrap();
    assert_eq!((cancel.workers(), cancel.sockets()), (0, 0));
    (seen, read)
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_a_shutdown_ends_an_established_session() {
    if !sshd_available() {
        return;
    }
    assert_eq!(
        established_session_shut_down("libssh"),
        (vec!["Error the connection was cancelled".to_string()], 0)
    );
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_a_shutdown_ends_an_established_session() {
    if !sshd_available() {
        return;
    }
    assert_eq!(
        established_session_shut_down("ssh2"),
        (vec!["Error the connection was cancelled".to_string()], 0)
    );
}

/// A server that accepts and never sends its banner holds the handshake
/// until the cancel.
fn handshake_cancelled(backend: &str) -> Vec<String> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = assert_fs::TempDir::new().unwrap();
    let (cancel, released) = cancel();
    let (session, events) = Session::connect_cancellable(
        wezterm_ssh::ResolvedSshRoute::direct(config(port, dir.path(), backend)),
        &cancel,
    )
    .unwrap();
    let (silent, _) = listener.accept().unwrap();
    let mut identification = vec![];
    for byte in std::io::Read::bytes(&silent) {
        identification.push(byte.unwrap());
        if identification.ends_with(b"\r\n") {
            break;
        }
    }
    assert!(identification.starts_with(b"SSH-2.0-"));
    assert!(cancel.cancel());
    let mut seen = vec![];
    smol::block_on(async {
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::Banner(_) => {}
                SessionEvent::Error(err) => {
                    seen.push(format!("Error {err}").replace(&port.to_string(), "PORT"))
                }
                other => panic!("{:?}", other),
            }
        }
    });
    drop(session);
    released.recv().unwrap();
    assert_eq!((cancel.workers(), cancel.sockets()), (0, 0));
    let mut rest = vec![];
    std::io::Read::read_to_end(&mut &silent, &mut rest).unwrap();
    seen
}

#[rstest]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_a_cancel_ends_a_handshake_the_server_never_answers() {
    assert_eq!(
        handshake_cancelled("libssh"),
        ["Error connecting final SSH target localhost: \
          Connecting to localhost:PORT: Fatal: Socket error: disconnected"]
    );
}

#[rstest]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_a_cancel_ends_a_handshake_the_server_never_answers() {
    assert_eq!(
        handshake_cancelled("ssh2"),
        ["Error connecting final SSH target localhost: \
          ssh handshake with localhost:PORT: [Session(-13)] Failed getting banner"]
    );
}
