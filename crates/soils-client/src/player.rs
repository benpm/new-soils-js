//! First-person player. The simulation itself (movement, gravity, AABB voxel
//! collision) lives in `soils-sim` and advances on the fixed tick; this module
//! owns the Bevy plumbing: input collection, the fixed-step driver, render
//! interpolation of the camera transform, mouse-look, pointer-lock, and
//! distance-based chunk streaming requests.
//!
//! Ownership split: `Player::sim` (a `soils_sim::PlayerState`) is the
//! authoritative-local position/velocity, stepped at `soils_sim::TICK_HZ`;
//! `Transform.translation` is *derived* each frame by interpolating between
//! the last two ticks, so all position writers must go through [`teleport`].
//! `Transform.rotation` stays owned by [`mouse_look`] (and ad-hoc `look_at`
//! callers like the self-test framing).

use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use std::collections::VecDeque;

use soils_protocol::{CHUNK_BIT, ClientMsg, InputFrame};
use soils_sim::{PlayerInput, PlayerState};

use crate::chunk::{ChunkMap, VoxelChunk, voxel_at};
use crate::net::NetClient;

const MOUSE_SENS: f32 = 0.0022;

#[derive(Component)]
pub struct Player {
    pub yaw: f32,
    pub pitch: f32,
    /// Predicted simulation state: stepped locally every fixed tick through
    /// the shared `soils-sim`, reconciled against server snapshots (rewind to
    /// `last_input_seq`, replay newer inputs on divergence).
    pub sim: PlayerState,
    /// Sim position at the previous fixed tick, for render interpolation.
    pub prev_pos: Vec3,
}

impl Player {
    /// A player standing at `pos` (eye position), looking slightly downward so
    /// terrain is in view on spawn.
    pub fn at(pos: Vec3) -> Self {
        Self {
            yaw: 0.0,
            pitch: -0.5,
            sim: PlayerState { pos, ..PlayerState::default() },
            prev_pos: pos,
        }
    }
}

/// Move the player instantly: sets the sim state and the interpolation
/// baseline (no smear across the jump) and writes the Transform immediately
/// so same-frame readers see the new position. Prediction history is invalid
/// across a teleport; [`InputRing::reset`] handles that at the call sites
/// that own the resource.
pub fn teleport(player: &mut Player, transform: &mut Transform, pos: Vec3) {
    player.sim.pos = pos;
    player.sim.vel = Vec3::ZERO;
    player.prev_pos = pos;
    transform.translation = pos;
}

/// Input gathered each frame for the fixed tick. Held state (move axes, yaw,
/// sprint, up/down) is rebuilt every frame; `jump`/`toggle_fly` are edge
/// latches that survive frames with zero fixed ticks and are cleared by the
/// tick that consumes them.
#[derive(Resource, Default)]
pub struct PendingInput {
    pub input: PlayerInput,
}

impl PendingInput {
    /// Drop queued edge events (e.g. when the console opens, so a pre-console
    /// keypress doesn't fire on close).
    pub fn clear_latches(&mut self) {
        self.input.jump = false;
        self.input.toggle_fly = false;
    }
}

/// Gather keyboard state into [`PendingInput`]. Runs just before the fixed
/// main loop so the freshest input feeds this frame's ticks.
pub fn collect_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut pending: ResMut<PendingInput>,
    query: Query<&Player>,
) {
    let Ok(player) = query.single() else { return };
    let mut axes = Vec2::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        axes.y += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) {
        axes.y -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) {
        axes.x += 1.0;
    }
    if keys.pressed(KeyCode::KeyA) {
        axes.x -= 1.0;
    }
    pending.input.move_axes = axes;
    pending.input.yaw = player.yaw;
    pending.input.sprint = keys.pressed(KeyCode::ShiftLeft);
    pending.input.up = keys.pressed(KeyCode::Space);
    pending.input.down = keys.pressed(KeyCode::ControlLeft);
    if keys.just_pressed(KeyCode::Space) {
        pending.input.jump = true;
    }
    if keys.just_pressed(KeyCode::KeyF) {
        pending.input.toggle_fly = true;
    }
}

/// The outgoing input stream: one frame per fixed tick, the last few bundled
/// per send for loss robustness on future unreliable transports (the server
/// dedupes by `seq`).
#[derive(Resource, Default)]
pub struct InputRing {
    seq: u32,
    frames: Vec<InputFrame>,
    /// `(seq, input, predicted state after stepping it)` — the rewind/replay
    /// source for reconciliation.
    history: VecDeque<(u32, PlayerInput, PlayerState)>,
}

/// History depth: ~4 s at 64 Hz, far beyond any sane RTT.
const HISTORY_CAP: usize = 256;

impl InputRing {
    /// Drop history across a warp (it describes a dead timeline).
    pub fn reset(&mut self) {
        self.frames.clear();
        self.history.clear();
    }
}

/// One fixed tick: predict locally through the shared sim, record the
/// (input, state) pair, and send the frame bundle. The server integrates the
/// same inputs authoritatively; [`reconcile_self`] corrects us on divergence.
pub fn predict_and_send(
    net: Res<NetClient>,
    mut pending: ResMut<PendingInput>,
    mut ring: ResMut<InputRing>,
    tracker: Res<crate::server_msg::SnapTracker>,
    map: Res<ChunkMap>,
    chunks: Query<&VoxelChunk>,
    mut query: Query<&mut Player>,
) {
    let Ok(mut player) = query.single_mut() else { return };
    let input = pending.input;
    pending.clear_latches();

    // Predict: step the local sim exactly as the server will.
    let player = &mut *player;
    player.prev_pos = player.sim.pos;
    let sampler = |v: IVec3| voxel_at(&map, &chunks, v);
    soils_sim::step_player(&mut player.sim, &input, 1.0 / soils_sim::TICK_HZ as f32, &sampler);

    ring.seq += 1;
    let seq = ring.seq;
    ring.history.push_back((seq, input, player.sim));
    if ring.history.len() > HISTORY_CAP {
        ring.history.pop_front();
    }
    let (buttons, flags, yaw) = soils_sim::pack_input(&input);
    ring.frames.push(InputFrame { seq, buttons, flags, yaw });
    if ring.frames.len() > 3 {
        ring.frames.remove(0);
    }
    net.send(ClientMsg::Inputs {
        ack_tick: tracker.0.latest_tick,
        frames: ring.frames.clone(),
    });
}

/// Predicted-vs-authoritative tolerance (world units) before a rewind+replay.
const RECONCILE_EPSILON: f32 = 0.05;

/// Reconcile the prediction against the server's echo of our own entity at
/// `last_input_seq`: within epsilon → keep the prediction; diverged → rewind
/// to the authoritative state and replay every newer pending input.
pub fn reconcile_self(
    mut reader: MessageReader<crate::server_msg::EntitiesUpdated>,
    local: Res<crate::actor::LocalPlayer>,
    mut ring: ResMut<InputRing>,
    map: Res<ChunkMap>,
    chunks: Query<&VoxelChunk>,
    mut query: Query<&mut Player>,
) {
    for msg in reader.read() {
        let Some(state) = msg.states.iter().find(|s| s.id == local.self_entity) else {
            continue;
        };
        let Ok(mut player) = query.single_mut() else { continue };
        let server_pos = Vec3::from_array(state.pos);
        let seq = msg.last_input_seq;

        // Everything before the acked input is settled history.
        while ring.history.front().is_some_and(|(s, ..)| *s < seq) {
            ring.history.pop_front();
        }
        let predicted_then = match ring.history.front() {
            Some((s, _, st)) if *s == seq => *st,
            // No matching entry (fresh join, warp, or pre-input echo): adopt
            // the server state outright only if we're far off.
            _ => {
                if (player.sim.pos - server_pos).length() > 1.0 {
                    player.sim.pos = server_pos;
                    player.sim.vel = Vec3::from_array(state.velocity);
                    player.prev_pos = server_pos;
                }
                continue;
            }
        };

        if (predicted_then.pos - server_pos).length() <= RECONCILE_EPSILON {
            continue; // prediction holds; nothing to correct
        }

        // Mispredicted: rewind to the authoritative state at `seq` (position
        // and velocity from the server; fly/grounded from the recorded state
        // at that seq — they evolve deterministically from the same inputs,
        // and taking them from the *current* prediction would double-apply
        // any fly toggles the replay is about to re-run), then replay the
        // unacknowledged inputs and rebase the recorded states. The anchor
        // entry rebases too, so a repeated echo of the same seq is a no-op.
        let base = PlayerState {
            pos: server_pos,
            vel: Vec3::from_array(state.velocity),
            flying: predicted_then.flying,
            grounded: predicted_then.grounded,
        };
        if let Some(front) = ring.history.front_mut() {
            front.2 = base;
        }
        let mut sim = base;
        let sampler = |v: IVec3| voxel_at(&map, &chunks, v);
        for (_, input, recorded) in ring.history.iter_mut().skip(1) {
            soils_sim::step_player(&mut sim, input, 1.0 / soils_sim::TICK_HZ as f32, &sampler);
            *recorded = sim;
        }
        player.sim = sim;
        player.prev_pos = sim.pos; // snap to the corrected timeline
    }
}

/// When set, the camera transform is under manual control (self-test framing)
/// and the prediction must not move it.
#[derive(Resource, Default)]
pub struct CameraHold(pub bool);

/// Derive the rendered camera position by interpolating the last two
/// predicted ticks. Translation only — rotation belongs to [`mouse_look`].
pub fn sync_camera(
    fixed_time: Res<Time<Fixed>>,
    hold: Res<CameraHold>,
    mut query: Query<(&Player, &mut Transform)>,
) {
    if hold.0 {
        return;
    }
    let Ok((player, mut transform)) = query.single_mut() else { return };
    transform.translation = player.prev_pos.lerp(player.sim.pos, fixed_time.overstep_fraction());
}

/// Toggle pointer-lock with Escape; re-grab on click.
pub fn cursor_toggle(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut cursor: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    let Ok(mut cursor) = cursor.single_mut() else { return };
    if keys.just_pressed(KeyCode::Escape) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    } else if buttons.just_pressed(MouseButton::Left)
        || buttons.just_pressed(MouseButton::Right)
    {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
}

/// Mouse-look: accumulate yaw/pitch and orient the camera.
pub fn mouse_look(
    motion: Res<AccumulatedMouseMotion>,
    cursor: Query<&CursorOptions, With<PrimaryWindow>>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    // Only look while the cursor is grabbed.
    if let Ok(cursor) = cursor.single() {
        if cursor.grab_mode == CursorGrabMode::None {
            return;
        }
    }
    let delta = motion.delta;
    if delta == Vec2::ZERO {
        return;
    }
    for (mut player, mut transform) in &mut query {
        player.yaw -= delta.x * MOUSE_SENS;
        player.pitch = (player.pitch - delta.y * MOUSE_SENS)
            .clamp(-std::f32::consts::FRAC_PI_2 + 0.01, std::f32::consts::FRAC_PI_2 - 0.01);
        transform.rotation =
            Quat::from_axis_angle(Vec3::Y, player.yaw) * Quat::from_axis_angle(Vec3::X, player.pitch);
    }
}

/// Tracks which chunk the player was last in, to drive the HUD streaming
/// estimate (the *server* owns the subscription since chunk streaming v2 —
/// the client never requests chunks).
#[derive(Resource)]
pub struct Streaming {
    pub last_chunk: Option<IVec3>,
    pub load_radius: i32,
    /// The view radius last told to the server, so a change (console, pause
    /// menu) sends exactly one `ViewRadius`.
    pub sent_radius: Option<i32>,
    /// Chunks inside the local view box not yet applied — a live estimate of
    /// how much of the surrounding world is still streaming in (HUD).
    pub pending: usize,
}

impl Default for Streaming {
    fn default() -> Self {
        // `SOILS_RADIUS` sets the starting view radius (same clamp as the
        // `loadradius` console command), so perf runs can pin the chunk count
        // without driving the console.
        let load_radius = std::env::var("SOILS_RADIUS")
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
            .map_or(4, |r| r.clamp(2, 8));
        Self { last_chunk: None, load_radius, sent_radius: None, pending: 0 }
    }
}

/// Keep the server's view of our radius current, and recompute the HUD
/// streaming estimate when the player crosses a chunk boundary. The server
/// pushes/unloads chunks on its own; this mirrors the same box locally so the
/// HUD can show progress without extra protocol.
pub fn track_streaming(
    net: Res<NetClient>,
    map: Res<ChunkMap>,
    queue: Res<crate::server_msg::ChunkApplyQueue>,
    mut streaming: ResMut<Streaming>,
    query: Query<&Transform, With<Player>>,
) {
    if streaming.sent_radius != Some(streaming.load_radius) {
        streaming.sent_radius = Some(streaming.load_radius);
        net.send(ClientMsg::ViewRadius { radius: streaming.load_radius as u8 });
    }

    let Ok(transform) = query.single() else { return };
    let p = transform.translation;
    let pc = IVec3::new(
        (p.x.floor() as i32) >> CHUNK_BIT,
        (p.y.floor() as i32) >> CHUNK_BIT,
        (p.z.floor() as i32) >> CHUNK_BIT,
    );
    if streaming.last_chunk == Some(pc) {
        return;
    }
    streaming.last_chunk = Some(pc);

    let r = streaming.load_radius;
    let mut pending = 0;
    for dx in -r..=r {
        for dy in -r..=r {
            for dz in -r..=r {
                let cpos = pc + IVec3::new(dx, dy, dz);
                if !map.map.contains_key(&cpos) && !queue.queued.contains(&cpos) {
                    pending += 1;
                }
            }
        }
    }
    streaming.pending = pending;
}
