//! Server-message routing. One thin system drains the network bridge and fans
//! the decoded [`ServerMsg`]s out as typed Bevy messages; small consumer
//! systems apply each type to the ECS. Replaces the old single `net_receive`
//! god-system, so new message types grow a new consumer instead of one giant
//! match.
//!
//! Cross-type ordering within a frame is lost by the split, which matters only
//! for `Warp`: a chunk bundle from the *old* world can share a drain with the
//! `Warp` that despawns that world. [`WorldEpoch`] restores the ordering —
//! chunk/edit messages are stamped with the epoch current when they were
//! routed, the epoch bumps when a `Warp` routes, and consumers drop stale
//! stamps.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;
use soils_protocol::{ChunkVolume, EntityState, GenParams, ServerMsg, SnapshotTracker};
use soils_worldgen::{TerrainGen, WorldType};

use crate::actor::{Actor, ActorAssets, ActorMap, LocalPlayer};
use crate::chunk::{ChunkMap, VoxelChunk, WorldTime};
use crate::demand::{self, ChunkDirectory, DemandProcessor, DirMsg};
use crate::edit;
use crate::light::LightQueue;
use crate::login::LoginState;
use crate::net::{NetClient, NetEvent};
use crate::player::{self, Player, Streaming};
use crate::pool::{ChunkSlots, DirtyMesh, PoolOpQueue};

/// Bumps every time a `Warp` is routed; chunk/edit messages carry the epoch
/// they were routed under so consumers can drop leftovers from the old world.
#[derive(Resource, Default)]
pub struct WorldEpoch(pub u32);

/// Client-side chunk generation (worldgen v2): the generator mirror built from
/// the server's [`GenParams`], the off-thread results map, and the fetch queue
/// for chunks that can't be generated (graph-hash mismatch, gen failure).
#[derive(Resource)]
pub struct ClientGen {
    terrain: Option<Arc<TerrainGen>>,
    /// Server's `graph_hash` matches our compiled generator — pristine
    /// entries generate locally. On mismatch every pristine position goes
    /// through [`ClientMsg::ChunkFetch`] and `ViewRadius.full_streams` flips.
    pub hash_ok: bool,
    tx: crossbeam_channel::Sender<(u32, Vec<(IVec3, ChunkVolume)>)>,
    pub(crate) rx: crossbeam_channel::Receiver<(u32, Vec<(IVec3, ChunkVolume)>)>,
    /// Generated volumes awaiting materialization (current epoch only).
    pub(crate) ready: HashMap<IVec3, ChunkVolume>,
    /// Positions to request as full payloads (batched by `flush_chunk_fetch`).
    pub(crate) fetch: Vec<[i32; 3]>,
    fetch_cooldown: f32,
}

impl Default for ClientGen {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { terrain: None, hash_ok: false, tx, rx, ready: HashMap::new(), fetch: Vec::new(), fetch_cooldown: 0.0 }
    }
}

impl ClientGen {
    /// (Re)build the local generator from a world's identity (login/warp).
    pub(crate) fn configure(&mut self, p: GenParams) {
        self.ready.clear();
        self.fetch.clear();
        let world_type = match p.world_type {
            0 => Some(WorldType::Normal),
            1 => Some(WorldType::Flat),
            _ => None,
        };
        let terrain = world_type.map(|wt| Arc::new(TerrainGen::new(p.seed as u32, wt)));
        self.hash_ok = terrain
            .as_ref()
            .is_some_and(|t| soils_worldgen::graph_hash(t.graph()) == p.graph_hash);
        if !self.hash_ok {
            warn!(
                "worldgen identity mismatch (server hash {:#x}); falling back to full streaming",
                p.graph_hash
            );
        }
        self.terrain = terrain;
    }

    /// Dispatch a batch of pristine positions to a worker thread (the batch
    /// generator fans out on rayon internally).
    pub(crate) fn dispatch(&self, epoch: u32, positions: Vec<IVec3>) {
        let Some(terrain) = self.terrain.clone() else { return };
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let registry = soils_worldgen::default_registry();
            let volumes = terrain.generate_batch(&positions, &registry);
            let _ = tx.send((epoch, positions.into_iter().zip(volumes).collect()));
        });
    }
}

#[derive(Message)]
pub struct EditReceived {
    pub pos: [i32; 3],
    pub value: u8,
    pub epoch: u32,
}

#[derive(Message)]
pub struct EntitySpawned {
    pub id: u32,
    pub kind: u16,
    pub pos: [f32; 3],
}

#[derive(Message)]
pub struct EntitiesUpdated {
    pub states: Vec<EntityState>,
    /// Snapshot tick (drives the remote-body interpolation clock).
    pub tick: u32,
    /// Reconciliation anchor: the highest input seq the server had applied.
    pub last_input_seq: u32,
}

/// Client-side snapshot state: per-entity quantized baselines (the shared
/// `soils-protocol` decode path) plus the latest applied tick, echoed to the
/// server as `ack_tick` on every `Inputs` send.
#[derive(Resource, Default)]
pub struct SnapTracker(pub SnapshotTracker);

#[derive(Message)]
pub struct EntityDespawned(pub u32);

#[derive(Message)]
pub struct TimeReceived(pub f32);

#[derive(Message)]
pub struct InitReceived {
    pub id: u16,
    pub self_entity: u32,
    pub spawn: [f32; 3],
    pub daytime: f32,
}

#[derive(Message)]
pub struct WarpReceived {
    pub spawn: [f32; 3],
    pub daytime: f32,
}

/// Batch and send queued [`ClientGen::fetch`] positions (hash-mismatch and
/// gen-failure repairs). Paced: the server charges each fetch against the edit
/// token bucket.
pub fn flush_chunk_fetch(time: Res<Time>, net: Res<NetClient>, mut cgen: ResMut<ClientGen>) {
    cgen.fetch_cooldown -= time.delta_secs();
    if cgen.fetch.is_empty() || cgen.fetch_cooldown > 0.0 {
        return;
    }
    cgen.fetch_cooldown = 0.25;
    let n = cgen.fetch.len().min(64);
    let positions: Vec<[i32; 3]> = cgen.fetch.drain(..n).collect();
    net.send(soils_protocol::ClientMsg::ChunkFetch { positions });
}

/// The server's verdict on one of our own edits (see `edit::PendingEdits`).
#[derive(Message)]
pub struct EditAck {
    pub seq: u32,
    pub accepted: bool,
}

#[derive(Message)]
pub struct LoginFailed(pub String);

/// Client-local connection status changes (handshake succeeded/failed).
#[derive(Message)]
pub struct NetStatus(pub String);

/// Register every message type plus the epoch resource.
pub fn register(app: &mut App) {
    app.init_resource::<WorldEpoch>()
        .init_resource::<ClientGen>()
        .init_resource::<SnapTracker>()
        .init_resource::<ChunkDirectory>()
        .init_resource::<DemandProcessor>()
        .init_resource::<demand::MirrorSet>()
        .add_message::<DirMsg>()
        .add_message::<EditReceived>()
        .add_message::<EntitySpawned>()
        .add_message::<EntitiesUpdated>()
        .add_message::<EntityDespawned>()
        .add_message::<TimeReceived>()
        .add_message::<InitReceived>()
        .add_message::<WarpReceived>()
        .add_message::<EditAck>()
        .add_message::<LoginFailed>()
        .add_message::<NetStatus>();
}

/// Drain the network bridge and fan out typed messages. `Bundle`s flatten into
/// per-chunk [`ChunkReceived`]s. (One writer param per message type — the
/// param count is the point of this system.)
#[allow(clippy::too_many_arguments)]
pub fn route_server_messages(
    net: Res<NetClient>,
    mut epoch: ResMut<WorldEpoch>,
    mut tracker: ResMut<SnapTracker>,
    mut cgen: ResMut<ClientGen>,
    mut dir_msgs: MessageWriter<DirMsg>,
    mut edits: MessageWriter<EditReceived>,
    mut spawns: MessageWriter<EntitySpawned>,
    mut entities: MessageWriter<EntitiesUpdated>,
    mut despawns: MessageWriter<EntityDespawned>,
    mut times: MessageWriter<TimeReceived>,
    mut inits: MessageWriter<InitReceived>,
    mut warps: MessageWriter<WarpReceived>,
    mut edit_acks: MessageWriter<EditAck>,
    mut login_fails: MessageWriter<LoginFailed>,
    mut statuses: MessageWriter<NetStatus>,
) {
    for ev in net.drain() {
        let msg = match ev {
            NetEvent::Connected => {
                statuses.write(NetStatus("connected".into()));
                continue;
            }
            NetEvent::ConnectFailed(e) => {
                statuses.write(NetStatus(format!("could not reach server: {e}")));
                continue;
            }
            NetEvent::Msg(msg) => msg,
        };
        match msg {
            ServerMsg::Init { id, self_entity, spawn, worldgen, daytime } => {
                cgen.configure(worldgen);
                inits.write(InitReceived { id, self_entity, spawn, daytime });
            }
            ServerMsg::LoginError { message } => {
                login_fails.write(LoginFailed(message));
            }
            ServerMsg::Manifest { chunks: infos } => {
                dir_msgs.write(DirMsg::Manifest { infos, epoch: epoch.0 });
            }
            ServerMsg::ChunkUnload { pos } => {
                dir_msgs.write(DirMsg::Unload { pos, epoch: epoch.0 });
            }
            ServerMsg::Edit { pos, value } => {
                edits.write(EditReceived { pos, value, epoch: epoch.0 });
            }
            ServerMsg::Time { daytime } => {
                times.write(TimeReceived(daytime));
            }
            ServerMsg::Warp { spawn, worldgen, daytime } => {
                epoch.0 += 1;
                tracker.0.clear();
                cgen.configure(worldgen);
                warps.write(WarpReceived { spawn, daytime });
            }
            ServerMsg::EditAccepted { seq, .. } => {
                edit_acks.write(EditAck { seq, accepted: true });
            }
            ServerMsg::EditRejected { seq } => {
                edit_acks.write(EditAck { seq, accepted: false });
            }
            ServerMsg::EntitySpawn { id, kind, pos } => {
                spawns.write(EntitySpawned { id, kind, pos });
            }
            ServerMsg::Snapshot { tick, baseline_tick, last_input_seq, payload } => {
                if let Some(updated) = tracker.0.apply(tick, baseline_tick, &payload) {
                    entities.write(EntitiesUpdated { states: updated, tick, last_input_seq });
                }
            }
            ServerMsg::EntityDespawn { id } => {
                tracker.0.forget(id);
                despawns.write(EntityDespawned(id));
            }
        }
    }
}

/// Authenticated: adopt our id and the world clock, drop the login screen,
/// spawn at the server-provided position.
pub fn apply_init(
    mut reader: MessageReader<InitReceived>,
    mut local: ResMut<LocalPlayer>,
    mut world_time: ResMut<WorldTime>,
    mut login: ResMut<LoginState>,
    mut streaming: ResMut<Streaming>,
    mut ring: ResMut<player::InputRing>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    for msg in reader.read() {
        local.id = msg.id;
        local.self_entity = msg.self_entity;
        world_time.daytime = msg.daytime;
        ring.reset(); // any prediction history predates this session
        login.done = true; // authenticated — drop the login screen
        // A (re)login may be a fresh connection whose server-side radius reset
        // to the default; re-send ours (idempotent on the same connection).
        streaming.sent_radius = None;
        if let Ok((mut player, mut transform)) = query.single_mut() {
            player::teleport(&mut player, &mut transform, Vec3::from_array(msg.spawn));
        }
    }
}

pub fn apply_login_failed(mut reader: MessageReader<LoginFailed>, mut login: ResMut<LoginState>) {
    for msg in reader.read() {
        login.status = msg.0.clone();
    }
}

pub fn apply_net_status(mut reader: MessageReader<NetStatus>, mut login: ResMut<LoginState>) {
    for msg in reader.read() {
        login.status = msg.0.clone();
    }
}

/// Confirmed `Warp`: drop the old world entirely and re-stream the new one.
#[allow(clippy::too_many_arguments)]
pub fn apply_warp(
    mut reader: MessageReader<WarpReceived>,
    mut commands: Commands,
    mut map: ResMut<ChunkMap>,
    mut actor_map: ResMut<ActorMap>,
    mut world_time: ResMut<WorldTime>,
    mut streaming: ResMut<Streaming>,
    mut light_queue: ResMut<LightQueue>,
    mut dir: ResMut<ChunkDirectory>,
    mut proc: ResMut<DemandProcessor>,
    mut demanded: ResMut<crate::cull::DemandedChunks>,
    mut pending_edits: ResMut<crate::edit::PendingEdits>,
    mut ring: ResMut<player::InputRing>,
    mut slots: ResMut<ChunkSlots>,
    mut pool_ops: ResMut<PoolOpQueue>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    for msg in reader.read() {
        pending_edits.clear(); // old-world verdicts are moot
        ring.reset(); // prediction history describes the old world
        slots.clear_all(&mut pool_ops);
        for (_, entity) in map.map.drain() {
            commands.entity(entity).despawn();
        }
        for (_, entity) in actor_map.map.drain() {
            commands.entity(entity).despawn();
        }
        light_queue.clear();
        world_time.daytime = msg.daytime;
        if let Ok((mut player, mut transform)) = query.single_mut() {
            player::teleport(&mut player, &mut transform, Vec3::from_array(msg.spawn));
        }
        streaming.last_chunk = None; // force a fresh stream
        streaming.pending = 0; // old world's outstanding requests are moot
        dir.entries.clear();
        dir.edited.clear();
        dir.overlay.clear();
        proc.clear();
        // In-flight readbacks describe the old world; drop them so the demand
        // pump doesn't age them toward bogus fetches.
        demanded.positions.clear();
        demanded.total = 0;
    }
}

pub fn apply_time(mut reader: MessageReader<TimeReceived>, mut world_time: ResMut<WorldTime>) {
    for msg in reader.read() {
        world_time.daytime = msg.0;
    }
}

/// A voxel edit made by another player. Chunks with CPU bytes apply
/// directly; the rest overlay the edit and materialize through the mirror
/// path (an edited chunk becomes CPU-resident by policy).
#[allow(clippy::too_many_arguments)]
pub fn apply_edits(
    mut reader: MessageReader<EditReceived>,
    epoch: Res<WorldEpoch>,
    map: Res<ChunkMap>,
    mut dir: ResMut<ChunkDirectory>,
    mut proc: ResMut<DemandProcessor>,
    mut chunks: Query<&mut VoxelChunk>,
    mut slots: ResMut<ChunkSlots>,
    mut pool_ops: ResMut<PoolOpQueue>,
    mut dirty_mesh: ResMut<DirtyMesh>,
    mut light_queue: ResMut<LightQueue>,
) {
    for msg in reader.read() {
        if msg.epoch != epoch.0 {
            continue;
        }
        let v = IVec3::from_array(msg.pos);
        let cpos = IVec3::new(
            v.x >> soils_protocol::CHUNK_BIT,
            v.y >> soils_protocol::CHUNK_BIT,
            v.z >> soils_protocol::CHUNK_BIT,
        );
        if map.map.contains_key(&cpos) {
            dir.edited.insert(cpos);
            edit::apply_edit(&map, &mut chunks, &mut slots, &mut pool_ops, &mut dirty_mesh, v, msg.value);
        } else {
            demand::overlay_edit(&mut dir, &mut proc, v, msg.value);
        }
        light_queue.edits.push(v);
    }
}

/// An entity entered interest: spawn its body (shaped by its registry kind).
/// Our own player entity gets no body — its updates drive the camera.
pub fn apply_entity_spawns(
    mut reader: MessageReader<EntitySpawned>,
    mut commands: Commands,
    local: Res<LocalPlayer>,
    mut map: ResMut<ActorMap>,
    assets: Res<ActorAssets>,
    physics: Res<crate::physics::ClientPhysics>,
) {
    for msg in reader.read() {
        if msg.id == local.self_entity || map.map.contains_key(&msg.id) {
            continue;
        }
        // When the local physics world is on, physics props are simulated and
        // rendered there, not through the interpolation actor path.
        if physics.enabled && msg.kind == soils_sim::KIND_PHYSICS_CUBE {
            continue;
        }
        let target = Vec3::from_array(msg.pos);
        let Some(kind) = assets.kinds.get(msg.kind as usize) else { continue };
        let entity = commands
            .spawn((
                Actor::new(msg.kind, 0, target),
                Mesh3d(kind.mesh.clone()),
                MeshMaterial3d(kind.material.clone()),
                Transform::from_translation(target - Vec3::Y * kind.body_drop),
            ))
            .id();
        map.map.insert(msg.id, entity);
    }
}

/// Snapshot states for entities in interest: push remote bodies' interp
/// buffers (our own entity is handled by `player::reconcile_self`). Must run
/// after [`apply_entity_spawns`] and before [`apply_entity_despawns`] — the
/// reverse order turns an update+despawn sharing a frame into a permanent
/// ghost body.
pub fn apply_entity_updates(
    mut reader: MessageReader<EntitiesUpdated>,
    local: Res<LocalPlayer>,
    map: Res<ActorMap>,
    mut clock: ResMut<crate::actor::InterpClock>,
    mut actors: Query<&mut Actor>,
) {
    for msg in reader.read() {
        clock.observe(msg.tick);
        for state in &msg.states {
            if state.id == local.self_entity {
                continue;
            }
            if let Some(&entity) = map.map.get(&state.id)
                && let Ok(mut actor) = actors.get_mut(entity)
            {
                actor.push_snapshot(
                    msg.tick,
                    Vec3::from_array(state.pos),
                    Vec3::from_array(state.velocity),
                    Quat::from_array(state.rot),
                );
            }
        }
    }
}

/// An entity left interest (or despawned): drop its body.
pub fn apply_entity_despawns(
    mut reader: MessageReader<EntityDespawned>,
    mut commands: Commands,
    mut map: ResMut<ActorMap>,
) {
    for msg in reader.read() {
        if let Some(entity) = map.map.remove(&msg.0) {
            commands.entity(entity).despawn();
        }
    }
}
