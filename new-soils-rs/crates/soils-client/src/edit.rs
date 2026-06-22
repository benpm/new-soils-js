//! Raycast block editing: left click breaks, right click places. Mirrors the
//! JS `player.update` raycast + `Voxels.edit` flow (optimistic local apply plus
//! an `Edit` sent to the server).

use bevy::prelude::*;
use bevy::render::storage::ShaderStorageBuffer;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ClientMsg};

use crate::chunk::{Blocks, ChunkMap, VoxelChunk};
use crate::gpu_mesh::{self, GpuChunk};
use crate::net::NetClient;
use crate::player::Player;

const REACH: i32 = 8;
/// Block placed on right-click (resolved by name at runtime).
const PLACE_BLOCK: &str = "Stone";

pub fn edit_blocks(
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Query<&CursorOptions, With<PrimaryWindow>>,
    net: Res<NetClient>,
    registry: Res<Blocks>,
    map: Res<ChunkMap>,
    mut chunks: Query<&mut VoxelChunk>,
    mut gpu_chunks: Query<&mut GpuChunk>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    camera: Query<&Transform, With<Player>>,
) {
    // Ignore clicks while the cursor isn't grabbed (UI/escape state).
    if let Ok(cursor) = cursor.single() {
        if cursor.grab_mode == CursorGrabMode::None {
            return;
        }
    }
    let break_block = buttons.just_pressed(MouseButton::Left);
    let place_block = buttons.just_pressed(MouseButton::Right);
    if !break_block && !place_block {
        return;
    }
    let Ok(transform) = camera.single() else { return };

    let origin = transform.translation;
    let dir = (transform.rotation * Vec3::NEG_Z).normalize();

    let Some(hit) = raycast_voxel(&map, &chunks, origin, dir) else { return };

    let (target, value) = if break_block {
        (hit.voxel, 0u8)
    } else {
        let id = registry.0.id_of(PLACE_BLOCK).unwrap_or(1);
        (hit.prev, id)
    };

    apply_edit(&map, &mut chunks, &mut gpu_chunks, &mut buffers, target, value);
    net.send(ClientMsg::Edit { pos: [target.x, target.y, target.z], value });
}

struct RayHit {
    /// The solid voxel that was hit.
    voxel: IVec3,
    /// The empty voxel just before it (where a new block is placed).
    prev: IVec3,
}

/// Amanatides–Woo voxel traversal from `origin` along `dir`.
fn raycast_voxel(
    map: &ChunkMap,
    chunks: &Query<&mut VoxelChunk>,
    origin: Vec3,
    dir: Vec3,
) -> Option<RayHit> {
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
        if read_voxel(map, chunks, voxel) != 0 {
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

/// Read a voxel through the (mutable) chunk query without mutating.
fn read_voxel(map: &ChunkMap, chunks: &Query<&mut VoxelChunk>, v: IVec3) -> u8 {
    let cpos = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
    let Some(&e) = map.map.get(&cpos) else { return 0 };
    let Ok(chunk) = chunks.get(e) else { return 0 };
    chunk.volume.get(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP)
}

/// Apply an edit to a local chunk: update the CPU voxels, re-upload the GPU
/// voxel buffer, and mark the chunk dirty so the compute mesher regenerates it.
pub fn apply_edit(
    map: &ChunkMap,
    chunks: &mut Query<&mut VoxelChunk>,
    gpu_chunks: &mut Query<&mut GpuChunk>,
    buffers: &mut Assets<ShaderStorageBuffer>,
    v: IVec3,
    value: u8,
) {
    let cpos = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
    let Some(&e) = map.map.get(&cpos) else { return };
    let Ok(mut chunk) = chunks.get_mut(e) else { return };
    chunk.volume.set(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP, value);
    let vol = chunk.volume.clone();
    if let Ok(mut gc) = gpu_chunks.get_mut(e) {
        gpu_mesh::refresh_gpu_chunk(buffers, &mut gc, &vol);
    }
}
