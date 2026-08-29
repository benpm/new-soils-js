//! Scripted input, so a real graphical client can play itself.
//!
//! Recording a first-person view means the client has to *be* the player —
//! following someone else's replicated body would show their interpolated,
//! delayed position, which is precisely not what that player sees. So this
//! drives `PendingInput` and the camera directly, in place of the keyboard,
//! and everything downstream (prediction, reconciliation, the local physics
//! mirror) runs exactly as it does for a person.
//!
//! Two clients running mirrored roles walk toward each other through the prop
//! pile and meet in the middle.
//!
//! Environment:
//! * `SOILS_BOT=a|b|inv` — role. `a` starts west facing east; `b` mirrors it.
//!   `inv` is a solo routine that demonstrates the inventory loop: place, mine,
//!   watch the drop fall, walk onto it, and open the inventory screen.
//! * `SOILS_BOT_START=<path>` — wait for this file before moving. Both clients
//!   watch the same one, so the two routines are choreographed against a single
//!   signal rather than each client's own stream-in time. Without it the takes
//!   drift apart and stop being comparable.

use std::path::PathBuf;

use bevy::prelude::*;

use crate::player::{PendingInput, Player};

const EAST: f32 = -std::f32::consts::FRAC_PI_2;
const WEST: f32 = std::f32::consts::FRAC_PI_2;

/// Look slightly down: the props are underfoot, and a level camera puts the
/// whole pile below the frame.
const PITCH: f32 = -0.13;

/// Fly outward from the shared spawn point for this long, so the two players
/// start on opposite sides of the pile.
const OUTBOUND_SECS: f32 = 1.2;
/// Drop out of fly mode here, then fall and settle.
const LAND_AT: f32 = 1.4;
/// Start walking inward here.
const WALK_AT: f32 = 6.0;
/// One advance-and-retreat cycle. Walking in and simply staying there fills
/// the frame with the other player's body for the rest of the take; backing
/// off and coming again makes the contact legible.
const CYCLE: f32 = 7.0;
/// Jump interval while walking.
const JUMP_EVERY: f32 = 1.2;
/// How much of a cycle is spent closing the distance.
const ADVANCE: f32 = 3.5;
/// When the retreat starts.
const RETREAT: f32 = 4.5;
/// End of the retreat. Deliberately much shorter than the advance: the advance
/// is cut short by the other player's body, so a symmetric retreat walks the
/// pair a little further apart every cycle until they have left the prop pile
/// behind entirely.
const RETREAT_END: f32 = 5.7;

#[derive(Clone, Copy, PartialEq)]
enum Role {
    A,
    B,
    /// Solo inventory demonstration. Ignores the mirrored walk entirely.
    Inventory,
}

impl Role {
    /// Facing while flying clear of spawn.
    fn outbound(self) -> f32 {
        match self {
            Role::A | Role::Inventory => WEST,
            Role::B => EAST,
        }
    }
    /// Facing while walking back through the pile toward the other player.
    fn inbound(self) -> f32 {
        match self {
            Role::A | Role::Inventory => EAST,
            Role::B => WEST,
        }
    }
}

/// Seconds between scripted chat lines.
const CHAT_EVERY: f32 = 4.0;

/// What each role says, in order, cycling.
const LINES_A: &[&str] = &[
    "hey, you there?",
    "server list came from the database",
    "watch this jump",
    "nice cubes",
];
const LINES_B: &[&str] =
    &["reading you", "same registry here", "go on then", "they barely moved"];
const LINES_INV: &[&str] = &[
    "placing one from the kit",
    "mining it back",
    "it drops where it stood",
    "walk over it to collect",
];

#[derive(Resource)]
pub struct Bot {
    role: Role,
    start_file: Option<PathBuf>,
    started: Option<f32>,
    toggled_fly: bool,
    jumps: u32,
    lines_said: u32,
    /// Index of the last inventory-demo action performed, so each fires once.
    beat: usize,
    /// When the inventory routine's feet first touched ground. The script is
    /// timed from here, not from the start signal: how long the fall takes
    /// depends on the terrain under the spawn point, and a script that assumes
    /// a fixed fall plays its first beats in mid-air.
    landed: Option<f32>,
}

/// The configured bot, if `SOILS_BOT` names a role.
pub fn configured() -> Option<Bot> {
    let role = match std::env::var("SOILS_BOT").ok()?.to_ascii_lowercase().as_str() {
        "a" => Role::A,
        "b" => Role::B,
        "inv" | "inventory" => Role::Inventory,
        other => {
            error!("SOILS_BOT: unknown role {other:?} (expected a, b or inv)");
            return None;
        }
    };
    Some(Bot {
        role,
        start_file: std::env::var("SOILS_BOT_START").ok().map(PathBuf::from),
        started: None,
        toggled_fly: false,
        jumps: 0,
        lines_said: 0,
        beat: 0,
        landed: None,
    })
}

/// True while a bot is driving, so the keyboard path can stand down.
pub fn active(bot: Option<Res<Bot>>) -> bool {
    bot.is_some()
}

/// Replace keyboard input with the scripted routine and aim the camera.
pub fn drive(
    time: Res<Time>,
    mut bot: ResMut<Bot>,
    mut pending: ResMut<PendingInput>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    let Ok((mut player, mut transform)) = query.single_mut() else { return };
    let now = time.elapsed_secs();

    let started = match bot.started {
        Some(t) => t,
        None => {
            // Hold still, facing the direction we will fly, until the signal.
            let waiting = bot.start_file.as_ref().is_some_and(|p| !p.exists());
            aim(&mut player, &mut transform, bot.role.outbound());
            hold(&mut pending, Vec2::ZERO, player.yaw);
            if waiting {
                return;
            }
            info!("bot: routine started at {now:.1}s");
            bot.started = Some(now);
            now
        }
    };

    let t = now - started;
    let mut axes = Vec2::ZERO;

    if bot.role == Role::Inventory {
        drive_inventory(t, &mut bot, &mut pending, &mut player, &mut transform);
        return;
    }

    if t < OUTBOUND_SECS {
        // Still flying: move clear of the shared spawn point.
        aim(&mut player, &mut transform, bot.role.outbound());
        axes = Vec2::new(0.0, 1.0);
    } else if t < LAND_AT {
        aim(&mut player, &mut transform, bot.role.outbound());
    } else {
        // Turn to face the other player while falling, then walk in.
        aim(&mut player, &mut transform, bot.role.inbound());
        if !bot.toggled_fly {
            bot.toggled_fly = true;
            // Latched, not assigned: a frame with no fixed tick would
            // otherwise drop the edge and the player would never land.
            pending.input.toggle_fly = true;
        }
        if t >= WALK_AT {
            // Always face the other player; approach forwards and retreat
            // backwards, so they stay in frame the whole time.
            let phase = (t - WALK_AT) % CYCLE;
            axes = if phase < ADVANCE {
                Vec2::new(0.0, 1.0)
            } else if (RETREAT..RETREAT_END).contains(&phase) {
                Vec2::new(0.0, -1.0)
            } else {
                Vec2::ZERO
            };
            // Hop steadily rather than once a cycle. The terrain around
            // spawn is stepped, and a walking player cannot climb a one-block
            // ledge — without regular jumps the pair simply stall against the
            // nearest ridge and never reach each other.
            let due = ((t - WALK_AT) / JUMP_EVERY) as u32 + 1;
            if due > bot.jumps {
                bot.jumps = due;
                pending.input.jump = true;
            }
        }
    }

    hold(&mut pending, axes, player.yaw);
}

/// Write the *held* part of the input, leaving the edge latches alone.
///
/// `jump` and `toggle_fly` are consumed and cleared by the fixed tick
/// (`clear_latches`), which does not run on every frame. Assigning a whole
/// `PlayerInput` each frame would wipe a latch set moments earlier — the
/// keyboard path ORs them in for exactly this reason, and a bot that assigns
/// instead of latching silently never jumps and never leaves fly mode.
fn hold(pending: &mut PendingInput, axes: Vec2, yaw: f32) {
    pending.input.move_axes = axes;
    pending.input.yaw = yaw;
    pending.input.sprint = false;
    pending.input.up = false;
    pending.input.down = false;
}

/// Say a scripted line every few seconds once the routine is running.
///
/// Chat goes straight to SpacetimeDB as this client's own identity, so this is
/// also what proves the client half of the integration is live: nothing here
/// travels through the game server.
pub fn chatter(
    time: Res<Time>,
    mut bot: ResMut<Bot>,
    social: Res<crate::social::Social>,
) {
    let Some(started) = bot.started else { return };
    if !social.enabled() {
        return;
    }
    let due = ((time.elapsed_secs() - started) / CHAT_EVERY) as u32;
    if due < bot.lines_said {
        return;
    }
    bot.lines_said = due + 1;
    let lines = match bot.role {
        Role::A => LINES_A,
        Role::B => LINES_B,
        Role::Inventory => LINES_INV,
    };
    // Role B answers half a beat later, so the two do not collide on the
    // module's per-account chat cooldown and the exchange reads as a
    // conversation rather than two monologues.
    let idx = (due as usize) % lines.len();
    social.say(social.chat_world, lines[idx]);
}


// --- inventory demonstration -------------------------------------------------

/// Earliest the routine may begin, in seconds after the start signal. The
/// script actually waits for `grounded`; this only stops a frame of spurious
/// ground contact during the first tick from starting it early.
const INV_LAND_BY: f32 = 2.0;
/// Pitch while working: steeper than the walking pitch, so the block being
/// placed and the item that drops from it are both in frame.
const INV_PITCH: f32 = -0.62;

/// How long the collecting walk runs. The drop sits about two blocks ahead of
/// where the camera was aimed, so this only has to cover that.
const INV_WALK_SECS: f32 = 1.3;

/// The script, as (time, action) beats. Times are seconds since landing.
///
/// Written as data rather than an if-ladder because the *timing* is the whole
/// design here: each beat has to leave the previous result on screen long
/// enough to read before the next one changes it.
///
/// There is deliberately no step-back beat. The first version retreated before
/// walking in, on the theory that a longer approach reads better — but walking
/// backwards is unobstructed while walking forwards runs into the one-block
/// ledge by the spawn, so the player returned 3 units instead of the 8 it had
/// given up and stalled short of its own drop. The item is already ahead; the
/// walk only has to close two blocks.
const INV_BEATS: &[(f32, InvAction)] = &[
    (0.5, InvAction::Place),      // a block appears at the crosshair
    (2.5, InvAction::Break),      // and drops as an item where it stood
    (4.0, InvAction::WalkOn),     // walk onto it -> collected
    (7.0, InvAction::OpenScreen), // the inventory itself, icons and counts
    (11.0, InvAction::CloseScreen),
    (12.0, InvAction::Place),     // place what was just collected
    (14.0, InvAction::Break),
    (15.5, InvAction::WalkOn),
    (18.5, InvAction::OpenScreen),
    (22.5, InvAction::CloseScreen),
    // Then the container loop. `SelectKey` picks the crate off the hotbar, the
    // first `Place` puts it down, and the second one lands on the crate that is
    // now under the crosshair — which opens it rather than building on it.
    // That is the real binding, not a shortcut: right-click on a container
    // block is the open gesture.
    (24.0, InvAction::SelectKey(CRATE_KEY)),
    (25.0, InvAction::Place),
    (27.5, InvAction::Place),
    (32.0, InvAction::CloseScreen),
];

/// Hotbar key the Wooden Crate lands on: an empty bar auto-fills in inventory
/// order, and the crate is the sixth of the server's nine starter blocks.
///
/// Coupled to `STARTER_BLOCKS` in `soils-server`, which is why the demo test
/// asserts on beats rather than on what appeared — a change there makes this
/// place the wrong block, and the recording is what shows it.
const CRATE_KEY: u8 = 5;

#[derive(Clone, Copy, PartialEq)]
enum InvAction {
    Place,
    Break,
    WalkOn,
    OpenScreen,
    CloseScreen,
    /// Press a hotbar digit (0-based).
    SelectKey(u8),
}

/// Held movement for the current beat, and any one-shot button presses.
#[derive(Resource, Default)]
pub struct BotActions {
    /// Set for exactly one frame; consumed by [`press_bot_buttons`].
    pub click_left: bool,
    pub click_right: bool,
    pub toggle_inventory: bool,
    /// Hotbar digit to tap this frame (0-based).
    pub select_key: Option<u8>,
}

fn drive_inventory(
    t: f32,
    bot: &mut Bot,
    pending: &mut PendingInput,
    player: &mut Player,
    transform: &mut Transform,
) {
    // Fall to the surface first. Facing is fixed so the whole take is framed
    // on one patch of ground.
    aim_pitch(player, transform, bot.role.inbound(), INV_PITCH);
    if !bot.toggled_fly && t > 0.6 {
        bot.toggled_fly = true;
        pending.input.toggle_fly = true;
    }
    // Wait for real ground contact, with the timer only as a floor.
    if bot.landed.is_none() {
        if t > INV_LAND_BY && player.sim.grounded {
            bot.landed = Some(t);
            info!("bot: landed at {t:.1}s — inventory routine starts");
        }
        hold(pending, Vec2::ZERO, player.yaw);
        return;
    }
    let script_t = t - bot.landed.unwrap_or(INV_LAND_BY);
    let mut axes = Vec2::ZERO;

    // Movement is a function of where we are in the script, not a one-shot.
    if let Some((at, action)) = current_beat(script_t) {
        let since = script_t - at;
        if action == InvAction::WalkOn && since < INV_WALK_SECS {
            axes = Vec2::new(0.0, 1.0);
            // Hop while closing. The terrain by the spawn is stepped and a
            // walking player cannot climb a one-block ledge; without this the
            // approach stalls against the nearest ridge short of the drop,
            // which is exactly how the third take failed to collect anything.
            let due = (since / 0.45) as u32 + 1;
            if due > bot.jumps {
                bot.jumps = due;
                pending.input.jump = true;
            }
        }
    }
    hold(pending, axes, player.yaw);
}

/// The most recent beat at or before `t`, with its scheduled time.
fn current_beat(t: f32) -> Option<(f32, InvAction)> {
    INV_BEATS.iter().rev().find(|(at, _)| t >= *at).copied()
}

/// Fire the one-shot half of each beat exactly once.
pub fn inventory_actions(
    time: Res<Time>,
    mut bot: ResMut<Bot>,
    mut actions: ResMut<BotActions>,
) {
    if bot.role != Role::Inventory {
        return;
    }
    let Some(started) = bot.started else { return };
    let Some(landed) = bot.landed else { return };
    let t = time.elapsed_secs() - started - landed;
    if t < 0.0 {
        return;
    }
    // Everything scheduled at or before now that has not fired yet.
    while bot.beat < INV_BEATS.len() && t >= INV_BEATS[bot.beat].0 {
        // Logged so the demo test can assert the routine actually ran. A take
        // where the script silently stalls looks exactly like a good one from
        // the outside: right duration, right file size, plausible first frame.
        info!("bot: beat {} of {}", bot.beat + 1, INV_BEATS.len());
        match INV_BEATS[bot.beat].1 {
            InvAction::Place => actions.click_right = true,
            InvAction::Break => actions.click_left = true,
            InvAction::OpenScreen | InvAction::CloseScreen => actions.toggle_inventory = true,
            InvAction::WalkOn => bot.jumps = 0,
            InvAction::SelectKey(k) => actions.select_key = Some(k),
        }
        bot.beat += 1;
    }
}

/// Turn scheduled actions into real button presses.
///
/// Deliberately synthesizes input rather than calling `edit_blocks` or the UI
/// systems directly: the point of a recording is to show the paths a player
/// exercises, and a bot with its own private edit path would prove nothing
/// about the one people use. Runs before those systems consume the input.
pub fn press_bot_buttons(
    mut actions: ResMut<BotActions>,
    mut mouse: ResMut<ButtonInput<MouseButton>>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
) {
    // `reset` before `press` is load-bearing. `ButtonInput::press` only sets
    // `just_pressed` when the button was not already pressed, and nothing here
    // ever releases — so without the reset, the first beat fires and every
    // later one is silently a no-op. The symptom is a demo that places exactly
    // one block and then does nothing for thirty seconds.
    if std::mem::take(&mut actions.click_left) {
        mouse.reset(MouseButton::Left);
        mouse.press(MouseButton::Left);
    }
    if std::mem::take(&mut actions.click_right) {
        mouse.reset(MouseButton::Right);
        mouse.press(MouseButton::Right);
    }
    if std::mem::take(&mut actions.toggle_inventory) {
        keys.reset(KeyCode::KeyE);
        keys.press(KeyCode::KeyE);
    }
    if let Some(k) = actions.select_key.take()
        && let Some(&code) = crate::inventory::hotbar::HOTBAR_KEYS.get(k as usize)
    {
        keys.reset(code);
        keys.press(code);
    }
}

/// Aim with an explicit pitch (the walking routine's [`aim`] fixes it).
fn aim_pitch(player: &mut Player, transform: &mut Transform, yaw: f32, pitch: f32) {
    player.yaw = yaw;
    player.pitch = pitch;
    transform.rotation =
        Quat::from_axis_angle(Vec3::Y, yaw) * Quat::from_axis_angle(Vec3::X, pitch);
}

fn aim(player: &mut Player, transform: &mut Transform, yaw: f32) {
    player.yaw = yaw;
    player.pitch = PITCH;
    transform.rotation =
        Quat::from_axis_angle(Vec3::Y, yaw) * Quat::from_axis_angle(Vec3::X, PITCH);
}
