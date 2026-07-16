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
    /// Authenticate (or, with `signup`, register) and join. The server replies
    /// with `Init` on success or `LoginError` on failure. `password` may be
    /// empty (optional-password accounts).
    Login { name: String, password: String, signup: bool },
    /// The client's chunk view radius. The server owns the subscription set
    /// (which chunks stream in and when they unload); this only sizes it.
    ViewRadius { radius: u8 },
    /// Movement inputs (server authority: the server simulates the player via
    /// `soils-sim`, so positions can't be forged). One frame per client fixed
    /// tick; the last few frames are bundled for loss/ordering robustness on
    /// future unreliable transports, deduped server-side by `seq`. `ack_tick`
    /// piggybacks the snapshot ack: the highest snapshot tick applied — the
    /// server may then use any state sent at or before it as a delta baseline.
    Inputs { ack_tick: u32, frames: Vec<InputFrame> },
    /// Set a voxel at an absolute voxel position. Applied optimistically
    /// client-side; the server answers `EditAccepted`/`EditRejected` by `seq`
    /// and the client rolls back on rejection.
    Edit { seq: u32, pos: [i32; 3], value: u8 },
    /// Switch to a (server-created-on-demand) named world.
    Warp { world: String },
    /// Request a rigid-body physics cube at `pos` (world units). Server-gated
    /// on `SOILS_PHYSICS`, reach-checked against the authoritative player
    /// position, and rate-limited like edits. Spawns a replicated
    /// `KIND_PHYSICS_CUBE` on success.
    SpawnCube { pos: [f32; 3] },
}

/// One fixed tick of movement input (see `soils_sim::pack_input`). `seq`
/// increments per client tick — it doubles as the client tick counter.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputFrame {
    pub seq: u32,
    pub buttons: u8,
    pub flags: u8,
    pub yaw: u16,
}

/// Messages sent server → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Sent once after a successful `Login` with spawn + world info.
    /// `self_entity` is the NetId of this client's own player entity — its
    /// updates drive the local camera rather than spawning a body.
    Init { id: u16, self_entity: u32, spawn: [f32; 3], seed: i64, daytime: f32 },
    /// A failed `Login` (bad password, name taken, etc.).
    LoginError { message: String },
    /// A chunk's voxel data as a [`chunk_codec`](crate::chunk_codec) payload
    /// (palette + LZ4; an all-air chunk is the 2-byte Uniform payload).
    Chunk { pos: [i32; 3], payload: Vec<u8> },
    /// Several chunks in one frame, to cut per-message overhead when streaming
    /// a region. Pushed by the server as the subscription set grows.
    Bundle { chunks: Vec<ChunkData> },
    /// The chunk left the client's subscription (moved out of radius +
    /// hysteresis). The client drops its copy and frees GPU resources.
    ChunkUnload { pos: [i32; 3] },
    /// A voxel edit made by another player (apply locally).
    Edit { pos: [i32; 3], value: u8 },
    /// The server validated and applied the editor's own edit `seq`.
    EditAccepted { seq: u32, pos: [i32; 3], value: u8 },
    /// The server refused edit `seq` (reach, unknown block, rate, unloaded
    /// chunk); the editor must roll its optimistic application back.
    EditRejected { seq: u32 },
    /// An entity entered this client's interest set: create it. Kind ids
    /// index the shared `entities.yaml` registry.
    EntitySpawn { id: u32, kind: u16, pos: [f32; 3] },
    /// An entity left interest (or despawned): drop it.
    EntityDespawn { id: u32 },
    /// Per-tick delta snapshot of entities in interest (see
    /// [`snapshot`](crate::snapshot) for the payload codec). Deltas are
    /// encoded against the receiver's state at `baseline_tick` (its last
    /// acked tick; 0 on fresh joins → FULL records). Includes the receiver's
    /// own player entity; `last_input_seq` is the reconciliation anchor for
    /// it (highest input the server had applied at `tick`).
    Snapshot { tick: u32, baseline_tick: u32, last_input_seq: u32, payload: Vec<u8> },
    /// Current world time of day, 0.0..1.0.
    Time { daytime: f32 },
    /// Confirms a `Warp`: the client should drop all chunks/actors, teleport to
    /// `spawn`, and re-stream the new world.
    Warp { spawn: [f32; 3], daytime: f32 },
}

/// One chunk's data within a [`ServerMsg::Bundle`]. `payload` is a
/// [`chunk_codec`](crate::chunk_codec) encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkData {
    pub pos: [i32; 3],
    pub payload: Vec<u8>,
}

/// One entity's replicated state (full-state form; the delta pipeline of a
/// later phase replaces this on the wire). `yaw` is a u16 turn fraction
/// (`soils_sim::pack_input` convention). `rot` is a full orientation quaternion
/// `[x, y, z, w]`, identity for entities that only use `yaw` (players,
/// critters) and the real body orientation for rigid-body physics entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityState {
    pub id: u32,
    pub pos: [f32; 3],
    pub velocity: [f32; 3],
    pub yaw: u16,
    pub rot: [f32; 4],
    /// Angular velocity (rad/s), zero for yaw-only entities. Lets the client
    /// predict a physics body's spin between snapshots.
    pub angvel: [f32; 3],
}
