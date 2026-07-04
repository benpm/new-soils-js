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

use std::collections::{HashSet, VecDeque};

use bevy::prelude::*;
use bevy::render::storage::ShaderStorageBuffer;
use soils_protocol::{EntityState, ServerMsg};

use crate::actor::{Actor, ActorAssets, ActorMap, LocalPlayer};
use crate::chunk::{ChunkMap, VoxelChunk, WorldTime};
use crate::edit;
use crate::gi;
use crate::gpu_mesh::{self, AtlasAssets, GpuChunk};
use crate::light::{LightQueue, SkyTerm};
use crate::login::LoginState;
use crate::material::{self, ChunkMeshMaterial};
use crate::net::{NetClient, NetEvent};
use crate::pause::RenderToggles;
use crate::player::{self, Player, Streaming};

/// Bumps every time a `Warp` is routed; chunk/edit messages carry the epoch
/// they were routed under so consumers can drop leftovers from the old world.
#[derive(Resource, Default)]
pub struct WorldEpoch(pub u32);

#[derive(Clone)]
pub struct ChunkReceived {
    pub pos: [i32; 3],
    /// `chunk_codec` payload (palette + LZ4), decoded at apply time.
    pub payload: Vec<u8>,
    pub epoch: u32,
}

/// The ordered chunk stream from the server. Data and unloads share one
/// message type (and one apply queue) because their *relative order* is the
/// contract: a chunk that leaves and re-enters the subscription arrives as
/// `Unload` then `Data`, and applying them out of order would drop the chunk.
#[derive(Message, Clone)]
pub enum ChunkStream {
    Data(ChunkReceived),
    Unload { pos: [i32; 3], epoch: u32 },
}

/// Hard cap on chunks turned into GPU resources per frame. A fresh world
/// floods ~729 chunks in a burst; applying them all at once allocates hundreds
/// of MB of SSBOs and dispatches hundreds of compute jobs in one frame, which
/// hangs (and loses) an integrated GPU — the cap protects weak devices.
const CHUNK_APPLY_MAX: usize = 32;
/// Time box within the cap: a fixed per-frame count collapses when burst
/// frames run long (8/frame at ~8 fps was ~60 chunks/s, >10 s to fill a fresh
/// world). Applying against wall time instead self-regulates: fast frames
/// apply more, slow frames back off but always make progress.
const CHUNK_APPLY_MS: f32 = 3.0;

/// The ordered chunk stream awaiting application, drained under a time budget
/// by [`apply_chunks`]. `queued` mirrors the positions of queued *data*
/// entries so [`player::track_streaming`] can estimate outstanding work.
#[derive(Resource, Default)]
pub struct ChunkApplyQueue {
    pub queue: VecDeque<ChunkStream>,
    pub queued: HashSet<IVec3>,
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
pub struct EntitiesUpdated(pub Vec<EntityState>);

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
        .init_resource::<ChunkApplyQueue>()
        .add_message::<ChunkStream>()
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
    mut chunks: MessageWriter<ChunkStream>,
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
            ServerMsg::Init { id, self_entity, spawn, daytime, .. } => {
                inits.write(InitReceived { id, self_entity, spawn, daytime });
            }
            ServerMsg::LoginError { message } => {
                login_fails.write(LoginFailed(message));
            }
            ServerMsg::Chunk { pos, payload } => {
                chunks.write(ChunkStream::Data(ChunkReceived { pos, payload, epoch: epoch.0 }));
            }
            ServerMsg::Bundle { chunks: datas } => {
                for d in datas {
                    chunks.write(ChunkStream::Data(ChunkReceived {
                        pos: d.pos,
                        payload: d.payload,
                        epoch: epoch.0,
                    }));
                }
            }
            ServerMsg::ChunkUnload { pos } => {
                chunks.write(ChunkStream::Unload { pos, epoch: epoch.0 });
            }
            ServerMsg::Edit { pos, value } => {
                edits.write(EditReceived { pos, value, epoch: epoch.0 });
            }
            ServerMsg::Time { daytime } => {
                times.write(TimeReceived(daytime));
            }
            ServerMsg::Warp { spawn, daytime } => {
                epoch.0 += 1;
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
            ServerMsg::EntityUpdate { entities: states } => {
                entities.write(EntitiesUpdated(states));
            }
            ServerMsg::EntityDespawn { id } => {
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
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    for msg in reader.read() {
        local.id = msg.id;
        local.self_entity = msg.self_entity;
        world_time.daytime = msg.daytime;
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
    mut queue: ResMut<ChunkApplyQueue>,
    mut pending_edits: ResMut<crate::edit::PendingEdits>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    for msg in reader.read() {
        pending_edits.clear(); // old-world verdicts are moot
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
        // Drop any queued chunks from the old world (the epoch bump also makes
        // them safe, but this frees their buffers immediately).
        queue.queue.clear();
        queue.queued.clear();
    }
}

pub fn apply_time(mut reader: MessageReader<TimeReceived>, mut world_time: ResMut<WorldTime>) {
    for msg in reader.read() {
        world_time.daytime = msg.0;
    }
}

/// Apply streamed chunks: update an existing chunk's voxels or spawn a new
/// (meshed or empty-tracked) chunk entity.
#[allow(clippy::too_many_arguments)]
pub fn apply_chunks(
    mut reader: MessageReader<ChunkStream>,
    epoch: Res<WorldEpoch>,
    mut commands: Commands,
    mut map: ResMut<ChunkMap>,
    mut chunks: Query<&mut VoxelChunk>,
    mut gpu_chunks: Query<&mut GpuChunk>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut materials: ResMut<Assets<ChunkMeshMaterial>>,
    atlas: Res<AtlasAssets>,
    toggles: Res<RenderToggles>,
    gi: Res<gi::GiAssets>,
    sky: Res<SkyTerm>,
    mut light_queue: ResMut<LightQueue>,
    mut streaming: ResMut<Streaming>,
    mut queue: ResMut<ChunkApplyQueue>,
) {
    // (A) Move this frame's arrivals into the persistent queue. Bevy messages are
    // double-buffered and dropped after ~2 frames, so we must capture them now
    // even though only a few are applied per frame. Stale entries (a world we've
    // since warped out of) are dropped here, cheaply. Data and unloads stay in
    // one queue: their relative order is part of the protocol.
    for msg in reader.read() {
        match msg {
            ChunkStream::Data(d) if d.epoch == epoch.0 => {
                queue.queued.insert(IVec3::from_array(d.pos));
                queue.queue.push_back(msg.clone());
            }
            ChunkStream::Unload { epoch: e, .. } if *e == epoch.0 => {
                queue.queue.push_back(msg.clone());
            }
            _ => {}
        }
    }
    if queue.queue.is_empty() {
        return;
    }
    let gi_cascade0 = gi.cascade0();
    let (gi_origin, gi_enabled) = gi.apply_params();
    let params = material::AtlasParams {
        ambient_occlusion: if toggles.ao { 1.0 } else { 0.0 },
        fog_density: if toggles.fog { material::FOG_DENSITY } else { 0.0 },
        gi_origin,
        gi_enabled,
        sky_term: sky.0,
        light_enabled: if toggles.light { 1.0 } else { 0.0 },
        ..default()
    };

    // (B) Apply chunks until the time box (or hard cap) is hit. Turning a chunk
    // into GPU resources allocates a ~655 KB quad SSBO and queues a compute
    // dispatch; doing hundreds at once on a burst loses the device, so we
    // spread the work — but by wall time, so slow frames don't starve the fill.
    let t0 = std::time::Instant::now();
    let mut applied = 0;
    while applied < CHUNK_APPLY_MAX
        && (applied < 2 || t0.elapsed().as_secs_f32() * 1000.0 < CHUNK_APPLY_MS)
    {
        let Some(entry) = queue.queue.pop_front() else { break };
        let msg = match entry {
            ChunkStream::Data(d) => d,
            ChunkStream::Unload { pos, epoch: e } => {
                // Left the server-side subscription: drop our copy (entity,
                // GPU buffers via asset handles, pending light work). Cheap —
                // doesn't spend the apply budget.
                if e == epoch.0 {
                    let cpos = IVec3::from_array(pos);
                    if let Some(entity) = map.map.remove(&cpos) {
                        commands.entity(entity).despawn();
                    }
                    light_queue.unload(cpos);
                }
                continue;
            }
        };
        let cpos = IVec3::from_array(msg.pos);
        queue.queued.remove(&cpos);
        if msg.epoch != epoch.0 {
            continue; // warped away since it was queued; drop without spending budget
        }
        let is_air = soils_protocol::payload_is_air(&msg.payload);
        let Some(volume) = soils_protocol::decode_chunk(&msg.payload) else {
            warn!("dropping undecodable chunk payload at {cpos}");
            continue;
        };
        if let Some(&entity) = map.map.get(&cpos) {
            // Existing chunk: update CPU copy + re-upload voxels if it has a
            // GPU mesh, else (was empty) leave as-is.
            if let Ok(mut vc) = chunks.get_mut(entity) {
                vc.volume = volume.clone();
            }
            if !is_air
                && let Ok(mut gc) = gpu_chunks.get_mut(entity)
            {
                gpu_mesh::refresh_gpu_chunk(&mut buffers, &mut gc, &volume);
            }
        } else if is_air {
            // Track empty chunks so they aren't re-requested; no mesh (but
            // they still carry light — sky crosses them into caves below).
            let e = commands
                .spawn(VoxelChunk {
                    pos: cpos,
                    volume,
                    light: soils_sim::light::ChunkLight::dark(),
                })
                .id();
            map.map.insert(cpos, e);
            streaming.pending = streaming.pending.saturating_sub(1);
        } else {
            let e = gpu_mesh::spawn_gpu_chunk(
                &mut commands,
                &mut buffers,
                &mut materials,
                &atlas,
                cpos,
                volume,
                params.clone(),
                gi_cascade0.clone(),
            );
            map.map.insert(cpos, e);
            streaming.pending = streaming.pending.saturating_sub(1);
        }
        light_queue.chunks.push(cpos);
        applied += 1;
    }
}

/// A voxel edit made by another player.
pub fn apply_edits(
    mut reader: MessageReader<EditReceived>,
    epoch: Res<WorldEpoch>,
    map: Res<ChunkMap>,
    mut chunks: Query<&mut VoxelChunk>,
    mut gpu_chunks: Query<&mut GpuChunk>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut light_queue: ResMut<LightQueue>,
) {
    for msg in reader.read() {
        if msg.epoch != epoch.0 {
            continue;
        }
        let v = IVec3::from_array(msg.pos);
        edit::apply_edit(&map, &mut chunks, &mut gpu_chunks, &mut buffers, v, msg.value);
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
) {
    for msg in reader.read() {
        if msg.id == local.self_entity || map.map.contains_key(&msg.id) {
            continue;
        }
        let target = Vec3::from_array(msg.pos);
        let Some(kind) = assets.kinds.get(msg.kind as usize) else { continue };
        let entity = commands
            .spawn((
                Actor { target, kind: msg.kind },
                Mesh3d(kind.mesh.clone()),
                MeshMaterial3d(kind.material.clone()),
                Transform::from_translation(target - Vec3::Y * kind.body_drop),
            ))
            .id();
        map.map.insert(msg.id, entity);
    }
}

/// Full-state updates for entities in interest: retarget bodies; our own
/// entity's echo eases the camera (interpolation-only self until phase 11).
/// Must run after [`apply_entity_spawns`] and before
/// [`apply_entity_despawns`] — the reverse order turns an update+despawn
/// sharing a frame into a permanent ghost body.
pub fn apply_entity_updates(
    mut reader: MessageReader<EntitiesUpdated>,
    local: Res<LocalPlayer>,
    map: Res<ActorMap>,
    mut actors: Query<&mut Actor>,
    mut player_q: Query<&mut Player>,
) {
    for msg in reader.read() {
        for state in &msg.0 {
            if state.id == local.self_entity {
                if let Ok(mut player) = player_q.single_mut() {
                    player.net_target = Some(Vec3::from_array(state.pos));
                    player.sim.pos = Vec3::from_array(state.pos);
                    player.sim.vel = Vec3::from_array(state.velocity);
                }
                continue;
            }
            if let Some(&entity) = map.map.get(&state.id)
                && let Ok(mut actor) = actors.get_mut(entity)
            {
                actor.target = Vec3::from_array(state.pos);
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
