use crate::sshd::*;
use std::net::TcpListener;

/// Asked first for a port another listener holds, the fixture serves from
/// a port its own sshd listens on.
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
}
