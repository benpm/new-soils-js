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

use bevy::prelude::*;
use bevy::render::storage::ShaderStorageBuffer;
use soils_protocol::{ActorState, ChunkVolume, ServerMsg};

use crate::actor::{Actor, ActorAssets, ActorMap, LocalPlayer};
use crate::chunk::{ChunkMap, VoxelChunk, WorldTime};
use crate::edit;
use crate::gi;
use crate::gpu_mesh::{self, AtlasAssets, GpuChunk};
use crate::login::LoginState;
use crate::material::{self, ChunkMeshMaterial};
use crate::net::{NetClient, NetEvent};
use crate::pause::RenderToggles;
use crate::player::{self, Player, Streaming};

/// Bumps every time a `Warp` is routed; chunk/edit messages carry the epoch
/// they were routed under so consumers can drop leftovers from the old world.
#[derive(Resource, Default)]
pub struct WorldEpoch(pub u32);

#[derive(Message)]
pub struct ChunkReceived {
    pub pos: [i32; 3],
    pub empty: bool,
    pub voxels: Vec<u8>,
    pub epoch: u32,
}

#[derive(Message)]
pub struct EditReceived {
    pub pos: [i32; 3],
    pub value: u8,
    pub epoch: u32,
}

#[derive(Message)]
pub struct ActorsUpdated(pub Vec<ActorState>);

#[derive(Message)]
pub struct ActorRemoved(pub u16);

#[derive(Message)]
pub struct TimeReceived(pub f32);

#[derive(Message)]
pub struct InitReceived {
    pub id: u16,
    pub spawn: [f32; 3],
    pub daytime: f32,
}

#[derive(Message)]
pub struct WarpReceived {
    pub spawn: [f32; 3],
    pub daytime: f32,
}

#[derive(Message)]
pub struct PositionCorrected(pub [f32; 3]);

#[derive(Message)]
pub struct LoginFailed(pub String);

/// Client-local connection status changes (handshake succeeded/failed).
#[derive(Message)]
pub struct NetStatus(pub String);

/// Register every message type plus the epoch resource.
pub fn register(app: &mut App) {
    app.init_resource::<WorldEpoch>()
        .add_message::<ChunkReceived>()
        .add_message::<EditReceived>()
        .add_message::<ActorsUpdated>()
        .add_message::<ActorRemoved>()
        .add_message::<TimeReceived>()
        .add_message::<InitReceived>()
        .add_message::<WarpReceived>()
        .add_message::<PositionCorrected>()
        .add_message::<LoginFailed>()
        .add_message::<NetStatus>();
}

/// Drain the network bridge and fan out typed messages. `Bundle`s flatten into
/// per-chunk [`ChunkReceived`]s.
pub fn route_server_messages(
    net: Res<NetClient>,
    mut epoch: ResMut<WorldEpoch>,
    mut chunks: MessageWriter<ChunkReceived>,
    mut edits: MessageWriter<EditReceived>,
    mut actors: MessageWriter<ActorsUpdated>,
    mut removes: MessageWriter<ActorRemoved>,
    mut times: MessageWriter<TimeReceived>,
    mut inits: MessageWriter<InitReceived>,
    mut warps: MessageWriter<WarpReceived>,
    mut positions: MessageWriter<PositionCorrected>,
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
            ServerMsg::Init { id, spawn, daytime, .. } => {
                inits.write(InitReceived { id, spawn, daytime });
            }
            ServerMsg::LoginError { message } => {
                login_fails.write(LoginFailed(message));
            }
            ServerMsg::Chunk { pos, empty, voxels } => {
                chunks.write(ChunkReceived { pos, empty, voxels, epoch: epoch.0 });
            }
            ServerMsg::Bundle { chunks: datas } => {
                for d in datas {
                    chunks.write(ChunkReceived {
                        pos: d.pos,
                        empty: d.empty,
                        voxels: d.voxels,
                        epoch: epoch.0,
                    });
                }
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
            ServerMsg::Position { pos } => {
                positions.write(PositionCorrected(pos));
            }
            ServerMsg::ActorUpdate { actors: states } => {
                actors.write(ActorsUpdated(states));
            }
            ServerMsg::ActorRemove { id } => {
                removes.write(ActorRemoved(id));
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
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    for msg in reader.read() {
        local.id = msg.id;
        world_time.daytime = msg.daytime;
        login.done = true; // authenticated — drop the login screen
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
pub fn apply_warp(
    mut reader: MessageReader<WarpReceived>,
    mut commands: Commands,
    mut map: ResMut<ChunkMap>,
    mut actor_map: ResMut<ActorMap>,
    mut world_time: ResMut<WorldTime>,
    mut streaming: ResMut<Streaming>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    for msg in reader.read() {
        for (_, entity) in map.map.drain() {
            commands.entity(entity).despawn();
        }
        for (_, entity) in actor_map.map.drain() {
            commands.entity(entity).despawn();
        }
        world_time.daytime = msg.daytime;
        if let Ok((mut player, mut transform)) = query.single_mut() {
            player::teleport(&mut player, &mut transform, Vec3::from_array(msg.spawn));
        }
        streaming.last_chunk = None; // force a fresh stream
        streaming.pending = 0; // old world's outstanding requests are moot
    }
}

/// Server rejected our movement — snap back.
pub fn apply_position_correction(
    mut reader: MessageReader<PositionCorrected>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    for msg in reader.read() {
        if let Ok((mut player, mut transform)) = query.single_mut() {
            player::teleport(&mut player, &mut transform, Vec3::from_array(msg.0));
        }
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
    mut reader: MessageReader<ChunkReceived>,
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
    mut streaming: ResMut<Streaming>,
) {
    if reader.is_empty() {
        return;
    }
    let gi_cascade0 = gi.cascade0();
    let (gi_origin, gi_enabled) = gi.apply_params();
    let params = material::AtlasParams {
        ambient_occlusion: if toggles.ao { 1.0 } else { 0.0 },
        fog_density: if toggles.fog { material::FOG_DENSITY } else { 0.0 },
        gi_origin,
        gi_enabled,
        ..default()
    };
    for msg in reader.read() {
        if msg.epoch != epoch.0 {
            continue; // stale: streamed for a world we've since warped out of
        }
        let cpos = IVec3::from_array(msg.pos);
        let volume =
            if msg.empty { ChunkVolume::empty() } else { ChunkVolume::from_bytes(&msg.voxels) };
        if let Some(&entity) = map.map.get(&cpos) {
            // Existing chunk: update CPU copy + re-upload voxels if it has a
            // GPU mesh, else (was empty) leave as-is.
            if let Ok(mut vc) = chunks.get_mut(entity) {
                vc.volume = volume.clone();
            }
            if !msg.empty {
                if let Ok(mut gc) = gpu_chunks.get_mut(entity) {
                    gpu_mesh::refresh_gpu_chunk(&mut buffers, &mut gc, &volume);
                }
            }
        } else if msg.empty {
            // Track empty chunks so they aren't re-requested; no mesh.
            let e = commands.spawn(VoxelChunk { pos: cpos, volume }).id();
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
) {
    for msg in reader.read() {
        if msg.epoch != epoch.0 {
            continue;
        }
        let v = IVec3::from_array(msg.pos);
        edit::apply_edit(&map, &mut chunks, &mut gpu_chunks, &mut buffers, v, msg.value);
    }
}

/// Positions of nearby actors: move existing bodies, spawn new ones. Must run
/// before [`apply_actor_removes`] — the reverse order turns an update+remove
/// sharing a frame into a permanent ghost body.
pub fn apply_actor_updates(
    mut reader: MessageReader<ActorsUpdated>,
    mut commands: Commands,
    local: Res<LocalPlayer>,
    mut map: ResMut<ActorMap>,
    assets: Res<ActorAssets>,
    mut actors: Query<&mut Actor>,
) {
    for msg in reader.read() {
        for state in &msg.0 {
            if state.id == local.id {
                continue; // don't render ourselves
            }
            let target = Vec3::from_array(state.pos);
            if let Some(&entity) = map.map.get(&state.id) {
                if let Ok(mut actor) = actors.get_mut(entity) {
                    actor.target = target;
                }
            } else {
                let entity = commands
                    .spawn((
                        Actor { target },
                        Mesh3d(assets.mesh.clone()),
                        MeshMaterial3d(assets.material.clone()),
                        Transform::from_translation(target - Vec3::Y * 0.9),
                    ))
                    .id();
                map.map.insert(state.id, entity);
            }
        }
    }
}

/// An actor left view / disconnected.
pub fn apply_actor_removes(
    mut reader: MessageReader<ActorRemoved>,
    mut commands: Commands,
    mut map: ResMut<ActorMap>,
) {
    for msg in reader.read() {
        if let Some(entity) = map.map.remove(&msg.0) {
            commands.entity(entity).despawn();
        }
    }
}
