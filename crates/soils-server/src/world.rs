//! In-memory world state plus region-file persistence: terrain generation, a
//! refcounted chunk cache, and load/save through `region`.
//!
//! Since the ECS rework (TODO phase 5) a `World` is owned single-threaded by
//! the sim app — no mutex. The chunk pipeline splits into three calls so
//! generation can run off-thread while the tick stays free:
//! [`ensure_resident`](World::ensure_resident) (cache/disk probe),
//! [`gen_ctx`](World::gen_ctx) + `TerrainGen::generate_batch` (pure, off the
//! tick thread), and [`adopt`](World::adopt) (guarded insert).
//!
//! Lifecycle (TODO phase 6, plan-game-systems §6/§8): subscriptions refcount
//! chunks via [`inc_ref`](World::inc_ref)/[`dec_ref`](World::dec_ref); a
//! resident chunk with zero refs starts an unload timer and
//! [`tick_lifecycle`](World::tick_lifecycle) evicts it (save-if-dirty) once it
//! expires. Edits mark chunks dirty instead of persisting per edit; dirty
//! chunks flush on an interval, on eviction, and on shutdown
//! ([`flush_dirty`](World::flush_dirty)). Freshly *generated* chunks still
//! persist immediately on adopt — they are written once and only rewritten if
//! edited.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::IVec3;
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ChunkVolume};
use soils_worldgen::{BlockRegistry, TerrainGen, WorldType, default_registry};

use crate::persist::PersistHandle;
use crate::region;

/// A resident chunk plus its lifecycle state: `dirty` marks unpersisted edits,
/// `zero_since` runs the unload timer while no client subscribes.
struct ChunkEntry {
    volume: ChunkVolume,
    dirty: bool,
    zero_since: Option<Instant>,
}

pub struct World {
    pub registry: Arc<BlockRegistry>,
    terrain: Arc<TerrainGen>,
    chunks: HashMap<IVec3, ChunkEntry>,
    /// Subscription refcounts, kept for *all* subscribed positions — including
    /// ones still generating — so a chunk adopted mid-flight starts its
    /// lifecycle with the right count.
    refs: HashMap<IVec3, u32>,
    regions_dir: PathBuf,
    /// Handle to the background writer: chunk saves are enqueued here instead
    /// of being written on the tick path.
    persist: PersistHandle,
    /// Memoised region-file headers for the read path. `None` = the region file
    /// doesn't exist (nothing there was ever persisted). Turns a per-chunk file
    /// open into one header read per region.
    ///
    /// Coherent with the background writer because it is only consulted for
    /// chunks NOT resident in `chunks`, and the writer only ever (re)writes
    /// header entries for resident chunks — disjoint sets. Eviction therefore
    /// drops the evicted chunk's region entry (see `tick_lifecycle`).
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
            refs: HashMap::new(),
            regions_dir,
            persist,
            header_cache: HashMap::new(),
            // Surface near here sits around y=256; spawn a little above it so
            // the player starts in the open air rather than buried in rock.
            spawn: [282.0, 285.0, 268.0],
            seed: seed as i64,
        }
    }

    fn entry(&mut self, pos: IVec3, volume: ChunkVolume) -> ChunkEntry {
        let zero_since = if self.refs.get(&pos).copied().unwrap_or(0) > 0 {
            None
        } else {
            Some(Instant::now())
        };
        ChunkEntry { volume, dirty: false, zero_since }
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
                let entry = self.entry(pos, volume);
                self.chunks.insert(pos, entry);
                true
            }
            None => false,
        }
    }

    /// Adopt a generated chunk — unless something got there first (another
    /// client's wave, or a generate-then-edit race), in which case the resident
    /// chunk wins. Enqueues background persistence for what was adopted (a
    /// generated chunk is written once; later rewrites only happen via edits).
    pub fn adopt(&mut self, pos: IVec3, volume: ChunkVolume) {
        if !self.chunks.contains_key(&pos) {
            self.persist.enqueue(self.regions_dir.clone(), pos, volume.clone());
            let entry = self.entry(pos, volume);
            self.chunks.insert(pos, entry);
        }
    }

    /// Serialize a resident chunk for the wire as a `chunk_codec` payload
    /// (palette + LZ4). `None` if not resident.
    pub fn serve(&self, pos: IVec3) -> Option<Vec<u8>> {
        Some(soils_protocol::encode_chunk(&self.chunks.get(&pos)?.volume))
    }

    /// Handles for generating chunks off-thread (generation is pure).
    pub fn gen_ctx(&self) -> (Arc<TerrainGen>, Arc<BlockRegistry>) {
        (self.terrain.clone(), self.registry.clone())
    }

    /// Apply a voxel edit at an absolute voxel position, marking the chunk
    /// dirty for the next flush. Returns false if the containing chunk has not
    /// been loaded yet.
    pub fn edit(&mut self, x: i32, y: i32, z: i32, value: u8) -> bool {
        let cpos = IVec3::new(x >> CHUNK_BIT, y >> CHUNK_BIT, z >> CHUNK_BIT);
        let Some(entry) = self.chunks.get_mut(&cpos) else { return false };
        entry.volume.set(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP, value);
        entry.dirty = true;
        true
    }

    /// A client subscribed to `pos`: cancel any unload timer.
    pub fn inc_ref(&mut self, pos: IVec3) {
        *self.refs.entry(pos).or_insert(0) += 1;
        if let Some(entry) = self.chunks.get_mut(&pos) {
            entry.zero_since = None;
        }
    }

    /// A client unsubscribed from `pos`: on the last ref, start the unload
    /// timer (the chunk stays warm for quick returns until it expires).
    pub fn dec_ref(&mut self, pos: IVec3) {
        match self.refs.get_mut(&pos) {
            Some(1) => {
                self.refs.remove(&pos);
                if let Some(entry) = self.chunks.get_mut(&pos) {
                    entry.zero_since = Some(Instant::now());
                }
            }
            Some(n) => *n -= 1,
            None => {}
        }
    }

    /// Enqueue every dirty chunk for background persistence. Called on an
    /// interval and at shutdown.
    pub fn flush_dirty(&mut self) {
        for (pos, entry) in self.chunks.iter_mut() {
            if entry.dirty {
                entry.dirty = false;
                self.persist.enqueue(self.regions_dir.clone(), *pos, entry.volume.clone());
            }
        }
    }

    /// Evict chunks whose unload timer exceeded `ttl` (save-if-dirty first).
    /// Bounds server memory to roughly what clients are subscribed to.
    pub fn tick_lifecycle(&mut self, ttl: Duration) {
        let expired: Vec<IVec3> = self
            .chunks
            .iter()
            .filter(|(_, e)| e.zero_since.is_some_and(|t| t.elapsed() >= ttl))
            .map(|(&p, _)| p)
            .collect();
        for pos in expired {
            let entry = self.chunks.remove(&pos).expect("collected above");
            if entry.dirty {
                self.persist.enqueue(self.regions_dir.clone(), pos, entry.volume);
            }
            // The background writer will rewrite this chunk's region header;
            // the memoised copy is stale the moment the write lands.
            self.header_cache.remove(&region::region_path(&self.regions_dir, pos));
        }
    }

    /// Resident-chunk count (memory-bound assertions in tests).
    #[cfg(test)]
    pub fn resident(&self) -> usize {
        self.chunks.len()
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
            let payload = world.serve(pos).expect("adopted chunk is resident");
            let vol = soils_protocol::decode_chunk(&payload).expect("payload decodes");
            assert!(!vol.is_empty(), "below-surface chunk should be non-empty");
            persister.shutdown(); // flush the writer
            (payload, vol)
        };

        // The region file now exists and holds exactly the generated voxels.
        let regions = dir.join("worlds").join("default").join("regions");
        assert!(regions.is_dir(), "region dir should exist after flush");
        let loaded = region::load(&regions, pos).unwrap().expect("chunk persisted");
        assert_eq!(loaded.as_bytes(), generated.1.as_bytes());

        // A fresh world loads the chunk from disk (identical bytes) rather than
        // regenerating it.
        let persister2 = Persister::new();
        let mut world2 = World::new(&dir, "default", 0, persister2.handle());
        assert!(world2.ensure_resident(pos), "chunk should load from disk");
        assert_eq!(world2.serve(pos).unwrap(), generated.0);
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
        let vol = soils_protocol::decode_chunk(&world.serve(pos).unwrap()).unwrap();
        assert_eq!(vol.get(0, 0, 0), 9, "adopt must not overwrite the edited chunk");

        persister.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refcounted_eviction_saves_dirty_chunks_and_reloads_them() {
        let dir = std::env::temp_dir().join(format!("soils-world-evict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let pos = IVec3::new(8, 7, 8);
        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle());
        world.inc_ref(pos);
        world.adopt(pos, generate_one(&world, pos));
        assert!(world.edit(pos.x * 32, pos.y * 32, pos.z * 32, 9), "edit marks dirty");

        // Subscribed: never evicts, even with an expired timer.
        world.tick_lifecycle(Duration::ZERO);
        assert_eq!(world.resident(), 1, "subscribed chunk must not evict");

        // Unsubscribed but young: ttl not reached.
        world.dec_ref(pos);
        world.tick_lifecycle(Duration::from_secs(3600));
        assert_eq!(world.resident(), 1, "unload timer hasn't expired yet");

        // Expired: evicted, and the dirty edit is enqueued on the way out.
        world.tick_lifecycle(Duration::ZERO);
        assert_eq!(world.resident(), 0, "zero-ref chunk evicts after ttl");
        drop(world);
        persister.shutdown(); // flush the save-if-dirty write

        // A fresh world sees the edited voxels: nothing was lost to eviction.
        let persister2 = Persister::new();
        let mut world2 = World::new(&dir, "default", 0, persister2.handle());
        assert!(world2.ensure_resident(pos), "evicted chunk reloads from disk");
        let vol = soils_protocol::decode_chunk(&world2.serve(pos).unwrap()).unwrap();
        assert_eq!(vol.get(0, 0, 0), 9, "edit survived eviction via save-if-dirty");

        persister2.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
