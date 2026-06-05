//! Bevy client for the new-soils Rust port: connects to the server, streams and
//! meshes chunks, renders them with the atlas material, and runs the
//! first-person player + block editing.

mod chunk;
mod edit;
mod mesh;
mod material;
mod net;
mod player;

use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use soils_protocol::{ChunkVolume, ClientMsg, ServerMsg};
use soils_worldgen::default_registry;

use chunk::{Blocks, ChunkMap, NeedsRemesh, VoxelChunk, WorldTime};
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
        .insert_resource(ClearColor(Color::srgb(0.55, 0.75, 1.0)))
        .insert_resource(ChunkMap::default())
        .insert_resource(WorldTime::default())
        .insert_resource(Streaming::default())
        .insert_resource(Blocks(default_registry()))
        .insert_resource(net::connect())
        .add_systems(Startup, (setup, material::setup_material, player::grab_cursor, login))
        .add_systems(
            Update,
            (
                net_receive,
                player::request_chunks,
                mesh::dispatch_meshing,
                mesh::apply_meshing,
                player::mouse_look,
                player::movement,
                player::cursor_toggle,
                edit::edit_blocks,
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
) {
    if *taken || std::env::var("SOILS_SELFTEST").is_err() {
        return;
    }
    if time.elapsed_secs() > 6.5 {
        *taken = true;
        if let Ok(mut t) = camera.single_mut() {
            t.translation = Vec3::new(282.0, 330.0, 268.0);
            t.look_at(Vec3::new(300.0, 256.0, 250.0), Vec3::Y);
            info!("SELFTEST: camera at {:?} looking_at terrain", t.translation);
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
    mut exit: MessageWriter<AppExit>,
) {
    if std::env::var("SOILS_SELFTEST").is_err() {
        return;
    }
    if time.elapsed_secs() > 8.0 {
        let chunks = map.map.len();
        let meshes = meshed.iter().count();
        info!("SELFTEST: {chunks} chunks loaded, {meshes} chunk meshes built");
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
    mut world_time: ResMut<WorldTime>,
    mut player: Query<&mut Transform, With<Player>>,
) {
    for msg in net.drain() {
        match msg {
            ServerMsg::Init { spawn, daytime, .. } => {
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
                    if let Ok(mut vc) = chunks.get_mut(entity) {
                        vc.volume = volume;
                    }
                    if !empty {
                        commands.entity(entity).insert(NeedsRemesh);
                    }
                } else {
                    let mut e = commands.spawn(VoxelChunk { pos: cpos, volume });
                    if !empty {
                        e.insert(NeedsRemesh);
                    }
                    map.map.insert(cpos, e.id());
                }
            }
            ServerMsg::Edit { pos, value } => {
                let v = IVec3::from_array(pos);
                edit::apply_edit(&mut commands, &map, &mut chunks, v, value);
            }
            ServerMsg::Time { daytime } => {
                world_time.daytime = daytime;
            }
            ServerMsg::ActorUpdate { .. } | ServerMsg::ActorRemove { .. } => {
                // Other-player rendering is out of scope for the slice.
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
