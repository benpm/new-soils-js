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
use soils_server::{Chamber, ServerConfig, ServerHandle};

/// Fixed local account name. Sent with `signup: true`, which the server treats
/// as a login when the credentials already exist, so it works on every run.
pub const LOCAL_NAME: &str = "Player";

/// A world the demo tests build, offered in single-player so it can be walked
/// around by hand.
///
/// These scenes only existed inside `#[ignore]`d recording tests, which meant
/// the only way to see one was to film it. They are ordinary `ServerConfig`
/// tweaks, so exposing them costs nothing and makes "does the lighting
/// actually work in the lamp room" a question you can answer by looking
/// instead of by reading a video.
pub struct Demo {
    /// Stable id: the button's identity and its data directory suffix, so each
    /// demo persists separately and none of them touch the real save.
    pub id: &'static str,
    pub label: &'static str,
    /// What the scene is for, shown under the button.
    pub blurb: &'static str,
    apply: fn(&mut ServerConfig),
}

/// Every demo world, in menu order.
///
/// Adding one is a row here plus whatever `ServerConfig` field it needs; the
/// login screen builds its buttons from this table.
pub const DEMOS: &[Demo] = &[
    Demo {
        id: "lamp-room",
        label: "Lamp room",
        blurb: "A sealed hall ~100 voxels down. Bring lamps.",
        apply: |c| c.chamber = Some(Chamber::DEMO),
    },
    Demo {
        id: "prop-pile",
        label: "Prop pile",
        blurb: "300 physics props dropped at spawn.",
        apply: |c| {
            c.physics = true;
            c.props = 300;
        },
    },
    Demo {
        id: "critters",
        label: "Critters",
        blurb: "Wandering test critters that path toward you.",
        apply: |c| c.critters = 8,
    },
];

/// The demo named by `SOILS_DEMO`, for scripted runs and screenshots.
pub fn from_env() -> Option<&'static Demo> {
    let want = std::env::var("SOILS_DEMO").ok()?;
    let found = DEMOS.iter().find(|d| d.id == want);
    if found.is_none() {
        let ids: Vec<&str> = DEMOS.iter().map(|d| d.id).collect();
        error!("SOILS_DEMO: no demo world {want:?} (have {ids:?})");
    }
    found
}

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
            physics: std::env::var("SOILS_PHYSICS").is_ok_and(|v| v != "0"),
            ..ServerConfig::default()
        })
    }

    /// Start (or reuse) an embedded server running `demo`'s scene.
    ///
    /// Its own data directory, for two reasons: a demo must not scribble on
    /// the real single-player save, and the chamber in particular is carved
    /// only as chunks are *generated* — pointed at a directory that already
    /// has terrain in it, the room would silently not be there.
    pub fn ensure_started_demo(&mut self, demo: &Demo) -> Result<u16, String> {
        let mut config = ServerConfig {
            bind: "0.0.0.0:0".into(),
            data_dir: std::path::PathBuf::from(format!("data/demo-{}", demo.id)),
            enable_discovery: false,
            name: format!("demo-{}", demo.id),
            physics: std::env::var("SOILS_PHYSICS").is_ok_and(|v| v != "0"),
            ..ServerConfig::default()
        };
        (demo.apply)(&mut config);
        self.ensure_started_with(config)
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
