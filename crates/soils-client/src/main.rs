//! Bevy client for the new-soils Rust port: connects to the server, streams and
//! meshes chunks, renders them with the atlas material, and runs the
//! first-person player + block editing.

mod actor;
mod chunk;
mod console;
mod discovery;
mod edit;
mod gi;
mod gi_demo;
mod gpu_mesh;
mod hud;
mod login;
mod material;
mod net;
mod pause;
mod player;
mod singleplayer;

use bevy::prelude::*;
use bevy::ecs::system::SystemParam;
use bevy::camera::Exposure;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::light::{AtmosphereEnvironmentMapLight, light_consts::lux};
use bevy::pbr::{Atmosphere, AtmosphereSettings, ScatteringMedium};
use bevy::render::storage::ShaderStorageBuffer;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use soils_protocol::{ChunkVolume, ClientMsg, ServerMsg};
use soils_worldgen::default_registry;

use actor::{Actor, ActorAssets, ActorMap, LocalPlayer};
use chunk::{Blocks, ChunkMap, VoxelChunk, WorldTime};
use gpu_mesh::{AtlasAssets, GpuChunk, GpuMeshPlugin};
use material::ChunkMeshMaterial;
use net::{NetClient, NetEvent};
use player::{Player, Streaming};

/// Marks the sun so we can swing it with the day/night cycle.
#[derive(Component)]
struct Sun;

/// Camera exposure (EV100) at noon and midnight. Lower = brighter image; the
/// day/night cycle interpolates between them so the whole scene dims at night.
const EV100_DAY: f32 = 13.0;
const EV100_NIGHT: f32 = 16.5;

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
        .add_plugins(gi::GiPlugin)
        .add_plugins(bevy::diagnostic::FrameTimeDiagnosticsPlugin::default())
        .insert_resource(ClearColor(Color::srgb(0.55, 0.75, 1.0)))
        .insert_resource(ChunkMap::default())
        .insert_resource(WorldTime::default())
        .insert_resource(Streaming::default())
        .insert_resource(Blocks(default_registry()))
        .insert_resource(LocalPlayer::default())
        .insert_resource(ActorMap::default())
        .insert_resource(edit::Hotbar::default())
        .insert_resource(pause::RenderToggles::default())
        .init_resource::<console::Console>()
        .init_resource::<login::LoginState>()
        .init_resource::<singleplayer::Singleplayer>()
        .insert_resource(net::connect())
        .insert_resource(discovery::spawn())
        .add_systems(
            Startup,
            (
                setup,
                actor::setup_actor_assets,
                edit::setup_crosshair,
                hud::setup_hud,
                pause::setup_pause_menu,
                console::setup_console,
                login::setup_login,
                selftest_login,
            ),
        )
        // Always-on: networking, login flow, day/night, self-test.
        .add_systems(
            Update,
            (
                net_receive,
                login::login_keyboard,
                login::login_buttons,
                login::update_login_text,
                login::finish_login,
                discovery::discovery_poll,
                login::update_server_list,
                login::server_buttons,
                hud::toggle_hud,
                actor::interpolate_actors,
                self_test_daytime.after(net_receive).before(day_night),
                day_night,
                self_test,
                screenshot_once,
                gi_demo::setup_gi_demo,
                gi_demo::gi_demo_keep_dirty,
            ),
        )
        // Gameplay: only once authenticated.
        .add_systems(
            Update,
            (
                player::request_chunks,
                player::cursor_toggle,
                edit::selection_highlight,
                actor::send_move,
                console::console_input,
                console::update_console_text,
                hud::update_hud,
                pause::pause_menu_visibility,
                pause::pause_menu_buttons,
                pause::update_pause_labels,
            )
                .run_if(login::logged_in),
        )
        // Direct player input: authenticated and console closed.
        .add_systems(
            Update,
            (player::mouse_look, player::movement, edit::edit_blocks, edit::hotbar_select)
                .run_if(console::console_closed)
                .run_if(login::logged_in),
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
    // Configurable so slow software-GPU CI (lavapipe) can allow more time to
    // stream/mesh/trace before the shot; defaults preserve local behaviour.
    if time.elapsed_secs() > env_secs("SOILS_SHOT_SECS", 9.0) {
        *taken = true;
        // In GI-demo mode keep the scene's own framing (see gi_demo.rs).
        if !gi_demo::demo_enabled() {
        if let Ok(mut t) = camera.single_mut() {
            if let Some(actor) = remote_actors.iter().next() {
                // Frame a remote actor so its body is visible in the shot.
                let p = actor.translation;
                t.translation = p + Vec3::new(4.0, 1.5, 4.0);
                t.look_at(p, Vec3::Y);
                info!("SELFTEST: framing actor at {:?}", p);
            } else if std::env::var("SOILS_CAM").as_deref() == Ok("ground") {
                // Player-eye view: at the surface looking out to the horizon, to
                // judge the chunk-load boundary the way it's actually seen.
                t.translation = Vec3::new(282.0, 273.0, 268.0);
                t.look_at(Vec3::new(360.0, 271.0, 300.0), Vec3::Y);
                info!("SELFTEST: ground camera at {:?}", t.translation);
            } else {
                // Natural horizon view: terrain fills the lower frame, sky the
                // upper, so atmosphere + terrain can be judged together.
                t.translation = Vec3::new(240.0, 280.0, 268.0);
                t.look_at(Vec3::new(320.0, 264.0, 290.0), Vec3::Y);
                info!("SELFTEST: camera at {:?} looking_at terrain", t.translation);
            }
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

/// In self-test mode, pin the time of day so screenshots are deterministic
/// (the server's clock drifts with wall-time). `SOILS_DAYTIME` overrides the
/// default noon (0.0); e.g. 0.25 = dawn/dusk, 0.5 = midnight.
fn self_test_daytime(mut world_time: ResMut<WorldTime>) {
    if std::env::var("SOILS_SELFTEST").is_err() {
        return;
    }
    let d = std::env::var("SOILS_DAYTIME").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    world_time.daytime = d;
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
    if time.elapsed_secs() > env_secs("SOILS_EXIT_SECS", 11.0) {
        let chunks = map.map.len();
        let meshes = meshed.iter().count();
        let actors = remote_actors.iter().count();
        info!("SELFTEST: {chunks} chunks loaded, {meshes} chunk meshes built, {actors} actors");
        // The login-screen shot (`SOILS_LOGINSHOT`) has no world by design, so
        // skip the world asserts there and just exit cleanly after the shot.
        if std::env::var("SOILS_LOGINSHOT").is_err() {
            assert!(chunks > 0, "no chunks streamed from server");
            assert!(meshes > 0, "no chunk meshes were built");
        }
        info!("SELFTEST PASSED");
        exit.write(AppExit::Success);
    }
}

/// Read a float from an env var, or fall back to `default`. Used to let CI
/// stretch the self-test's screenshot/exit deadlines for slow software GPUs.
fn env_secs(key: &str, default: f32) -> f32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Spawn the camera/player and the sun.
fn setup(mut commands: Commands, mut mediums: ResMut<Assets<ScatteringMedium>>) {
    commands.spawn((
        Camera3d::default(),
        Projection::from(PerspectiveProjection {
            fov: 65.0_f32.to_radians(),
            // A sane far plane keeps reverse-Z depth precise; an enormous one
            // (the old 2e6) crushes near-terrain depth toward 0.
            far: 8_000.0,
            ..default()
        }),
        // Provisional spawn; corrected by the server's `Init` message. The
        // rotation matches the Player's default yaw/pitch (looking down-forward)
        // so the view is sensible before any mouse input.
        Transform::from_xyz(282.0, 285.0, 268.0)
            .with_rotation(Quat::from_axis_angle(Vec3::X, -0.5)),
        Player::default(),
        // Physically-based sky. `Atmosphere` requires (and auto-inserts) `Hdr`;
        // pair it with a tonemapper, an exposure the day/night cycle drives, and
        // sky-derived image-based lighting for the lit actors. 1 world unit ==
        // 1 block ~= 1 metre, so the default `scene_units_to_m` is correct.
        //
        // NOTE: no `Bloom` — with our unlit, manually-exposed terrain the bright
        // HDR sky bloom washes the whole frame to a flat haze regardless of
        // prefilter threshold; the atmosphere still draws the sun disc itself.
        Atmosphere::earthlike(mediums.add(ScatteringMedium::default())),
        AtmosphereSettings::default(),
        AtmosphereEnvironmentMapLight::default(),
        Exposure { ev100: EV100_DAY },
        Tonemapping::AcesFitted,
    ));

    commands.spawn((
        Sun,
        // RAW (pre-atmosphere) sunlight is the correct input for the atmosphere
        // to filter; `day_night` rotates it and dims it toward night.
        DirectionalLight { illuminance: lux::RAW_SUNLIGHT, shadows_enabled: false, ..default() },
        Transform::default(),
    ));
}

/// In self-test mode there's no login screen, so auto-authenticate as a guest.
fn selftest_login(net: Res<NetClient>) {
    if gi_demo::demo_enabled() {
        return; // demo builds a local scene; no server/login
    }
    if std::env::var("SOILS_SELFTEST").is_ok() && std::env::var("SOILS_LOGINSHOT").is_err() {
        net.connect("ws://127.0.0.1:9001".into());
        net.send(ClientMsg::Login { name: "player".into(), password: String::new(), signup: true });
    }
}

/// Bundled remote-actor params, so `net_receive` stays under the 16-param limit.
#[derive(SystemParam)]
struct ActorCtx<'w, 's> {
    map: ResMut<'w, ActorMap>,
    assets: Res<'w, ActorAssets>,
    actors: Query<'w, 's, &'static mut Actor>,
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
    mut player: Query<(&mut Transform, &mut Player)>,
    mut local: ResMut<LocalPlayer>,
    mut actor_ctx: ActorCtx,
    toggles: Res<pause::RenderToggles>,
    mut streaming: ResMut<Streaming>,
    mut login_state: ResMut<login::LoginState>,
    gi: Res<gi::GiAssets>,
) {
    let gi_cascade0 = gi.cascade0();
    let (gi_origin, gi_enabled) = gi.apply_params();
    let chunk_params = material::AtlasParams {
        ambient_occlusion: if toggles.ao { 1.0 } else { 0.0 },
        fog_density: if toggles.fog { material::FOG_DENSITY } else { 0.0 },
        gi_origin,
        gi_enabled,
        ..default()
    };
    for ev in net.drain() {
        let msg = match ev {
            NetEvent::Connected => {
                login_state.status = "connected".into();
                continue;
            }
            NetEvent::ConnectFailed(e) => {
                login_state.status = format!("could not reach server: {e}");
                continue;
            }
            NetEvent::Msg(msg) => msg,
        };
        match msg {
            ServerMsg::Init { id, spawn, daytime, .. } => {
                local.id = id;
                world_time.daytime = daytime;
                login_state.done = true; // authenticated — drop the login screen
                if let Ok((mut transform, _)) = player.single_mut() {
                    transform.translation = Vec3::from_array(spawn);
                }
            }
            ServerMsg::LoginError { message } => {
                login_state.status = message;
            }
            ServerMsg::Chunk { pos, empty, voxels } => {
                apply_chunk(
                    &mut commands, &mut map, &mut chunks, &mut gpu_chunks, &mut buffers,
                    &mut materials, &atlas, &chunk_params, &gi_cascade0, pos, empty, voxels,
                );
            }
            ServerMsg::Bundle { chunks: datas } => {
                for d in datas {
                    apply_chunk(
                        &mut commands, &mut map, &mut chunks, &mut gpu_chunks, &mut buffers,
                        &mut materials, &atlas, &chunk_params, &gi_cascade0, d.pos, d.empty, d.voxels,
                    );
                }
            }
            ServerMsg::Edit { pos, value } => {
                let v = IVec3::from_array(pos);
                edit::apply_edit(&map, &mut chunks, &mut gpu_chunks, &mut buffers, v, value);
            }
            ServerMsg::Time { daytime } => {
                world_time.daytime = daytime;
            }
            ServerMsg::Warp { spawn, daytime } => {
                // Drop the old world entirely and re-stream the new one.
                for (_, entity) in map.map.drain() {
                    commands.entity(entity).despawn();
                }
                for (_, entity) in actor_ctx.map.map.drain() {
                    commands.entity(entity).despawn();
                }
                world_time.daytime = daytime;
                if let Ok((mut transform, mut p)) = player.single_mut() {
                    transform.translation = Vec3::from_array(spawn);
                    p.velocity = Vec3::ZERO;
                }
                streaming.last_chunk = None; // force a fresh stream
            }
            ServerMsg::Position { pos } => {
                // Server rejected our movement — snap back.
                if let Ok((mut transform, mut p)) = player.single_mut() {
                    transform.translation = Vec3::from_array(pos);
                    p.velocity = Vec3::ZERO;
                }
            }
            ServerMsg::ActorUpdate { actors: states } => {
                for state in states {
                    if state.id == local.id {
                        continue; // don't render ourselves
                    }
                    let target = Vec3::from_array(state.pos);
                    if let Some(&entity) = actor_ctx.map.map.get(&state.id) {
                        if let Ok(mut actor) = actor_ctx.actors.get_mut(entity) {
                            actor.target = target;
                        }
                    } else {
                        let entity = commands
                            .spawn((
                                Actor { target },
                                Mesh3d(actor_ctx.assets.mesh.clone()),
                                MeshMaterial3d(actor_ctx.assets.material.clone()),
                                Transform::from_translation(target - Vec3::Y * 0.9),
                            ))
                            .id();
                        actor_ctx.map.map.insert(state.id, entity);
                    }
                }
            }
            ServerMsg::ActorRemove { id } => {
                if let Some(entity) = actor_ctx.map.map.remove(&id) {
                    commands.entity(entity).despawn();
                }
            }
        }
    }
}

/// Apply one streamed chunk: update an existing chunk's voxels or spawn a new
/// (meshed or empty-tracked) chunk entity. Shared by `Chunk` and `Bundle`.
#[allow(clippy::too_many_arguments)]
fn apply_chunk(
    commands: &mut Commands,
    map: &mut ChunkMap,
    chunks: &mut Query<&mut VoxelChunk>,
    gpu_chunks: &mut Query<&mut GpuChunk>,
    buffers: &mut Assets<ShaderStorageBuffer>,
    materials: &mut Assets<ChunkMeshMaterial>,
    atlas: &AtlasAssets,
    params: &material::AtlasParams,
    gi_cascade0: &Handle<ShaderStorageBuffer>,
    pos: [i32; 3],
    empty: bool,
    voxels: Vec<u8>,
) {
    let cpos = IVec3::from_array(pos);
    let volume = if empty { ChunkVolume::empty() } else { ChunkVolume::from_bytes(&voxels) };
    if let Some(&entity) = map.map.get(&cpos) {
        // Existing chunk: update CPU copy + re-upload voxels if it has a GPU
        // mesh, else (was empty) leave as-is.
        if let Ok(mut vc) = chunks.get_mut(entity) {
            vc.volume = volume.clone();
        }
        if !empty {
            if let Ok(mut gc) = gpu_chunks.get_mut(entity) {
                gpu_mesh::refresh_gpu_chunk(buffers, &mut gc, &volume);
            }
        }
    } else if empty {
        // Track empty chunks so they aren't re-requested; no mesh.
        let e = commands.spawn(VoxelChunk { pos: cpos, volume }).id();
        map.map.insert(cpos, e);
    } else {
        let e = gpu_mesh::spawn_gpu_chunk(
            commands, buffers, materials, atlas, cpos, volume, params.clone(), gi_cascade0.clone(),
        );
        map.map.insert(cpos, e);
    }
}

/// Day-length easing ported from the JS `ease10`: a steep ease-in/out that
/// holds bright through midday and dark through midnight.
fn ease10(t: f32) -> f32 {
    let v = if t < 0.5 {
        512.0 * t.powi(10)
    } else {
        -512.0 * (t - 1.0).powi(10) + 1.0
    };
    v.clamp(0.0, 1.0)
}

/// Swing the sun with the day/night cycle and dim the world toward night.
/// JS convention: `daytime` 0.0 = noon (sun overhead), 0.5 = midnight.
fn day_night(
    world_time: Res<WorldTime>,
    mut sun: Query<(&mut Transform, &mut DirectionalLight), With<Sun>>,
    mut exposure: Query<&mut Exposure, With<Player>>,
) {
    // JS: theta = PI*(dayTime*2 - 0.5); the sun sweeps the Y-Z plane. The light
    // travels in `dir` (straight down at noon); a small +X tilt keeps it off the
    // exact vertical / antiparallel singularities of `look_to`.
    let theta = std::f32::consts::PI * (world_time.daytime * 2.0 - 0.5);
    let dir = Vec3::new(0.15, theta.sin(), theta.cos()).normalize();
    // Daylight factor: 1 at noon, 0 at midnight (JS `ease10(dayTime*2 - 1)`).
    let day = ease10(world_time.daytime * 2.0 - 1.0);

    if let Ok((mut transform, mut light)) = sun.single_mut() {
        transform.look_to(dir, Vec3::Y);
        light.illuminance = lux::RAW_SUNLIGHT * (0.02 + 0.98 * day);
    }
    if let Ok(mut exp) = exposure.single_mut() {
        exp.ev100 = EV100_NIGHT + (EV100_DAY - EV100_NIGHT) * day;
    }
}
