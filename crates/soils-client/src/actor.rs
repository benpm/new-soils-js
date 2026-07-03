//! Other-player (actor) rendering. We report our position to the server a few
//! times a second; the server broadcasts everyone's positions back, and we spawn
//! a body per remote actor and smoothly interpolate it toward its latest target.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use soils_protocol::ClientMsg;

use crate::net::NetClient;
use crate::player::Player;

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
/// Network send rate for our own position.
const MOVE_INTERVAL: f32 = 0.05;

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

/// Report our position/velocity to the server at a fixed rate.
pub fn send_move(
    time: Res<Time>,
    mut acc: Local<f32>,
    net: Res<NetClient>,
    query: Query<(&Transform, &Player)>,
) {
    *acc += time.delta_secs();
    if *acc < MOVE_INTERVAL {
        return;
    }
    *acc = 0.0;
    if let Ok((transform, player)) = query.single() {
        net.send(ClientMsg::Move {
            pos: transform.translation.to_array(),
            velocity: player.sim.vel.to_array(),
        });
    }
}

/// Smoothly move actor bodies toward their networked target each frame.
pub fn interpolate_actors(time: Res<Time>, mut query: Query<(&mut Transform, &Actor)>) {
    let t = (time.delta_secs() * 12.0).min(1.0);
    for (mut transform, actor) in &mut query {
        let body = actor.target - Vec3::Y * BODY_DROP;
        transform.translation = transform.translation.lerp(body, t);
    }
}
