//! Other-player (actor) rendering. The server simulates everyone from their
//! inputs and broadcasts positions; we spawn a body per remote actor and
//! smoothly interpolate it toward its latest target.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;

/// Our own network id (set from the server's `Init`); used to skip rendering
/// ourselves as an actor.
#[derive(Resource, Default)]
pub struct LocalPlayer {
    pub id: u16,
}

/// Maps actor ids to their spawned entity.
#[derive(Resource, Default)]
pub struct ActorMap {
    pub map: HashMap<u16, Entity>,
}

/// Shared mesh/material for actor bodies.
#[derive(Resource)]
pub struct ActorAssets {
    pub mesh: Handle<Mesh>,
    pub material: Handle<StandardMaterial>,
}

/// A remote player body. `target` is the latest networked eye position.
#[derive(Component)]
pub struct Actor {
    pub target: Vec3,
}

/// Vertical offset from the eye position to the body center.
const BODY_DROP: f32 = 0.9;

/// Build the shared actor body mesh + material.
pub fn setup_actor_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mesh = meshes.add(Cuboid::new(0.6, 1.8, 0.6));
    let material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.9, 0.45, 0.2),
        perceptual_roughness: 0.8,
        ..default()
    });
    commands.insert_resource(ActorAssets { mesh, material });
}

/// Smoothly move actor bodies toward their networked target each frame.
pub fn interpolate_actors(time: Res<Time>, mut query: Query<(&mut Transform, &Actor)>) {
    let t = (time.delta_secs() * 12.0).min(1.0);
    for (mut transform, actor) in &mut query {
        let body = actor.target - Vec3::Y * BODY_DROP;
        transform.translation = transform.translation.lerp(body, t);
    }
}
