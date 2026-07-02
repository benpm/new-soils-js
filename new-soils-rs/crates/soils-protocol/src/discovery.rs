//! LAN server discovery, kept separate from the game wire protocol.
//!
//! Discovery is a tiny UDP request/response: a client broadcasts the
//! [`PROBE_MAGIC`] bytes to [`DISCOVERY_PORT`], and every server on the subnet
//! replies (unicast, to the probe's source) with `PROBE_MAGIC` followed by a
//! bincode-encoded [`ServerInfo`]. The client learns the server's IP from the
//! reply's source address and the game port from `ServerInfo`.

use serde::{Deserialize, Serialize};

/// UDP port servers listen on for discovery probes.
pub const DISCOVERY_PORT: u16 = 9002;

/// Magic bytes that prefix both the probe and every reply, so unrelated UDP
/// traffic on the port is ignored. The trailing digit is a format version.
pub const PROBE_MAGIC: &[u8] = b"SOILSdisco1";

/// What a server advertises in a discovery reply. The client pairs this with
/// the reply's source IP to form the address it dials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Human-readable server name shown in the list.
    pub name: String,
    /// TCP port the game WebSocket listens on.
    pub game_port: u16,
    /// Number of currently connected players.
    pub players: u16,
}
