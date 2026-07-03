//! In-memory world state plus region-file persistence: terrain generation, a
//! chunk cache, and load/save through `region`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use glam::IVec3;
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ChunkVolume};
use soils_worldgen::{BlockRegistry, TerrainGen, WorldType, default_registry};

use crate::persist::PersistHandle;
use crate::region;

pub struct World {
    pub registry: BlockRegistry,
    terrain: TerrainGen,
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
            registry: default_registry(),
            terrain: TerrainGen::new(seed, WorldType::Normal),
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
    /// cache, loaded from disk (edited chunks), or generated **in parallel**
    /// across cores. Freshly generated chunks are deterministic from the seed,
    /// so they are *not* persisted — only edits are saved (see [`edit`](Self::edit)).
    /// Returns `(pos, is_empty, bytes)` for each requested position, in input
    /// order (`bytes` empty when the chunk is empty).
    ///
    /// This is the hot path for a fresh world's first chunk burst: the parallel
    /// generate replaces the old one-at-a-time loop that left the screen blank.
    pub fn get_or_generate_batch(&mut self, positions: &[IVec3]) -> Vec<(IVec3, bool, Vec<u8>)> {
        // Phase A: pull in anything already cached or on disk (edited chunks),
        // and collect the positions that still need generating.
        let mut missing: Vec<IVec3> = Vec::new();
        for &pos in positions {
            if self.chunks.contains_key(&pos) {
                continue;
            }
            match self.probe(pos) {
                Some(volume) => {
                    self.chunks.insert(pos, volume);
                }
                None => missing.push(pos),
            }
        }

        // Phase B: generate all missing chunks in parallel, then enqueue each for
        // background persistence (so a fresh world is saved without blocking this
        // request). They're also kept in `chunks`, so re-requests are free.
        if !missing.is_empty() {
            let volumes = self.terrain.generate_batch(&missing, &self.registry);
            for (pos, volume) in missing.iter().zip(volumes) {
                self.persist.enqueue(self.regions_dir.clone(), *pos, volume.clone());
                self.chunks.insert(*pos, volume);
            }
        }

        // Phase C: serialize results in the requested order.
        positions
            .iter()
            .map(|&pos| {
                let vol = &self.chunks[&pos];
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
            let mut world = World::new(&dir, "default", 0, persister.handle());
            let out = world.get_or_generate_batch(&[pos]);
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
        let mut world2 = World::new(&dir, "default", 0, persister2.handle());
        let out2 = world2.get_or_generate_batch(&[pos]);
        assert_eq!(out2[0].2, generated);
        persister2.shutdown();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
