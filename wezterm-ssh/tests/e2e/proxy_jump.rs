use crate::sshd::*;
use assert_fs::prelude::*;
use portable_pty::Child;
use rstest::*;
use std::io::Read;
use wezterm_ssh::{Config, ResolvedSshRoute, Session, SessionEvent};

fn route_via_jumps(jumps: &[&Sshd], target: &Sshd, backend: &str) -> ResolvedSshRoute {
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
        target_port = target.port,
        target_identity = target.tmp.child("id_rsa").path().display(),
        target_known_hosts = target.tmp.child("known_hosts").path().display(),
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
    route_via_jumps(&[jump], target, backend)
}

async fn connect_and_trust(route: ResolvedSshRoute) -> Session {
    let (session, events) = Session::connect_route(route).expect("connect to sshd");
    let mut authenticated = 0;

    while let Ok(event) = events.recv().await {
        match event {
            SessionEvent::Banner(_) => {}
            SessionEvent::HostVerify(verify) => verify.answer(true).await.unwrap(),
            SessionEvent::Authenticate(auth) => {
                let len = auth.prompts.len();
                auth.answer(vec![String::new(); len]).await.unwrap();
            }
            SessionEvent::HostVerificationFailed(failed) => panic!("{}", failed),
            SessionEvent::Error(err) => panic!("{}", err),
            SessionEvent::Authenticated => {
                authenticated += 1;
                break;
            }
        }
    }

    assert_eq!(authenticated, 1);
    session
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
    let route = route_via_jumps(&[&jump0, &jump1], &target, backend);
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
