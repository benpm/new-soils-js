//! The client's half of SpacetimeDB: the lobby and chat.
//!
//! Strictly optional and strictly non-blocking. Single-player and offline LAN
//! play must never need a database, and a database that is down or slow must
//! never stand between a player and the game — so every accessor here returns
//! an empty list rather than an error, and nothing waits on a connection.
//!
//! What it reads is the *lobby* half of the schema: live servers, known worlds,
//! and chat. Not `account` (verifiers are none of a client's business) and not
//! `chunk_blob` (which would stream the stored world into a player's memory).
//!
//! Enabled by `SOILS_STDB_URI`, the same variable the server uses, so pointing
//! both at one database is a single setting.

use bevy::prelude::*;

/// Chat lines kept for display.
const CHAT_BACKLOG: usize = 64;

/// How often the lobby view is refreshed from the local cache. The cache is
/// updated by the link's own thread; this only decides how often the UI reads
/// it, and a server registry that moves on a 5 s heartbeat does not reward
/// looking more often than this.
const REFRESH_SECS: f32 = 1.0;

/// A live game server, flattened for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerEntry {
    pub name: String,
    pub addr: String,
    pub players: u32,
}

/// One chat line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatLine {
    pub sender: String,
    pub text: String,
}

#[derive(Resource, Default)]
pub struct Social {
    link: Option<std::sync::Arc<soils_stdb::StdbLink>>,
    /// Snapshot of the lobby, refreshed on a timer so UI systems can read it
    /// without touching the link every frame.
    pub servers: Vec<ServerEntry>,
    pub worlds: Vec<String>,
    pub chat: Vec<ChatLine>,
    /// The world whose chat is currently shown.
    pub chat_world: u16,
    since_refresh: f32,
    /// Set once the identity has been handed to the game server, so it is sent
    /// exactly once per session.
    linked: bool,
}

impl Social {
    /// Whether a database is configured at all.
    pub fn enabled(&self) -> bool {
        self.link.is_some()
    }

    /// Whether the connection is up and its first snapshot has arrived.
    pub fn ready(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.accounts_ready())
    }

    /// This client's SpacetimeDB identity, once the handshake completes.
    pub fn identity(&self) -> Option<[u8; 32]> {
        Some(self.link.as_ref()?.identity()?.to_byte_array())
    }

    /// Post a chat line as this client. Silently does nothing without a
    /// connection — chat is a nicety, not a failure the player needs told
    /// about mid-game.
    pub fn say(&self, world_id: u16, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(link) = &self.link {
            let _ = link.send(soils_stdb::StdbCmd::SendChat {
                world_id,
                text: text.to_string(),
            });
        }
    }
}

/// Connect if `SOILS_STDB_URI` is set. Returns an inert `Social` otherwise.
pub fn configured() -> Social {
    let Ok(uri) = std::env::var("SOILS_STDB_URI") else { return Social::default() };
    if uri.is_empty() {
        return Social::default();
    }
    let database = std::env::var("SOILS_STDB_DB").unwrap_or_else(|_| "soils".into());
    let token = std::env::var("SOILS_STDB_TOKEN").ok().filter(|t| !t.is_empty());
    info!("social: connecting to {uri} / {database}");
    Social {
        link: Some(std::sync::Arc::new(soils_stdb::StdbLink::connect_with(
            &uri,
            &database,
            token,
            soils_stdb::CLIENT_SUBSCRIPTIONS,
        ))),
        ..Social::default()
    }
}

/// Refresh the lobby snapshot, and drain link events so failures are visible.
pub fn refresh(time: Res<Time>, mut social: ResMut<Social>) {
    social.since_refresh += time.delta_secs();
    if social.since_refresh < REFRESH_SECS {
        return;
    }
    social.since_refresh = 0.0;

    let Some(link) = social.link.clone() else { return };
    for event in link.drain() {
        match event {
            soils_stdb::StdbEvent::Connected(id) => info!("social: connected as {id}"),
            soils_stdb::StdbEvent::ConnectError(e) => {
                // Not fatal, and not worth a popup: the game is playable
                // without a lobby.
                warn!("social: could not connect ({e}); lobby and chat are unavailable")
            }
            soils_stdb::StdbEvent::Disconnected(e) => warn!("social: disconnected ({e:?})"),
            soils_stdb::StdbEvent::ReducerFailed { reducer, error } => {
                warn!("social: {reducer} failed: {error}")
            }
        }
    }

    social.servers = link
        .servers()
        .into_iter()
        .map(|s| ServerEntry { name: s.name, addr: s.addr, players: s.players })
        .collect();
    social.worlds = link.worlds().into_iter().map(|w| w.name).collect();

    let world = social.chat_world;
    social.chat = link
        .chat(world, CHAT_BACKLOG)
        .into_iter()
        .map(|m| ChatLine {
            // Identities are long; the tail is enough to tell speakers apart
            // until the name lookup lands.
            sender: {
                let s = m.sender.to_string();
                s.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect()
            },
            text: m.text,
        })
        .collect();
}

/// Hand this client's identity to the game server once, after login, so the
/// server can bind it to the authenticated account.
pub fn link_identity(net: Res<crate::net::NetClient>, mut social: ResMut<Social>) {
    if social.linked || !social.enabled() {
        return;
    }
    let Some(identity) = social.identity() else { return };
    net.send(soils_protocol::ClientMsg::LinkIdentity { identity });
    social.linked = true;
    info!("social: sent identity to the game server for account linking");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_social_with_no_database_is_inert() {
        // Every accessor has to be safe to call with no connection: the lobby
        // is optional, and offline play must not depend on it.
        let s = Social::default();
        assert!(!s.enabled());
        assert!(!s.ready());
        assert!(s.identity().is_none());
        s.say(1, "hello"); // must not panic
        assert!(s.servers.is_empty() && s.worlds.is_empty() && s.chat.is_empty());
    }

    #[test]
    fn blank_uri_is_treated_as_unset() {
        // An empty variable is a common way to "turn it off"; it must not be
        // read as a URI and produce a connection attempt to nowhere.
        unsafe { std::env::set_var("SOILS_STDB_URI", "") };
        assert!(!configured().enabled());
        unsafe { std::env::remove_var("SOILS_STDB_URI") };
        assert!(!configured().enabled());
    }
}
