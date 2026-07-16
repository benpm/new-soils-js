//! Shared 3D rigid-body physics (Avian), simulated identically on the server
//! (authority) and client (local prediction) — the same split `soils-sim` uses
//! for player movement.
//!
//! This crate owns the Avian configuration, body/collider builders, the
//! voxel→collider conversion, and (later stages) the rollback save/restore/step
//! primitives. It is engine-agnostic beyond Avian + a headless Bevy ECS/math
//! subset, so it links into the headless server as well as the render client.
//!
//! Determinism: the netcode is predict + authoritative-snapshot-reconcile, not
//! lockstep, so Avian need not be bit-identical across machines — the server is
//! authoritative and clients rebase to it. We still run single-threaded
//! (`parallel` off) with `enhanced-determinism` so a client's rollback
//! *re-simulation* of unchanged inputs matches its original prediction, which
//! keeps corrections rare.

use avian3d::prelude::*;
use bevy::prelude::*;

mod collider;
pub use collider::{chunk_collider, chunk_collider_bundle, chunk_origin_world};

/// Physics tick rate, shared by client and server so both integrate the same
/// way. Independent of the 20 Hz server ECS / 64 Hz client input cadence; the
/// reconcile loop bridges the rates.
pub const PHYSICS_HZ: f64 = 60.0;

/// World gravity vector (game gravity from `soils-sim`, pointing down).
pub fn gravity() -> Vec3 {
    Vec3::NEG_Y * soils_sim::GRAVITY
}

/// Add Avian to a Bevy app with our gravity. Physics runs in `FixedPostUpdate`
/// (Avian's default schedule).
///
/// `interpolate` turns on Avian's transform interpolation so rendered bodies
/// ease between the fixed physics ticks — set it on the render client (props
/// look smooth above the physics rate and corrections don't pop), leave it off
/// on the headless server (which reads `Position`, never `Transform`).
///
/// Avian's prepare step needs transform propagation; the headless server has no
/// `TransformPlugin`, so add it here if the app lacks it. The render client
/// already has it via `DefaultPlugins`, so guard against a double-add (which
/// would panic).
pub fn add_physics(app: &mut App, interpolate: bool) {
    // Avian's broad-phase/collider-tree work uses the Bevy task pools; a bare
    // headless app (server) never initialised them. Add the pools + transform
    // propagation if missing; the render client already has both via
    // `DefaultPlugins`, so guard against a double-add (which panics).
    if !app.is_plugin_added::<bevy::app::TaskPoolPlugin>() {
        app.add_plugins(bevy::app::TaskPoolPlugin::default());
    }
    if !app.is_plugin_added::<bevy::transform::TransformPlugin>() {
        app.add_plugins(bevy::transform::TransformPlugin);
    }
    if interpolate {
        app.add_plugins(
            PhysicsPlugins::default().set(PhysicsInterpolationPlugin::interpolate_all()),
        );
    } else {
        app.add_plugins(PhysicsPlugins::default());
    }
    app.insert_resource(Gravity(gravity()));
}

/// A dynamic cube of the given full side length centred at `pos`. Avian derives
/// `Position`/`Rotation` from the `Transform` during preparation.
pub fn cube_body(pos: Vec3, size: f32) -> impl Bundle {
    (
        RigidBody::Dynamic,
        Collider::cuboid(size, size, size),
        Transform::from_translation(pos),
    )
}

/// A dynamic sphere of the given radius centred at `pos`.
pub fn sphere_body(pos: Vec3, radius: f32) -> impl Bundle {
    (
        RigidBody::Dynamic,
        Collider::sphere(radius),
        Transform::from_translation(pos),
    )
}

/// A static cuboid collider (immovable), full side lengths, centred at `pos`.
pub fn static_cuboid(pos: Vec3, extents: Vec3) -> impl Bundle {
    (
        RigidBody::Static,
        Collider::cuboid(extents.x, extents.y, extents.z),
        Transform::from_translation(pos),
    )
}

/// Half the player collider's height (matches the `soils-sim` player AABB:
/// eye-to-feet + eye-to-head).
pub const PLAYER_HALF_HEIGHT: f32 = (soils_sim::EYE_TO_FEET + soils_sim::EYE_TO_HEAD) / 2.0;
/// Offset (Y) from the replicated eye position to the player collider centre.
pub const PLAYER_CENTER_DY: f32 = (soils_sim::EYE_TO_HEAD - soils_sim::EYE_TO_FEET) / 2.0;

/// The player collider centre for an eye position `pos`.
pub fn player_center(pos: Vec3) -> Vec3 {
    pos + Vec3::new(0.0, PLAYER_CENTER_DY, 0.0)
}

/// A kinematic player proxy: it collides with and pushes dynamic props but is
/// itself driven externally (its `Position` is set from `soils-sim` each tick),
/// so movement feel is unchanged. `pos` is the eye position.
pub fn player_proxy(pos: Vec3) -> impl Bundle {
    (
        RigidBody::Kinematic,
        Collider::cuboid(
            soils_sim::HALF_WIDTH * 2.0,
            PLAYER_HALF_HEIGHT * 2.0,
            soils_sim::HALF_WIDTH * 2.0,
        ),
        Transform::from_translation(player_center(pos)),
    )
}

/// Test-only helpers shared across this crate's test modules.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use core::time::Duration;

    /// Build a headless physics app driven by a manual fixed timestep at
    /// `PHYSICS_HZ` — the pattern Avian's own integrator tests use, so one
    /// `app.update()` advances exactly one physics tick.
    pub fn headless_app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::transform::TransformPlugin));
        add_physics(&mut app, false);
        app.insert_resource(Time::<Fixed>::from_hz(PHYSICS_HZ));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_secs_f64(1.0 / PHYSICS_HZ),
        ));
        // Avian registers some resources in plugin `finish()`, which `run()`
        // would call but a manual `update()` loop does not.
        app.finish();
        app.cleanup();
        app
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tests_support::headless_app;

    #[test]
    fn cube_drops_and_settles_on_floor() {
        let mut app = headless_app();
        // Floor: full height 1, top face at y = 0.
        app.world_mut().spawn(static_cuboid(Vec3::new(0.0, -0.5, 0.0), Vec3::new(64.0, 1.0, 64.0)));
        // Unit cube dropped from 10 m up.
        let cube = app.world_mut().spawn(cube_body(Vec3::new(0.0, 10.0, 0.0), 1.0)).id();

        // ~4 s of simulation: plenty to fall and come to rest.
        app.update(); // prepare + first tick
        for _ in 0..(PHYSICS_HZ as usize * 4) {
            app.update();
        }

        let pos = app.world().entity(cube).get::<Position>().expect("prepared Position").0;
        let vel = app.world().entity(cube).get::<LinearVelocity>().expect("LinearVelocity").0;

        // Rests with its centre a half-extent above the floor top (y = 0.5),
        // and has stopped moving.
        assert!(
            (pos.y - 0.5).abs() < 0.15,
            "cube should settle near y=0.5, got y={}",
            pos.y
        );
        assert!(vel.length() < 0.5, "cube should be at rest, got vel={:?}", vel);
    }
}
