//! Shared gameplay simulation used by the client and (in later phases) the
//! authoritative server: player movement with AABB voxel collision, voxel
//! raycasting, and edit rules. Engine-free — pure functions over a
//! [`VoxelSampler`], so each side supplies its own world storage.
//!
//! The movement and raycast code is ported verbatim from the client's original
//! per-frame systems (`player.rs`, `edit.rs`) so behavior is unchanged; only
//! the voxel lookup is abstracted.

pub mod light;

use glam::{IVec3, Quat, Vec2, Vec3};
use soils_worldgen::BlockRegistry;

/// Simulation tick rate (Hz) for fixed-step gameplay logic. The client's
/// `FixedUpdate` runs at this rate.
pub const TICK_HZ: f64 = 64.0;

/// The server's fixed tick rate (Hz): net-message drain, replication cadence,
/// and (from plan M4) server-side player stepping. Lower than [`TICK_HZ`]
/// because the server replicates rather than renders; when M4 makes the server
/// step players via `soils-sim`, it will run multiple sim steps per server
/// tick (or these two get unified) so both sides integrate at the same dt.
pub const SERVER_TICK_HZ: f64 = 20.0;

// Movement tuning.
pub const MOVE_SPEED: f32 = 8.0;
pub const SPRINT_MULT: f32 = 4.0;
pub const GRAVITY: f32 = 28.0;
pub const JUMP_SPEED: f32 = 9.0;

// Player AABB relative to the eye position (eye near the top of the body).
pub const EYE_TO_FEET: f32 = 1.6;
pub const EYE_TO_HEAD: f32 = 0.2;
pub const HALF_WIDTH: f32 = 0.3;

/// Max interaction distance (Chebyshev, in voxels) for raycasts and edits.
pub const REACH: i32 = 8;

/// Read-only voxel access the simulation runs against. Unloaded space must
/// read as Air (id 0) — the established contract on both sides.
pub trait VoxelSampler {
    fn voxel(&self, v: IVec3) -> u8;

    #[inline]
    fn is_solid(&self, v: IVec3) -> bool {
        self.voxel(v) != 0
    }
}

/// Closures work as samplers, so callers needn't define adapter types.
impl<F: Fn(IVec3) -> u8> VoxelSampler for F {
    #[inline]
    fn voxel(&self, v: IVec3) -> u8 {
        self(v)
    }
}

/// A player's simulation state. `pos` is the eye position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerState {
    pub pos: Vec3,
    pub vel: Vec3,
    pub flying: bool,
    pub grounded: bool,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self { pos: Vec3::ZERO, vel: Vec3::ZERO, flying: true, grounded: false }
    }
}

/// One tick of player input. `move_axes` is local-space (strafe right+,
/// forward+); [`step_player`] builds the yaw basis, so this struct already has
/// the shape a future network input message needs. `jump` and `toggle_fly` are
/// edge events: the caller latches them between ticks and clears them after
/// the tick that consumes them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerInput {
    pub move_axes: Vec2,
    pub yaw: f32,
    pub sprint: bool,
    pub jump: bool,
    pub up: bool,
    pub down: bool,
    pub toggle_fly: bool,
}

/// Advance a player by `dt`. Fly mode is free 6-DOF; walk mode applies gravity
/// and axis-separated AABB voxel collision, stopping on contact.
pub fn step_player(
    state: &mut PlayerState,
    input: &PlayerInput,
    dt: f32,
    world: &impl VoxelSampler,
) {
    if input.toggle_fly {
        state.flying = !state.flying;
        state.vel = Vec3::ZERO;
    }

    // Horizontal basis from yaw only.
    let yaw_rot = Quat::from_axis_angle(Vec3::Y, input.yaw);
    let forward = yaw_rot * Vec3::NEG_Z;
    let right = yaw_rot * Vec3::X;
    let wish = (right * input.move_axes.x + forward * input.move_axes.y).normalize_or_zero();

    let mut speed = MOVE_SPEED;
    if input.sprint {
        speed *= SPRINT_MULT;
    }

    if state.flying {
        let mut dir = wish * speed;
        if input.up {
            dir.y += speed;
        }
        if input.down {
            dir.y -= speed;
        }
        state.pos += dir * dt;
        return;
    }

    // Walking: horizontal from input, vertical integrates gravity.
    state.vel.x = wish.x * speed;
    state.vel.z = wish.z * speed;
    state.vel.y -= GRAVITY * dt;
    if state.grounded && input.jump {
        state.vel.y = JUMP_SPEED;
    }

    let delta = state.vel * dt;
    let mut pos = state.pos;

    // Resolve one axis at a time; stop on contact.
    pos.x += delta.x;
    if collides(world, pos) {
        pos.x -= delta.x;
        state.vel.x = 0.0;
    }
    pos.z += delta.z;
    if collides(world, pos) {
        pos.z -= delta.z;
        state.vel.z = 0.0;
    }
    pos.y += delta.y;
    state.grounded = false;
    if collides(world, pos) {
        pos.y -= delta.y;
        if state.vel.y < 0.0 {
            state.grounded = true;
        }
        state.vel.y = 0.0;
    }

    state.pos = pos;
}

/// True if the player AABB at eye position `eye` overlaps any solid voxel.
fn collides(world: &impl VoxelSampler, eye: Vec3) -> bool {
    let min = Vec3::new(eye.x - HALF_WIDTH, eye.y - EYE_TO_FEET, eye.z - HALF_WIDTH);
    let max = Vec3::new(eye.x + HALF_WIDTH, eye.y + EYE_TO_HEAD, eye.z + HALF_WIDTH);
    let (x0, y0, z0) = (min.x.floor() as i32, min.y.floor() as i32, min.z.floor() as i32);
    let (x1, y1, z1) = (max.x.floor() as i32, max.y.floor() as i32, max.z.floor() as i32);
    for x in x0..=x1 {
        for y in y0..=y1 {
            for z in z0..=z1 {
                if world.is_solid(IVec3::new(x, y, z)) {
                    return true;
                }
            }
        }
    }
    false
}

/// A voxel raycast hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RayHit {
    /// The solid voxel that was hit.
    pub voxel: IVec3,
    /// The empty voxel just before it (where a new block is placed). Equals
    /// `voxel` when the ray starts inside a solid voxel.
    pub prev: IVec3,
}

/// Amanatides–Woo voxel traversal from `origin` along `dir`, out to [`REACH`].
pub fn raycast_voxel(origin: Vec3, dir: Vec3, world: &impl VoxelSampler) -> Option<RayHit> {
    let mut voxel = origin.floor().as_ivec3();
    let step = IVec3::new(
        dir.x.signum() as i32,
        dir.y.signum() as i32,
        dir.z.signum() as i32,
    );

    // Distance along the ray to the next grid line on each axis.
    let next_boundary = |o: f32, d: f32, v: i32, s: i32| -> f32 {
        if d == 0.0 {
            f32::INFINITY
        } else if s > 0 {
            ((v + 1) as f32 - o) / d
        } else {
            (v as f32 - o) / d
        }
    };
    let mut t_max = Vec3::new(
        next_boundary(origin.x, dir.x, voxel.x, step.x),
        next_boundary(origin.y, dir.y, voxel.y, step.y),
        next_boundary(origin.z, dir.z, voxel.z, step.z),
    );
    let t_delta = Vec3::new(
        if dir.x == 0.0 { f32::INFINITY } else { (1.0 / dir.x).abs() },
        if dir.y == 0.0 { f32::INFINITY } else { (1.0 / dir.y).abs() },
        if dir.z == 0.0 { f32::INFINITY } else { (1.0 / dir.z).abs() },
    );

    let mut prev = voxel;
    for _ in 0..(REACH * 3) {
        if world.voxel(voxel) != 0 {
            return Some(RayHit { voxel, prev });
        }
        prev = voxel;
        // Advance to the nearest axis boundary.
        if t_max.x < t_max.y && t_max.x < t_max.z {
            voxel.x += step.x;
            t_max.x += t_delta.x;
        } else if t_max.y < t_max.z {
            voxel.y += step.y;
            t_max.y += t_delta.y;
        } else {
            voxel.z += step.z;
            t_max.z += t_delta.z;
        }
        if (voxel - origin.floor().as_ivec3()).abs().max_element() > REACH {
            break;
        }
    }
    None
}

/// Edit legality shared by client and (later) server: the target must be
/// within [`REACH`] (Chebyshev, matching the raycast metric) of the eye, and
/// `value` must be a known block id (Air = break is id 0 and always known).
pub fn validate_edit(eye: Vec3, target: IVec3, value: u8, registry: &BlockRegistry) -> bool {
    let within = (target - eye.floor().as_ivec3()).abs().max_element() <= REACH;
    within && registry.get(value).is_some()
}

/// Day-length easing ported from the JS `ease10`: a steep ease-in/out that
/// holds bright through midday and dark through midnight. Input is
/// `daytime * 2 - 1` (daytime 0 = noon); output 1 at noon, 0 at midnight.
/// Shared by rendering (sun/exposure/sky term) and, later, server gameplay
/// (night spawn gating).
pub fn ease10(t: f32) -> f32 {
    let v = if t < 0.5 { 512.0 * t.powi(10) } else { -512.0 * (t - 1.0).powi(10) + 1.0 };
    v.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Sparse voxel map: anything absent is Air, like unloaded chunks.
    struct SparseWorld(HashMap<IVec3, u8>);

    impl SparseWorld {
        fn new() -> Self {
            Self(HashMap::new())
        }

        fn set(&mut self, x: i32, y: i32, z: i32, id: u8) {
            self.0.insert(IVec3::new(x, y, z), id);
        }

        /// Solid floor plane at `y` covering [-r, r]² in x/z.
        fn with_floor(y: i32, r: i32) -> Self {
            let mut w = Self::new();
            for x in -r..=r {
                for z in -r..=r {
                    w.set(x, y, z, 1);
                }
            }
            w
        }
    }

    impl VoxelSampler for SparseWorld {
        fn voxel(&self, v: IVec3) -> u8 {
            *self.0.get(&v).unwrap_or(&0)
        }
    }

    const DT: f32 = 1.0 / TICK_HZ as f32;

    /// A grounded walking state settled onto a floor whose top surface is y=1.
    /// Settling via gravity (rather than placing feet at exactly 1.0) matters:
    /// in f32, `(1.0 + EYE_TO_FEET) - EYE_TO_FEET < 1.0`, which would dip the
    /// AABB into the floor row and wedge the player — a state normal play
    /// never reaches, since landing leaves feet a sub-step above the surface.
    fn standing_state(world: &impl VoxelSampler) -> PlayerState {
        let mut state = PlayerState {
            pos: Vec3::new(0.5, 1.0 + EYE_TO_FEET + 0.2, 0.5),
            vel: Vec3::ZERO,
            flying: false,
            grounded: false,
        };
        for _ in 0..30 {
            step_player(&mut state, &default_input(), DT, world);
        }
        assert!(state.grounded, "settled onto the floor");
        state
    }

    #[test]
    fn wall_stops_x_but_slides_z() {
        // Floor at y=0 (top surface y=1) and a wall filling x=3.
        let mut world = SparseWorld::with_floor(0, 10);
        for y in 1..=4 {
            for z in -10..=10 {
                world.set(3, y, z, 1);
            }
        }
        let mut state = standing_state(&world);
        // yaw=0: right=+X, forward=-Z. Move diagonally toward +x/-z.
        let input = PlayerInput { move_axes: Vec2::new(1.0, 1.0), ..default_input() };
        // 100 steps: enough to hit the wall (~25) and slide well past z=-5,
        // but not enough to reach the floor edge at z=-10 and drop under the wall.
        for _ in 0..100 {
            step_player(&mut state, &input, DT, &world);
        }
        // Blocked before the wall face at x=3, but z kept sliding.
        assert!(state.pos.x < 3.0 - HALF_WIDTH + 1e-3, "x stopped at wall: {}", state.pos.x);
        assert!(state.pos.z < -5.0, "z kept sliding: {}", state.pos.z);
        assert_eq!(state.vel.x, 0.0);
    }

    #[test]
    fn fall_lands_grounded_on_floor() {
        let world = SparseWorld::with_floor(0, 4);
        let mut state = PlayerState {
            pos: Vec3::new(0.5, 8.0, 0.5),
            vel: Vec3::ZERO,
            flying: false,
            grounded: false,
        };
        let input = default_input();
        for _ in 0..300 {
            step_player(&mut state, &input, DT, &world);
        }
        assert!(state.grounded, "landed");
        assert_eq!(state.vel.y, 0.0);
        // Feet rest on (or a sub-step above) the floor surface at y=1.
        let feet = state.pos.y - EYE_TO_FEET;
        assert!((1.0..1.3).contains(&feet), "feet at {feet}");
    }

    #[test]
    fn jump_reaches_expected_apex() {
        let world = SparseWorld::with_floor(0, 4);
        let mut state = standing_state(&world);
        let start_y = state.pos.y;
        let mut input = default_input();
        input.jump = true;
        step_player(&mut state, &input, DT, &world);
        input.jump = false; // edge consumed
        let mut apex = state.pos.y;
        for _ in 0..200 {
            step_player(&mut state, &input, DT, &world);
            apex = apex.max(state.pos.y);
        }
        // Analytic apex: v²/2g ≈ 1.45 above the start; discrete integration
        // lands slightly under.
        let height = apex - start_y;
        assert!((1.1..1.6).contains(&height), "apex height {height}");
        assert!(state.grounded, "came back down");
    }

    #[test]
    fn fly_ignores_gravity_and_moves_vertically() {
        let world = SparseWorld::new();
        let mut state = PlayerState { pos: Vec3::splat(10.0), ..PlayerState::default() };
        assert!(state.flying);
        let input = default_input();
        for _ in 0..100 {
            step_player(&mut state, &input, DT, &world);
        }
        assert_eq!(state.pos, Vec3::splat(10.0), "no drift without input");

        let up = PlayerInput { up: true, ..default_input() };
        step_player(&mut state, &up, DT, &world);
        assert!(state.pos.y > 10.0);
    }

    #[test]
    fn toggle_fly_zeroes_velocity() {
        let world = SparseWorld::new();
        let mut state = PlayerState {
            pos: Vec3::new(0.5, 20.0, 0.5),
            vel: Vec3::ZERO,
            flying: false,
            grounded: false,
        };
        let input = default_input();
        for _ in 0..30 {
            step_player(&mut state, &input, DT, &world);
        }
        assert!(state.vel.y < 0.0, "falling");
        let toggle = PlayerInput { toggle_fly: true, ..default_input() };
        step_player(&mut state, &toggle, DT, &world);
        assert!(state.flying);
        assert_eq!(state.vel, Vec3::ZERO);
    }

    #[test]
    fn raycast_hits_voxel_and_prev_per_axis() {
        let mut world = SparseWorld::new();
        world.set(5, 2, 3, 7);
        // +X approach.
        let hit = raycast_voxel(Vec3::new(0.5, 2.5, 3.5), Vec3::X, &world).unwrap();
        assert_eq!(hit.voxel, IVec3::new(5, 2, 3));
        assert_eq!(hit.prev, IVec3::new(4, 2, 3));
        // -Z approach.
        let hit = raycast_voxel(Vec3::new(5.5, 2.5, 8.5), Vec3::NEG_Z, &world).unwrap();
        assert_eq!(hit.voxel, IVec3::new(5, 2, 3));
        assert_eq!(hit.prev, IVec3::new(5, 2, 4));
        // Diagonal approach still resolves a face-adjacent prev.
        let hit = raycast_voxel(Vec3::new(2.5, 0.5, 1.5), Vec3::new(1.0, 0.7, 0.6).normalize(), &world);
        if let Some(hit) = hit {
            assert_eq!((hit.voxel - hit.prev).abs().max_element(), 1);
        }
    }

    #[test]
    fn raycast_respects_reach() {
        let mut world = SparseWorld::new();
        world.set(REACH + 2, 0, 0, 1);
        assert!(raycast_voxel(Vec3::new(0.5, 0.5, 0.5), Vec3::X, &world).is_none());
    }

    #[test]
    fn raycast_from_inside_solid_hits_self() {
        let mut world = SparseWorld::new();
        world.set(1, 1, 1, 1);
        let hit = raycast_voxel(Vec3::new(1.5, 1.5, 1.5), Vec3::X, &world).unwrap();
        assert_eq!(hit.voxel, IVec3::new(1, 1, 1));
        assert_eq!(hit.prev, hit.voxel, "prev == voxel when starting inside");
    }

    #[test]
    fn deterministic_across_runs() {
        let mut world = SparseWorld::with_floor(0, 20);
        for y in 1..=3 {
            world.set(6, y, 2, 1);
        }
        let run = || {
            let mut state = standing_state(&world);
            for i in 0..500u32 {
                let input = PlayerInput {
                    move_axes: Vec2::new(if i % 3 == 0 { 1.0 } else { 0.0 }, 1.0),
                    yaw: (i as f32) * 0.01,
                    sprint: i % 7 == 0,
                    jump: i % 64 == 0,
                    ..default_input()
                };
                step_player(&mut state, &input, DT, &world);
            }
            state
        };
        assert_eq!(run(), run(), "identical inputs give bit-identical states");
    }

    #[test]
    fn edit_validation_reach_and_id() {
        let yaml = "Air:\n  faces: [0,0,0]\nDirt:\n  faces: [1,1,1]\n";
        let reg = BlockRegistry::from_yaml(yaml).unwrap();
        let eye = Vec3::new(0.5, 0.5, 0.5);
        assert!(validate_edit(eye, IVec3::new(REACH, 0, 0), 1, &reg));
        assert!(!validate_edit(eye, IVec3::new(REACH + 1, 0, 0), 1, &reg), "out of reach");
        assert!(validate_edit(eye, IVec3::new(1, 0, 0), 0, &reg), "break (Air) is legal");
        assert!(!validate_edit(eye, IVec3::new(1, 0, 0), 99, &reg), "unknown id");
    }

    fn default_input() -> PlayerInput {
        PlayerInput::default()
    }
}
