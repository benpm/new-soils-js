//! World/chunk coordinate constants and conversions.
//!
//! Mirrors the constants from the JS original (`server.js`): a chunk is a
//! 32×32×32 cube of voxels, voxels indexed `(y + z*32)*32 + x`.

use glam::IVec3;

/// Edge length of a chunk in voxels.
pub const CHUNK_SIZE: i32 = 32;
/// `CHUNK_SIZE - 1`, used as an `&` mask to take the in-chunk component.
pub const CHUNK_CLIP: i32 = CHUNK_SIZE - 1;
/// `log2(CHUNK_SIZE)`, used as a `>>` shift to take the chunk component.
pub const CHUNK_BIT: i32 = 5;
/// Number of voxels in a chunk (`32^3`).
pub const CHUNK_CUBED: usize = (CHUNK_SIZE * CHUNK_SIZE * CHUNK_SIZE) as usize;

/// Edge length of a region in chunks (used by on-disk persistence).
pub const REGION_SIZE: i32 = 16;

/// Flattened voxel index within a chunk, matching the JS layout.
#[inline]
pub fn voxel_index(x: i32, y: i32, z: i32) -> usize {
    ((y + z * CHUNK_SIZE) * CHUNK_SIZE + x) as usize
}

/// Convert an absolute voxel position to the chunk that contains it.
#[inline]
pub fn chunk_of(voxel: IVec3) -> IVec3 {
    IVec3::new(voxel.x >> CHUNK_BIT, voxel.y >> CHUNK_BIT, voxel.z >> CHUNK_BIT)
}

/// In-chunk component of an absolute voxel position (0..CHUNK_SIZE).
#[inline]
pub fn local_of(voxel: IVec3) -> IVec3 {
    IVec3::new(voxel.x & CHUNK_CLIP, voxel.y & CHUNK_CLIP, voxel.z & CHUNK_CLIP)
}

/// World-space origin (in voxels) of a chunk.
#[inline]
pub fn chunk_origin(chunk: IVec3) -> IVec3 {
    chunk * CHUNK_SIZE
}
