use crate::sshd::*;
use assert_fs::prelude::*;
use rstest::*;
use std::process::Command;
use wezterm_ssh::{Config, ConfigMap, Session, SessionEvent};

fn config(sshd: &Sshd, backend: &str) -> ConfigMap {
    let mut config = Config::new().for_host("localhost");
    for (key, value) in [
        ("port", sshd.port.to_string()),
        ("addressfamily", "inet".to_string()),
        ("wezterm_ssh_backend", backend.to_string()),
        (
            "wezterm_ssh_config_dir",
            sshd.tmp.path().display().to_string(),
        ),
        ("user", whoami::username()),
        ("identitiesonly", "yes".to_string()),
        (
            "identityfile",
            sshd.tmp.child("id_rsa").path().display().to_string(),
        ),
        (
            "userknownhostsfile",
            sshd.tmp.child("known_hosts").path().display().to_string(),
        ),
    ] {
        config.insert(key.to_string(), value);
    }
    config
}

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

    let seen = events(config(&sshd, backend));
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

/// `declined` is the error after the host trust prompt is declined.
fn destination_networks_restrict_the_dialed_address(backend: &str, declined: &str) {
    if !sshd_available() {
        return;
    }
    let sshd = Sshd::spawn(Default::default()).unwrap();
    // ssh2 verifies hosts only against a known_hosts file that exists.
    sshd.tmp.child("known_hosts").touch().unwrap();
    let mut tailnet = config(&sshd, backend);
    tailnet.insert(
        "wezterm_ssh_destination_networks".to_string(),
        "100.64.0.0/10 fd7a:115c:a1e0::/48".to_string(),
    );
    assert_eq!(
        events(tailnet),
        ["Error connecting final SSH target localhost: \
          localhost has no address in 100.64.0.0/10 fd7a:115c:a1e0::/48"]
    );

    let mut loopback = config(&sshd, backend);
    loopback.insert(
        "wezterm_ssh_destination_networks".to_string(),
        "127.0.0.0/8".to_string(),
    );
    assert_eq!(events(loopback), ["HostVerify", declined]);
    assert_eq!(
        std::fs::read_to_string(sshd.tmp.child("known_hosts").path()).unwrap(),
        ""
    );
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn libssh_destination_networks_restrict_the_dialed_address() {
    destination_networks_restrict_the_dialed_address(
        "libssh",
        "Error connecting final SSH target localhost: user declined to trust host",
    );
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "ssh2"), ignore)]
fn ssh2_destination_networks_restrict_the_dialed_address() {
    destination_networks_restrict_the_dialed_address(
        "ssh2",
        "Error connecting final SSH target localhost: \
         host verification: user declined to trust host",
    );
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn destination_networks_refuse_a_proxy_command_and_a_proxy_jump() {
    if !sshd_available() {
        return;
    }
    let sshd = Sshd::spawn(Default::default()).unwrap();
    let mut proxied = config(&sshd, "libssh");
    proxied.insert(
        "wezterm_ssh_destination_networks".to_string(),
        "127.0.0.0/8".to_string(),
    );
    proxied.insert("proxycommand".to_string(), "false".to_string());
    assert_eq!(
        events(proxied),
        ["Error connecting final SSH target localhost: \
          wezterm_ssh_destination_networks cannot restrict a ProxyCommand"]
    );

    let mut config = Config::new();
    config.add_config_string(&format!(
        r#"
Host target
    HostName localhost
    Port {port}
    Wezterm_Ssh_Destination_Networks 127.0.0.0/8
    ProxyJump jump
Host jump
    HostName localhost
    Port {port}
    User {user}
    AddressFamily inet
    IdentityFile {identity}
    UserKnownHostsFile {known_hosts}
    Wezterm_Ssh_Config_Dir {dir}
    IdentitiesOnly yes
"#,
        port = sshd.port,
        user = whoami::username(),
        identity = sshd.tmp.child("id_rsa").path().display(),
        known_hosts = sshd.tmp.child("known_hosts").path().display(),
        dir = sshd.tmp.path().display(),
    ));
    let route = config.resolve_route(config.for_host("target")).unwrap();
    let (_session, events) = Session::connect_route(route).unwrap();
    let mut seen = vec![];
    smol::block_on(async {
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::HostVerify(verify) => verify.answer(true).await.unwrap(),
                SessionEvent::Authenticate(auth) => {
                    let answers = vec![String::new(); auth.prompts.len()];
                    auth.answer(answers).await.unwrap();
                }
                SessionEvent::Error(err) => seen.push(err),
                _ => {}
            }
        }
    });
    assert_eq!(
        seen,
        ["connecting final SSH target localhost: \
          wezterm_ssh_destination_networks cannot restrict a ProxyJump hop"]
    );
}
