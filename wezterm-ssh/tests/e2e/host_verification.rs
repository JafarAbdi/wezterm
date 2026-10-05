use crate::sshd::*;
use assert_fs::prelude::*;
use rstest::*;
use std::process::Command;
use wezterm_ssh::{ConfigMap, Session, SessionEvent};

/// Every event but banners until the session ends or authenticates, as
/// text; a host trust prompt is declined.
fn events(config: ConfigMap) -> Vec<String> {
    let (_session, events) = Session::connect(config).expect("start the session");
    let mut seen = vec![];
    smol::block_on(async {
        while let Ok(event) = events.recv().await {
            seen.push(match event {
                SessionEvent::Banner(_) => continue,
                SessionEvent::HostVerify(verify) => {
                    verify.answer(false).await.unwrap();
                    "HostVerify".to_string()
                }
                SessionEvent::HostVerified => "HostVerified".to_string(),
                SessionEvent::HostKeyTypeChanged => "HostKeyTypeChanged".to_string(),
                SessionEvent::HostVerificationFailed(failed) => {
                    format!("HostVerificationFailed {}", failed.remote_address)
                }
                SessionEvent::Authenticate(_) => "Authenticate".to_string(),
                SessionEvent::Authenticated => {
                    seen.push("Authenticated".to_string());
                    break;
                }
                SessionEvent::Error(err) => format!("Error {err}"),
            });
        }
    });
    seen
}

/// The server presents its RSA host key while known_hosts trusts an
/// Ed25519 key for it.
fn trusted_key_of_another_type_fails_closed(backend: &str) -> (Vec<String>, u16) {
    let sshd = Sshd::spawn(Default::default()).unwrap();
    let trusted = sshd.tmp.child("trusted_ed25519");
    assert!(Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(trusted.path())
        .status()
        .unwrap()
        .success());
    let public = std::fs::read_to_string(trusted.path().with_extension("pub")).unwrap();
    let key = public
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(key.starts_with("ssh-ed25519 "));
    let known_hosts = format!("[localhost]:{} {key}\n", sshd.port);
    sshd.tmp
        .child("known_hosts")
        .write_str(&known_hosts)
        .unwrap();

    let seen = events(private_config(sshd.port, sshd.tmp.path(), backend));
    assert_eq!(
        std::fs::read_to_string(sshd.tmp.child("known_hosts").path()).unwrap(),
        known_hosts,
        "trust is not overwritten"
    );
    (seen, sshd.port)
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_reports_a_trusted_key_of_another_type_without_asking() {
    if !sshd_available() {
        return;
    }
    let (seen, _) = trusted_key_of_another_type_fails_closed("libssh");
    assert_eq!(
        seen,
        [
            "HostKeyTypeChanged",
            "Error connecting final SSH target localhost: \
             The host key for this server was not found, but another\n\
             type of key exists. An attacker might change the default\n\
             server key to confuse your client into thinking the key\n\
             does not exist",
        ]
    );
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_reports_a_trusted_key_of_another_type_as_a_mismatch() {
    if !sshd_available() {
        return;
    }
    let (seen, port) = trusted_key_of_another_type_fails_closed("ssh2");
    assert_eq!(
        seen,
        [
            format!("HostVerificationFailed localhost:{port}"),
            "Error connecting final SSH target localhost: \
             host verification: Host key verification failed"
                .to_string(),
        ]
    );
}
