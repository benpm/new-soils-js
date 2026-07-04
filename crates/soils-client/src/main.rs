//! Bevy client for the new-soils Rust port: connects to the server, streams and
//! meshes chunks, renders them with the atlas material, and runs the
//! first-person player + block editing.
//!
//! Simulation runs in `FixedUpdate` at `soils_sim::TICK_HZ` on the shared
//! `soils-sim` crate; server messages arrive as typed Bevy messages routed by
//! `server_msg` (one consumer system per type instead of one god-system).

mod actor;
mod chunk;
mod console;
mod discovery;
mod edit;
mod gi;
mod gi_demo;
mod gpu_mesh;
mod hud;
mod indirect_draw;
mod light;
mod login;
mod material;
mod net;
mod pause;
mod player;
mod server_msg;
mod singleplayer;

use bevy::app::{RunFixedMainLoop, RunFixedMainLoopSystems};
use bevy::camera::Exposure;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::light::{AtmosphereEnvironmentMapLight, light_consts::lux};
use bevy::pbr::{Atmosphere, AtmosphereSettings, ScatteringMedium};
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use soils_protocol::ClientMsg;
use soils_worldgen::default_registry;

use actor::{Actor, ActorMap, LocalPlayer};
use chunk::{Blocks, ChunkMap, VoxelChunk, WorldTime};
use gpu_mesh::GpuMeshPlugin;
use net::NetClient;
use player::{Player, Streaming};

/// Marks the sun so we can swing it with the day/night cycle.
#[derive(Component)]
struct Sun;

/// Camera exposure (EV100) at noon and midnight. Lower = brighter image; the
/// day/night cycle interpolates between them so the whole scene dims at night.
const EV100_DAY: f32 = 13.0;
const EV100_NIGHT: f32 = 16.5;

/// Provisional spawn position; corrected by the server's `Init` message.
const PROVISIONAL_SPAWN: Vec3 = Vec3::new(282.0, 285.0, 268.0);

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
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
    .insert_resource(Time::<Fixed>::from_hz(soils_sim::TICK_HZ))
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
    .init_resource::<player::PendingInput>()
    .init_resource::<player::InputRing>()
    .init_resource::<player::CameraHold>()
    .init_resource::<actor::InterpClock>()
    .init_resource::<edit::PendingEdits>()
    .init_resource::<light::LightQueue>()
    .init_resource::<light::SkyTerm>()
    .insert_resource(net::connect())
    .insert_resource(discovery::spawn());

    server_msg::register(&mut app);

    app.add_systems(
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
    // Server messages: route, then apply. Init/warp first (they (re)base the
    // world the other consumers apply into); actor updates strictly before
    // removes (see server_msg.rs).
    .add_systems(
        Update,
        (
            server_msg::route_server_messages,
            (
                server_msg::apply_init,
                server_msg::apply_warp,
                server_msg::apply_login_failed,
                server_msg::apply_net_status,
            )
                .after(server_msg::route_server_messages),
            (
                server_msg::apply_chunks,
                server_msg::apply_edits,
                server_msg::apply_time,
                edit::apply_edit_acks,
                server_msg::apply_entity_spawns,
            )
                .after(server_msg::apply_init)
                .after(server_msg::apply_warp),
            server_msg::apply_entity_updates.after(server_msg::apply_entity_spawns),
            server_msg::apply_entity_despawns.after(server_msg::apply_entity_updates),
            player::reconcile_self
                .after(server_msg::apply_init)
                .after(server_msg::apply_warp)
                .after(server_msg::apply_chunks),
            // Baked lighting runs once all voxel changes for the frame landed.
            light::process_light
                .after(server_msg::apply_chunks)
                .after(server_msg::apply_edits)
                .after(edit::edit_blocks),
            light::update_sky_term.after(server_msg::apply_time),
        ),
    )
    // Always-on: login flow, day/night, camera interpolation, self-test.
    .add_systems(
        Update,
        (
            login::login_keyboard,
            login::login_buttons,
            login::update_login_text,
            login::finish_login,
            discovery::discovery_poll,
            login::update_server_list,
            login::server_buttons,
            hud::toggle_hud,
            actor::interpolate_actors,
            player::sync_camera,
            self_test_daytime.after(server_msg::apply_time).before(day_night),
            day_night,
            self_test,
            screenshot_once.after(player::sync_camera),
            gi_demo::setup_gi_demo,
            gi_demo::gi_demo_keep_dirty,
        ),
    )
    // Gameplay: only once authenticated.
    .add_systems(
        Update,
        (
            player::track_streaming,
            player::cursor_toggle,
            edit::selection_highlight,
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
        (player::mouse_look, edit::edit_blocks, edit::hotbar_select)
            .run_if(console::console_closed)
            .run_if(login::logged_in),
    )
    // Fixed-tick simulation: gather input just before the fixed loop (freshest
    // input, no frame of latency), step inside it.
    .add_systems(
        RunFixedMainLoop,
        player::collect_input
            .in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop)
            .run_if(login::logged_in)
            .run_if(console::console_closed),
    )
    .add_systems(
        FixedUpdate,
        player::predict_and_send.run_if(login::logged_in).run_if(console::console_closed),
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
    mut camera: Query<(&mut Player, &mut Transform)>,
    mut hold: ResMut<player::CameraHold>,
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
        // The hold stops the server position echo from dragging the framed
        // camera back toward the player's authoritative position mid-capture.
        if !gi_demo::demo_enabled() {
            hold.0 = true;
            if let Ok((mut p, mut t)) = camera.single_mut() {
                if let Some(actor) = remote_actors.iter().next() {
                    // Frame a remote actor so its body is visible in the shot.
                    let target = actor.translation;
                    player::teleport(&mut p, &mut t, target + Vec3::new(4.0, 1.5, 4.0));
                    t.look_at(target, Vec3::Y);
                    info!("SELFTEST: framing actor at {:?}", target);
                } else if std::env::var("SOILS_CAM").as_deref() == Ok("ground") {
                    // Player-eye view: at the surface looking out to the horizon, to
                    // judge the chunk-load boundary the way it's actually seen.
                    player::teleport(&mut p, &mut t, Vec3::new(282.0, 273.0, 268.0));
                    t.look_at(Vec3::new(360.0, 271.0, 300.0), Vec3::Y);
                    info!("SELFTEST: ground camera at {:?}", t.translation);
                } else {
                    // Natural horizon view: terrain fills the lower frame, sky the
                    // upper, so atmosphere + terrain can be judged together.
                    player::teleport(&mut p, &mut t, Vec3::new(240.0, 280.0, 268.0));
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
        Transform::from_translation(PROVISIONAL_SPAWN)
            .with_rotation(Quat::from_axis_angle(Vec3::X, -0.5)),
        Player::at(PROVISIONAL_SPAWN),
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
    let day = soils_sim::ease10(world_time.daytime * 2.0 - 1.0);

    if let Ok((mut transform, mut light)) = sun.single_mut() {
        transform.look_to(dir, Vec3::Y);
        light.illuminance = lux::RAW_SUNLIGHT * (0.02 + 0.98 * day);
    }
    if let Ok(mut exp) = exposure.single_mut() {
        exp.ev100 = EV100_NIGHT + (EV100_DAY - EV100_NIGHT) * day;
    }
}
