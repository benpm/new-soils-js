//! In-memory world state for the slice: terrain generation + a chunk cache.
//!
//! On-disk region persistence (the `Region` class in `server.js`) is stubbed
//! out here; chunks live only in memory and are regenerated on restart.

use std::collections::HashMap;

use glam::IVec3;
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ChunkVolume};
use soils_worldgen::{BlockRegistry, TerrainGen, WorldType, default_registry};

pub struct World {
    pub registry: BlockRegistry,
    terrain: TerrainGen,
    chunks: HashMap<IVec3, ChunkVolume>,
    /// Spawn point in voxel space (matches the JS default world spawn).
    pub spawn: [f32; 3],
    pub seed: i64,
    pub daytime: f32,
}

impl World {
    pub fn new(seed: u32) -> Self {
        Self {
            registry: default_registry(),
            terrain: TerrainGen::new(seed, WorldType::Normal),
            chunks: HashMap::new(),
            // Surface near here sits around y=256; spawn a little above it so
            // the player starts in the open air rather than buried in rock.
            spawn: [282.0, 285.0, 268.0],
            seed: seed as i64,
            daytime: 0.0,
        }
    }

    /// Get a chunk, generating and caching it on first access.
    pub fn get_or_generate(&mut self, pos: IVec3) -> &ChunkVolume {
        self.chunks
            .entry(pos)
            .or_insert_with(|| self.terrain.generate(pos, &self.registry))
    }

    /// Apply a voxel edit at an absolute voxel position. Returns false if the
    /// containing chunk has not been generated yet (the edit is ignored).
    pub fn edit(&mut self, x: i32, y: i32, z: i32, value: u8) -> bool {
        let cpos = IVec3::new(x >> CHUNK_BIT, y >> CHUNK_BIT, z >> CHUNK_BIT);
        if let Some(chunk) = self.chunks.get_mut(&cpos) {
            chunk.set(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP, value);
            true
        } else {
            false
        }
    }
}
