//! A small command console (open with `/`), mirroring the JS command box.
//! Supported: `tp x y z`, `daytime t`, `loadradius n`, `sens n`, `playerlight n`,
//! `fog on|off`,
//! `ao on|off`, `gi on|off`, `debugviz on|off`, `wireframe on|off`,
//! `spawn`/`cube` (drop a physics cube ahead of the
//! camera). While open, gameplay input is suppressed (see `console_closed`).

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;

use soils_protocol::ClientMsg;

use crate::chunk::WorldTime;
use crate::debugviz::DebugViz;
use crate::gi::GiSettings;
use crate::net::NetClient;
use crate::light::PlayerLight;
use crate::pause::RenderToggles;
use crate::player::{self, LookSettings, PendingInput, Player, Streaming};

/// Console open-state and current input buffer.
#[derive(Resource, Default)]
pub struct Console {
    pub open: bool,
    pub buffer: String,
}

/// Run condition: gameplay input systems run only while the console is closed.
pub fn console_closed(console: Res<Console>) -> bool {
    !console.open
}

/// Marker for the console text bar.
#[derive(Component)]
pub struct ConsoleBar;

/// Spawn the (hidden) console bar along the bottom of the screen.
pub fn setup_console(mut commands: Commands) {
    commands.spawn((
        ConsoleBar,
        Visibility::Hidden,
        Text::new(""),
        TextFont { font_size: 16.0.into(), ..default() },
        TextColor(Color::WHITE),
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(8.0),
            left: Val::Px(8.0),
            padding: UiRect::axes(Val::Px(6.0), Val::Px(3.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
    ));
}

/// Handle console open/close, text entry, and command execution.
#[allow(clippy::too_many_arguments)]
pub fn console_input(
    mut events: MessageReader<KeyboardInput>,
    mut console: ResMut<Console>,
    mut pending: ResMut<PendingInput>,
    mut player: Query<(&mut Player, &mut Transform)>,
    mut world_time: ResMut<WorldTime>,
    mut streaming: ResMut<Streaming>,
    mut look: ResMut<LookSettings>,
    mut player_light: ResMut<PlayerLight>,
    mut toggles: ResMut<RenderToggles>,
    mut gi: ResMut<GiSettings>,
    mut viz: ResMut<DebugViz>,
    net: Res<NetClient>,
    social: Res<crate::social::Social>,
) {
    for ev in events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        if !console.open {
            // Open on `/`; the slash is the prompt, not part of the command.
            if ev.key_code == KeyCode::Slash {
                console.open = true;
                console.buffer.clear();
                // A jump/fly-toggle queued this frame must not fire on close.
                pending.clear_latches();
            }
            continue;
        }
        match &ev.logical_key {
            Key::Enter => {
                let cmd = std::mem::take(&mut console.buffer);
                console.open = false;
                run_command(
                    &cmd, &mut player, &mut world_time, &mut streaming, &mut look,
                    &mut player_light, &mut toggles, &mut gi, &mut viz, &net, &social,
                );
            }
            Key::Escape => {
                console.open = false;
                console.buffer.clear();
            }
            Key::Backspace => {
                console.buffer.pop();
            }
            Key::Space => console.buffer.push(' '),
            Key::Character(s) => console.buffer.push_str(s),
            _ => {}
        }
    }
}

/// Reflect the console state into its text bar.
pub fn update_console_text(
    console: Res<Console>,
    mut bar: Query<(&mut Text, &mut Visibility), With<ConsoleBar>>,
) {
    let Ok((mut text, mut vis)) = bar.single_mut() else { return };
    let want = if console.open { Visibility::Inherited } else { Visibility::Hidden };
    if *vis != want {
        *vis = want;
    }
    if console.open {
        text.0 = format!("/{}_", console.buffer);
    }
}

/// Parse and apply a single console command line.
#[allow(clippy::too_many_arguments)]
fn run_command(
    line: &str,
    player: &mut Query<(&mut Player, &mut Transform)>,
    world_time: &mut WorldTime,
    streaming: &mut Streaming,
    look: &mut LookSettings,
    player_light: &mut PlayerLight,
    toggles: &mut RenderToggles,
    gi: &mut GiSettings,
    viz: &mut DebugViz,
    net: &NetClient,
    social: &crate::social::Social,
) {
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else { return };
    let args: Vec<&str> = parts.collect();
    let on_off = |a: Option<&&str>| matches!(a, Some(&"on") | Some(&"1") | Some(&"true"));

    match cmd {
        "tp" | "teleport" if args.len() == 3 => {
            if let (Ok(x), Ok(y), Ok(z)) =
                (args[0].parse::<f32>(), args[1].parse::<f32>(), args[2].parse::<f32>())
            {
                if let Ok((mut p, mut t)) = player.single_mut() {
                    player::teleport(&mut p, &mut t, Vec3::new(x, y, z));
                }
                streaming.last_chunk = None; // re-stream around the new position
            }
        }
        "daytime" if !args.is_empty() => {
            if let Ok(t) = args[0].parse::<f32>() {
                world_time.daytime = t.rem_euclid(1.0);
            }
        }
        "loadradius" if !args.is_empty() => {
            if let Ok(n) = args[0].parse::<i32>() {
                streaming.load_radius = n.clamp(2, 8);
                streaming.last_chunk = None;
            }
        }
        // Multiplier over the base radians-per-count, clamped by `set`.
        "sens" | "sensitivity" if !args.is_empty() => {
            if let Ok(n) = args[0].parse::<f32>() {
                look.set(n);
            }
        }
        // Emission level of the player's own light, 0-15 (0 = off). The
        // emitter follows the camera's voxel; `track_player_light` refloods.
        "playerlight" if !args.is_empty() => {
            if let Ok(n) = args[0].parse::<i32>() {
                player_light.set_level(n);
                player_light.voxel = None; // force a reseed at the new level
            }
        }
        // The toggles flow into the terrain uniform every frame
        // (world_draw::update_terrain_params); flipping them is enough.
        "fog" => toggles.fog = on_off(args.first()),
        "ao" | "occlusion" => toggles.ao = on_off(args.first()),
        "light" => toggles.light = on_off(args.first()),
        "gi" => {
            gi.enabled = on_off(args.first());
        }
        // The debug view (F1) and its wireframe overlay (F2), for scripted
        // runs and screenshots — the self-test and the demo bots press no
        // keys. `wireframe on` opens the mode too, exactly as F2 does.
        "debugviz" | "debug" => viz.enabled = on_off(args.first()),
        "wireframe" | "wire" => {
            viz.wireframe = on_off(args.first());
            if viz.wireframe {
                viz.enabled = true;
            }
        }
        "warp" if !args.is_empty() => {
            // Server creates the world on demand and replies with `Warp`.
            net.send(ClientMsg::Warp { world: args[0].to_string() });
        }
        "spawn" | "cube" => {
            // Drop a physics cube a few metres ahead of the camera (server-gated
            // on SOILS_PHYSICS, reach-checked, rate-limited).
            if let Ok((_, t)) = player.single() {
                let pos = t.translation + t.forward() * 3.0;
                net.send(ClientMsg::SpawnCube { pos: pos.to_array() });
            }
        }
        "say" | "chat" if !args.is_empty() => {
            // Chat goes to SpacetimeDB directly, not through the game server:
            // the message is attributed to this client's own identity, which
            // the game server has no way to speak for. With no database
            // configured this is a no-op rather than an error — the game is
            // playable without a lobby.
            social.say(social.chat_world, &args.join(" "));
        }
        _ => {}
    }
}
