//! In-memory world state plus region-file persistence: terrain generation, a
//! chunk cache, and load/save through `region`.

use std::collections::HashMap;
use std::path::PathBuf;

use glam::IVec3;
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ChunkVolume};
use soils_worldgen::{BlockRegistry, TerrainGen, WorldType, default_registry};

use crate::region;

pub struct World {
    pub registry: BlockRegistry,
    terrain: TerrainGen,
    chunks: HashMap<IVec3, ChunkVolume>,
    regions_dir: PathBuf,
    /// Spawn point in voxel space (matches the JS default world spawn).
    pub spawn: [f32; 3],
    pub seed: i64,
    pub daytime: f32,
}

impl World {
    /// Create (or open) a named world. Each world persists to its own region
    /// directory and generates from its own `seed`, so different names yield
    /// different terrain.
    pub fn new(name: &str, seed: u32) -> Self {
        let regions_dir = PathBuf::from(format!("data/worlds/{name}/regions"));
        Self {
            registry: default_registry(),
            terrain: TerrainGen::new(seed, WorldType::Normal),
            chunks: HashMap::new(),
            regions_dir,
            // Surface near here sits around y=256; spawn a little above it so
            // the player starts in the open air rather than buried in rock.
            spawn: [282.0, 285.0, 268.0],
            seed: seed as i64,
            daytime: 0.0,
        }
    }

    /// Get a chunk, loading it from disk or generating (and persisting) it on
    /// first access.
    pub fn get_or_generate(&mut self, pos: IVec3) -> &ChunkVolume {
        if !self.chunks.contains_key(&pos) {
            let volume = match region::load(&self.regions_dir, pos) {
                Ok(Some(volume)) => volume,
                Ok(None) => {
                    let volume = self.terrain.generate(pos, &self.registry);
                    if let Err(e) = region::save(&self.regions_dir, pos, &volume) {
                        eprintln!("failed to save generated chunk {pos:?}: {e}");
                    }
                    volume
                }
                Err(e) => {
                    eprintln!("failed to load chunk {pos:?}, regenerating: {e}");
                    self.terrain.generate(pos, &self.registry)
                }
            };
            self.chunks.insert(pos, volume);
        }
        &self.chunks[&pos]
    }

    /// Apply a voxel edit at an absolute voxel position and persist the chunk.
    /// Returns false if the containing chunk has not been loaded yet.
    pub fn edit(&mut self, x: i32, y: i32, z: i32, value: u8) -> bool {
        let cpos = IVec3::new(x >> CHUNK_BIT, y >> CHUNK_BIT, z >> CHUNK_BIT);
        let Some(chunk) = self.chunks.get_mut(&cpos) else { return false };
        chunk.set(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP, value);
        if let Err(e) = region::save(&self.regions_dir, cpos, chunk) {
            eprintln!("failed to persist edit in chunk {cpos:?}: {e}");
        }
        true
    }
}
