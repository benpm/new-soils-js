//! SpacetimeDB module for new-soils: the cold, relational, persistent and
//! social half of the game state.
//!
//! Authority stays in `soils-server`. Players never write voxel or position
//! state here — every world-mutating reducer requires the caller to be a
//! registered game server (see [`require_server`]). The module is a durable,
//! queryable store plus a lobby/social layer, not a game simulation.
//!
//! Terrain is bit-exact reproducible on the client from `GenParams`, so only
//! *edited* chunks are ever stored. Edits arrive as a fine-grained journal
//! (`chunk_edit`) and are periodically coalesced into a `chunk_blob` holding
//! the shipping `soils_protocol::chunk_codec` payload — the same
//! journal-then-compact shape the region files already use.

use soils_protocol::chunk_key;
use spacetimedb::{
    Identity, ReducerContext, SpacetimeType, ScheduleAt, Table, Timestamp, reducer, table,
};
use std::time::Duration;

/// Upper bound on edits applied in one [`submit_edits`] call. A reducer that
/// exhausts its fuel budget has its **entire transaction rolled back**, so the
/// server must chunk large flushes rather than risk losing all of them.
///
/// Provisional: the A0 spike measures the real ceiling and re-pins this.
pub const MAX_EDITS_PER_CALL: usize = 4096;

/// Longest accepted chat message, in bytes.
pub const MAX_CHAT_LEN: usize = 512;

/// Minimum gap between two chat messages from one account.
pub const CHAT_COOLDOWN: Duration = Duration::from_millis(500);

/// A `game_server` or `presence` row older than this is considered dead and
/// reaped. Must comfortably exceed the server's heartbeat period.
pub const LIVENESS_TTL: Duration = Duration::from_secs(30);

/// How often [`reap_stale`] runs.
pub const REAP_INTERVAL: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// A player account, keyed by SpacetimeDB Identity. This replaces the
/// `DefaultHasher`-and-fixed-salt scheme in `soils-server/src/auth.rs`, which
/// is self-documented as not production-grade.
#[table(accessor = account, public)]
pub struct Account {
    #[primary_key]
    pub identity: Identity,
    #[unique]
    pub name: String,
    pub created_at: Timestamp,
    /// Drives the [`send_chat`] cooldown.
    pub last_chat_at: Timestamp,
}

/// A named world. `seed`/`world_type`/`graph_hash` mirror
/// `soils_protocol::GenParams` so a client can reproduce terrain locally.
#[table(accessor = world, public)]
pub struct World {
    /// Chosen by the *server* as a stable hash of `name` (see
    /// `soils_protocol::chunk_key::world_id_for`) rather than auto-assigned,
    /// so the server can pack chunk keys immediately on startup without a
    /// round-trip. [`upsert_world`] rejects an id already held by a different
    /// name, turning the (unlikely) 16-bit collision into a hard error instead
    /// of two worlds silently sharing chunk storage.
    #[primary_key]
    pub world_id: u16,
    #[unique]
    pub name: String,
    pub seed: i64,
    pub world_type: u8,
    pub graph_hash: u64,
    pub daytime: f32,
    pub created_at: Timestamp,
}

/// One voxel edit. Append-only; rows are pruned once subsumed by a
/// `chunk_blob` (see [`prune_edits`]). Deliberately small so a burst of edits
/// is cheap to commit and cheap to push.
#[table(accessor = chunk_edit, public)]
pub struct ChunkEdit {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// [`chunk_key::pack_chunk_key`] of `(world_id, cx, cy, cz)`.
    #[index(btree)]
    pub chunk_key: u64,
    /// `soils_protocol::voxel_index` of the cell within its 32³ chunk.
    pub voxel: u16,
    pub value: u8,
    pub tick: u64,
    pub by: Identity,
}

/// Coalesced chunk contents. `payload` is exactly what
/// `soils_protocol::chunk_codec::encode_chunk` produces, so the server can
/// serve it without transcoding.
///
/// Payloads above 992 bytes land in SpacetimeDB's BLAKE3 content-addressed,
/// refcounted blob store, so identical chunks (all-air, all-stone) dedupe.
#[table(accessor = chunk_blob, public)]
pub struct ChunkBlob {
    #[primary_key]
    pub chunk_key: u64,
    /// Denormalised from `chunk_key` so range queries stay expressible in the
    /// subscription SQL subset, which cannot call functions.
    #[index(btree)]
    pub world_id: u16,
    pub cx: i32,
    pub cy: i32,
    pub cz: i32,
    pub payload: Vec<u8>,
    /// Mirrors the server's per-chunk edit version.
    pub version: u32,
    /// Highest `chunk_edit.id` folded into `payload`; the reconcile path
    /// replays only journal rows above this.
    pub edits_through: u64,
    pub updated_at: Timestamp,
}

/// Where a player was when they last logged out.
#[table(accessor = player_profile, public)]
pub struct PlayerProfile {
    #[primary_key]
    pub identity: Identity,
    pub world_id: u16,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub yaw: f32,
    pub view_radius: u8,
    pub last_seen: Timestamp,
}

/// Who is online, and on which game server.
#[table(accessor = presence, public)]
pub struct Presence {
    #[primary_key]
    pub identity: Identity,
    pub world_id: u16,
    #[index(btree)]
    pub server_id: u32,
    pub connected_at: Timestamp,
    pub heartbeat: Timestamp,
}

/// Registry of live game servers. Complements — does not replace — the UDP LAN
/// discovery in `soils_protocol::discovery`, which still serves local play.
#[table(accessor = game_server, public)]
pub struct GameServer {
    #[primary_key]
    pub server_id: u32,
    pub name: String,
    /// Dial string for the hot-path transport, e.g. `ws://host:9001`.
    pub addr: String,
    pub players: u32,
    pub heartbeat: Timestamp,
}

#[table(accessor = chat_message, public)]
pub struct ChatMessage {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub world_id: u16,
    pub sender: Identity,
    pub text: String,
    pub at: Timestamp,
}

/// Allowlist of identities permitted to write world state. Bootstrapped
/// trust-on-first-use by [`grant_server`].
#[table(accessor = server_identity)]
pub struct ServerIdentity {
    #[primary_key]
    pub identity: Identity,
    pub granted_at: Timestamp,
}

#[table(accessor = reap_timer, scheduled(reap_stale))]
pub struct ReapTimer {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

/// One entry of a [`submit_edits`] batch.
#[derive(SpacetimeType, Clone, Copy)]
pub struct PackedEdit {
    pub chunk_key: u64,
    pub voxel: u16,
    pub value: u8,
}

// ---------------------------------------------------------------------------
// Authority
// ---------------------------------------------------------------------------

/// Reject callers that are not a registered game server.
fn require_server(ctx: &ReducerContext) -> Result<(), String> {
    if ctx.db.server_identity().identity().find(ctx.sender()).is_some() {
        Ok(())
    } else {
        Err("not authorized: caller is not a registered game server".to_string())
    }
}

/// Add a game-server identity to the allowlist.
///
/// Bootstrap is trust-on-first-use: while the allowlist is empty, any caller
/// may claim it. Afterwards only an existing server may grant another. For a
/// public deployment, seed the first entry from a trusted console before
/// exposing the database.
#[reducer]
pub fn grant_server(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    let bootstrapping = ctx.db.server_identity().count() == 0;
    if !bootstrapping {
        require_server(ctx)?;
    }
    if ctx.db.server_identity().identity().find(identity).is_some() {
        return Ok(());
    }
    ctx.db
        .server_identity()
        .try_insert(ServerIdentity { identity, granted_at: ctx.timestamp })
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[reducer(init)]
pub fn init(ctx: &ReducerContext) -> Result<(), String> {
    ctx.db
        .reap_timer()
        .try_insert(ReapTimer {
            scheduled_id: 0,
            scheduled_at: ScheduleAt::Interval(REAP_INTERVAL.into()),
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[reducer(client_disconnected)]
pub fn on_disconnect(ctx: &ReducerContext) -> Result<(), String> {
    ctx.db.presence().identity().delete(ctx.sender());
    Ok(())
}

/// Drop game servers and presences whose heartbeat has gone stale, so a
/// crashed server does not linger in the browser forever.
#[reducer]
pub fn reap_stale(ctx: &ReducerContext, _timer: ReapTimer) -> Result<(), String> {
    let cutoff = ctx.timestamp - LIVENESS_TTL;

    let dead: Vec<u32> = ctx
        .db
        .game_server()
        .iter()
        .filter(|s| s.heartbeat < cutoff)
        .map(|s| s.server_id)
        .collect();
    for server_id in dead {
        ctx.db.game_server().server_id().delete(server_id);
        let orphaned: Vec<Identity> =
            ctx.db.presence().server_id().filter(server_id).map(|p| p.identity).collect();
        for identity in orphaned {
            ctx.db.presence().identity().delete(identity);
        }
    }

    let stale: Vec<Identity> = ctx
        .db
        .presence()
        .iter()
        .filter(|p| p.heartbeat < cutoff)
        .map(|p| p.identity)
        .collect();
    for identity in stale {
        ctx.db.presence().identity().delete(identity);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Accounts (player-callable)
// ---------------------------------------------------------------------------

/// Claim a display name for the calling identity. Idempotent for the owner;
/// fails if another identity already holds the name.
#[reducer]
pub fn register_account(ctx: &ReducerContext, name: String) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 32 {
        return Err("name must be 1..=32 bytes".to_string());
    }
    if let Some(existing) = ctx.db.account().name().find(&name) {
        return if existing.identity == ctx.sender() {
            Ok(())
        } else {
            Err("name already taken".to_string())
        };
    }
    if let Some(mut account) = ctx.db.account().identity().find(ctx.sender()) {
        account.name = name;
        ctx.db.account().identity().update(account);
        return Ok(());
    }
    ctx.db
        .account()
        .try_insert(Account {
            identity: ctx.sender(),
            name,
            created_at: ctx.timestamp,
            // Epoch-ish: no cooldown owed on a fresh account.
            last_chat_at: Timestamp::UNIX_EPOCH,
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[reducer]
pub fn send_chat(ctx: &ReducerContext, world_id: u16, text: String) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() || text.len() > MAX_CHAT_LEN {
        return Err(format!("message must be 1..={MAX_CHAT_LEN} bytes"));
    }
    let mut account = ctx
        .db
        .account()
        .identity()
        .find(ctx.sender())
        .ok_or_else(|| "no account: call register_account first".to_string())?;

    if ctx.timestamp < account.last_chat_at + CHAT_COOLDOWN {
        return Err("slow down".to_string());
    }
    account.last_chat_at = ctx.timestamp;
    ctx.db.account().identity().update(account);

    ctx.db
        .chat_message()
        .try_insert(ChatMessage {
            id: 0,
            world_id,
            sender: ctx.sender(),
            text,
            at: ctx.timestamp,
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// World state (server-only)
// ---------------------------------------------------------------------------

/// Create a world if absent, otherwise refresh its mutable fields. Returns the
/// `world_id` the server should use when packing chunk keys.
#[reducer]
pub fn upsert_world(
    ctx: &ReducerContext,
    world_id: u16,
    name: String,
    seed: i64,
    world_type: u8,
    graph_hash: u64,
    daytime: f32,
) -> Result<(), String> {
    require_server(ctx)?;
    if let Some(mut world) = ctx.db.world().world_id().find(world_id) {
        // Two names hashing to one id would share chunk storage. Refuse.
        if world.name != name {
            return Err(format!(
                "world_id {world_id} collision: already held by '{}', refused for '{name}'",
                world.name
            ));
        }
        // Generator identity changing under a live world would invalidate every
        // stored chunk, so refuse rather than silently corrupt it.
        if world.seed != seed || world.world_type != world_type || world.graph_hash != graph_hash {
            return Err(format!(
                "world '{name}' exists with a different generator; \
                 bump the world name or clear its chunks"
            ));
        }
        world.daytime = daytime;
        ctx.db.world().world_id().update(world);
        return Ok(());
    }
    if ctx.db.world().name().find(&name).is_some() {
        return Err(format!("world '{name}' already exists under a different world_id"));
    }
    ctx.db
        .world()
        .try_insert(World {
            world_id,
            name,
            seed,
            world_type,
            graph_hash,
            daytime,
            created_at: ctx.timestamp,
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Append a batch of voxel edits to the journal.
#[reducer]
pub fn submit_edits(ctx: &ReducerContext, tick: u64, edits: Vec<PackedEdit>) -> Result<(), String> {
    require_server(ctx)?;
    if edits.len() > MAX_EDITS_PER_CALL {
        return Err(format!(
            "batch of {} exceeds MAX_EDITS_PER_CALL ({MAX_EDITS_PER_CALL})",
            edits.len()
        ));
    }
    for edit in &edits {
        if edit.voxel as usize >= soils_protocol::CHUNK_CUBED {
            return Err(format!("voxel index {} out of range", edit.voxel));
        }
    }
    let sender = ctx.sender();
    for edit in edits {
        ctx.db
            .chunk_edit()
            .try_insert(ChunkEdit {
                id: 0,
                chunk_key: edit.chunk_key,
                voxel: edit.voxel,
                value: edit.value,
                tick,
                by: sender,
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Write the coalesced contents of one chunk.
///
/// `payload` must be a `soils_protocol::chunk_codec` payload; it is decoded
/// here purely as validation, so a malformed blob can never enter the store.
#[reducer]
pub fn put_chunk_blob(
    ctx: &ReducerContext,
    key: u64,
    payload: Vec<u8>,
    version: u32,
    edits_through: u64,
) -> Result<(), String> {
    require_server(ctx)?;
    let (world_id, cx, cy, cz) = chunk_key::unpack_chunk_key(key);
    if ctx.db.world().world_id().find(world_id).is_none() {
        return Err(format!("no such world_id {world_id}"));
    }
    if soils_protocol::decode_chunk(&payload).is_none() {
        return Err("payload is not a valid chunk_codec blob".to_string());
    }

    if let Some(mut blob) = ctx.db.chunk_blob().chunk_key().find(key) {
        // Late or duplicated flushes must not roll the chunk backwards.
        if version < blob.version {
            return Err(format!(
                "stale write: incoming version {version} < stored {}",
                blob.version
            ));
        }
        blob.payload = payload;
        blob.version = version;
        blob.edits_through = edits_through;
        blob.updated_at = ctx.timestamp;
        ctx.db.chunk_blob().chunk_key().update(blob);
        return Ok(());
    }
    ctx.db
        .chunk_blob()
        .try_insert(ChunkBlob {
            chunk_key: key,
            world_id,
            cx,
            cy,
            cz,
            payload,
            version,
            edits_through,
            updated_at: ctx.timestamp,
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Drop journal rows already folded into a chunk's blob. Bounds commit-log
/// growth; the mirror of region-file compaction.
#[reducer]
pub fn prune_edits(ctx: &ReducerContext, key: u64, up_to_id: u64) -> Result<(), String> {
    require_server(ctx)?;
    let doomed: Vec<u64> = ctx
        .db
        .chunk_edit()
        .chunk_key()
        .filter(key)
        .filter(|e| e.id <= up_to_id)
        .map(|e| e.id)
        .collect();
    for id in doomed {
        ctx.db.chunk_edit().id().delete(id);
    }
    Ok(())
}

#[reducer]
pub fn save_profile(
    ctx: &ReducerContext,
    identity: Identity,
    world_id: u16,
    x: f32,
    y: f32,
    z: f32,
    yaw: f32,
    view_radius: u8,
) -> Result<(), String> {
    require_server(ctx)?;
    let profile = PlayerProfile {
        identity,
        world_id,
        x,
        y,
        z,
        yaw,
        view_radius,
        last_seen: ctx.timestamp,
    };
    if ctx.db.player_profile().identity().find(identity).is_some() {
        ctx.db.player_profile().identity().update(profile);
    } else {
        ctx.db.player_profile().try_insert(profile).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Refresh a server's registry row so it stays visible in a server browser and
/// the reaper doesn't drop it.
///
/// Takes a player *count* rather than a list of identities: game players
/// authenticate to the game server with a name/password and have no
/// SpacetimeDB identity until the client connects here directly. Per-identity
/// presence is maintained separately by [`mark_present`] once that exists.
#[reducer]
pub fn heartbeat(
    ctx: &ReducerContext,
    server_id: u32,
    name: String,
    addr: String,
    player_count: u32,
) -> Result<(), String> {
    require_server(ctx)?;
    let row =
        GameServer { server_id, name, addr, players: player_count, heartbeat: ctx.timestamp };
    if ctx.db.game_server().server_id().find(server_id).is_some() {
        ctx.db.game_server().server_id().update(row);
    } else {
        ctx.db.game_server().try_insert(row).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Record that an identity is online on `server_id` in `world_id`.
///
/// Separate from [`heartbeat`] because presence is per-identity and only
/// becomes meaningful once clients authenticate to SpacetimeDB themselves.
#[reducer]
pub fn mark_present(
    ctx: &ReducerContext,
    identity: Identity,
    server_id: u32,
    world_id: u16,
) -> Result<(), String> {
    require_server(ctx)?;
    if let Some(mut presence) = ctx.db.presence().identity().find(identity) {
        presence.world_id = world_id;
        presence.server_id = server_id;
        presence.heartbeat = ctx.timestamp;
        ctx.db.presence().identity().update(presence);
    } else {
        ctx.db
            .presence()
            .try_insert(Presence {
                identity,
                world_id,
                server_id,
                connected_at: ctx.timestamp,
                heartbeat: ctx.timestamp,
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
