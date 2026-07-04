//! In-memory world state plus region-file persistence: terrain generation, a
//! chunk cache, and load/save through `region`.
//!
//! Since the ECS rework (TODO phase 5) a `World` is owned single-threaded by
//! the sim app — no mutex. The chunk pipeline splits into three calls so
//! generation can run off-thread while the tick stays free:
//! [`ensure_resident`](World::ensure_resident) (cache/disk probe),
//! [`gen_ctx`](World::gen_ctx) + `TerrainGen::generate_batch` (pure, off the
//! tick thread), and [`adopt`](World::adopt) (guarded insert + background
//! persistence).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// here instead of being written on the tick path.
    persist: PersistHandle,
    /// Memoised region-file headers for the read path. `None` = the region file
    /// doesn't exist (nothing there was ever persisted). Turns a per-chunk file
    /// open into one header read per region.
    ///
    /// Coherent with the background writer because it is only consulted for
    /// chunks NOT resident in `chunks`, and the writer only ever (re)writes
    /// header entries for resident chunks — disjoint sets. NOTE: if `chunks`
    /// ever gains eviction, evicting a chunk must also drop its region's entry
    /// here.
    header_cache: HashMap<PathBuf, Option<Box<[u32; 4096]>>>,
    /// Spawn point in voxel space (matches the JS default world spawn).
    pub spawn: [f32; 3],
    pub seed: i64,
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

    /// Make `pos` resident from the in-memory cache or disk. `false` = never
    /// persisted: the caller must generate it (off-thread) and [`adopt`]
    /// (World::adopt) the result.
    pub fn ensure_resident(&mut self, pos: IVec3) -> bool {
        if self.chunks.contains_key(&pos) {
            return true;
        }
        match self.probe(pos) {
            Some(volume) => {
                self.chunks.insert(pos, volume);
                true
            }
            None => false,
        }
    }

    /// Adopt a generated chunk — unless something got there first (another
    /// client's wave, or a generate-then-edit race), in which case the resident
    /// chunk wins. Enqueues background persistence for what was adopted.
    pub fn adopt(&mut self, pos: IVec3, volume: ChunkVolume) {
        if !self.chunks.contains_key(&pos) {
            self.persist.enqueue(self.regions_dir.clone(), pos, volume.clone());
            self.chunks.insert(pos, volume);
        }
    }

    /// Serialize a resident chunk for the wire: `(is_empty, bytes)`, with
    /// `bytes` empty for an all-Air chunk. `None` if not resident.
    pub fn serve(&self, pos: IVec3) -> Option<(bool, Vec<u8>)> {
        let vol = self.chunks.get(&pos)?;
        Some(if vol.is_empty() { (true, Vec::new()) } else { (false, vol.as_bytes().to_vec()) })
    }

    /// Handles for generating chunks off-thread (generation is pure).
    pub fn gen_ctx(&self) -> (Arc<TerrainGen>, Arc<BlockRegistry>) {
        (self.terrain.clone(), self.registry.clone())
    }

    /// Apply a voxel edit at an absolute voxel position and persist the chunk.
    /// Returns false if the containing chunk has not been loaded yet.
    pub fn edit(&mut self, x: i32, y: i32, z: i32, value: u8) -> bool {
        let cpos = IVec3::new(x >> CHUNK_BIT, y >> CHUNK_BIT, z >> CHUNK_BIT);
        let Some(chunk) = self.chunks.get_mut(&cpos) else { return false };
        chunk.set(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP, value);
        // The in-memory chunk is authoritative; persist in the background so
        // the edit doesn't block the tick on zlib + disk.
        self.persist.enqueue(self.regions_dir.clone(), cpos, chunk.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::Persister;

    fn generate_one(world: &World, pos: IVec3) -> ChunkVolume {
        let (terrain, registry) = world.gen_ctx();
        terrain.generate_batch(&[pos], &registry).into_iter().next().unwrap()
    }

    #[test]
    fn generated_chunks_persist_and_reload_from_disk() {
        let dir = std::env::temp_dir().join(format!("soils-world-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Generate a below-surface (non-empty) chunk; adopting caches it and
        // enqueues background persistence.
        let pos = IVec3::new(8, 7, 8);
        let generated = {
            let persister = Persister::new();
            let mut world = World::new(&dir, "default", 0, persister.handle());
            assert!(!world.ensure_resident(pos), "fresh world: nothing on disk yet");
            world.adopt(pos, generate_one(&world, pos));
            let (empty, bytes) = world.serve(pos).expect("adopted chunk is resident");
            assert!(!empty, "below-surface chunk should be non-empty");
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
        assert!(world2.ensure_resident(pos), "chunk should load from disk");
        assert_eq!(world2.serve(pos).unwrap().1, generated);
        persister2.shutdown();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adopt_never_clobbers_a_resident_chunk() {
        let dir = std::env::temp_dir().join(format!("soils-world-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let pos = IVec3::new(8, 7, 8);
        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle());
        let fresh = generate_one(&world, pos);
        world.adopt(pos, fresh.clone());

        // An edit lands, then a stale concurrent generation of the same chunk
        // arrives: the edited chunk must survive.
        assert!(world.edit(pos.x * 32, pos.y * 32, pos.z * 32, 9));
        world.adopt(pos, fresh);
        let (_, bytes) = world.serve(pos).unwrap();
        assert_eq!(bytes[0], 9, "adopt must not overwrite the edited chunk");

        persister.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
