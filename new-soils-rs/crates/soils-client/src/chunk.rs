//! Chunk ECS types and the world voxel lookup used by physics and editing.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ChunkVolume};
use soils_worldgen::BlockRegistry;

/// One streamed chunk: its chunk coordinate and voxel data.
#[derive(Component)]
pub struct VoxelChunk {
    pub pos: IVec3,
    pub volume: ChunkVolume,
}

/// Maps chunk coordinates to their spawned entity.
#[derive(Resource, Default)]
pub struct ChunkMap {
    pub map: HashMap<IVec3, Entity>,
}

/// The parsed block table.
#[derive(Resource)]
pub struct Blocks(pub BlockRegistry);

/// Current time of day (0.0..1.0), driven by the server.
#[derive(Resource, Default)]
pub struct WorldTime {
    pub daytime: f32,
}

/// Read a voxel at an absolute voxel position, or 0 (Air) if its chunk is not
/// loaded. Shared by physics (solidity) and editing.
pub fn voxel_at(map: &ChunkMap, chunks: &Query<&VoxelChunk>, v: IVec3) -> u8 {
    let cpos = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
    let Some(&e) = map.map.get(&cpos) else { return 0 };
    let Ok(chunk) = chunks.get(e) else { return 0 };
    chunk.volume.get(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP)
}

#[inline]
pub fn is_solid(map: &ChunkMap, chunks: &Query<&VoxelChunk>, v: IVec3) -> bool {
    voxel_at(map, chunks, v) != 0
}
