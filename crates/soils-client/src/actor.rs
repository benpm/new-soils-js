//! Remote-entity rendering. The server simulates every entity (players,
//! critters, ...) and replicates spawn/despawn/state by NetId; we spawn a
//! body per remote entity — shaped by the shared `entities.yaml` registry —
//! and smoothly interpolate it toward its latest target.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;

/// Our own network identity (from the server's `Init`): connection id plus
/// the NetId of our player entity (whose updates drive the camera, not a
/// body).
#[derive(Resource, Default)]
pub struct LocalPlayer {
    pub id: u16,
    pub self_entity: u32,
}

/// Maps entity NetIds to their spawned body.
#[derive(Resource, Default)]
pub struct ActorMap {
    pub map: HashMap<u32, Entity>,
}

/// Per-kind mesh/material built from the shared entity registry (kind id =
/// index).
#[derive(Resource)]
pub struct ActorAssets {
    pub kinds: Vec<KindAssets>,
}

pub struct KindAssets {
    pub mesh: Handle<Mesh>,
    pub material: Handle<StandardMaterial>,
    /// Vertical offset from the replicated eye position to the body center.
    pub body_drop: f32,
}

/// A remote entity body, rendered from a small snapshot buffer at a short
/// delay behind the server clock (plan-game-systems §4 receive side): smooth
/// interpolation between buffered ticks, capped velocity extrapolation when
/// the buffer runs dry.
#[derive(Component)]
pub struct Actor {
    pub kind: u16,
    /// Buffered `(tick, eye position, velocity, orientation)` snapshots,
    /// tick-ascending. Orientation is identity for yaw-only entities and the
    /// real body quaternion for rigid-body physics entities.
    buffer: std::collections::VecDeque<(u32, Vec3, Vec3, Quat)>,
}

/// Render this many server ticks behind the newest snapshot.
const INTERP_DELAY_TICKS: f32 = 2.0;
/// Cap on velocity extrapolation past the newest snapshot (seconds).
const EXTRAPOLATE_CAP: f32 = 0.25;

impl Actor {
    pub fn new(kind: u16, tick: u32, pos: Vec3) -> Self {
        let mut buffer = std::collections::VecDeque::new();
        buffer.push_back((tick, pos, Vec3::ZERO, Quat::IDENTITY));
        Self { kind, buffer }
    }

    /// Newest buffered eye position. Prediction collides against this rather
    /// than the render-delayed interpolated one: the server resolves
    /// player-vs-player collision against its tick-boundary positions, and the
    /// newest snapshot is the closest a client has to those, so the two agree
    /// and contact does not spray reconciliations.
    pub fn latest_pos(&self) -> Option<Vec3> {
        self.buffer.back().map(|&(_, pos, ..)| pos)
    }

    pub fn push_snapshot(&mut self, tick: u32, pos: Vec3, vel: Vec3, rot: Quat) {
        if self.buffer.back().is_some_and(|(t, ..)| *t >= tick) {
            return; // stale or duplicate
        }
        self.buffer.push_back((tick, pos, vel, rot));
        while self.buffer.len() > 32 {
            self.buffer.pop_front();
        }
    }

    /// Sample the eye position and orientation at fractional server tick `t`.
    fn sample(&mut self, t: f32) -> Option<(Vec3, Quat)> {
        // Drop segments entirely behind the render time (keep one anchor).
        while self.buffer.len() >= 2 && (self.buffer[1].0 as f32) <= t {
            self.buffer.pop_front();
        }
        let &(t0, p0, v0, r0) = self.buffer.front()?;
        match self.buffer.get(1) {
            Some(&(t1, p1, _, r1)) => {
                let span = (t1 - t0).max(1) as f32;
                let f = ((t - t0 as f32) / span).clamp(0.0, 1.0);
                Some((p0.lerp(p1, f), r0.slerp(r1, f)))
            }
            None => {
                // Beyond the buffer: extrapolate along the last velocity, capped;
                // hold the last orientation.
                let dt = ((t - t0 as f32) / soils_sim::SERVER_TICK_HZ as f32)
                    .clamp(0.0, EXTRAPOLATE_CAP);
                Some((p0 + v0 * dt, r0))
            }
        }
    }
}

/// The remote-body render clock, in fractional server ticks. Advances at the
/// server tick rate and eases toward `newest snapshot − INTERP_DELAY_TICKS`
/// so drift (clock skew, hitches) corrects smoothly instead of snapping.
#[derive(Resource, Default)]
pub struct InterpClock {
    pub t: f32,
    newest: u32,
}

impl InterpClock {
    pub fn observe(&mut self, tick: u32) {
        self.newest = self.newest.max(tick);
    }
}

/// Build one body mesh + material per registry kind.
pub fn setup_actor_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let registry = soils_sim::default_entity_registry();
    let kinds = (0..registry.len() as u16)
        .map(|kind| {
            let def = registry.get(kind).unwrap();
            let [hx, hy, hz] = def.half_extents;
            let mesh = match def.render.as_str() {
                "capsule" => meshes.add(Capsule3d::new(hx, (hy - hx).max(0.1) * 2.0)),
                _ => meshes.add(Cuboid::new(hx * 2.0, hy * 2.0, hz * 2.0)),
            };
            // Simple per-kind tint: players orange, critters greenish, then
            // rotate hues for future kinds.
            let hue = 25.0 + kind as f32 * 95.0;
            let material = materials.add(StandardMaterial {
                base_color: Color::hsl(hue % 360.0, 0.7, 0.5),
                perceptual_roughness: 0.8,
                ..default()
            });
            // Physics props replicate their body *center* (Avian Position), so
            // no eye→center drop; players/critters replicate the eye position.
            let body_drop = if kind == soils_sim::KIND_PHYSICS_CUBE { 0.0 } else { hy };
            KindAssets { mesh, material, body_drop }
        })
        .collect();
    commands.insert_resource(ActorAssets { kinds });
}

/// Render remote bodies from their snapshot buffers at the delayed render
/// clock (replaces the old fixed-rate exponential lerp).
pub fn interpolate_actors(
    time: Res<Time>,
    assets: Res<ActorAssets>,
    mut clock: ResMut<InterpClock>,
    mut query: Query<(&mut Transform, &mut Actor)>,
) {
    if clock.newest == 0 {
        return; // no snapshots yet
    }
    let target = clock.newest as f32 - INTERP_DELAY_TICKS;
    if clock.t == 0.0 {
        clock.t = target;
    }
    clock.t += time.delta_secs() * soils_sim::SERVER_TICK_HZ as f32;
    clock.t += (target - clock.t) * (time.delta_secs() * 2.0).min(1.0);

    let t = clock.t;
    for (mut transform, mut actor) in &mut query {
        let Some((eye, rot)) = actor.sample(t) else { continue };
        let drop = assets.kinds.get(actor.kind as usize).map_or(0.9, |k| k.body_drop);
        transform.translation = eye - Vec3::Y * drop;
        transform.rotation = rot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Server ticks per rendered frame at 60 fps: the render clock advances in
    /// server ticks, so this is `SERVER_TICK_HZ / fps`.
    const TICKS_PER_FRAME: f32 = soils_sim::SERVER_TICK_HZ as f32 / 60.0;
    /// A body walking at a constant 8 u/s along +X.
    const SPEED: f32 = 8.0;

    /// Replicate a constant-velocity body at 20 Hz. `drop_every` simulates
    /// packet loss by omitting every Nth snapshot (0 = lossless).
    fn linear_actor(ticks: u32, drop_every: u32) -> Actor {
        let vel = Vec3::new(SPEED, 0.0, 0.0);
        let per_tick = vel / soils_sim::SERVER_TICK_HZ as f32;
        let mut a = Actor::new(soils_sim::KIND_PLAYER, 0, Vec3::ZERO);
        for t in 1..=ticks {
            if drop_every != 0 && t % drop_every == 0 {
                continue; // dropped in flight
            }
            a.push_snapshot(t, per_tick * t as f32, vel, Quat::IDENTITY);
        }
        a
    }

    /// Walk the render clock across everything the buffer holds, one 60 fps
    /// frame at a time, returning the per-frame displacement along +X.
    ///
    /// The span is taken from the buffer rather than assumed: the buffer keeps
    /// only the newest 32 ticks, so a render clock older than that sits behind
    /// the oldest segment and `sample` correctly clamps to a constant. Real
    /// rendering never gets there — `INTERP_DELAY_TICKS` keeps the clock two
    /// ticks behind the newest — but a test that started at tick 0 would.
    fn frame_steps(actor: &mut Actor) -> Vec<f32> {
        let first = actor.buffer.front().expect("a buffered snapshot").0 as f32;
        let last = actor.buffer.back().expect("a buffered snapshot").0 as f32;
        let mut t = first;
        let mut prev = actor.sample(t).expect("a buffered sample").0;
        let mut steps = Vec::new();
        while t + TICKS_PER_FRAME <= last {
            t += TICKS_PER_FRAME;
            let now = actor.sample(t).expect("a buffered sample").0;
            steps.push(now.x - prev.x);
            prev = now;
        }
        assert!(!steps.is_empty(), "nothing sampled");
        steps
    }

    /// What one 60 fps frame of 8 u/s motion should cover.
    fn expected_step() -> f32 {
        SPEED / 60.0
    }

    #[test]
    fn interpolation_is_smooth_at_60hz() {
        let mut a = linear_actor(40, 0);
        let steps = frame_steps(&mut a);
        let want = expected_step();
        for (i, s) in steps.iter().enumerate() {
            assert!(
                (s - want).abs() < 1e-3,
                "frame {i} moved {s}, expected {want} — interpolation is not uniform"
            );
        }
    }

    #[test]
    fn lost_snapshots_do_not_produce_a_jump() {
        // Every third snapshot never arrives. The segments either side simply
        // span two ticks instead of one, so constant-velocity motion must stay
        // exactly as smooth — this is the property that makes delta snapshot
        // loss survivable rather than visible.
        let mut a = linear_actor(40, 3);
        let steps = frame_steps(&mut a);
        let want = expected_step();
        let worst = steps.iter().map(|s| (s - want).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-3, "worst frame deviated by {worst} under 33% snapshot loss");
    }

    #[test]
    fn heavy_loss_still_never_moves_backwards() {
        // Half the stream gone. Positions may be coarser, but motion must stay
        // monotonic: a backwards step is the visible stutter players notice.
        let mut a = linear_actor(60, 2);
        for (i, s) in frame_steps(&mut a).iter().enumerate() {
            assert!(*s >= -1e-4, "frame {i} moved backwards by {s}");
        }
    }

    #[test]
    fn the_interp_delay_keeps_sampling_off_the_extrapolator() {
        // Rendering INTERP_DELAY_TICKS behind the newest snapshot should mean
        // the next segment has always arrived, so `sample` interpolates rather
        // than extrapolating. Extrapolation is the fallback that snaps when the
        // body changes direction, so staying off it is what buys smoothness.
        let mut a = linear_actor(40, 0);
        let newest = 40.0;
        let mut t = a.buffer.front().expect("a buffered snapshot").0 as f32;
        while t < newest - INTERP_DELAY_TICKS {
            a.sample(t);
            assert!(
                a.buffer.len() >= 2,
                "at render tick {t} the buffer held only {} entries — sampling fell \
                 through to extrapolation",
                a.buffer.len()
            );
            t += TICKS_PER_FRAME;
        }
    }

    #[test]
    fn extrapolation_is_capped_when_the_stream_dies() {
        // The link drops entirely after tick 10. The body must coast, not fly
        // off: the cap bounds how far a dead stream can carry it.
        let mut a = linear_actor(10, 0);
        let far = a.sample(10.0 + 100.0 * soils_sim::SERVER_TICK_HZ as f32).expect("sample").0;
        let last = SPEED / soils_sim::SERVER_TICK_HZ as f32 * 10.0;
        assert!(
            far.x - last <= SPEED * EXTRAPOLATE_CAP + 1e-3,
            "extrapolated {} past the last snapshot, cap allows {}",
            far.x - last,
            SPEED * EXTRAPOLATE_CAP
        );
    }

    #[test]
    fn stale_and_duplicate_snapshots_are_ignored() {
        // Jitter can deliver an older snapshot after a newer one; accepting it
        // would drag the body backwards.
        let mut a = Actor::new(soils_sim::KIND_PLAYER, 5, Vec3::ZERO);
        a.push_snapshot(6, Vec3::X, Vec3::ZERO, Quat::IDENTITY);
        a.push_snapshot(5, Vec3::new(-99.0, 0.0, 0.0), Vec3::ZERO, Quat::IDENTITY);
        a.push_snapshot(6, Vec3::new(-99.0, 0.0, 0.0), Vec3::ZERO, Quat::IDENTITY);
        assert_eq!(a.buffer.len(), 2, "stale and duplicate ticks must be dropped");
        assert_eq!(a.buffer[1].1, Vec3::X);
    }

    #[test]
    fn latest_pos_tracks_the_newest_snapshot() {
        // Prediction collides against this, so it must be the newest known
        // state and not the render-delayed one.
        let mut a = linear_actor(20, 0);
        a.sample(4.0); // advancing the render clock must not change it
        let per_tick = SPEED / soils_sim::SERVER_TICK_HZ as f32;
        assert!((a.latest_pos().unwrap().x - per_tick * 20.0).abs() < 1e-4);
    }
}
