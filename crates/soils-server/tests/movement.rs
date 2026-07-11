//! Server-authoritative walk-mode physics, observed through the real snapshot
//! stream. The other movement tests (`scenarios.rs`, `prediction.rs`) drive
//! *fly* mode (noclip, no gravity); these drop the player into walk mode and
//! pin that the **server** — not just the client's local sim — integrates
//! gravity, landing, jumping, and the grounded-gate anti-cheat.
//!
//! Two snapshot facts shape every assertion here:
//!  - The server emits a snapshot every server tick, but delta-encodes an
//!    entity *out* once its state stops changing, so a resting player vanishes
//!    from the stream. Detect landing on the transition tick where `vel.y`
//!    returns to ~0 (guaranteed sent), not on the steady rest state.
//!  - The server steps a player only while its input frames arrive, so reads
//!    are bounded by snapshot *count*, never by waiting for the socket to fall
//!    quiet (it never does).

mod common;

use common::{Client, TestServer};
use soils_protocol::EntityState;
use soils_sim::{EYE_TO_FEET, PlayerInput};
use std::time::Duration;

/// Players spawn ~13 voxels above the deterministic terrain in fly mode. Drop
/// into walk mode on tick 0 and fall; return the landing snapshot (the tick
/// `vel.y` snaps back to ~0 after the descent — the fall lands well within the
/// 160-tick drive, so it is emitted mid-stream).
async fn drop_and_land(c: &mut Client) -> EntityState {
    let (net, spawn) = (c.self_entity, c.spawn);
    c.drive(160, |i| PlayerInput { toggle_fly: i == 0, ..Default::default() }).await;
    c.await_entity(net, |s| s.velocity[1].abs() < 0.3 && s.pos[1] < spawn[1] - 2.0).await
}

/// Read exactly `count` snapshots, returning every self-state seen. Bounded and
/// always terminating: the server sends a snapshot per tick regardless of
/// motion, and a no-change tick simply contributes no self entry.
async fn sample_self(c: &mut Client, net: u32, count: u32) -> Vec<EntityState> {
    let mut out = Vec::new();
    for _ in 0..count {
        for s in c.next_snapshot().await {
            if s.id == net {
                out.push(s);
            }
        }
    }
    out
}

/// Perform exactly one jump from a grounded rest and return the peak height
/// reached above `rest_y`. Starvation-proof by construction: the input token
/// bucket *drops* overflow frames (it never queues them), so a lone edge can
/// vanish — but a grounded player cannot move until a jump frame is admitted,
/// so we keep pressing jump until the echo shows the player airborne, then stop
/// (no re-jump) and drive idle frames to play the arc out and sample its apex.
async fn jump_and_measure(c: &mut Client, net: u32, rest_y: f32) -> f32 {
    let mut max_y = rest_y;
    let mut airborne = false;
    for _ in 0..80 {
        c.drive(8, |_| PlayerInput { jump: true, ..Default::default() }).await;
        for s in sample_self(c, net, 2).await {
            max_y = max_y.max(s.pos[1]);
            if s.pos[1] > rest_y + 0.3 {
                airborne = true;
            }
        }
        if airborne {
            break;
        }
    }
    // Advance the server sim through the rest of the arc (it only steps players
    // on input) and capture the apex.
    c.drive(80, |_| PlayerInput::default()).await;
    for s in sample_self(c, net, 60).await {
        max_y = max_y.max(s.pos[1]);
    }
    max_y - rest_y
}

/// Gravity + landing are server-authoritative: the player free-falls off spawn
/// and the *server* arrests the fall on the terrain surface.
#[tokio::test]
async fn walking_falls_and_lands_under_server_gravity() {
    let server = TestServer::start("walk-fall");
    let mut a = Client::join(server.addr(), "alice").await;
    let spawn = a.spawn;

    let landed = drop_and_land(&mut a).await;
    eprintln!("landed at {:?} vel {:?} (spawn {:?})", landed.pos, landed.velocity, spawn);

    // Fell a real distance (not still hovering at spawn).
    assert!(landed.pos[1] < spawn[1] - 2.0, "player never fell: {:?}", landed.pos);
    // The fall was arrested, not tunneled through: feet rest just above an
    // integer surface (matches the offline `fall_lands_grounded_on_floor`
    // envelope, feet ∈ [surface, surface+~0.3]).
    let feet = landed.pos[1] - EYE_TO_FEET;
    let above_surface = feet - feet.floor();
    assert!(
        above_surface < 0.35,
        "feet {feet} not resting on a surface (frac {above_surface}) — collision let it sink?"
    );
    assert!(landed.velocity[1].abs() < 0.3, "landed with residual vertical velocity");
}

/// A jump is server-authoritative and bounded: launching from a grounded rest
/// produces an upward arc of the expected height, then the player comes back.
#[tokio::test]
async fn jump_is_server_authoritative_and_bounded() {
    let server = TestServer::start("walk-jump");
    let mut a = Client::join(server.addr(), "alice").await;
    let net = a.self_entity;

    let rest_y = drop_and_land(&mut a).await.pos[1];

    let apex = jump_and_measure(&mut a, net, rest_y).await;
    eprintln!("jump apex {apex} above rest {rest_y}");

    // Analytic apex JUMP²/2·GRAVITY ≈ 1.45; the 20 Hz stream samples the arc
    // coarsely, so assert a band that a real jump clears but a no-op never does.
    assert!(
        (0.8..1.9).contains(&apex),
        "server jump apex {apex} outside [0.8, 1.9] — gravity/jump not authoritative?"
    );
}

/// Anti-cheat: holding jump every tick cannot climb. `step_player` only jumps
/// when `grounded`, so the player re-launches to the *same* apex on each
/// landing and never accumulates height — proven against the server echo.
#[tokio::test]
async fn held_jump_cannot_climb_midair() {
    let server = TestServer::start("walk-holdjump");
    let mut a = Client::join(server.addr(), "alice").await;
    let net = a.self_entity;

    let rest_y = drop_and_land(&mut a).await.pos[1];

    // Jump held on every one of 200 ticks (~3 s of bouncing).
    a.drive(200, |_| PlayerInput { jump: true, ..Default::default() }).await;
    let peak = sample_self(&mut a, net, 120)
        .await
        .iter()
        .map(|s| s.pos[1])
        .fold(f32::MIN, f32::max)
        - rest_y;
    eprintln!("held-jump peak {peak} above rest {rest_y}");

    // It does leave the ground (jumping works)...
    assert!(peak > 0.8, "held jump never launched: peak {peak}");
    // ...but never climbs beyond a single jump: no double/air jump server-side.
    assert!(
        peak < 1.9,
        "held jump climbed to {peak} above rest — grounded gate failed (air jump?)"
    );
}

/// Determinism across the network: two clients from the shared spawn, driven by
/// the identical input sequence, land at bit-for-bit the same server position —
/// the shared deterministic sim over identical terrain.
#[tokio::test]
async fn identical_inputs_land_at_identical_positions() {
    let server = TestServer::start("walk-determinism");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;
    assert_eq!(a.spawn, b.spawn, "scenario assumes a single shared spawn point");

    // Players collide only with voxels, never each other, so co-located falls
    // don't interfere. Land sequentially (each is at rest while the other runs).
    let la = drop_and_land(&mut a).await.pos;
    let lb = drop_and_land(&mut b).await.pos;
    eprintln!("alice landed {la:?} / bob landed {lb:?}");

    let d = ((la[0] - lb[0]).powi(2) + (la[1] - lb[1]).powi(2) + (la[2] - lb[2]).powi(2)).sqrt();
    assert!(d < 0.05, "identical inputs diverged: {la:?} vs {lb:?} ({d} apart)");
}

/// Walk-mode collision is server-authoritative: a wall of edited solid blocks
/// stops the player, unlike fly mode which noclips. The guarantee under test is
/// no-tunnel — the player never appears on the far side of the wall plane
/// (noclip would sail straight through to z < wall_z).
#[tokio::test]
async fn walking_cannot_pass_through_an_edited_wall() {
    let server = TestServer::start("walk-wall");
    let mut a = Client::join(server.addr(), "alice").await;
    let net = a.self_entity;

    let landed = drop_and_land(&mut a).await.pos;
    let feet_y = (landed[1] - EYE_TO_FEET).floor() as i32;
    let (x0, z0) = (landed[0].floor() as i32, landed[2].floor() as i32);
    // yaw 0 → forward = -Z, so the player walks toward decreasing z. Build a
    // 3-wide, 3-tall solid wall two voxels ahead (well within REACH=8 of the
    // eye), block id 5 (a known solid id, per scenarios.rs edits). The extra
    // row below the feet covers a possible one-voxel step-down toward the wall.
    let wall_z = z0 - 2;
    for dx in -1..=1 {
        for dy in -1..=1 {
            a.edit([x0 + dx, feet_y + dy, wall_z], 5).await;
        }
        // Respect the server's edit-rate bucket (~24 per ~800 ms).
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Walk forward into the wall for ~2 s, then read where the server left us.
    a.drive(128, |_| PlayerInput { move_axes: glam::Vec2::new(0.0, 1.0), ..Default::default() })
        .await;
    let min_z = sample_self(&mut a, net, 80)
        .await
        .iter()
        .map(|s| s.pos[2])
        .fold(f32::INFINITY, f32::min);
    eprintln!("wall at z={wall_z}, player min z reached {min_z}");

    // The wall voxel spans [wall_z, wall_z+1); the player's -Z face stops on its
    // +Z face. Absent walk-mode collision, the player would sail through to
    // z < wall_z. Assert it never does.
    assert!(
        min_z > wall_z as f32,
        "player tunneled through the wall: reached z {min_z} past wall_z {wall_z}"
    );
}
