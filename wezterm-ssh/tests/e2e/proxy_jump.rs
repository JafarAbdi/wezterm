use crate::sshd::*;
use assert_fs::prelude::*;
use portable_pty::Child;
use rstest::*;
use smol::channel::Receiver;
use std::io::Read;
use std::net::TcpListener;
use std::time::Duration;
use wezterm_ssh::{Config, ResolvedSshRoute, Session, SessionEvent};

/// A route to `target_port` on localhost through `jumps`, authenticating
/// with the keys of `target_keys`.
fn route_via_jumps(
    jumps: &[&Sshd],
    target_port: u16,
    target_keys: &Sshd,
    backend: &str,
) -> ResolvedSshRoute {
    let mut config = Config::new();
    config.set_option("wezterm_ssh_backend", backend);

    let jump_list = (0..jumps.len())
        .map(|idx| format!("jump{idx}"))
        .collect::<Vec<_>>()
        .join(",");
    config.add_config_string(&format!(
        r#"
Host target
    HostName localhost
    Port {target_port}
    User {user}
    IdentityFile {target_identity}
    UserKnownHostsFile {target_known_hosts}
    IdentitiesOnly yes
    ProxyJump {jump_list}
"#,
        user = whoami::username(),
        target_identity = target_keys.tmp.child("id_rsa").path().display(),
        target_known_hosts = target_keys.tmp.child("known_hosts").path().display(),
    ));

    for (idx, jump) in jumps.iter().enumerate() {
        config.add_config_string(&format!(
            r#"
Host jump{idx}
    HostName localhost
    Port {jump_port}
    User {user}
    IdentityFile {jump_identity}
    UserKnownHostsFile {jump_known_hosts}
    IdentitiesOnly yes
"#,
            user = whoami::username(),
            jump_port = jump.port,
            jump_identity = jump.tmp.child("id_rsa").path().display(),
            jump_known_hosts = jump.tmp.child("known_hosts").path().display(),
        ));
    }

    let target = config.for_host("target");
    config.resolve_route(target).unwrap()
}

fn route_via_jump(jump: &Sshd, target: &Sshd, backend: &str) -> ResolvedSshRoute {
    route_via_jumps(&[jump], target.port, target, backend)
}

/// Trusts every host key and answers every prompt until the session is
/// authenticated or reports an error.
async fn trust_and_authenticate(events: &Receiver<SessionEvent>) -> Result<(), String> {
    while let Ok(event) = events.recv().await {
        match event {
            SessionEvent::Banner(_) => {}
            SessionEvent::HostVerify(verify) => verify.answer(true).await.unwrap(),
            SessionEvent::Authenticate(auth) => {
                let len = auth.prompts.len();
                auth.answer(vec![String::new(); len]).await.unwrap();
            }
            SessionEvent::HostVerificationFailed(failed) => panic!("{}", failed),
            SessionEvent::Error(err) => return Err(err),
            SessionEvent::Authenticated => return Ok(()),
        }
    }
    Err("the session ended without authenticating".into())
}

async fn connect_and_trust(route: ResolvedSshRoute) -> Session {
    let (session, events) = Session::connect_route(route).expect("connect to sshd");
    trust_and_authenticate(&events).await.unwrap();
    session
}

/// The target behind the jump accepts the connection and closes it at once,
/// so the jump host ends the direct-tcpip channel with a clean EOF.
fn proxy_jump_to_a_target_that_closes_fails_to_connect(backend: &str) {
    if !sshd_available() {
        return;
    }

    let jump = Sshd::spawn(Default::default()).unwrap();
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = target.local_addr().unwrap().port();
    let accept_and_close = std::thread::spawn(move || drop(target.accept().unwrap()));
    let route = route_via_jumps(&[&jump], port, &jump, backend);

    smol::block_on(async move {
        let (_session, events) = Session::connect_route(route).unwrap();
        let outcome = smol::future::or(trust_and_authenticate(&events), async {
            smol::Timer::after(Duration::from_secs(10)).await;
            Err("still connecting after 10s".to_string())
        })
        .await;
        accept_and_close.join().unwrap();
        let err = outcome.expect_err("connected through a closed target");
        assert!(!err.contains("still connecting"), "{err}");
    });
}

fn proxy_jump_execs_on_final_target(backend: &str) {
    if !sshd_available() {
        return;
    }

    let jump = Sshd::spawn(Default::default()).unwrap();
    let target = Sshd::spawn(Default::default()).unwrap();
    let route = route_via_jump(&jump, &target, backend);

    smol::block_on(async move {
        let session = connect_and_trust(route).await;
        let mut exec = session.exec("printf proxyjump-ok", None).await.unwrap();
        let mut output = String::new();
        exec.stdout.read_to_string(&mut output).unwrap();
        assert_eq!(output, "proxyjump-ok");
        assert!(exec.child.wait().unwrap().success());
    });
}

fn two_jump_proxy_jump_execs_on_final_target(backend: &str) {
    if !sshd_available() {
        return;
    }

    let jump0 = Sshd::spawn(Default::default()).unwrap();
    let jump1 = Sshd::spawn(Default::default()).unwrap();
    let target = Sshd::spawn(Default::default()).unwrap();
    let route = route_via_jumps(&[&jump0, &jump1], target.port, &target, backend);
    assert_eq!(route.jumps().len(), 2);

    smol::block_on(async move {
        let session = connect_and_trust(route).await;
        let mut exec = session.exec("printf two-jump-ok", None).await.unwrap();
        let mut output = String::new();
        exec.stdout.read_to_string(&mut output).unwrap();
        assert_eq!(output, "two-jump-ok");
        assert!(exec.child.wait().unwrap().success());
    });
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_proxy_jump_execs_on_final_target() {
    proxy_jump_execs_on_final_target("libssh");
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_proxy_jump_execs_on_final_target() {
    proxy_jump_execs_on_final_target("ssh2");
}

/// 4MB through the jump. A direct session moves it in under a second on
/// this machine; a bridge that waits out its poll timeout between 8KB
/// reads needs over 30s.
fn proxy_jump_streams_without_stalling(backend: &str) {
    if !sshd_available() {
        return;
    }
    let jump = Sshd::spawn(Default::default()).unwrap();
    let target = Sshd::spawn(Default::default()).unwrap();
    let route = route_via_jump(&jump, &target, backend);

    smol::block_on(async move {
        let session = connect_and_trust(route).await;
        let mut exec = session
            .exec("head -c 4000000 /dev/zero", None)
            .await
            .unwrap();
        let start = std::time::Instant::now();
        let mut out = Vec::new();
        exec.stdout.read_to_end(&mut out).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(out.len(), 4_000_000);
        assert!(
            elapsed < Duration::from_secs(15),
            "4MB through the jump took {:?}",
            elapsed
        );
    });
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_proxy_jump_streams_without_stalling() {
    proxy_jump_streams_without_stalling("libssh");
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_proxy_jump_streams_without_stalling() {
    proxy_jump_streams_without_stalling("ssh2");
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_proxy_jump_to_a_target_that_closes_fails_to_connect() {
    proxy_jump_to_a_target_that_closes_fails_to_connect("libssh");
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_proxy_jump_to_a_target_that_closes_fails_to_connect() {
    proxy_jump_to_a_target_that_closes_fails_to_connect("ssh2");
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_two_jump_proxy_jump_execs_on_final_target() {
    two_jump_proxy_jump_execs_on_final_target("libssh");
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_two_jump_proxy_jump_execs_on_final_target() {
    two_jump_proxy_jump_execs_on_final_target("ssh2");
}
