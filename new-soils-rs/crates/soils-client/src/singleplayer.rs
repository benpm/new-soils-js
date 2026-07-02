//! Single-player: an embedded server instance running inside the client
//! process. The full networking/auth/streaming path is reused unchanged —
//! single-player is "loopback multiplayer" against a server on an ephemeral
//! port, persisting to its own `data/singleplayer/` directory.
//!
//! The server binds all interfaces (the client itself always dials loopback)
//! so that the LAN discovery toggle in the pause menu can actually invite LAN
//! peers in: discovered servers are dialed at the host's LAN IP. Discovery is
//! OFF by default — the world is unadvertised (ephemeral port, no UDP
//! presence), though like any server here, connections just need a login.

use bevy::prelude::*;
use soils_server::{ServerConfig, ServerHandle};

/// Fixed local account name. Sent with `signup: true`, which the server treats
/// as a login when the credentials already exist, so it works on every run.
pub const LOCAL_NAME: &str = "Player";

#[derive(Resource, Default)]
pub struct Singleplayer {
    handle: Option<ServerHandle>,
}

impl Singleplayer {
    /// Start the embedded server, or reuse the running one (re-clicks after a
    /// failed connect must not spawn a second instance). Returns the loopback
    /// port to dial.
    pub fn ensure_started(&mut self) -> Result<u16, String> {
        self.ensure_started_with(ServerConfig {
            bind: "0.0.0.0:0".into(),
            data_dir: std::path::PathBuf::from("data/singleplayer"),
            enable_discovery: false,
            name: "singleplayer".into(),
            ..ServerConfig::default()
        })
    }

    /// [`ensure_started`](Self::ensure_started) with an explicit config; split
    /// out so tests can inject a temp data dir and an ephemeral discovery port.
    pub fn ensure_started_with(&mut self, config: ServerConfig) -> Result<u16, String> {
        if let Some(h) = &self.handle {
            return Ok(h.port());
        }
        let handle = soils_server::spawn(config).map_err(|e| e.to_string())?;
        info!("embedded single-player server on port {}", handle.port());
        self.handle = Some(handle);
        Ok(self.handle.as_ref().unwrap().port())
    }

    /// Whether an embedded server is running (i.e. this is a single-player
    /// session, so the pause menu should show the LAN discovery toggle).
    pub fn is_running(&self) -> bool {
        self.handle.is_some()
    }

    /// Flip LAN discovery on/off. No-op outside single-player.
    pub fn toggle_discovery(&mut self) {
        if let Some(h) = &self.handle {
            h.set_discovery(!h.discovery_enabled());
        }
    }

    /// `(desired_on, actual_udp_port)` for the pause-menu label, or `None`
    /// when no embedded server is running. The port is `None` while discovery
    /// is off, still binding, or failed to bind.
    pub fn discovery_status(&self) -> Option<(bool, Option<u16>)> {
        self.handle.as_ref().map(|h| (h.discovery_enabled(), h.discovery_port()))
    }
}
