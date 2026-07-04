//! In-memory world state plus region-file persistence: terrain generation, a
//! chunk cache, and load/save through `region`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use glam::IVec3;
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ChunkVolume};
use soils_worldgen::{BlockRegistry, TerrainGen, WorldType, default_registry};

use crate::persist::PersistHandle;
use crate::region;

pub struct World {
    pub registry: Arc<BlockRegistry>,
    terrain: Arc<TerrainGen>,
    chunks: HashMap<IVec3, ChunkVolume>,
    regions_dir: PathBuf,
    /// Handle to the background writer: generated + edited chunks are enqueued
    /// here instead of being written on the request/connection path.
    persist: PersistHandle,
    /// Memoised region-file headers for the read path. `None` = the region file
    /// doesn't exist (nothing there was ever persisted). Turns a per-chunk file
    /// open into one header read per region.
    ///
    /// Coherent with the background writer without locking because it is only
    /// consulted for chunks NOT resident in `chunks`, and the writer only ever
    /// (re)writes header entries for resident chunks — disjoint sets. NOTE: if
    /// `chunks` ever gains eviction, evicting a chunk must also drop its
    /// region's entry here.
    header_cache: HashMap<PathBuf, Option<Box<[u32; 4096]>>>,
    /// Spawn point in voxel space (matches the JS default world spawn).
    pub spawn: [f32; 3],
    pub seed: i64,
    pub daytime: f32,
}

impl World {
    /// Create (or open) a named world under `data_dir`. Each world persists to
    /// its own region directory and generates from its own `seed`, so different
    /// names yield different terrain.
    pub fn new(data_dir: &Path, name: &str, seed: u32, persist: PersistHandle) -> Self {
        let regions_dir = data_dir.join("worlds").join(name).join("regions");
        Self {
            registry: Arc::new(default_registry()),
            terrain: Arc::new(TerrainGen::new(seed, WorldType::Normal)),
            chunks: HashMap::new(),
            regions_dir,
            persist,
            header_cache: HashMap::new(),
            // Surface near here sits around y=256; spawn a little above it so
            // the player starts in the open air rather than buried in rock.
            spawn: [282.0, 285.0, 268.0],
            seed: seed as i64,
            daytime: 0.0,
        }
    }

    /// Read a persisted chunk via the memoised region-header cache, opening the
    /// region file at most once per region instead of once per chunk. Returns
    /// `None` for a chunk that has never been persisted (caller generates it).
    fn probe(&mut self, pos: IVec3) -> Option<ChunkVolume> {
        let path = region::region_path(&self.regions_dir, pos);
        let header = self
            .header_cache
            .entry(path)
            .or_insert_with(|| region::read_header(&self.regions_dir, pos).unwrap_or(None));
        let hdr = header.as_ref()?;
        region::read_chunk(&self.regions_dir, pos, hdr[region::header_index(pos)]).unwrap_or(None)
    }

    /// Resolve many chunk positions at once: each is served from the in-memory
    /// cache, loaded from disk, or generated **in parallel** across cores.
    /// Returns `(pos, is_empty, bytes)` for each requested position, in input
    /// order (`bytes` empty when the chunk is empty).
    ///
    /// This is the hot path for a fresh world's first chunk burst. Generation
    /// runs with the world lock **released** so other connections can edit and
    /// load chunks while a wave generates; only the cache probe and the adopt/
    /// serialize phases hold the lock. Adoption never overwrites a chunk that
    /// appeared meanwhile (another connection's generate, or a generate-then-
    /// edit), so concurrent requests stay consistent.
    pub fn get_or_generate_batch(
        world: &Mutex<World>,
        positions: &[IVec3],
    ) -> Vec<(IVec3, bool, Vec<u8>)> {
        // Phase A: under the lock, pull in anything cached or on disk (edited
        // chunks) and collect the positions that still need generating.
        let (missing, terrain, registry) = {
            let mut w = world.lock().unwrap();
            let mut missing: Vec<IVec3> = Vec::new();
            for &pos in positions {
                if w.chunks.contains_key(&pos) {
                    continue;
                }
                match w.probe(pos) {
                    Some(volume) => {
                        w.chunks.insert(pos, volume);
                    }
                    None => missing.push(pos),
                }
            }
            (missing, w.terrain.clone(), w.registry.clone())
        };

        // Phase B: generate lock-free.
        let volumes = if missing.is_empty() {
            Vec::new()
        } else {
            let t0 = Instant::now();
            let volumes = terrain.generate_batch(&missing, &registry);
            println!("worldgen: {} chunks in {} ms", missing.len(), t0.elapsed().as_millis());
            volumes
        };

        // Phase C: under the lock again, adopt what we generated (unless a
        // concurrent request beat us to it) and serialize in request order.
        // Generated chunks are enqueued for background persistence so a fresh
        // world is saved without blocking this request.
        let mut w = world.lock().unwrap();
        for (pos, volume) in missing.iter().zip(volumes) {
            if !w.chunks.contains_key(pos) {
                w.persist.enqueue(w.regions_dir.clone(), *pos, volume.clone());
                w.chunks.insert(*pos, volume);
            }
        }
        positions
            .iter()
            .map(|&pos| {
                let vol = &w.chunks[&pos];
                if vol.is_empty() {
                    (pos, true, Vec::new())
                } else {
                    (pos, false, vol.as_bytes().to_vec())
                }
            })
            .collect()
    }

    /// Apply a voxel edit at an absolute voxel position and persist the chunk.
    /// Returns false if the containing chunk has not been loaded yet.
    pub fn edit(&mut self, x: i32, y: i32, z: i32, value: u8) -> bool {
        let cpos = IVec3::new(x >> CHUNK_BIT, y >> CHUNK_BIT, z >> CHUNK_BIT);
        let Some(chunk) = self.chunks.get_mut(&cpos) else { return false };
        chunk.set(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP, value);
        // The in-memory chunk is authoritative; persist in the background so the
        // edit doesn't block the connection task on zlib + disk.
        self.persist.enqueue(self.regions_dir.clone(), cpos, chunk.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::Persister;

    #[test]
    fn generated_chunks_persist_and_reload_from_disk() {
        let dir = std::env::temp_dir().join(format!("soils-world-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Generate a below-surface (non-empty) chunk; it is cached and enqueued
        // for background persistence.
        let pos = IVec3::new(8, 7, 8);
        let generated = {
            let persister = Persister::new();
            let world = Mutex::new(World::new(&dir, "default", 0, persister.handle()));
            let out = World::get_or_generate_batch(&world, &[pos]);
            assert_eq!(out.len(), 1);
            assert!(!out[0].1, "below-surface chunk should be non-empty");
            let bytes = out[0].2.clone();
            persister.shutdown(); // flush the writer
            bytes
        };

        // The region file now exists and holds exactly the generated voxels.
        let regions = dir.join("worlds").join("default").join("regions");
        assert!(regions.is_dir(), "region dir should exist after flush");
        let loaded = region::load(&regions, pos).unwrap().expect("chunk persisted");
        assert_eq!(loaded.as_bytes(), generated.as_slice());

        // A fresh world loads the chunk from disk (identical bytes) rather than
        // regenerating it.
        let persister2 = Persister::new();
        let world2 = Mutex::new(World::new(&dir, "default", 0, persister2.handle()));
        let out2 = World::get_or_generate_batch(&world2, &[pos]);
        assert_eq!(out2[0].2, generated);
        persister2.shutdown();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
