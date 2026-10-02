//! The validated connection profile: which laptop mux to attach to.
//!
//! A [`Profile`] exists only for a tailnet endpoint, a plain SSH user name
//! and, optionally, an absolute path to the laptop's `wezterm`.  It maps to
//! the existing `SshDomain` with multiplexing; nothing in it can name a
//! local program, and the proxy command is derived, never entered.

#![forbid(unsafe_code)]

use config::{SshDomain, SshMultiplexing};
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use thiserror::Error;
use wezterm_ssh::DestinationNetworks;

/// The networks Tailscale assigns addresses from.  A literal address must
/// lie in them, and SSH dials only an address in them: a MagicDNS name is
/// resolved once, on the SSH thread, and the address it dials is the one
/// checked.
const TAILNET: &str = "100.64.0.0/10 fd7a:115c:a1e0::/48";

/// What the connection form holds, as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileFields {
    /// Tailnet address or MagicDNS name of the laptop.
    pub host: String,
    /// SSH port; empty means 22.
    pub port: String,
    /// SSH user on the laptop.
    pub user: String,
    /// Absolute path of `wezterm` on the laptop; empty means `wezterm`
    /// from the laptop's `PATH`.
    pub remote_wezterm: String,
}

/// A tailnet endpoint.  Tailscale assigns addresses from 100.64.0.0/10 and
/// fd7a:115c:a1e0::/48 and MagicDNS names under `ts.net`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailnetHost {
    /// An address in 100.64.0.0/10.
    V4(Ipv4Addr),
    /// An address in fd7a:115c:a1e0::/48.
    V6(Ipv6Addr),
    /// A fully qualified MagicDNS name.
    Name(String),
}

impl std::fmt::Display for TailnetHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::V4(addr) => addr.fmt(f),
            Self::V6(addr) => addr.fmt(f),
            Self::Name(name) => name.fmt(f),
        }
    }
}

/// Which field of [`ProfileFields`] was refused, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileError {
    /// The host is empty.
    #[error("enter the laptop's Tailscale address")]
    HostMissing,
    /// The host is an IP address outside the tailnet ranges.
    #[error("the address is not a Tailscale address (100.64.0.0/10 or fd7a:115c:a1e0::/48)")]
    HostNotTailnetAddress,
    /// The host is a name that is not a MagicDNS name.
    #[error("the name is not a Tailscale MagicDNS name ending in .ts.net")]
    HostNotTailnetName,
    /// The port is not 1 to 65535.
    #[error("the port must be a number from 1 to 65535")]
    Port,
    /// The user name is empty or has characters a login name cannot have.
    #[error("the SSH user must be a login name: letters, digits, '_', '.', '-'")]
    User,
    /// The remote wezterm path is not an absolute path.
    #[error("the remote wezterm path must be an absolute path without control characters")]
    RemoteWezterm,
}

impl ProfileError {
    /// The form field the error belongs to.
    pub fn field(self) -> &'static str {
        match self {
            Self::HostMissing | Self::HostNotTailnetAddress | Self::HostNotTailnetName => "host",
            Self::Port => "port",
            Self::User => "user",
            Self::RemoteWezterm => "remote_wezterm",
        }
    }
}

/// The laptop mux to attach to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    host: TailnetHost,
    port: u16,
    user: String,
    remote_wezterm: Option<String>,
}

/// `useradd(8)`: login names may be up to 32 characters long.
const LOGIN_NAME_MAX: usize = 32;
/// Linux `PATH_MAX`, including the terminating NUL.
const PATH_MAX: usize = 4096;

fn parse_host(host: &str) -> Result<TailnetHost, ProfileError> {
    if host.is_empty() {
        return Err(ProfileError::HostMissing);
    }
    if let Ok(addr) = host.parse::<IpAddr>() {
        let tailnet: DestinationNetworks = TAILNET.parse().expect("TAILNET is valid");
        return match addr {
            _ if !tailnet.contains(addr) => Err(ProfileError::HostNotTailnetAddress),
            IpAddr::V4(addr) => Ok(TailnetHost::V4(addr)),
            IpAddr::V6(addr) => Ok(TailnetHost::V6(addr)),
        };
    }
    let name = host.to_ascii_lowercase();
    let label_ok = |label: &str| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    let is_magic_dns = name.len() <= 253
        && name
            .strip_suffix(".ts.net")
            .is_some_and(|machine| machine.split('.').all(label_ok));
    if is_magic_dns {
        Ok(TailnetHost::Name(name))
    } else {
        Err(ProfileError::HostNotTailnetName)
    }
}

fn parse_user(user: &str) -> Result<String, ProfileError> {
    let name = user.strip_suffix('$').unwrap_or(user);
    let valid = (1..=LOGIN_NAME_MAX).contains(&user.len())
        && !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if valid {
        Ok(user.to_string())
    } else {
        Err(ProfileError::User)
    }
}

impl Profile {
    /// Validate what the form holds.  Surrounding whitespace is ignored.
    pub fn parse(fields: &ProfileFields) -> Result<Self, ProfileError> {
        let host = parse_host(fields.host.trim())?;
        let port = match fields.port.trim() {
            "" => 22,
            port => port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or(ProfileError::Port)?,
        };
        let user = parse_user(fields.user.trim())?;
        let remote_wezterm = match fields.remote_wezterm.trim() {
            "" => None,
            path if path.starts_with('/')
                && path.len() < PATH_MAX
                && !path.chars().any(char::is_control) =>
            {
                Some(path.to_string())
            }
            _ => return Err(ProfileError::RemoteWezterm),
        };
        Ok(Self {
            host,
            port,
            user,
            remote_wezterm,
        })
    }

    /// The command the SSH session runs on the laptop.  It proxies to a mux
    /// server that is already running and never starts one.
    pub fn proxy_command(&self) -> String {
        let wezterm = self.remote_wezterm.as_deref().unwrap_or("wezterm");
        format!(
            "{} cli --prefer-mux --no-auto-start proxy",
            shell_words::quote(wezterm)
        )
    }

    /// The SSHMUX domain for this profile, named `name`.  Host trust is
    /// read from and written to `known_hosts` only; `identity`, when the
    /// user imported one, is the only key offered.  No agent, no ssh
    /// configuration file and no default identity is consulted.  Only a
    /// tailnet address is dialed, under the host name the user entered.
    pub fn ssh_domain(&self, name: &str, known_hosts: &Path, identity: Option<&Path>) -> SshDomain {
        let ssh_option = std::collections::HashMap::from([
            ("port".to_string(), self.port.to_string()),
            (
                "userknownhostsfile".to_string(),
                known_hosts.display().to_string(),
            ),
            (
                "wezterm_ssh_config_dir".to_string(),
                known_hosts
                    .parent()
                    .expect("private SSH directory")
                    .display()
                    .to_string(),
            ),
            (
                "wezterm_ssh_destination_networks".to_string(),
                TAILNET.to_string(),
            ),
            ("identitiesonly".to_string(), "yes".to_string()),
            (
                "identityfile".to_string(),
                identity
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            ),
        ]);
        SshDomain {
            name: name.to_string(),
            remote_address: self.host.to_string(),
            no_agent_auth: true,
            username: Some(self.user.clone()),
            multiplexing: SshMultiplexing::WezTerm,
            remote_wezterm_path: self.remote_wezterm.clone(),
            override_proxy_command: Some(self.proxy_command()),
            ssh_option,
            ..SshDomain::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(host: &str, port: &str, user: &str, remote_wezterm: &str) -> ProfileFields {
        ProfileFields {
            host: host.into(),
            port: port.into(),
            user: user.into(),
            remote_wezterm: remote_wezterm.into(),
        }
    }

    #[test]
    fn accepts_tailnet_endpoints_only() {
        let host = |host: &str| Profile::parse(&fields(host, "", "me", "")).map(|p| p.host);
        assert_eq!(
            host("100.64.0.1"),
            Ok(TailnetHost::V4(Ipv4Addr::new(100, 64, 0, 1)))
        );
        assert_eq!(
            host(" 100.127.255.254 "),
            Ok(TailnetHost::V4(Ipv4Addr::new(100, 127, 255, 254)))
        );
        assert_eq!(
            host("fd7a:115c:a1e0::1"),
            Ok(TailnetHost::V6("fd7a:115c:a1e0::1".parse().unwrap()))
        );
        assert_eq!(
            host("Laptop.Tail1234.ts.net"),
            Ok(TailnetHost::Name("laptop.tail1234.ts.net".into()))
        );
        for outside in [
            "100.63.255.255",
            "100.128.0.0",
            "127.0.0.1",
            "10.0.2.2",
            "192.168.1.5",
            "8.8.8.8",
            "::1",
            "::ffff:100.64.0.1",
            "fd7a:115c:a1e1::1",
        ] {
            assert_eq!(
                host(outside),
                Err(ProfileError::HostNotTailnetAddress),
                "{outside}"
            );
        }
        for name in [
            "laptop",
            "localhost",
            "example.com",
            "ts.net",
            ".ts.net",
            "-a.ts.net",
            "a b.ts.net",
            "a;b.ts.net",
        ] {
            assert_eq!(host(name), Err(ProfileError::HostNotTailnetName), "{name}");
        }
        assert_eq!(host("  "), Err(ProfileError::HostMissing));
    }

    #[test]
    fn port_defaults_to_22_and_must_be_a_port() {
        let port =
            |port: &str| Profile::parse(&fields("100.64.0.1", port, "me", "")).map(|p| p.port);
        assert_eq!(port(""), Ok(22));
        assert_eq!(port("2222"), Ok(2222));
        assert_eq!(port("65535"), Ok(65535));
        for bad in ["0", "65536", "-1", "22 23", "ssh"] {
            assert_eq!(port(bad), Err(ProfileError::Port), "{bad}");
        }
    }

    #[test]
    fn user_is_a_login_name() {
        let user = |user: &str| Profile::parse(&fields("100.64.0.1", "", user, "")).map(|p| p.user);
        assert_eq!(user("juruc"), Ok("juruc".into()));
        assert_eq!(user("svc_x.y-z$"), Ok("svc_x.y-z$".into()));
        for bad in [
            "",
            "$",
            "-oProxyCommand=x",
            "a b",
            "a@b",
            "a\nb",
            "root;id",
            &"a".repeat(33),
        ] {
            assert_eq!(user(bad), Err(ProfileError::User), "{bad:?}");
        }
    }

    #[test]
    fn proxy_command_quotes_the_remote_path_and_never_starts_a_server() {
        let command = |path: &str| {
            Profile::parse(&fields("100.64.0.1", "", "me", path)).map(|p| p.proxy_command())
        };
        assert_eq!(
            command(""),
            Ok("wezterm cli --prefer-mux --no-auto-start proxy".into())
        );
        assert_eq!(
            command("/usr/local/bin/wezterm"),
            Ok("/usr/local/bin/wezterm cli --prefer-mux --no-auto-start proxy".into())
        );
        assert_eq!(
            command("/opt/my apps/wez'term; rm -rf $HOME"),
            Ok(
                r#"'/opt/my apps/wez'\''term; rm -rf $HOME' cli --prefer-mux --no-auto-start proxy"#
                    .into()
            )
        );
        for bad in ["wezterm", "bin/wezterm", "~/bin/wezterm", "/bin/wez\nterm"] {
            assert_eq!(command(bad), Err(ProfileError::RemoteWezterm), "{bad:?}");
        }
    }

    #[test]
    fn errors_name_their_field() {
        assert_eq!(ProfileError::HostNotTailnetName.field(), "host");
        assert_eq!(ProfileError::Port.field(), "port");
        assert_eq!(ProfileError::User.field(), "user");
        assert_eq!(ProfileError::RemoteWezterm.field(), "remote_wezterm");
    }

    #[test]
    fn ssh_domain_is_sshmux_with_private_trust_and_identity() {
        let profile =
            Profile::parse(&fields("fd7a:115c:a1e0::9", "2022", "me", "/opt/w/wezterm")).unwrap();
        let known_hosts = Path::new("/data/user/0/app/files/ssh/known_hosts");
        let identity = Path::new("/data/user/0/app/files/ssh/identity");
        let domain = profile.ssh_domain("laptop-1", known_hosts, Some(identity));
        assert_eq!(domain.name, "laptop-1");
        assert_eq!(domain.remote_address, "fd7a:115c:a1e0::9");
        assert_eq!(domain.username.as_deref(), Some("me"));
        assert_eq!(domain.multiplexing, SshMultiplexing::WezTerm);
        assert!(domain.no_agent_auth);
        assert!(!domain.connect_automatically);
        assert_eq!(domain.default_prog, None);
        assert_eq!(
            domain.override_proxy_command.as_deref(),
            Some("/opt/w/wezterm cli --prefer-mux --no-auto-start proxy")
        );
        let mut options: Vec<_> = domain
            .ssh_option
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        options.sort();
        assert_eq!(
            options,
            [
                ("identitiesonly", "yes"),
                ("identityfile", "/data/user/0/app/files/ssh/identity"),
                ("port", "2022"),
                (
                    "userknownhostsfile",
                    "/data/user/0/app/files/ssh/known_hosts"
                ),
                ("wezterm_ssh_config_dir", "/data/user/0/app/files/ssh"),
                (
                    "wezterm_ssh_destination_networks",
                    "100.64.0.0/10 fd7a:115c:a1e0::/48"
                ),
            ]
        );

        let without = profile.ssh_domain("laptop-2", known_hosts, None);
        assert_eq!(without.ssh_option["identityfile"], "");
    }
}
