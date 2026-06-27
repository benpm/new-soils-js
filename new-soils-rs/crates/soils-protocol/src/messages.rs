//! Wire protocol shared by client and server.
//!
//! This is a clean Rust/bincode protocol rather than a port of the JS
//! `schemapack` format — both ends are rewritten, so there is no need to keep
//! the JS-specific encoding. The *logical* message set mirrors `server.js`.
//!
//! Each message is bincode-encoded and sent as a single binary WebSocket frame.

use serde::{Deserialize, Serialize};

/// Bincode configuration used on both ends. Standard little-endian, variable
/// int encoding.
pub fn config() -> bincode::config::Configuration {
    bincode::config::standard()
}

/// Encode a message to bytes for transmission.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    bincode::serde::encode_to_vec(msg, config()).expect("bincode encode")
}

/// Decode a message from received bytes.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Option<T> {
    bincode::serde::decode_from_slice(bytes, config()).ok().map(|(v, _)| v)
}

/// Messages sent client → server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Join with a display name; server replies with `Init`.
    Login { name: String },
    /// Request a batch of chunks by chunk coordinate.
    ReqChunks { positions: Vec<[i32; 3]> },
    /// Player movement update (absolute voxel-space position + velocity).
    Move { pos: [f32; 3], velocity: [f32; 3] },
    /// Set a voxel at an absolute voxel position.
    Edit { pos: [i32; 3], value: u8 },
}

/// Messages sent server → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Sent once after `Login` with spawn + world info.
    Init { id: u16, spawn: [f32; 3], seed: i64, daytime: f32 },
    /// A chunk's voxel data. `voxels` is empty for an all-Air chunk.
    Chunk { pos: [i32; 3], empty: bool, voxels: Vec<u8> },
    /// Several chunks in one frame (response to `ReqChunks`), to cut per-message
    /// overhead when streaming a region. Mirrors the JS `bundle` message.
    Bundle { chunks: Vec<ChunkData> },
    /// A voxel edit made by another player (apply locally).
    Edit { pos: [i32; 3], value: u8 },
    /// Positions of nearby actors (other players).
    ActorUpdate { actors: Vec<ActorState> },
    /// An actor left view / disconnected.
    ActorRemove { id: u16 },
    /// Current world time of day, 0.0..1.0.
    Time { daytime: f32 },
}

/// One chunk's data within a [`ServerMsg::Bundle`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkData {
    pub pos: [i32; 3],
    pub empty: bool,
    pub voxels: Vec<u8>,
}

/// A single actor's networked state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorState {
    pub id: u16,
    pub pos: [f32; 3],
    pub velocity: [f32; 3],
}
