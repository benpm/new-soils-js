//! Bevy client for the new-soils Rust port: connects to the server, streams and
//! meshes chunks, renders them with the atlas material, and runs the
//! first-person player + block editing.

mod actor;
mod chunk;
mod edit;
mod gpu_mesh;
mod material;
mod net;
mod player;

use bevy::prelude::*;
use bevy::render::storage::ShaderStorageBuffer;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use soils_protocol::{ChunkVolume, ClientMsg, ServerMsg};
use soils_worldgen::default_registry;

use actor::{Actor, ActorAssets, ActorMap, LocalPlayer};
use chunk::{Blocks, ChunkMap, VoxelChunk, WorldTime};
use gpu_mesh::{AtlasAssets, GpuChunk, GpuMeshPlugin};
use material::ChunkMeshMaterial;
use net::NetClient;
use player::{Player, Streaming};

/// Marks the sun so we can swing it with the day/night cycle.
#[derive(Component)]
struct Sun;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "new-soils (Rust/Bevy)".into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(GpuMeshPlugin)
        .insert_resource(ClearColor(Color::srgb(0.55, 0.75, 1.0)))
        .insert_resource(ChunkMap::default())
        .insert_resource(WorldTime::default())
        .insert_resource(Streaming::default())
        .insert_resource(Blocks(default_registry()))
        .insert_resource(LocalPlayer::default())
        .insert_resource(ActorMap::default())
        .insert_resource(net::connect())
        .add_systems(
            Startup,
            (setup, actor::setup_actor_assets, player::grab_cursor, login),
        )
        .add_systems(
            Update,
            (
                net_receive,
                player::request_chunks,
                player::mouse_look,
                player::movement,
                player::cursor_toggle,
                edit::edit_blocks,
                actor::send_move,
                actor::interpolate_actors,
                day_night,
                self_test,
                screenshot_once,
            ),
        )
        .run();
}

/// In self-test mode, save one screenshot a few seconds in so the rendered
/// terrain can be inspected as an artifact. Also parks the camera high above
/// spawn looking straight down so terrain is guaranteed to be in frame.
fn screenshot_once(
    mut commands: Commands,
    time: Res<Time>,
    mut taken: Local<bool>,
    mut camera: Query<&mut Transform, With<Player>>,
    meshed: Query<(&VoxelChunk, &Transform), (With<Mesh3d>, Without<Player>)>,
    remote_actors: Query<&Transform, (With<Actor>, Without<Player>)>,
) {
    if *taken || std::env::var("SOILS_SELFTEST").is_err() {
        return;
    }
    if time.elapsed_secs() > 6.5 {
        *taken = true;
        if let Ok(mut t) = camera.single_mut() {
            if let Some(actor) = remote_actors.iter().next() {
                // Frame a remote actor so its body is visible in the shot.
                let p = actor.translation;
                t.translation = p + Vec3::new(4.0, 1.5, 4.0);
                t.look_at(p, Vec3::Y);
                info!("SELFTEST: framing actor at {:?}", p);
            } else {
                t.translation = Vec3::new(282.0, 330.0, 268.0);
                t.look_at(Vec3::new(300.0, 256.0, 250.0), Vec3::Y);
                info!("SELFTEST: camera at {:?} looking_at terrain", t.translation);
            }
        }
        let mut sample = 0;
        for (chunk, t) in &meshed {
            if sample < 3 {
                info!("SELFTEST: meshed chunk {:?} at world {:?}", chunk.pos, t.translation);
            }
            sample += 1;
        }
        info!("SELFTEST: {sample} chunks currently have meshes");
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk("/tmp/soils-selftest.png"));
        info!("SELFTEST: screenshot requested");
    }
}

/// When `SOILS_SELFTEST` is set, report how much of the world streamed in and
/// meshed after a few seconds, then exit. Lets the full client path (connect →
/// stream → mesh → render) be validated headlessly under xvfb + lavapipe.
fn self_test(
    time: Res<Time>,
    map: Res<ChunkMap>,
    meshed: Query<&Mesh3d>,
    remote_actors: Query<&Actor>,
    mut exit: MessageWriter<AppExit>,
) {
    if std::env::var("SOILS_SELFTEST").is_err() {
        return;
    }
    if time.elapsed_secs() > 8.0 {
        let chunks = map.map.len();
        let meshes = meshed.iter().count();
        let actors = remote_actors.iter().count();
        info!("SELFTEST: {chunks} chunks loaded, {meshes} chunk meshes built, {actors} actors");
        assert!(chunks > 0, "no chunks streamed from server");
        assert!(meshes > 0, "no chunk meshes were built");
        info!("SELFTEST PASSED");
        exit.write(AppExit::Success);
    }
}

/// Spawn the camera/player and the sun.
fn setup(mut commands: Commands) {
    commands.spawn((
        Camera3d::default(),
        Projection::from(PerspectiveProjection {
            fov: 65.0_f32.to_radians(),
            far: 2_000_000.0,
            ..default()
        }),
        // Provisional spawn; corrected by the server's `Init` message. The
        // rotation matches the Player's default yaw/pitch (looking down-forward)
        // so the view is sensible before any mouse input.
        Transform::from_xyz(282.0, 285.0, 268.0)
            .with_rotation(Quat::from_axis_angle(Vec3::X, -0.5)),
        Player::default(),
        // Global ambient fill so shaded faces aren't pure black.
        AmbientLight { brightness: 350.0, ..default() },
    ));

    commands.spawn((
        Sun,
        DirectionalLight { illuminance: 10_000.0, shadows_enabled: false, ..default() },
        Transform::from_xyz(0.0, 1.0, 0.0).looking_to(Vec3::new(-0.3, -1.0, -0.2), Vec3::Y),
    ));
}

/// Announce ourselves to the server.
fn login(net: Res<NetClient>) {
    net.send(ClientMsg::Login { name: "player".into() });
}

/// Drain server messages and apply them to the ECS world.
fn net_receive(
    mut commands: Commands,
    net: Res<NetClient>,
    mut map: ResMut<ChunkMap>,
    mut chunks: Query<&mut VoxelChunk>,
    mut gpu_chunks: Query<&mut GpuChunk>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut materials: ResMut<Assets<ChunkMeshMaterial>>,
    atlas: Res<AtlasAssets>,
    mut world_time: ResMut<WorldTime>,
    mut player: Query<&mut Transform, With<Player>>,
    mut local: ResMut<LocalPlayer>,
    mut actor_map: ResMut<ActorMap>,
    actor_assets: Res<ActorAssets>,
    mut actors: Query<&mut Actor>,
) {
    for msg in net.drain() {
        match msg {
            ServerMsg::Init { id, spawn, daytime, .. } => {
                local.id = id;
                world_time.daytime = daytime;
                if let Ok(mut transform) = player.single_mut() {
                    transform.translation = Vec3::from_array(spawn);
                }
            }
            ServerMsg::Chunk { pos, empty, voxels } => {
                let cpos = IVec3::from_array(pos);
                let volume = if empty {
                    ChunkVolume::empty()
                } else {
                    ChunkVolume::from_bytes(&voxels)
                };
                if let Some(&entity) = map.map.get(&cpos) {
                    // Existing chunk: update CPU copy + re-upload voxels if it has
                    // a GPU mesh, else (was empty) leave as-is.
                    if let Ok(mut vc) = chunks.get_mut(entity) {
                        vc.volume = volume.clone();
                    }
                    if !empty {
                        if let Ok(mut gc) = gpu_chunks.get_mut(entity) {
                            gpu_mesh::refresh_gpu_chunk(&mut buffers, &mut gc, &volume);
                        }
                    }
                } else if empty {
                    // Track empty chunks so they aren't re-requested; no mesh.
                    let e = commands.spawn(VoxelChunk { pos: cpos, volume }).id();
                    map.map.insert(cpos, e);
                } else {
                    let e = gpu_mesh::spawn_gpu_chunk(
                        &mut commands,
                        &mut buffers,
                        &mut materials,
                        &atlas,
                        cpos,
                        volume,
                    );
                    map.map.insert(cpos, e);
                }
            }
            ServerMsg::Edit { pos, value } => {
                let v = IVec3::from_array(pos);
                edit::apply_edit(&map, &mut chunks, &mut gpu_chunks, &mut buffers, v, value);
            }
            ServerMsg::Time { daytime } => {
                world_time.daytime = daytime;
            }
            ServerMsg::ActorUpdate { actors: states } => {
                for state in states {
                    if state.id == local.id {
                        continue; // don't render ourselves
                    }
                    let target = Vec3::from_array(state.pos);
                    if let Some(&entity) = actor_map.map.get(&state.id) {
                        if let Ok(mut actor) = actors.get_mut(entity) {
                            actor.target = target;
                        }
                    } else {
                        let entity = commands
                            .spawn((
                                Actor { target },
                                Mesh3d(actor_assets.mesh.clone()),
                                MeshMaterial3d(actor_assets.material.clone()),
                                Transform::from_translation(target - Vec3::Y * 0.9),
                            ))
                            .id();
                        actor_map.map.insert(state.id, entity);
                    }
                }
            }
            ServerMsg::ActorRemove { id } => {
                if let Some(entity) = actor_map.map.remove(&id) {
                    commands.entity(entity).despawn();
                }
            }
        }
    }
}

/// Swing the sun around with the day/night cycle and dim it at night.
fn day_night(
    world_time: Res<WorldTime>,
    mut sun: Query<(&mut Transform, &mut DirectionalLight), With<Sun>>,
) {
    let Ok((mut transform, mut light)) = sun.single_mut() else { return };
    // daytime 0.0 = noon, 0.5 = midnight (matching the JS convention).
    let angle = world_time.daytime * std::f32::consts::TAU;
    let dir = Vec3::new(angle.sin() * 0.6, -angle.cos(), 0.3).normalize();
    transform.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, dir);
    // Brightest at noon, dark at midnight.
    light.illuminance = 10_000.0 * ((-angle.cos()).max(0.0) * 0.9 + 0.1);
}
