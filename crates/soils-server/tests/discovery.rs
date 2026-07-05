//! End-to-end checks of the runtime LAN-discovery toggle over real UDP: the
//! responder must answer probes while enabled, go silent (socket released)
//! while disabled, and advertise the actually-bound game port. Uses
//! `discovery_port: 0` so tests never collide with a real server on udp/9002.

use std::net::UdpSocket;
use std::path::Path;
use std::time::{Duration, Instant};

use soils_protocol::{PROBE_MAGIC, ServerInfo, decode};
use soils_server::{ServerConfig, ServerHandle};

fn test_config(name: &str, enable_discovery: bool, data_dir: &Path) -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1:0".into(),
        data_dir: data_dir.into(),
        enable_discovery,
        discovery_port: 0,
        name: name.into(),
        ..ServerConfig::default()
    }
}

/// Poll until the responder reaches the desired bound/unbound state.
fn wait_for_port(handle: &ServerHandle, want_bound: bool) -> Option<u16> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let port = handle.discovery_port();
        if port.is_some() == want_bound {
            return port;
        }
        assert!(
            Instant::now() < deadline,
            "discovery responder did not become {}",
            if want_bound { "bound" } else { "unbound" }
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Send one probe; `Some` if answered with a valid reply, `None` on timeout.
fn probe(port: u16) -> Option<ServerInfo> {
    let sock = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(800))).unwrap();
    sock.send_to(PROBE_MAGIC, ("127.0.0.1", port)).unwrap();
    let mut buf = [0u8; 256];
    match sock.recv_from(&mut buf) {
        Ok((n, _)) => {
            assert!(n >= PROBE_MAGIC.len() && &buf[..PROBE_MAGIC.len()] == PROBE_MAGIC);
            decode::<ServerInfo>(&buf[PROBE_MAGIC.len()..n])
        }
        Err(_) => None,
    }
}

/// Probe with retries, for the "must answer" direction (a lone UDP datagram is
/// in principle droppable even on loopback).
fn probe_expect(port: u16) -> ServerInfo {
    for _ in 0..3 {
        if let Some(info) = probe(port) {
            return info;
        }
    }
    panic!("discovery probe went unanswered while discovery is on");
}

#[test]
fn toggle_discovery_at_runtime() {
    let data_dir = std::env::temp_dir().join(format!("soils-disco-test-{}", std::process::id()));
    let handle = soils_server::spawn(test_config("disco-test", false, &data_dir)).expect("spawn");

    // Off by default: no desire, no responder.
    assert!(!handle.discovery_enabled());
    assert_eq!(handle.discovery_port(), None);

    // On: responder binds and answers with the right name and game port.
    handle.set_discovery(true);
    let port = wait_for_port(&handle, true).unwrap();
    let info = probe_expect(port);
    assert_eq!(info.name, "disco-test");
    assert_eq!(info.game_port, handle.port());

    // Off again: socket released, probes to the old port go unanswered.
    handle.set_discovery(false);
    wait_for_port(&handle, false);
    assert!(probe(port).is_none(), "probe must not be answered while discovery is off");

    // Re-enable: proves the supervisor loops rather than dying after one cycle.
    handle.set_discovery(true);
    let port = wait_for_port(&handle, true).unwrap();
    probe_expect(port);

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn discovery_on_at_startup() {
    // Dedicated-server parity: `enable_discovery: true` answers probes from
    // startup with no toggling involved.
    let data_dir =
        std::env::temp_dir().join(format!("soils-disco-boot-test-{}", std::process::id()));
    let handle = soils_server::spawn(test_config("boot-test", true, &data_dir)).expect("spawn");

    assert!(handle.discovery_enabled());
    let port = wait_for_port(&handle, true).unwrap();
    let info = probe_expect(port);
    assert_eq!(info.name, "boot-test");
    assert_eq!(info.game_port, handle.port());

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&data_dir);
}
