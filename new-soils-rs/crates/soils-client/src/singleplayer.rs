//! Single-player: an embedded server instance running inside the client
//! process. The full networking/auth/streaming path is reused unchanged —
//! single-player is "loopback multiplayer" against a server on an ephemeral
//! localhost port, persisting to its own `data/singleplayer/` directory.

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
        if let Some(h) = &self.handle {
            return Ok(h.port());
        }
        let config = ServerConfig {
            bind: "127.0.0.1:0".into(),
            data_dir: std::path::PathBuf::from("data/singleplayer"),
            enable_discovery: false,
            name: "singleplayer".into(),
        };
        let handle = soils_server::spawn(config).map_err(|e| e.to_string())?;
        info!("embedded single-player server on 127.0.0.1:{}", handle.port());
        self.handle = Some(handle);
        Ok(self.handle.as_ref().unwrap().port())
    }
}
