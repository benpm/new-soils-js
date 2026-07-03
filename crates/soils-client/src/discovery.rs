//! LAN server discovery for the login screen.
//!
//! A dedicated OS thread (mirroring the `net.rs` bridge) periodically UDP-
//! broadcasts a probe and collects replies, feeding them over a crossbeam
//! channel into the [`DiscoveredServers`] resource. A Bevy system drains the
//! channel each frame, deduping by address and expiring stale entries.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use soils_protocol::{DISCOVERY_PORT, PROBE_MAGIC, ServerInfo, decode};

/// How often a probe is broadcast.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// How long to listen for replies after each probe.
const LISTEN_WINDOW: Duration = Duration::from_millis(1000);
/// Drop a server from the list if it hasn't replied within this long.
const EXPIRY: Duration = Duration::from_secs(10);

/// A server that answered a discovery probe.
#[derive(Clone)]
pub struct DiscoveredServer {
    /// `ip:game_port` — what the client dials (the IP comes from the reply's
    /// source, the port from [`ServerInfo`]).
    pub addr: SocketAddr,
    pub name: String,
    pub players: u16,
    pub last_seen: Instant,
}

/// Live list of LAN servers, refreshed by [`discovery_poll`].
#[derive(Resource)]
pub struct DiscoveredServers {
    rx: Receiver<DiscoveredServer>,
    pub list: Vec<DiscoveredServer>,
}

/// Spawn the discovery thread and return the ECS-side resource.
pub fn spawn() -> DiscoveredServers {
    let (tx, rx): (Sender<DiscoveredServer>, Receiver<DiscoveredServer>) = unbounded();

    std::thread::Builder::new()
        .name("soils-discovery".into())
        .spawn(move || run(tx))
        .expect("spawn discovery thread");

    DiscoveredServers { rx, list: Vec::new() }
}

/// Broadcast probes forever, forwarding each reply over `tx`. Exits if the
/// socket can't be set up or the receiver is dropped.
fn run(tx: Sender<DiscoveredServer>) {
    let sock = match UdpSocket::bind(("0.0.0.0", 0)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("LAN discovery disabled (UDP bind failed): {e}");
            return;
        }
    };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));
    let target = (std::net::Ipv4Addr::BROADCAST, DISCOVERY_PORT);

    let mut buf = [0u8; 256];
    loop {
        let _ = sock.send_to(PROBE_MAGIC, target);

        // Collect replies for a short window.
        let window_start = Instant::now();
        while window_start.elapsed() < LISTEN_WINDOW {
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(_) => continue, // read timeout (no reply this slice)
            };
            if n < PROBE_MAGIC.len() || &buf[..PROBE_MAGIC.len()] != PROBE_MAGIC {
                continue;
            }
            let Some(info) = decode::<ServerInfo>(&buf[PROBE_MAGIC.len()..n]) else {
                continue;
            };
            let addr = SocketAddr::new(src.ip(), info.game_port);
            if tx
                .send(DiscoveredServer {
                    addr,
                    name: info.name,
                    players: info.players,
                    last_seen: Instant::now(),
                })
                .is_err()
            {
                return; // ECS side gone.
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Drain replies into the resource list each frame: dedupe by address (updating
/// the existing entry in place) and drop entries not seen recently.
///
/// To keep `is_changed()` meaningful for the UI system, the resource is only
/// mutably touched when there is actually new data or an expiry — reads above go
/// through immutable `Deref`, which doesn't trip change detection.
pub fn discovery_poll(mut servers: ResMut<DiscoveredServers>) {
    let incoming: Vec<DiscoveredServer> = servers.rx.try_iter().collect();
    let has_expired = servers.list.iter().any(|s| s.last_seen.elapsed() >= EXPIRY);
    if incoming.is_empty() && !has_expired {
        return;
    }

    for found in incoming {
        if let Some(existing) = servers.list.iter_mut().find(|s| s.addr == found.addr) {
            existing.name = found.name;
            existing.players = found.players;
            existing.last_seen = found.last_seen;
        } else {
            servers.list.push(found);
        }
    }
    servers.list.retain(|s| s.last_seen.elapsed() < EXPIRY);
}
