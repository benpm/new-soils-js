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
//! * `SOILS_BOT=a|b` — role. `a` starts west facing east; `b` mirrors it.
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
}

impl Role {
    /// Facing while flying clear of spawn.
    fn outbound(self) -> f32 {
        match self {
            Role::A => WEST,
            Role::B => EAST,
        }
    }
    /// Facing while walking back through the pile toward the other player.
    fn inbound(self) -> f32 {
        match self {
            Role::A => EAST,
            Role::B => WEST,
        }
    }
}

#[derive(Resource)]
pub struct Bot {
    role: Role,
    start_file: Option<PathBuf>,
    started: Option<f32>,
    toggled_fly: bool,
    jumps: u32,
}

/// The configured bot, if `SOILS_BOT` names a role.
pub fn configured() -> Option<Bot> {
    let role = match std::env::var("SOILS_BOT").ok()?.to_ascii_lowercase().as_str() {
        "a" => Role::A,
        "b" => Role::B,
        other => {
            error!("SOILS_BOT: unknown role {other:?} (expected a or b)");
            return None;
        }
    };
    Some(Bot {
        role,
        start_file: std::env::var("SOILS_BOT_START").ok().map(PathBuf::from),
        started: None,
        toggled_fly: false,
        jumps: 0,
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

fn aim(player: &mut Player, transform: &mut Transform, yaw: f32) {
    player.yaw = yaw;
    player.pitch = PITCH;
    transform.rotation =
        Quat::from_axis_angle(Vec3::Y, yaw) * Quat::from_axis_angle(Vec3::X, PITCH);
}
