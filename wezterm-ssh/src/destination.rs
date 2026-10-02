use std::net::IpAddr;

/// The networks a session may dial, from the `wezterm_ssh_destination_networks`
/// option: whitespace-separated `address/prefix` entries such as
/// `100.64.0.0/10 fd7a:115c:a1e0::/48`.
///
/// The host name is resolved once, on the session thread; the first resolved
/// address inside these networks is the one dialed, and host trust is still
/// looked up under the host name.  A name with no such address fails without
/// a connection.  The option cannot restrict a ProxyCommand or a ProxyJump
/// hop, so a session that would use one is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationNetworks(Vec<(IpAddr, u8)>);

impl std::str::FromStr for DestinationNetworks {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let networks = text
            .split_whitespace()
            .map(|entry| {
                let parsed = entry.split_once('/').and_then(|(addr, prefix)| {
                    let addr = addr.parse::<IpAddr>().ok()?;
                    let bits = if addr.is_ipv4() { 32 } else { 128 };
                    let prefix = prefix.parse::<u8>().ok().filter(|p| *p <= bits)?;
                    Some((addr, prefix))
                });
                parsed.ok_or_else(|| anyhow::anyhow!("invalid destination network {entry:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(!networks.is_empty(), "no destination network given");
        Ok(Self(networks))
    }
}

impl DestinationNetworks {
    /// Whether `addr` lies in one of the networks.
    pub fn contains(&self, addr: IpAddr) -> bool {
        self.0.iter().any(|(network, prefix)| {
            let (network, addr, bits) = match (network, addr) {
                (IpAddr::V4(n), IpAddr::V4(a)) => {
                    (u128::from(u32::from(*n)), u128::from(u32::from(a)), 32)
                }
                (IpAddr::V6(n), IpAddr::V6(a)) => (u128::from(*n), u128::from(a), 128u32),
                _ => return false,
            };
            let host_bits = bits - u32::from(*prefix);
            network.checked_shr(host_bits).unwrap_or(0) == addr.checked_shr(host_bits).unwrap_or(0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains(networks: &str, addr: &str) -> bool {
        networks
            .parse::<DestinationNetworks>()
            .unwrap()
            .contains(addr.parse().unwrap())
    }

    #[test]
    fn contains_exactly_the_prefix() {
        let tailnet = "100.64.0.0/10 fd7a:115c:a1e0::/48";
        for inside in [
            "100.64.0.0",
            "100.64.0.1",
            "100.127.255.255",
            "fd7a:115c:a1e0::1",
            "fd7a:115c:a1e0:ffff:ffff:ffff:ffff:ffff",
        ] {
            assert!(contains(tailnet, inside), "{}", inside);
        }
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
            "fd7a:115c:a0e0::1",
        ] {
            assert!(!contains(tailnet, outside), "{}", outside);
        }
        assert!(contains("0.0.0.0/0", "8.8.8.8"));
        assert!(!contains("0.0.0.0/0", "::1"));
        assert!(contains("127.0.0.1/32", "127.0.0.1"));
        assert!(!contains("127.0.0.1/32", "127.0.0.2"));
    }

    #[test]
    fn refuses_malformed_networks() {
        for (bad, error) in [
            ("", "no destination network given"),
            ("100.64.0.0", r#"invalid destination network "100.64.0.0""#),
            (
                "100.64.0.0/33",
                r#"invalid destination network "100.64.0.0/33""#,
            ),
            ("fd7a::/129", r#"invalid destination network "fd7a::/129""#),
            (
                "laptop.ts.net/32",
                r#"invalid destination network "laptop.ts.net/32""#,
            ),
            ("100.64.0.0/10 x", r#"invalid destination network "x""#),
        ] {
            assert_eq!(
                bad.parse::<DestinationNetworks>()
                    .map_err(|err| err.to_string()),
                Err(error.to_string()),
                "{bad:?}"
            );
        }
    }
}
