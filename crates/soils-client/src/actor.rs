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

/// A remote entity body. `target` is the latest networked eye position.
#[derive(Component)]
pub struct Actor {
    pub target: Vec3,
    pub kind: u16,
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

/// Smoothly move entity bodies toward their networked target each frame.
pub fn interpolate_actors(
    time: Res<Time>,
    assets: Res<ActorAssets>,
    mut query: Query<(&mut Transform, &Actor)>,
) {
    let t = (time.delta_secs() * 12.0).min(1.0);
    for (mut transform, actor) in &mut query {
        let drop = assets.kinds.get(actor.kind as usize).map_or(0.9, |k| k.body_drop);
        let body = actor.target - Vec3::Y * drop;
        transform.translation = transform.translation.lerp(body, t);
    }
}
