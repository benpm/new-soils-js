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

use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use std::collections::VecDeque;

use soils_protocol::{CHUNK_BIT, ClientMsg, InputFrame};
use soils_sim::{PlayerInput, PlayerState};

use crate::chunk::{ChunkMap, VoxelChunk, voxel_at};
use crate::net::NetClient;

/// Radians of camera rotation per raw mouse count at sensitivity 1.0. Roughly
/// Minecraft's default, which assumes an 800-DPI mouse; anything faster wants
/// [`LookSettings::sensitivity`] below 1.
const BASE_SENS: f32 = 0.0022;

/// Sensitivity bounds. The low end has to stay usable on an 8000-DPI mouse and
/// the high end on a 400-DPI one, which is most of two decades.
pub const SENS_MIN: f32 = 0.1;
pub const SENS_MAX: f32 = 5.0;
/// One press of the pause menu's -/+ .
pub const SENS_STEP: f32 = 0.1;

/// Mouse-look tuning. A multiplier over [`BASE_SENS`], so 1.0 is the default
/// feel and the number means the same thing whatever the base becomes.
///
/// Not persisted — like the load radius and the render toggles, it lasts the
/// session. `SOILS_SENS` sets the starting value, which is also how a headless
/// or scripted run pins it.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct LookSettings {
    pub sensitivity: f32,
}

impl Default for LookSettings {
    fn default() -> Self {
        let from_env =
            std::env::var("SOILS_SENS").ok().and_then(|v| v.parse::<f32>().ok());
        Self { sensitivity: from_env.unwrap_or(1.0).clamp(SENS_MIN, SENS_MAX) }
    }
}

impl LookSettings {
    /// Set the multiplier, clamped to the supported range. Every writer (menu,
    /// console) goes through this so none of them can park the camera on a
    /// sensitivity of zero or a negative one that inverts both axes at once.
    pub fn set(&mut self, sensitivity: f32) {
        self.sensitivity = if sensitivity.is_finite() {
            sensitivity.clamp(SENS_MIN, SENS_MAX)
        } else {
            self.sensitivity
        };
    }

    /// Nudge by `steps` of [`SENS_STEP`], snapped to the step grid so repeated
    /// presses can't accumulate float drift into "0.7000001".
    pub fn nudge(&mut self, steps: i32) {
        let raw = self.sensitivity / SENS_STEP + steps as f32;
        self.set(raw.round() * SENS_STEP);
    }

    /// Radians per raw mouse count.
    fn radians_per_count(self) -> f32 {
        BASE_SENS * self.sensitivity
    }
}

/// Largest per-report mouse delta that can be a real relative movement, in
/// raw device counts. A 1000 Hz mouse would have to travel metres per second
/// at 8000 DPI to exceed this; nothing reaches it by hand.
///
/// Reports *above* it are not fast movement, they are a different coordinate
/// system. Some pointing devices — Wacom tablets, VM and remote-desktop
/// pointers, a few KVMs — send raw mouse input in *absolute* mode, where the
/// report carries a 0..65535 screen coordinate instead of a delta. winit's
/// Windows backend does not filter those out (it tests `MOUSE_MOVE_RELATIVE`,
/// which is zero, so the test always passes), so the coordinate arrives as a
/// `MouseMotion` delta. Integrated as one it swings yaw by tens of radians and
/// pins pitch to its clamp — the camera ends up stuck facing the ground while
/// the smallest movement spins the view.
const MAX_RAW_DELTA: f32 = 1500.0;

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
    /// `(seq, input, predicted state after stepping it, peers that tick)` —
    /// the rewind/replay source for reconciliation.
    ///
    /// Peer positions are recorded per tick, not read fresh at replay time. The
    /// server steps each tick against that tick's peer snapshot; replaying a
    /// whole ring against one current set would reproduce a different history
    /// than the server's, so the prediction could never converge while anyone
    /// was moving nearby — a permanent rewind every snapshot.
    history: VecDeque<(u32, PlayerInput, PlayerState, Vec<Vec3>)>,
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
    actors: Query<&crate::actor::Actor>,
    mut query: Query<&mut Player>,
) {
    let Ok(mut player) = query.single_mut() else { return };
    let input = pending.input;
    pending.clear_latches();

    // Predict: step the local sim exactly as the server will.
    let player = &mut *player;
    player.prev_pos = player.sim.pos;
    let sampler = |v: IVec3| voxel_at(&map, &chunks, v);
    let peers = peer_positions(&actors);
    soils_sim::step_player_peers(
        &mut player.sim,
        &input,
        1.0 / soils_sim::TICK_HZ as f32,
        &sampler,
        &peers,
    );
    // `peers` moves into the ring below, so this tick's obstacle set can be
    // replayed exactly if reconciliation rewinds through it.

    ring.seq += 1;
    let seq = ring.seq;
    ring.history.push_back((seq, input, player.sim, peers));
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

/// Eye positions of the other players we know about. The local player has no
/// `Actor` body (`spawn_actors` skips `self_entity`), so this is exactly the
/// peer set the server steps us against.
fn peer_positions(actors: &Query<&crate::actor::Actor>) -> Vec<Vec3> {
    actors
        .iter()
        .filter(|a| a.kind == soils_sim::KIND_PLAYER)
        .filter_map(|a| a.latest_pos())
        .collect()
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
            Some((s, _, st, _)) if *s == seq => *st,
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
        // Each tick replays against the peers recorded *for that tick*, which
        // is what the server stepped it against.
        for (_, input, recorded, peers) in ring.history.iter_mut().skip(1) {
            soils_sim::step_player_peers(
                &mut sim,
                input,
                1.0 / soils_sim::TICK_HZ as f32,
                &sampler,
                peers,
            );
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


/// Mouse-look: accumulate yaw/pitch and orient the camera.
///
/// Reads the individual reports rather than `AccumulatedMouseMotion` so a
/// single absolute-mode report (see [`MAX_RAW_DELTA`]) can be dropped on its
/// own; the accumulated resource has already folded it into the frame's sum by
/// the time we could see it.
pub fn mouse_look(
    mut motion: MessageReader<MouseMotion>,
    look: Res<LookSettings>,
    mut query: Query<(&mut Player, &mut Transform)>,
    mut warned: Local<bool>,
) {
    let mut delta = Vec2::ZERO;
    for report in motion.read() {
        if report.delta.x.abs() > MAX_RAW_DELTA || report.delta.y.abs() > MAX_RAW_DELTA {
            if !std::mem::replace(&mut *warned, true) {
                warn!(
                    "ignoring absolute-mode mouse reports (first {:?}): that device \
                     sends screen coordinates, not deltas; look will not follow it",
                    report.delta
                );
            }
            continue;
        }
        delta += report.delta;
    }
    if delta == Vec2::ZERO {
        return;
    }
    let sens = look.radians_per_count();
    for (mut player, mut transform) in &mut query {
        player.yaw -= delta.x * sens;
        player.pitch = (player.pitch - delta.y * sens)
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
    ///
    /// Deliberately *not* a readiness signal: it counts everything subscribed
    /// but unmapped, and a chunk is only ever materialized once something
    /// demands it. Chunks the renderer never asks for — most of the
    /// subscription when the player is enclosed — sit here forever, so this
    /// never reaches zero underground. Use [`wanted`](Self::wanted) for
    /// "is the world I can see actually here".
    pub pending: usize,
    /// Outstanding work the *renderer* is waiting on: demanded chunks not yet
    /// materialized, plus generation in flight.
    ///
    /// This is the honest "the visible world has arrived" signal, and it does
    /// reach zero. `record::cue` waits on it before starting a take.
    pub wanted: usize,
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
        Self { last_chunk: None, load_radius, sent_radius: None, pending: 0, wanted: 0 }
    }
}

/// Keep the server's view of our radius current, and recompute the HUD
/// streaming estimate when the player crosses a chunk boundary. The server
/// pushes/unloads chunks on its own; this mirrors the same box locally so the
/// HUD can show progress without extra protocol.
pub fn track_streaming(
    net: Res<NetClient>,
    cgen: Res<crate::server_msg::ClientGen>,
    mut streaming: ResMut<Streaming>,
    query: Query<&Transform, With<Player>>,
) {
    if streaming.sent_radius != Some(streaming.load_radius) {
        streaming.sent_radius = Some(streaming.load_radius);
        net.send(ClientMsg::ViewRadius {
            radius: streaming.load_radius as u8,
            full_streams: !cgen.hash_ok,
        });
    }
    // `streaming.pending` is maintained by `demand::process_demands`
    // (directory entries + in-flight gen).
    let Ok(transform) = query.single() else { return };
    let p = transform.translation;
    let pc = IVec3::new(
        (p.x.floor() as i32) >> CHUNK_BIT,
        (p.y.floor() as i32) >> CHUNK_BIT,
        (p.z.floor() as i32) >> CHUNK_BIT,
    );
    streaming.last_chunk = Some(pc);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run [`mouse_look`] over the given reports and return the resulting
    /// yaw/pitch. No window, no renderer — just the message queue.
    fn look(reports: &[Vec2]) -> (f32, f32) {
        look_at_sensitivity(1.0, reports)
    }

    /// As [`look`], but with an explicit sensitivity multiplier.
    fn look_at_sensitivity(sensitivity: f32, reports: &[Vec2]) -> (f32, f32) {
        let mut app = App::new();
        app.add_message::<MouseMotion>();
        app.insert_resource(LookSettings { sensitivity });
        app.add_systems(Update, mouse_look);
        let e = app.world_mut().spawn((Player::at(Vec3::ZERO), Transform::default())).id();
        for &delta in reports {
            app.world_mut().write_message(MouseMotion { delta });
        }
        app.update();
        let p = app.world().entity(e).get::<Player>().unwrap();
        (p.yaw, p.pitch)
    }

    #[test]
    fn ordinary_reports_turn_the_camera() {
        let (yaw, pitch) = look(&[Vec2::new(100.0, 0.0), Vec2::new(0.0, -50.0)]);
        assert!((yaw - -100.0 * BASE_SENS).abs() < 1e-6, "yaw {yaw}");
        assert!((pitch - (-0.5 + 50.0 * BASE_SENS)).abs() < 1e-6, "pitch {pitch}");
    }

    /// A Wacom tablet (or a VM/remote pointer) reports raw mouse motion in
    /// absolute mode, and winit passes the 0..65535 screen coordinate through
    /// as a delta. Integrating it pins pitch to the clamp and spins yaw — the
    /// camera gets stuck facing the ground. Those reports must be dropped.
    #[test]
    fn absolute_mode_reports_are_dropped() {
        let (yaw, pitch) = look(&[Vec2::new(30000.0, 30000.0), Vec2::new(32000.0, 31000.0)]);
        assert_eq!((yaw, pitch), (0.0, -0.5), "absolute reports must not move the camera");
    }

    /// One bad report in a frame must not take the good ones with it: the
    /// per-report filter exists precisely because the accumulated resource
    /// would have summed them together.
    #[test]
    fn a_bad_report_does_not_poison_the_frame() {
        let (yaw, _) = look(&[Vec2::new(10.0, 0.0), Vec2::new(40000.0, 0.0), Vec2::new(10.0, 0.0)]);
        assert!((yaw - -20.0 * BASE_SENS).abs() < 1e-6, "yaw {yaw}");
    }

    /// The gate has to sit above the fastest real flick, or a high-DPI mouse
    /// loses reports mid-swipe. 8000 DPI at 1000 Hz moving 3 m/s is ~950
    /// counts per report.
    #[test]
    fn a_fast_flick_still_gets_through() {
        let (yaw, _) = look(&[Vec2::new(950.0, 0.0)]);
        assert!((yaw - -950.0 * BASE_SENS).abs() < 1e-6, "yaw {yaw}");
    }

    #[test]
    fn sensitivity_scales_both_axes() {
        let reports = [Vec2::new(100.0, -100.0)];
        let (slow_yaw, slow_pitch) = look_at_sensitivity(0.5, &reports);
        let (fast_yaw, fast_pitch) = look_at_sensitivity(2.0, &reports);
        assert!((fast_yaw - slow_yaw * 4.0).abs() < 1e-6, "{slow_yaw} -> {fast_yaw}");
        // Pitch starts at -0.5, so compare the travel rather than the value.
        let (slow, fast) = (slow_pitch + 0.5, fast_pitch + 0.5);
        assert!((fast - slow * 4.0).abs() < 1e-6, "{slow} -> {fast}");
    }

    /// Nothing may set a sensitivity of zero (look dies) or a negative one
    /// (both axes invert at once), whichever writer asks.
    #[test]
    fn sensitivity_is_clamped_to_its_range() {
        let mut s = LookSettings { sensitivity: 1.0 };
        s.set(0.0);
        assert_eq!(s.sensitivity, SENS_MIN);
        s.set(-3.0);
        assert_eq!(s.sensitivity, SENS_MIN);
        s.set(1000.0);
        assert_eq!(s.sensitivity, SENS_MAX);
        s.set(f32::NAN);
        assert_eq!(s.sensitivity, SENS_MAX, "NaN must leave the setting alone");
    }

    /// The menu's -/+ walk the step grid and stop at the ends rather than
    /// drifting off it.
    #[test]
    fn nudging_stays_on_the_step_grid() {
        let mut s = LookSettings { sensitivity: 1.0 };
        for _ in 0..3 {
            s.nudge(-1);
        }
        assert!((s.sensitivity - 0.7).abs() < 1e-6, "{}", s.sensitivity);
        for _ in 0..100 {
            s.nudge(-1);
        }
        assert_eq!(s.sensitivity, SENS_MIN, "must stop at the floor");
        for _ in 0..1000 {
            s.nudge(1);
        }
        assert_eq!(s.sensitivity, SENS_MAX, "must stop at the ceiling");
    }
}
