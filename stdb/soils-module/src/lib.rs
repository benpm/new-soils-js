//! SpacetimeDB module for new-soils: the cold, relational, persistent and
//! social half of the game state.
//!
//! Authority stays in `soils-server`. Players never write voxel or position
//! state here — every world-mutating reducer requires the caller to be a
//! registered game server (see [`require_server`]). The module is a durable,
//! queryable store plus a lobby/social layer, not a game simulation.
//!
//! Terrain is bit-exact reproducible on the client from `GenParams`, so only
//! *edited* chunks are ever stored, as a `chunk_blob` holding the shipping
//! `soils_protocol::chunk_codec` payload. Region files remain authoritative;
//! this is a mirror written after a successful disk write.

use soils_protocol::chunk_key;
use spacetimedb::{
    Identity, ReducerContext, ScheduleAt, Table, Timestamp, reducer, table,
};
use std::time::Duration;

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

/// A player account, keyed by the name players log in with.
///
/// Keyed by name rather than `Identity` because the credential *is* the name
/// and password: a player logging in from a new machine has a new identity but
/// the same account. `identity` is the link to a client that has authenticated
/// to SpacetimeDB directly, filled in by [`link_identity`].
///
/// `verifier` is a PHC-format password hash produced and checked by the game
/// server. The module never sees a plaintext password and never verifies one —
/// it is a store, and the row is only writable by a registered game server.
#[table(accessor = account, public)]
pub struct Account {
    #[primary_key]
    pub name: String,
    pub verifier: String,
    pub identity: Option<Identity>,
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
    pub updated_at: Timestamp,
}

/// Where a player was when they last logged out.
///
/// Keyed by **account name**, not `Identity`: players authenticate to the game
/// server with a name/password, so the account is what durably identifies them.
/// `identity` is populated only once a player's own client authenticates to
/// SpacetimeDB directly, and exists to link the two.
#[table(accessor = player_profile, public)]
pub struct PlayerProfile {
    #[primary_key]
    pub account: String,
    pub identity: Option<Identity>,
    pub world_id: u16,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub yaw: f32,
    pub view_radius: u8,
    pub last_seen: Timestamp,
}

/// Who is online, and on which game server. Keyed by account name for the same
/// reason as [`PlayerProfile`].
#[table(accessor = presence, public)]
pub struct Presence {
    #[primary_key]
    pub account: String,
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
    /// The speaker's account name, denormalised at write time.
    ///
    /// Clients do not subscribe to `account` — it holds password verifiers —
    /// so they have no way to turn an identity into a name. Copying it here is
    /// what lets chat read `<ben>` instead of a truncated identity.
    pub sender_name: String,
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
    // Presence is keyed by account, and a disconnecting *identity* is usually
    // the game server itself rather than a player. Drop only a presence row
    // whose account has been linked to this identity.
    let mine: Vec<String> = ctx
        .db
        .player_profile()
        .iter()
        .filter(|p| p.identity == Some(ctx.sender()))
        .map(|p| p.account)
        .collect();
    for account in mine {
        ctx.db.presence().account().delete(&account);
    }
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
        let orphaned: Vec<String> =
            ctx.db.presence().server_id().filter(server_id).map(|p| p.account).collect();
        for account in orphaned {
            ctx.db.presence().account().delete(&account);
        }
    }

    let stale: Vec<String> =
        ctx.db.presence().iter().filter(|p| p.heartbeat < cutoff).map(|p| p.account).collect();
    for account in stale {
        ctx.db.presence().account().delete(&account);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Accounts (server-only)
// ---------------------------------------------------------------------------

/// Longest accepted account name, in bytes.
pub const MAX_NAME_LEN: usize = 32;

/// Create an account, or migrate one that already exists locally.
///
/// **Server-only.** The module stores a verifier and never sees or checks a
/// password: the game server owns hashing and verification, because it is the
/// only party a client actually proves anything to. Letting players call this
/// would let anyone claim any unclaimed name with a verifier of their choosing.
///
/// Idempotent for an account whose verifier already matches, so a server
/// migrating its local account file can call this for every account without
/// having to know which ones already crossed over.
#[reducer]
pub fn register_account(
    ctx: &ReducerContext,
    name: String,
    verifier: String,
) -> Result<(), String> {
    require_server(ctx)?;
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(format!("name must be 1..={MAX_NAME_LEN} bytes"));
    }
    if verifier.is_empty() {
        return Err("verifier must not be empty".to_string());
    }
    if let Some(existing) = ctx.db.account().name().find(&name) {
        return if existing.verifier == verifier {
            Ok(())
        } else {
            // Changing an existing account's password is a separate operation
            // that has to prove knowledge of the old one; this reducer cannot.
            Err(format!("account '{name}' already exists"))
        };
    }
    ctx.db
        .account()
        .try_insert(Account {
            name,
            verifier,
            identity: None,
            created_at: ctx.timestamp,
            // Epoch: no cooldown owed on a fresh account.
            last_chat_at: Timestamp::UNIX_EPOCH,
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Replace an account's verifier, for a password change the game server has
/// already authorised, or a rehash onto stronger parameters.
#[reducer]
pub fn set_verifier(
    ctx: &ReducerContext,
    name: String,
    verifier: String,
) -> Result<(), String> {
    require_server(ctx)?;
    if verifier.is_empty() {
        return Err("verifier must not be empty".to_string());
    }
    let mut account = ctx
        .db
        .account()
        .name()
        .find(&name)
        .ok_or_else(|| format!("no such account '{name}'"))?;
    account.verifier = verifier;
    ctx.db.account().name().update(account);
    Ok(())
}

#[reducer]
pub fn send_chat(ctx: &ReducerContext, world_id: u16, text: String) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() || text.len() > MAX_CHAT_LEN {
        return Err(format!("message must be 1..={MAX_CHAT_LEN} bytes"));
    }
    // Scanned rather than indexed: `identity` is optional (an account exists
    // before any client links one), and a unique index would make every
    // unlinked account collide on `None`. Chat is low-frequency, so a scan is
    // the cheaper trade than carrying a second table to invert the mapping.
    let mut account = ctx
        .db
        .account()
        .iter()
        .find(|a| a.identity == Some(ctx.sender()))
        .ok_or_else(|| "no account linked to this identity".to_string())?;

    if ctx.timestamp < account.last_chat_at + CHAT_COOLDOWN {
        return Err("slow down".to_string());
    }
    let sender_name = account.name.clone();
    account.last_chat_at = ctx.timestamp;
    ctx.db.account().name().update(account);

    ctx.db
        .chat_message()
        .try_insert(ChatMessage {
            id: 0,
            world_id,
            sender: ctx.sender(),
            sender_name,
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
            updated_at: ctx.timestamp,
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Drop journal rows already folded into a chunk's blob. Bounds commit-log
/// Refresh a server's registry row so it stays visible in a server browser and
/// the reaper doesn't drop it.
///
/// Takes a player *count* rather than a list of identities: game players
/// authenticate to the game server with a name/password and have no
/// SpacetimeDB identity until the client connects here directly. Per-identity
/// presence is maintained separately by [`mark_present`] once that exists.
/// Record where a player was when they logged out, so they resume there.
#[reducer]
pub fn save_profile(
    ctx: &ReducerContext,
    account: String,
    world_id: u16,
    x: f32,
    y: f32,
    z: f32,
    yaw: f32,
    view_radius: u8,
) -> Result<(), String> {
    require_server(ctx)?;
    // Preserve any linked identity across position updates.
    let existing = ctx.db.player_profile().account().find(&account);
    let identity = existing.as_ref().and_then(|p| p.identity);
    let existed = existing.is_some();
    let profile = PlayerProfile {
        account,
        identity,
        world_id,
        x,
        y,
        z,
        yaw,
        view_radius,
        last_seen: ctx.timestamp,
    };
    if existed {
        ctx.db.player_profile().account().update(profile);
    } else {
        ctx.db.player_profile().try_insert(profile).map_err(|e| e.to_string())?;
    }
    Ok(())
}

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

/// Record that `account` is online on `server_id` in `world_id`, refreshing its
/// heartbeat. Separate from [`heartbeat`], which covers the server itself.
///
/// Used at login; [`heartbeat_presence`] keeps the row alive after that. A row
/// inserted here and never refreshed is deleted by the reaper once it passes
/// `LIVENESS_TTL`, even though the player is still connected.
#[reducer]
pub fn mark_present(
    ctx: &ReducerContext,
    account: String,
    server_id: u32,
    world_id: u16,
) -> Result<(), String> {
    require_server(ctx)?;
    if let Some(mut presence) = ctx.db.presence().account().find(&account) {
        presence.world_id = world_id;
        presence.server_id = server_id;
        presence.heartbeat = ctx.timestamp;
        ctx.db.presence().account().update(presence);
    } else {
        ctx.db
            .presence()
            .try_insert(Presence {
                account,
                world_id,
                server_id,
                connected_at: ctx.timestamp,
                heartbeat: ctx.timestamp,
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Refresh the heartbeat of every account currently online on `server_id`, and
/// drop any presence row this server owns that is no longer among them.
///
/// One transaction for the whole roster rather than a `mark_present` per
/// player: the server calls this every few seconds, so per-player reducers
/// would put a transaction per player per interval on the database for no gain.
///
/// The removal half matters as much as the refresh. A player who leaves without
/// a clean logout would otherwise sit in `presence` until the reaper's
/// `LIVENESS_TTL` expires; here the next roster that omits them takes them out.
#[reducer]
pub fn heartbeat_presence(
    ctx: &ReducerContext,
    server_id: u32,
    world_id: u16,
    accounts: Vec<String>,
) -> Result<(), String> {
    require_server(ctx)?;
    for account in &accounts {
        if let Some(mut presence) = ctx.db.presence().account().find(account) {
            presence.world_id = world_id;
            presence.server_id = server_id;
            presence.heartbeat = ctx.timestamp;
            ctx.db.presence().account().update(presence);
        } else {
            ctx.db
                .presence()
                .try_insert(Presence {
                    account: account.clone(),
                    world_id,
                    server_id,
                    connected_at: ctx.timestamp,
                    heartbeat: ctx.timestamp,
                })
                .map_err(|e| e.to_string())?;
        }
    }
    // Only ever prunes rows belonging to *this* server, so two servers sharing
    // the database cannot evict each other's players.
    let departed: Vec<String> = ctx
        .db
        .presence()
        .server_id()
        .filter(server_id)
        .filter(|p| !accounts.contains(&p.account))
        .map(|p| p.account)
        .collect();
    for account in departed {
        ctx.db.presence().account().delete(&account);
    }
    Ok(())
}

/// Drop `account`'s presence row on a clean disconnect. The reaper covers
/// unclean ones, but only after `LIVENESS_TTL`.
#[reducer]
pub fn mark_absent(ctx: &ReducerContext, account: String) -> Result<(), String> {
    require_server(ctx)?;
    ctx.db.presence().account().delete(&account);
    Ok(())
}

/// Link a SpacetimeDB identity to an account, for when a player's own client
/// authenticates to SpacetimeDB directly (chat, social reads).
///
/// **Server-only, deliberately.** An earlier version let the claiming client
/// call this itself and checked only that any *existing* link matched the
/// sender — which is vacuous for an unlinked account, and unlinked is the
/// normal state for every account the game server creates. Any anonymous
/// client could therefore claim any account by name and inherit its profile
/// link. Nothing in this module can verify account ownership: the name and
/// password live in the game server's `auth.rs`, so only the game server is in
/// a position to assert that a given identity really is a given account.
///
/// The intended flow is that a client tells the game server its SpacetimeDB
/// identity over the game protocol, and the server — which has already
/// authenticated that connection — calls this.
#[reducer]
pub fn link_identity(
    ctx: &ReducerContext,
    account: String,
    identity: Identity,
) -> Result<(), String> {
    require_server(ctx)?;
    let mut row = ctx
        .db
        .account()
        .name()
        .find(&account)
        .ok_or_else(|| format!("no such account '{account}'"))?;
    if let Some(existing) = row.identity
        && existing != identity
    {
        return Err(format!("account '{account}' is already linked to another identity"));
    }
    row.identity = Some(identity);
    ctx.db.account().name().update(row);
    // Mirror onto the profile when there is one, so a profile row alone is
    // enough to answer "whose is this?".
    if let Some(mut profile) = ctx.db.player_profile().account().find(&account) {
        profile.identity = Some(identity);
        ctx.db.player_profile().account().update(profile);
    }
    Ok(())
}
