//! Bevy client for the new-soils Rust port: connects to the server, streams and
//! meshes chunks, renders them with the atlas material, and runs the
//! first-person player + block editing.
//!
//! Simulation runs in `FixedUpdate` at `soils_sim::TICK_HZ` on the shared
//! `soils-sim` crate; server messages arrive as typed Bevy messages routed by
//! `server_msg` (one consumer system per type instead of one god-system).

mod actor;
mod bot;
mod chunk;
mod console;
mod cull;
mod demand;
mod discovery;
mod edit;
mod gi;
mod gi_demo;
mod gpu_gen;
mod gpu_light;
mod gpu_mesh;
mod hud;
mod inventory;
mod light;
mod login;
mod material;
mod net;
mod pause;
mod physics;
mod player;
mod pool;
mod record;
mod world_draw;
mod server_msg;
mod singleplayer;
mod theme;
mod social;
mod ui;

use bevy::app::{RunFixedMainLoop, RunFixedMainLoopSystems};
use bevy::camera::{Exposure, Hdr};
use bevy::core_pipeline::tonemapping::Tonemapping;
// Bevy 0.19 moved the atmosphere *description* types into `bevy_light`; the
// render-side `AtmosphereSettings` stayed in `bevy_pbr`.
use bevy::light::{
    Atmosphere, AtmosphereEnvironmentMapLight, atmosphere::ScatteringMedium, light_consts::lux,
};
use bevy::pbr::AtmosphereSettings;
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use soils_protocol::ClientMsg;
use soils_worldgen::default_registry;

use actor::{Actor, ActorMap, LocalPlayer};
use chunk::{Blocks, ChunkMap, WorldTime};
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
    // Everything this client draws is 16x16 pixel art magnified hard, so the
    // linear default is never what's wanted. `load_atlas` pins the block atlas
    // itself; this is the backstop for anything loaded without settings.
    app.add_plugins(DefaultPlugins.set(ImagePlugin::default_nearest()).set(WindowPlugin {
        primary_window: Some(Window {
            // The player name is in the title so two clients of the same
            // executable are separable — OBS matches capture sources by
            // window title, and identical titles capture the same window twice.
            title: match std::env::var("SOILS_NAME") {
                Ok(n) if !n.is_empty() => format!("new-soils [{n}]"),
                _ => "new-soils (Rust/Bevy)".into(),
            },
            // `SOILS_VSYNC=0` uncaps the frame clock. Perf runs need this: with
            // vsync on, fps just reports the display refresh and says nothing
            // about how much headroom the frame actually has.
            present_mode: if std::env::var("SOILS_VSYNC").as_deref() == Ok("0") {
                bevy::window::PresentMode::AutoNoVsync
            } else {
                bevy::window::PresentMode::AutoVsync
            },
            // `SOILS_NOFOCUS=1` spawns the window visible but without taking
            // focus, so a perf run doesn't steal the desktop while you work.
            // Prefer this over `SOILS_HEADLESS` for measurements: the window
            // still presents through the normal swapchain path.
            focused: std::env::var("SOILS_NOFOCUS").as_deref() != Ok("1"),
            // `SOILS_HEADLESS=1` leaves the window unmapped entirely. Rendering
            // and `Screenshot::primary_window` still work (useful for CI), but
            // an unmapped window takes a different present path and measured
            // ~2 ms/frame slower here — do NOT use it for perf numbers.
            visible: std::env::var("SOILS_HEADLESS").as_deref() != Ok("1"),
            ..default()
        }),
        ..default()
    }))
    .add_plugins(GpuMeshPlugin)
    .add_plugins(pool::PoolPlugin)
    .add_plugins(world_draw::WorldDrawPlugin)
    .add_plugins(cull::CullPlugin)
    .add_plugins(gpu_light::GpuLightPlugin)
    .add_plugins(gpu_gen::GpuGenPlugin)
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
    .init_state::<ui::UiMode>()
    .init_resource::<inventory::PlayerInventory>()
    .init_resource::<inventory::Items>()
    .init_resource::<inventory::Hotbar>()
    .init_resource::<inventory::SelectedItem>()
    .init_resource::<inventory::hotbar::DragItem>()
    .init_resource::<inventory::container::OpenContainer>()
    .init_resource::<bot::BotActions>()
    .init_resource::<inventory::DroppedItemVisuals>()
    .init_resource::<ui::CursorFreed>()
    .insert_resource(pause::RenderToggles::default())
    .init_resource::<console::Console>()
    .init_resource::<login::LoginState>()
    .init_resource::<singleplayer::Singleplayer>()
    .init_resource::<player::LookSettings>()
    .init_resource::<player::PendingInput>()
    .init_resource::<player::InputRing>()
    .init_resource::<player::CameraHold>()
    .init_resource::<actor::InterpClock>()
    .init_resource::<edit::PendingEdits>()
    .init_resource::<light::LightQueue>()
    .init_resource::<light::PlayerLight>()
    .init_resource::<light::SkyTerm>()
    .insert_resource(net::connect())
    .insert_resource(discovery::spawn());

    // `SOILS_RENDERDIAG=1` records per-render-pass CPU/GPU elapsed time into the
    // DiagnosticsStore (dumped by the self-test at exit). Opt-in: it costs GPU
    // timestamp queries every pass, so it stays off for normal play.
    if std::env::var("SOILS_RENDERDIAG").as_deref() == Ok("1") {
        app.add_plugins(bevy::render::diagnostic::RenderDiagnosticsPlugin);
    }

    server_msg::register(&mut app);
    physics::register(&mut app);

    app.add_systems(
        Startup,
        (
            setup,
            actor::setup_actor_assets,
            edit::setup_crosshair,
            hud::setup_hud,
            pause::setup_pause_menu,
            console::setup_console,
            inventory::setup_item_icons,
            inventory::screen::setup_inventory_ui,
            inventory::hotbar::setup_hotbar,
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
                demand::apply_directory,
                server_msg::apply_edits,
                demand::maintain_cpu_mirror,
                demand::process_demands,
                gpu_gen::flush_gen_batch,
            )
                .chain()
                .after(server_msg::apply_init)
                .after(server_msg::apply_warp),
            (server_msg::apply_time, edit::apply_edit_acks, server_msg::apply_entity_spawns)
                .after(server_msg::apply_init)
                .after(server_msg::apply_warp),
            server_msg::flush_chunk_fetch.after(server_msg::route_server_messages),
            server_msg::apply_entity_updates.after(server_msg::apply_entity_spawns),
            server_msg::apply_entity_despawns.after(server_msg::apply_entity_updates),
            player::reconcile_self
                .after(server_msg::apply_init)
                .after(server_msg::apply_warp)
                .after(demand::process_demands),
            // The player's own emitter moves before the batch is planned, so
            // the voxel it vacated and the one it now occupies are both in
            // this frame's flood rather than a frame late.
            light::track_player_light.after(player::reconcile_self),
            // Light job planning runs once all voxel changes for the frame
            // landed (the flood itself is GPU compute — see gpu_light.rs).
            gpu_light::plan_light_jobs
                .after(demand::process_demands)
                .after(server_msg::apply_edits)
                .after(edit::edit_blocks)
                .after(light::track_player_light),
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
            // After the camera moves, so a captured frame matches the frame
            // the player would have seen.
            // After mouse_look too: it writes the camera rotation every frame
            // from accumulated look state, and would otherwise fight the
            // parked framing depending on system order.
            spectator_camera.after(player::sync_camera).after(player::mouse_look),
            record::cue.run_if(resource_exists::<record::CaptureCue>),
        ),
    )
    // A second block: Bevy's system tuples cap at 20 elements.
    .add_systems(
        Update,
        (
            social::refresh,
            hud::update_chat.after(social::refresh),
            social::link_identity.run_if(login::logged_in),
            bot::chatter.run_if(bot::active),
        ),
    )
    // Gameplay: only once authenticated.
    .add_systems(
        Update,
        (
            player::track_streaming,
            // Bot presses land before anything reads them, and outside the
            // `ui::playing` gate below — otherwise the bot could open the
            // inventory and never press the key that closes it again.
            bot::press_bot_buttons
                .run_if(bot::active)
                .before(ui::ui_hotkeys)
                .before(edit::edit_blocks),
            ui::track_alt,
            ui::ui_hotkeys.run_if(console::console_closed),
            ui::click_to_grab,
            ui::apply_cursor_mode,
            console::console_input,
            console::update_console_text,
            hud::update_hud,
            pause::pause_menu_visibility,
            pause::pause_menu_buttons,
            pause::update_pause_labels,
        )
            .run_if(login::logged_in),
    )
    // Inventory: the mirror changed, so settle the hotbar and redraw both
    // views. Its own block rather than folded into the gameplay tuple above —
    // `add_systems` takes at most twenty systems in one tuple, and that one was
    // already close to the limit.
    .add_systems(
        Update,
        (
            inventory::screen::update_inventory_visibility,
            inventory::hotbar::update_hotbar_visibility,
            // Reconcile first: both rebuilds draw what it settles on, so the
            // other order shows the bar one frame stale every time an item runs
            // out.
            inventory::hotbar::reconcile_hotbar,
            inventory::hotbar::rebuild_hotbar,
            inventory::screen::rebuild_inventory_ui,
            inventory::screen::select_item,
            inventory::screen::highlight_item_cells,
            inventory::screen::forget_missing_selection,
            inventory::screen::rebuild_detail_panel,
            inventory::hotbar::animate_wiggle,
            inventory::container::update_container_visibility,
            inventory::container::rebuild_container_ui,
            inventory::container::close_on_exit,
            inventory::screen::update_footer_hint,
        )
            .chain()
            .run_if(login::logged_in),
    )
    // Direct player input: authenticated and console closed.
    .add_systems(
        Update,
        (
            player::mouse_look,
            edit::edit_blocks,
            edit::selection_highlight,
            inventory::hotbar::select_hotbar_slot,
            inventory::hotbar::drop_selected,
        )
            .run_if(ui::playing)
            .run_if(console::console_closed)
            .run_if(login::logged_in),
    )
    // With the screen open the number keys assign the picked item to a key
    // instead of choosing between keys, so this deliberately does not share the
    // `ui::playing` gate above.
    .add_systems(
        Update,
        inventory::hotbar::bind_selected_to_hotbar
            .run_if(in_state(ui::UiMode::Inventory))
            .run_if(console::console_closed)
            .run_if(login::logged_in),
    )
    // Fixed-tick simulation: gather input just before the fixed loop (freshest
    // input, no frame of latency), step inside it.
    .add_systems(
        RunFixedMainLoop,
        (
            player::collect_input
                .run_if(not(bot::active))
                .run_if(console::console_closed),
            // Same slot as the keyboard it stands in for: freshest input, no
            // frame of latency before the fixed tick consumes it.
            bot::drive.run_if(bot::active),
            bot::inventory_actions.run_if(bot::active),
        )
            .in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop)
            .run_if(login::logged_in),
    )
    .add_systems(
        FixedUpdate,
        player::predict_and_send.run_if(login::logged_in).run_if(console::console_closed),
    );
    // Only present when SOILS_READY_FILE is set; `record::cue` is gated on it.
    if let Some(cue) = record::configured() {
        app.insert_resource(cue);
    }
    app.insert_resource(social::configured());
    // Only present when SOILS_BOT names a role.
    if let Some(b) = bot::configured() {
        app.insert_resource(b);
    }
    app.run();
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
    slots: Res<pool::ChunkSlots>,
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
        for (cpos, slot) in slots.iter() {
            if slot.mesh == pool::NO_MESH {
                continue;
            }
            if sample < 3 {
                info!("SELFTEST: meshed chunk {cpos:?} in mesh slot {}", slot.mesh);
            }
            sample += 1;
        }
        info!("SELFTEST: {sample} chunks currently have mesh slots");
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk("/tmp/soils-selftest.png"));
        info!("SELFTEST: screenshot requested");
    }
}

/// `SOILS_SPECTATE=px,py,pz,tx,ty,tz` parks the camera at `p` looking at `t`
/// and holds it there, turning the client into a fixed camera on the world.
///
/// Recording player-vs-player interaction needs a viewpoint that is not one of
/// the participants: a first-person camera cannot show two bodies meeting, and
/// driving a third player by hand would not be reproducible.
fn spectator_camera(
    mut hold: ResMut<player::CameraHold>,
    mut camera: Query<(&mut Player, &mut Transform)>,
) {
    let Ok(spec) = std::env::var("SOILS_SPECTATE") else { return };
    let v: Vec<f32> = spec.split(',').filter_map(|f| f.trim().parse().ok()).collect();
    if v.len() != 6 {
        return;
    }
    // Held every frame, not once: the server keeps echoing this client's own
    // authoritative position, which would otherwise drag the camera back.
    hold.0 = true;
    if let Ok((mut p, mut t)) = camera.single_mut() {
        let eye = Vec3::new(v[0], v[1], v[2]);
        player::teleport(&mut p, &mut t, eye);
        t.look_at(Vec3::new(v[3], v[4], v[5]), Vec3::Y);
    }
}

/// In self-test mode, pin the time of day so screenshots are deterministic
/// (the server's clock drifts with wall-time). `SOILS_DAYTIME` overrides the
/// default noon (0.0); e.g. 0.25 = dawn/dusk, 0.5 = midnight.
fn self_test_daytime(mut world_time: ResMut<WorldTime>) {
    // Honour SOILS_DAYTIME on its own, not only under SOILS_SELFTEST: a
    // recording session needs the light pinned too, or the take drifts into
    // night partway through.
    let explicit = std::env::var("SOILS_DAYTIME").ok().and_then(|v| v.parse::<f32>().ok());
    if explicit.is_none() && std::env::var("SOILS_SELFTEST").is_err() {
        return;
    }
    world_time.daytime = explicit.unwrap_or(0.0);
}

/// When `SOILS_SELFTEST` is set, report how much of the world streamed in and
/// meshed after a few seconds, then exit. Lets the full client path (connect →
/// stream → mesh → render) be validated headlessly under xvfb + lavapipe.
fn self_test(
    time: Res<Time>,
    slots: Res<pool::ChunkSlots>,
    remote_actors: Query<&Actor>,
    diagnostics: Res<bevy::diagnostic::DiagnosticsStore>,
    light_queue: Res<light::LightQueue>,
    mut exit: MessageWriter<AppExit>,
) {
    if std::env::var("SOILS_SELFTEST").is_err() {
        return;
    }
    if time.elapsed_secs() > env_secs("SOILS_EXIT_SECS", 11.0) {
        let chunks = slots.len();
        let meshes = slots.iter().filter(|(_, s)| s.mesh != pool::NO_MESH).count();
        let actors = remote_actors.iter().count();
        // Steady-state frame cost, sampled at exit (the camera parked at the
        // screenshot deadline, so recent frames are the static viewpoint, not
        // the join burst). Reported to stdout so perf runs are scriptable and
        // diffable instead of being read off the F3 HUD in a screenshot.
        let fps = diagnostics
            .get(&bevy::diagnostic::FrameTimeDiagnosticsPlugin::FPS)
            .and_then(|d| d.smoothed())
            .unwrap_or(0.0);
        let frame_ms = diagnostics
            .get(&bevy::diagnostic::FrameTimeDiagnosticsPlugin::FRAME_TIME)
            .and_then(|d| d.smoothed())
            .unwrap_or(0.0);
        info!("SELFTEST PERF: {fps:.1} fps, {frame_ms:.2} ms/frame");
        // With SOILS_RENDERDIAG=1, break the frame down per render pass so the
        // bottleneck is measured rather than guessed. Only the elapsed timers —
        // the pipeline-statistics counters (shader invocations etc.) live in the
        // same store but are counts, not milliseconds. Sorted slowest-first.
        let mut passes: Vec<(String, f64)> = diagnostics
            .iter()
            .filter(|d| {
                let p = d.path().as_str();
                p.starts_with("render/") && (p.ends_with("elapsed_gpu") || p.ends_with("elapsed_cpu"))
            })
            .filter_map(|d| d.smoothed().map(|v| (d.path().to_string(), v)))
            .filter(|(_, v)| *v > 0.001)
            .collect();
        passes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (path, ms) in passes.iter().take(24) {
            info!("SELFTEST PASS: {ms:8.3} ms  {path}");
        }
        let (lq_chunks, lq_edits, lq_pads) = light_queue.backlog();
        info!(
            "SELFTEST LIGHT BACKLOG: {lq_chunks} chunks to flood, {lq_edits} edits, {lq_pads} pads \
             pending (non-zero ⇒ still draining, fps above is not steady state)"
        );
        info!("SELFTEST: {chunks} chunks loaded, {meshes} chunk mesh slots, {actors} actors");
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
    let camera = commands
        .spawn((
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
        // Bevy 0.19 splits the sky in two: `Atmosphere` on the planet entity
        // (below), `AtmosphereSettings` on the camera. No `Bloom` — over unlit,
        // manually-exposed terrain it hazes the whole frame at any threshold.
        Hdr,
        Exposure { ev100: EV100_DAY },
        Tonemapping::AcesFitted,
        ))
        .id();

    // Sky + sky-derived image-based lighting for the lit actors. Measured
    // ~0.7 ms/frame combined on an RTX 5070 (0.55 env-map + 0.16 sky), and the
    // env-map cost is *not* resolution-bound — dropping `size` from 512 to 64
    // changed nothing, so it is fixed per-frame probe overhead.
    commands
        .entity(camera)
        .insert((AtmosphereSettings::default(), AtmosphereEnvironmentMapLight::default()));

    // The planet. Its `GlobalTransform` *is* the planet centre, so this must be
    // its own entity: left on the camera (which carries a real `Transform`) the
    // `on_add` hook that sinks the planet to `-inner_radius` on Y is skipped,
    // the centre lands on the camera, and the sky renders as a flat haze with
    // black aerial-perspective artefacts above the horizon.
    commands.spawn(Atmosphere::earth(mediums.add(ScatteringMedium::default())));

    commands.spawn((
        Sun,
        // RAW (pre-atmosphere) sunlight is the correct input for the atmosphere
        // to filter; `day_night` rotates it and dims it toward night.
        DirectionalLight { illuminance: lux::RAW_SUNLIGHT, shadow_maps_enabled: false, ..default() },
        Transform::default(),
    ));
}

/// Skip the login screen and authenticate as a guest.
///
/// Driven by `SOILS_SELFTEST` (fixed `127.0.0.1:9001`, user `player`) or by
/// `SOILS_AUTOLOGIN=<host:port>` with an optional `SOILS_NAME`. The latter
/// exists so a recording session can point a spectating client at a server on
/// an ephemeral port without anyone typing into a dialog.
fn selftest_login(net: Res<NetClient>) {
    if gi_demo::demo_enabled() {
        return; // demo builds a local scene; no server/login
    }
    let addr = match std::env::var("SOILS_AUTOLOGIN") {
        Ok(a) if !a.is_empty() => a,
        _ => {
            if std::env::var("SOILS_SELFTEST").is_err()
                || std::env::var("SOILS_LOGINSHOT").is_ok()
            {
                return;
            }
            "127.0.0.1:9001".to_string()
        }
    };
    let name = std::env::var("SOILS_NAME").unwrap_or_else(|_| "player".into());
    let url = if addr.contains("://") {
        addr
    } else {
        format!("{}://{addr}", net::default_scheme())
    };
    info!("auto-login: {url} as {name}");
    net.connect(url);
    net.send(ClientMsg::Login {
        name,
        password: String::new(),
        signup: true,
        protocol: soils_protocol::PROTOCOL_VERSION,
    });
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
