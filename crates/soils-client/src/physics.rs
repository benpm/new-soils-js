//! Client-side local physics for replicated rigid-body props (Stage 4).
//!
//! With `SOILS_PHYSICS` set, the client runs its own Avian world for the
//! physics entities in interest plus the terrain around the player, predicting
//! their motion locally and rebasing to the server's authoritative snapshots
//! when they diverge — the prop analogue of [`player::reconcile_self`]. Props
//! are not locally controlled, so there is no input to replay: a correction is
//! a rebase-and-continue, not a full input-replay rollback. The mesh renders
//! from the predicted Avian transform, so props move at the local physics rate
//! instead of the 2-tick-delayed interpolation used for other entities.
//!
//! When disabled, none of this runs and physics props fall back to the shared
//! [`actor`](crate::actor) interpolation path.

use avian3d::prelude::*;
use bevy::prelude::*;
use std::collections::{HashMap, HashSet};

use soils_protocol::CHUNK_BIT;
use soils_sim::KIND_PHYSICS_CUBE;

use crate::actor::{ActorAssets, LocalPlayer};
use crate::chunk::{ChunkMap, VoxelChunk};
use crate::player::Player;
use crate::server_msg::{EntitiesUpdated, EntityDespawned, EntitySpawned};

/// Whether the client runs a local physics world (`SOILS_PHYSICS`). Inserted
/// unconditionally so other systems (e.g. actor spawning) can branch on it.
#[derive(Resource, Clone, Copy)]
pub struct ClientPhysics {
    pub enabled: bool,
}

impl ClientPhysics {
    fn from_env() -> Self {
        Self { enabled: std::env::var("SOILS_PHYSICS").is_ok_and(|v| v != "0") }
    }
}

/// NetId → local predicted Avian body entity (rendered).
#[derive(Resource, Default)]
struct PhysicsActors {
    map: HashMap<u32, Entity>,
}

/// Chunk position → local static terrain collider entity.
#[derive(Resource, Default)]
struct ClientTerrain {
    colliders: HashMap<IVec3, Entity>,
}

/// The local kinematic collider mirroring our own player, so predicted props
/// collide with us the way the server's authoritative sim does.
#[derive(Resource, Default)]
struct ClientPlayerProxy(Option<Entity>);

/// Chebyshev chunk radius of terrain kept collidable around the player.
const TERRAIN_RADIUS: i32 = 2;
/// Divergence (world units) before a predicted body is rebased to the server.
const REBASE_EPSILON: f32 = 0.1;

/// Register the client physics world and systems. Always inserts
/// [`ClientPhysics`]; the rest is added only when enabled.
pub fn register(app: &mut App) {
    let cfg = ClientPhysics::from_env();
    app.insert_resource(cfg);
    if !cfg.enabled {
        return;
    }
    // Interpolation on: props ease between the fixed physics ticks so they
    // render smoothly above the tick rate and corrections don't visibly pop.
    soils_physics::add_physics(app, true);
    app.init_resource::<PhysicsActors>();
    app.init_resource::<ClientTerrain>();
    app.init_resource::<ClientPlayerProxy>();
    app.add_systems(
        Update,
        (
            spawn_physics_bodies.after(crate::server_msg::apply_entity_spawns),
            correct_physics_bodies
                .after(spawn_physics_bodies)
                .after(crate::server_msg::apply_entity_updates),
            despawn_physics_bodies.after(crate::server_msg::apply_entity_despawns),
            maintain_client_terrain,
        ),
    );
    // Drive our player proxy from the predicted sim just before the physics
    // step (Avian runs in FixedPostUpdate).
    app.add_systems(bevy::app::FixedUpdate, sync_client_player_proxy);
}

/// Spawn (once) and drive the local kinematic player collider from our own
/// predicted `soils-sim` state, mirroring the server's `sync_player_proxies`.
fn sync_client_player_proxy(
    player: Query<&Player>,
    mut proxy: ResMut<ClientPlayerProxy>,
    mut commands: Commands,
    mut bodies: Query<(&mut Position, &mut LinearVelocity)>,
) {
    let Ok(p) = player.single() else { return };
    match proxy.0 {
        Some(entity) => {
            if let Ok((mut pos, mut vel)) = bodies.get_mut(entity) {
                pos.0 = soils_physics::player_center(p.sim.pos);
                vel.0 = p.sim.vel;
            }
        }
        None => {
            proxy.0 = Some(commands.spawn(soils_physics::player_proxy(p.sim.pos)).id());
        }
    }
}

fn chunk_of(p: Vec3) -> IVec3 {
    IVec3::new(
        (p.x.floor() as i32) >> CHUNK_BIT,
        (p.y.floor() as i32) >> CHUNK_BIT,
        (p.z.floor() as i32) >> CHUNK_BIT,
    )
}

/// Spawn a local Avian body for each replicated physics prop entering interest.
fn spawn_physics_bodies(
    mut reader: MessageReader<EntitySpawned>,
    mut commands: Commands,
    mut actors: ResMut<PhysicsActors>,
    assets: Res<ActorAssets>,
    local: Res<LocalPlayer>,
) {
    for msg in reader.read() {
        if msg.kind != KIND_PHYSICS_CUBE || msg.id == local.self_entity {
            continue;
        }
        if actors.map.contains_key(&msg.id) {
            continue;
        }
        let Some(kind) = assets.kinds.get(msg.kind as usize) else { continue };
        let pos = Vec3::from_array(msg.pos);
        let entity = commands
            .spawn((
                RigidBody::Dynamic,
                Collider::cuboid(1.0, 1.0, 1.0),
                Transform::from_translation(pos),
                Mesh3d(kind.mesh.clone()),
                MeshMaterial3d(kind.material.clone()),
            ))
            .id();
        actors.map.insert(msg.id, entity);
    }
}

/// Reconcile a predicted body against the server's authoritative state.
/// Velocities (linear + angular) are corrected every snapshot — setting a
/// derivative is visually smooth and keeps the predicted trajectory/spin
/// tracking — while position and orientation are only hard-snapped when the
/// local prediction has drifted past [`REBASE_EPSILON`], so small mismatches
/// don't pop.
fn correct_physics_bodies(
    mut reader: MessageReader<EntitiesUpdated>,
    actors: Res<PhysicsActors>,
    mut bodies: Query<(
        &mut Position,
        &mut Rotation,
        &mut LinearVelocity,
        &mut AngularVelocity,
    )>,
) {
    for msg in reader.read() {
        for state in &msg.states {
            let Some(&entity) = actors.map.get(&state.id) else { continue };
            let Ok((mut pos, mut rot, mut vel, mut angvel)) = bodies.get_mut(entity) else {
                continue;
            };
            vel.0 = Vec3::from_array(state.velocity);
            angvel.0 = Vec3::from_array(state.angvel);
            let server_pos = Vec3::from_array(state.pos);
            if (pos.0 - server_pos).length() > REBASE_EPSILON {
                pos.0 = server_pos;
                rot.0 = Quat::from_array(state.rot);
            }
        }
    }
}

/// Drop a body's local proxy when it leaves interest / despawns.
fn despawn_physics_bodies(
    mut reader: MessageReader<EntityDespawned>,
    mut commands: Commands,
    mut actors: ResMut<PhysicsActors>,
) {
    for msg in reader.read() {
        if let Some(entity) = actors.map.remove(&msg.0) {
            commands.entity(entity).despawn();
        }
    }
}

/// Keep static `Collider::voxels` terrain resident within [`TERRAIN_RADIUS`]
/// chunks of the player, rebuilding an edited chunk (its `VoxelChunk` changed)
/// and dropping colliders for chunks that fall out of range or unload.
fn maintain_client_terrain(
    player: Query<&Transform, With<Player>>,
    map: Res<ChunkMap>,
    chunks: Query<&VoxelChunk>,
    edited: Query<&VoxelChunk, Changed<VoxelChunk>>,
    mut terrain: ResMut<ClientTerrain>,
    mut commands: Commands,
) {
    let Ok(ptf) = player.single() else { return };
    let pc = chunk_of(ptf.translation);

    // Edited chunks: drop the stale collider so it rebuilds below.
    for vc in &edited {
        if let Some(entity) = terrain.colliders.remove(&vc.pos) {
            commands.entity(entity).despawn();
        }
    }

    // Chunks that should be collidable this frame.
    let r = TERRAIN_RADIUS;
    let mut needed: HashSet<IVec3> = HashSet::new();
    for dx in -r..=r {
        for dy in -r..=r {
            for dz in -r..=r {
                needed.insert(pc + IVec3::new(dx, dy, dz));
            }
        }
    }

    // Despawn colliders that are no longer needed (out of range / unloaded).
    terrain.colliders.retain(|cpos, entity| {
        if needed.contains(cpos) && map.map.contains_key(cpos) {
            true
        } else {
            commands.entity(*entity).despawn();
            false
        }
    });

    // Build colliders for needed, resident chunks that lack one.
    for cpos in &needed {
        if terrain.colliders.contains_key(cpos) {
            continue;
        }
        let Some(&chunk_entity) = map.map.get(cpos) else { continue };
        let Ok(vc) = chunks.get(chunk_entity) else { continue };
        if let Some(bundle) = soils_physics::chunk_collider_bundle(*cpos, &vc.volume) {
            let entity = commands.spawn(bundle).id();
            terrain.colliders.insert(*cpos, entity);
        }
    }
}
