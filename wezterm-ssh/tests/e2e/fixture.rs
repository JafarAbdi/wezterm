use crate::sshd::*;
use std::net::TcpListener;
use wezterm_ssh::Config;

/// Asked first for a port another listener holds, the fixture serves from
/// a port its own sshd listens on, and never dials the taken one.
#[test]
fn a_fixture_never_reports_a_port_another_listener_holds() {
    if !sshd_available() {
        return;
    }
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = other.local_addr().unwrap().port();
    let ports = std::iter::once(taken).chain(std::iter::repeat_with(allocate_port));
    let sshd = Sshd::spawn_on(Default::default(), ports).unwrap();

    assert_ne!(
        sshd.port, taken,
        "the fixture reported the other listener's port"
    );
    other.set_nonblocking(true).unwrap();
    assert_eq!(
        other.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the fixture dialed the other listener"
    );

    let (sshd_pid, agent_pid) = sshd.pids();
    let session = smol::block_on(session(Config::new(), sshd));
    drop(session);
    #[cfg(target_os = "linux")]
    for pid in [sshd_pid, agent_pid] {
        let ours = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let after_name = &stat[stat.rfind(')')? + 2..];
                after_name.split(' ').nth(1)?.parse::<u32>().ok()
            })
            == Some(std::process::id());
        assert!(
            !ours,
            "fixture process {} is still a child of the test",
            pid
        );
    }
}
