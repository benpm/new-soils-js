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
    /// Buffered `(tick, eye position, velocity)` snapshots, tick-ascending.
    buffer: std::collections::VecDeque<(u32, Vec3, Vec3)>,
}

/// Render this many server ticks behind the newest snapshot.
const INTERP_DELAY_TICKS: f32 = 2.0;
/// Cap on velocity extrapolation past the newest snapshot (seconds).
const EXTRAPOLATE_CAP: f32 = 0.25;

impl Actor {
    pub fn new(kind: u16, tick: u32, pos: Vec3) -> Self {
        let mut buffer = std::collections::VecDeque::new();
        buffer.push_back((tick, pos, Vec3::ZERO));
        Self { kind, buffer }
    }

    pub fn push_snapshot(&mut self, tick: u32, pos: Vec3, vel: Vec3) {
        if self.buffer.back().is_some_and(|(t, ..)| *t >= tick) {
            return; // stale or duplicate
        }
        self.buffer.push_back((tick, pos, vel));
        while self.buffer.len() > 32 {
            self.buffer.pop_front();
        }
    }

    /// Sample the eye position at fractional server tick `t`.
    fn sample(&mut self, t: f32) -> Option<Vec3> {
        // Drop segments entirely behind the render time (keep one anchor).
        while self.buffer.len() >= 2 && (self.buffer[1].0 as f32) <= t {
            self.buffer.pop_front();
        }
        let &(t0, p0, v0) = self.buffer.front()?;
        match self.buffer.get(1) {
            Some(&(t1, p1, _)) => {
                let span = (t1 - t0).max(1) as f32;
                let f = ((t - t0 as f32) / span).clamp(0.0, 1.0);
                Some(p0.lerp(p1, f))
            }
            None => {
                // Beyond the buffer: extrapolate along the last velocity, capped.
                let dt = ((t - t0 as f32) / soils_sim::SERVER_TICK_HZ as f32)
                    .clamp(0.0, EXTRAPOLATE_CAP);
                Some(p0 + v0 * dt)
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
            KindAssets { mesh, material, body_drop: hy }
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
        let Some(eye) = actor.sample(t) else { continue };
        let drop = assets.kinds.get(actor.kind as usize).map_or(0.9, |k| k.body_drop);
        transform.translation = eye - Vec3::Y * drop;
    }
}
