//! Off-thread greedy meshing, mirroring the JS web-worker mesher. Chunks flagged
//! `NeedsRemesh` are meshed on the `AsyncComputeTaskPool`; finished geometry is
//! turned into a renderable entity.

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, futures_lite::future};
use soils_protocol::CHUNK_SIZE;
use soils_worldgen::{MeshData, greedy_mesh};

use crate::chunk::{Blocks, NeedsRemesh, VoxelChunk};
use crate::material::{AtlasMaterial, build_mesh};

/// An in-flight meshing job for a chunk.
#[derive(Component)]
pub struct MeshingTask(Task<MeshData>);

/// Kick off meshing jobs for chunks that need it.
pub fn dispatch_meshing(
    mut commands: Commands,
    query: Query<(Entity, &VoxelChunk), (With<NeedsRemesh>, Without<MeshingTask>)>,
) {
    let pool = AsyncComputeTaskPool::get();
    for (entity, chunk) in &query {
        let volume = chunk.volume.clone();
        // `merge = false`: per-face quads so atlas tiles aren't stretched.
        let task = pool.spawn(async move { greedy_mesh(&volume, false) });
        commands.entity(entity).insert(MeshingTask(task)).remove::<NeedsRemesh>();
    }
}

/// Poll meshing jobs and attach finished meshes to their chunk entities.
pub fn apply_meshing(
    mut commands: Commands,
    mut tasks: Query<(Entity, &VoxelChunk, &mut MeshingTask)>,
    mut meshes: ResMut<Assets<Mesh>>,
    registry: Res<Blocks>,
    material: Res<AtlasMaterial>,
) {
    for (entity, chunk, mut task) in &mut tasks {
        let Some(data) = block_on(future::poll_once(&mut task.0)) else { continue };
        commands.entity(entity).remove::<MeshingTask>();

        if data.is_empty() {
            commands.entity(entity).remove::<Mesh3d>();
            continue;
        }

        let mesh = build_mesh(&data, &registry.0);
        let handle = meshes.add(mesh);
        let origin = (chunk.pos * CHUNK_SIZE).as_vec3();
        commands.entity(entity).insert((
            Mesh3d(handle),
            MeshMaterial3d(material.0.clone()),
            Transform::from_translation(origin),
            Visibility::default(),
        ));
    }
}
