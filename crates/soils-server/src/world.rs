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

use soils_sim::light::{self, ChunkLight, LightWorld};
use soils_sim::nav;

use crate::persist::PersistHandle;
use crate::region;

/// Cells with effective light below this count as "dark" for gameplay
/// (spawn) queries.
pub const SPAWN_LIGHT: u8 = 4;

/// A resident chunk plus its lifecycle state: `dirty` marks unpersisted edits,
/// `zero_since` runs the unload timer while no client subscribes. Light is
/// derived data — recomputed on residency via the shared `soils-sim` flood,
/// never persisted or replicated (plan-rendering §3). `version` bumps on
/// every edit (plan-game-systems §8) and guards async light results against
/// racing edits.
struct ChunkEntry {
    volume: ChunkVolume,
    light: ChunkLight,
    summary: LightSummary,
    version: u32,
    dirty: bool,
    zero_since: Option<Instant>,
}

/// An off-thread light-flood job: one queued chunk *column* plus a one-chunk
/// shell, cloned into dense arrays so the flood costs index math instead of
/// per-voxel map lookups (trait-based flooding over the live map measured
/// ~300 ms per column and stalled the 20 Hz tick; dense is ~10-20×
/// cheaper and, more importantly, runs on the rayon pool).
struct LightJob {
    /// Region corner (chunk coords) and size (chunks).
    origin: IVec3,
    dims: IVec3,
    /// Per chunk slot: resident at clone time?
    present: Vec<bool>,
    voxels: Vec<u8>,
    light: Vec<u8>,
    /// (pos, version at clone) for every present chunk — the write-back guard.
    versions: Vec<(IVec3, u32)>,
    /// The chunks this job is responsible for lighting from scratch.
    batch: Vec<IVec3>,
    levels: Vec<u8>,
}

/// `soils_sim::light::LightWorld` over a [`LightJob`]'s dense region.
struct DenseWorld<'a> {
    job: &'a mut LightJob,
}

impl DenseWorld<'_> {
    #[inline]
    fn index(&self, v: IVec3) -> Option<usize> {
        let rc = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT) - self.job.origin;
        let d = self.job.dims;
        if rc.x < 0 || rc.y < 0 || rc.z < 0 || rc.x >= d.x || rc.y >= d.y || rc.z >= d.z {
            return None;
        }
        let slot = ((rc.y * d.z + rc.z) * d.x + rc.x) as usize;
        if !self.job.present[slot] {
            return None;
        }
        let l = soils_protocol::local_of(v);
        Some(slot * 32768 + soils_protocol::voxel_index(l.x, l.y, l.z))
    }
}

impl LightWorld for DenseWorld<'_> {
    fn solid(&self, v: IVec3) -> bool {
        self.index(v).is_some_and(|i| self.job.voxels[i] != 0)
    }

    fn emission(&self, v: IVec3) -> u8 {
        match self.index(v) {
            Some(i) => {
                self.job.levels.get(self.job.voxels[i] as usize).copied().unwrap_or(0)
            }
            None => 0,
        }
    }

    fn light(&self, v: IVec3) -> u8 {
        self.index(v).map_or(0, |i| self.job.light[i])
    }

    fn set_light(&mut self, v: IVec3, packed: u8) {
        if let Some(i) = self.index(v) {
            self.job.light[i] = packed;
        }
    }

    fn in_domain(&self, v: IVec3) -> bool {
        self.index(v).is_some()
    }

    fn open_sky_above(&self, _v: IVec3) -> bool {
        // Only consulted when the cell above is outside the region: assume
        // open sky; `reconcile_sky_below` corrects when the truth loads.
        true
    }
}

/// Run one job to completion (on the rayon pool, or inline in tests):
/// full relight of the batch, border inflow from the lit shell, sky
/// reconciliation below. Returns the new light for every present chunk.
fn run_light_job(mut job: LightJob) -> Vec<(IVec3, ChunkLight, u32)> {
    let batch = job.batch.clone();
    let mut dw = DenseWorld { job: &mut job };
    light::relight_full(&mut dw, &batch);

    // Border inflow: lit cells just outside each batch chunk seed the flood
    // back in (relight_full alone knows nothing beyond the batch set).
    let mut sky_seeds = std::collections::VecDeque::new();
    let mut block_seeds = std::collections::VecDeque::new();
    for &cpos in &batch {
        let origin = cpos * 32;
        for a in 0..32 {
            for b in 0..32 {
                for v in [
                    origin + IVec3::new(-1, a, b),
                    origin + IVec3::new(32, a, b),
                    origin + IVec3::new(a, -1, b),
                    origin + IVec3::new(a, 32, b),
                    origin + IVec3::new(a, b, -1),
                    origin + IVec3::new(a, b, 32),
                ] {
                    let packed = dw.light(v);
                    if light::sky(packed) > 1 {
                        sky_seeds.push_back(v);
                    }
                    if light::block(packed) > 1 {
                        block_seeds.push_back(v);
                    }
                }
            }
        }
    }
    light::propagate(&mut dw, light::Channel::Sky, sky_seeds);
    light::propagate(&mut dw, light::Channel::Block, block_seeds);
    if let Some(&lowest) = batch.iter().min_by_key(|c| c.y) {
        light::reconcile_sky_below(&mut dw, lowest);
    }

    let d = job.dims;
    job.versions
        .iter()
        .map(|&(pos, ver)| {
            let rc = pos - job.origin;
            let slot = ((rc.y * d.z + rc.z) * d.x + rc.x) as usize;
            // Collapse an all-sky / all-dark result to a single byte.
            let out = ChunkLight::from_bytes_collapsed(&job.light[slot * 32768..(slot + 1) * 32768]);
            (pos, out, ver)
        })
        .collect()
}

/// Per-chunk gameplay-lighting summary, maintained alongside the grid.
/// Counts are kept for both sun extremes so queries can pick by the *current*
/// daytime without rescanning voxels (effective light = max(block, sky·sun)).
#[derive(Default, Clone)]
struct LightSummary {
    /// Dark walkable-air cells under full sun.
    dark_day: u16,
    /// Dark walkable-air cells with no sun (night).
    dark_night: u16,
    /// Up to 8 sampled dark-at-night walkable cells: (packed local index,
    /// skylight, blocklight).
    samples: Vec<(u16, u8, u8)>,
}

/// `soils_sim::light::LightWorld` over the resident chunk map. Records which
/// chunks' light changed in `dirty` so summaries can be refreshed.
struct WorldLight<'a> {
    chunks: &'a mut HashMap<IVec3, ChunkEntry>,
    levels: &'a [u8],
    dirty: std::collections::HashSet<IVec3>,
}

impl WorldLight<'_> {
    fn voxel(&self, v: IVec3) -> u8 {
        let c = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
        match self.chunks.get(&c) {
            Some(e) => e.volume.get(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP),
            None => 0,
        }
    }
}

impl LightWorld for WorldLight<'_> {
    fn solid(&self, v: IVec3) -> bool {
        self.voxel(v) != 0
    }

    fn emission(&self, v: IVec3) -> u8 {
        self.levels.get(self.voxel(v) as usize).copied().unwrap_or(0)
    }

    fn light(&self, v: IVec3) -> u8 {
        let c = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
        match self.chunks.get(&c) {
            Some(e) => e.light.get(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP),
            None => 0,
        }
    }

    fn set_light(&mut self, v: IVec3, packed: u8) {
        let c = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
        if let Some(e) = self.chunks.get_mut(&c) {
            e.light.set(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP, packed);
            self.dirty.insert(c);
        }
    }

    fn in_domain(&self, v: IVec3) -> bool {
        let c = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
        self.chunks.contains_key(&c)
    }

    fn open_sky_above(&self, _v: IVec3) -> bool {
        // Only consulted when the chunk above isn't resident: assume open sky;
        // corrected by `reconcile_sky_below` when it loads.
        true
    }
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
    /// Chunks awaiting a light flood (made resident this session; processed
    /// top-of-column-first by [`pump_light`](World::pump_light)).
    light_queue: Vec<IVec3>,
    /// The in-flight async light job's result channel (one at a time, so
    /// region shells never overlap).
    light_inflight: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<(IVec3, ChunkLight, u32)>>>,
    /// Per-block emission levels from the registry, cached for the flood.
    light_levels: Vec<u8>,
    /// Memoised region-file headers for the read path. `None` = the region file
    /// doesn't exist (nothing there was ever persisted). Turns a per-chunk file
    /// open into one header read per region.
    ///
    /// Coherent with the background writer because it is only consulted for
    /// chunks NOT resident in `chunks`, and the writer only ever (re)writes
    /// header entries for resident chunks — disjoint sets. Eviction therefore
    /// drops the evicted chunk's region entry (see `tick_lifecycle`).
    header_cache: HashMap<PathBuf, Option<Box<[u32; 4096]>>>,
    /// Per-chunk pathfinding data (plan §10 stages 1+3): walkability grid +
    /// step-connected regions, lazily derived by [`ensure_nav`]
    /// (World::ensure_nav). Keyed by the (own, below, above) chunk edit
    /// versions — walk grids sample the vertical neighbors' border rows, so
    /// a neighbor edit must also invalidate. Pruned on eviction.
    navs: HashMap<IVec3, ([u32; 3], nav::WalkGrid, nav::ChunkNav)>,
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
        // Reclaim space leaked by append-only chunk rewrites. Best-effort and
        // bounded by the leak thresholds; runs before any header is memoised.
        region::compact_dir(&regions_dir);
        let registry = Arc::new(default_registry());
        Self {
            light_levels: registry.light_table(),
            registry,
            terrain: Arc::new(TerrainGen::new(seed, WorldType::Normal)),
            chunks: HashMap::new(),
            refs: HashMap::new(),
            light_queue: Vec::new(),
            light_inflight: None,
            regions_dir,
            persist,
            header_cache: HashMap::new(),
            navs: HashMap::new(),
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
        self.light_queue.push(pos);
        ChunkEntry {
            volume,
            light: ChunkLight::dark(),
            summary: LightSummary::default(),
            version: 0,
            dirty: false,
            zero_since,
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

    /// Whether a chunk is resident (used to freeze AI on unloaded terrain).
    pub fn has_chunk(&self, cpos: IVec3) -> bool {
        self.chunks.contains_key(&cpos)
    }

    /// Read one voxel at an absolute position. Unloaded space is Air (id 0) —
    /// the shared `soils-sim` sampler contract, used for server-side player
    /// stepping and edit validation.
    pub fn voxel(&self, v: IVec3) -> u8 {
        let cpos = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
        match self.chunks.get(&cpos) {
            Some(entry) => entry.volume.get(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP),
            None => 0,
        }
    }

    /// Apply a voxel edit at an absolute voxel position, marking the chunk
    /// dirty for the next flush and incrementally relighting around the cell.
    /// Returns false if the containing chunk has not been loaded yet.
    pub fn edit(&mut self, x: i32, y: i32, z: i32, value: u8) -> bool {
        let cpos = IVec3::new(x >> CHUNK_BIT, y >> CHUNK_BIT, z >> CHUNK_BIT);
        let Some(entry) = self.chunks.get_mut(&cpos) else { return false };
        entry.volume.set(x & CHUNK_CLIP, y & CHUNK_CLIP, z & CHUNK_CLIP, value);
        entry.dirty = true;
        entry.version = entry.version.wrapping_add(1);
        let mut lw = WorldLight {
            chunks: &mut self.chunks,
            levels: &self.light_levels,
            dirty: std::collections::HashSet::new(),
        };
        light::apply_voxel_change(&mut lw, IVec3::new(x, y, z));
        let touched = lw.dirty;
        for c in touched {
            self.rebuild_summary(c);
        }
        true
    }

    /// Advance the async lighting pipeline: apply a finished job's results
    /// (guarded by chunk versions — anything edited mid-flight requeues),
    /// then dispatch the next column job to the rayon pool if idle. The tick
    /// only ever pays for clones and write-backs; the flood itself runs off-
    /// thread over a dense region (see [`LightJob`]).
    pub fn pump_light(&mut self) {
        if let Some(rx) = &mut self.light_inflight {
            match rx.try_recv() {
                Ok(results) => {
                    self.light_inflight = None;
                    self.apply_light_results(results);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                Err(_) => self.light_inflight = None, // worker died; redispatch
            }
        }
        if let Some(job) = self.build_light_job() {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            self.light_inflight = Some(rx);
            rayon::spawn(move || {
                let _ = tx.send(run_light_job(job));
            });
        }
    }

    /// Take one column batch off the queue and clone its dense region.
    fn build_light_job(&mut self) -> Option<LightJob> {
        let batch = loop {
            let &top = self.light_queue.iter().max_by_key(|c| c.y)?;
            let column: Vec<IVec3> = self
                .light_queue
                .iter()
                .copied()
                .filter(|c| c.x == top.x && c.z == top.z && self.chunks.contains_key(c))
                .collect();
            self.light_queue.retain(|c| !(c.x == top.x && c.z == top.z));
            if !column.is_empty() {
                break column; // evicted-while-queued chunks just drop out
            }
        };
        let ymin = batch.iter().map(|c| c.y).min().unwrap() - 1;
        let ymax = batch.iter().map(|c| c.y).max().unwrap() + 1;
        let origin = IVec3::new(batch[0].x - 1, ymin, batch[0].z - 1);
        let dims = IVec3::new(3, ymax - ymin + 1, 3);
        let slots = (dims.x * dims.y * dims.z) as usize;
        let mut job = LightJob {
            origin,
            dims,
            present: vec![false; slots],
            voxels: vec![0u8; slots * 32768],
            light: vec![0u8; slots * 32768],
            versions: Vec::new(),
            batch,
            levels: self.light_levels.clone(),
        };
        for ry in 0..dims.y {
            for rz in 0..dims.z {
                for rx in 0..dims.x {
                    let pos = origin + IVec3::new(rx, ry, rz);
                    let Some(entry) = self.chunks.get(&pos) else { continue };
                    let slot = ((ry * dims.z + rz) * dims.x + rx) as usize;
                    job.present[slot] = true;
                    job.voxels[slot * 32768..(slot + 1) * 32768]
                        .copy_from_slice(entry.volume.as_bytes());
                    entry.light.write_into(&mut job.light[slot * 32768..(slot + 1) * 32768]);
                    job.versions.push((pos, entry.version));
                }
            }
        }
        Some(job)
    }

    fn apply_light_results(&mut self, results: Vec<(IVec3, ChunkLight, u32)>) {
        for (pos, new_light, ver) in results {
            match self.chunks.get_mut(&pos) {
                Some(entry) if entry.version == ver => {
                    entry.light = new_light;
                    self.rebuild_summary(pos);
                }
                // Edited (or reloaded) while the job flew: its inline relight
                // is fresher than ours; requeue for a clean pass.
                Some(_) => self.light_queue.push(pos),
                None => {}
            }
        }
    }

    /// Whether all queued light work has been applied (tests).
    #[cfg(test)]
    pub fn light_settled(&self) -> bool {
        self.light_queue.is_empty() && self.light_inflight.is_none()
    }

    /// Drive the lighting pipeline to completion synchronously (tests only).
    #[cfg(test)]
    pub fn pump_light_blocking(&mut self) {
        if let Some(mut rx) = self.light_inflight.take() {
            if let Some(results) = rx.blocking_recv() {
                self.apply_light_results(results);
            }
        }
        while let Some(job) = self.build_light_job() {
            let results = run_light_job(job);
            self.apply_light_results(results);
        }
    }

    /// Rebuild one chunk's gameplay-lighting summary: dark walkable-air cells
    /// under both sun extremes, plus a small sample of dark cells for spawn
    /// queries. Walkable-air ≈ air with air headroom above and solid below
    /// (in-chunk approximation; the pathfinding walkability grid refines this
    /// in a later phase).
    fn rebuild_summary(&mut self, cpos: IVec3) {
        let Some(entry) = self.chunks.get(&cpos) else { return };
        let mut summary = LightSummary::default();
        for y in 1..31 {
            for z in 0..32 {
                for x in 0..32 {
                    if entry.volume.get(x, y, z) != 0
                        || entry.volume.get(x, y + 1, z) != 0
                        || entry.volume.get(x, y - 1, z) == 0
                    {
                        continue;
                    }
                    let packed = entry.light.get(x, y, z);
                    let (sky, block) = (light::sky(packed), light::block(packed));
                    if block < SPAWN_LIGHT {
                        summary.dark_night += 1;
                        if sky.max(block) < SPAWN_LIGHT {
                            summary.dark_day += 1;
                        }
                        if summary.samples.len() < 8 {
                            let idx = (x + y * 32 + z * 1024) as u16;
                            summary.samples.push((idx, sky, block));
                        }
                    }
                }
            }
        }
        self.chunks.get_mut(&cpos).expect("checked above").summary = summary;
    }

    /// Gameplay spawn query (plan-rendering §3): the darkest currently-valid
    /// walkable cell within `radius` chunks of `center`, judged at sun level
    /// `sun` (0 = midnight, 1 = noon; effective light = max(block, sky·sun)).
    /// O(chunk summaries), no voxel scans beyond validating sampled cells.
    /// The first gameplay consumer is the mob spawner (pathfinding phase).
    #[allow(dead_code)]
    pub fn darkest_walkable_near(&self, center: IVec3, radius: i32, sun: f32) -> Option<IVec3> {
        let ccenter =
            IVec3::new(center.x >> CHUNK_BIT, center.y >> CHUNK_BIT, center.z >> CHUNK_BIT);
        let mut best: Option<(f32, IVec3)> = None;
        for dx in -radius..=radius {
            for dy in -radius..=radius {
                for dz in -radius..=radius {
                    let cpos = ccenter + IVec3::new(dx, dy, dz);
                    let Some(entry) = self.chunks.get(&cpos) else { continue };
                    let candidates =
                        if sun > 0.5 { entry.summary.dark_day } else { entry.summary.dark_night };
                    if candidates == 0 {
                        continue;
                    }
                    for &(idx, sky, block) in &entry.summary.samples {
                        let effective = (block as f32).max(sky as f32 * sun);
                        if effective >= SPAWN_LIGHT as f32 {
                            continue;
                        }
                        if best.is_none_or(|(b, _)| effective < b) {
                            let (x, y, z) =
                                ((idx % 32) as i32, ((idx / 32) % 32) as i32, (idx / 1024) as i32);
                            let world_pos = IVec3::new(
                                (cpos.x << CHUNK_BIT) + x,
                                (cpos.y << CHUNK_BIT) + y,
                                (cpos.z << CHUNK_BIT) + z,
                            );
                            // Validate against live voxels (samples can go
                            // stale between summary rebuilds).
                            if self.voxel(world_pos) == 0
                                && self.voxel(world_pos + IVec3::Y) == 0
                                && self.voxel(world_pos - IVec3::Y) != 0
                            {
                                best = Some((effective, world_pos));
                            }
                        }
                    }
                }
            }
        }
        best.map(|(_, p)| p)
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
            self.navs.remove(&pos);
        }
    }

    /// Refresh the cached pathfinding data for `cpos` if its version key
    /// (own + vertical-neighbor edit versions) moved; no-op when fresh, drops
    /// the entry for non-resident chunks. Cold builds scan the chunk's voxels
    /// (~1 ms), so callers only ensure the chunks a search will touch.
    pub fn ensure_nav(&mut self, cpos: IVec3) {
        if !self.chunks.contains_key(&cpos) {
            self.navs.remove(&cpos);
            return;
        }
        let ver = |c: IVec3| self.chunks.get(&c).map_or(u32::MAX, |e| e.version);
        let key = [ver(cpos), ver(cpos - IVec3::Y), ver(cpos + IVec3::Y)];
        if self.navs.get(&cpos).is_some_and(|(k, ..)| *k == key) {
            return;
        }
        let grid = nav::walk_grid(&|v: IVec3| self.voxel(v), cpos);
        let regions = nav::build_nav(&grid);
        self.navs.insert(cpos, (key, grid, regions));
    }

    /// Cached pathfinding data (build with [`ensure_nav`](Self::ensure_nav)
    /// first — this never derives).
    pub fn nav(&self, cpos: IVec3) -> Option<(&nav::WalkGrid, &nav::ChunkNav)> {
        self.navs.get(&cpos).map(|(_, g, n)| (g, n))
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
    fn nav_cache_tracks_own_and_neighbor_edits() {
        let dir = std::env::temp_dir().join(format!("soils-world-nav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle());

        // A surface chunk plus its vertical neighbors (the grid samples them).
        let cpos = IVec3::new(8, 8, 8);
        for dy in -1..=1 {
            let p = cpos + IVec3::Y * dy;
            let vol = generate_one(&world, p);
            world.adopt(p, vol);
        }
        world.ensure_nav(cpos);
        let count0 = world.nav(cpos).expect("nav built").0.count();
        assert!(count0 > 0, "a surface chunk has walkable cells");

        // Placing a block on a walkable cell (floor of the chunk interior)
        // must rebuild the grid after ensure_nav — find one walkable cell and
        // fill its headroom.
        let origin = cpos * 32;
        let cell = (0..32 * 32 * 32)
            .map(|i| origin + IVec3::new(i % 32, (i / 1024) % 32, (i / 32) % 32))
            .find(|c| {
                world.nav(cpos).unwrap().0.get(c.x - origin.x, c.y - origin.y, c.z - origin.z)
            })
            .expect("some walkable cell");
        assert!(world.edit(cell.x, cell.y, cell.z, 3));
        world.ensure_nav(cpos);
        // (Counts can be net-zero — the placed block's top becomes walkable —
        // so assert the cell itself: a stale cache would still say true.)
        assert!(
            !world.nav(cpos).unwrap().0.get(
                cell.x - origin.x,
                cell.y - origin.y,
                cell.z - origin.z
            ),
            "own edit must invalidate the cached grid"
        );

        // An edit in the chunk *below* also invalidates (border rows sample
        // it): after ensure_nav the cache must equal a fresh derivation.
        let below = origin - IVec3::Y;
        let was_solid = world.voxel(below) != 0;
        assert!(world.edit(below.x, below.y, below.z, if was_solid { 0 } else { 3 }));
        world.ensure_nav(cpos);
        let fresh = nav::walk_grid(&|v: IVec3| world.voxel(v), cpos);
        assert!(
            *world.nav(cpos).unwrap().0 == fresh,
            "cached grid must match a fresh derivation after a neighbor edit"
        );

        persister.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Make a 3×3×3 region around `center` resident and fully lit.
    fn lit_region(world: &mut World, center: IVec3) -> Vec<IVec3> {
        let mut chunks = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    chunks.push(center + IVec3::new(dx, dy, dz));
                }
            }
        }
        let (terrain, registry) = world.gen_ctx();
        let volumes = terrain.generate_batch(&chunks, &registry);
        for (pos, vol) in chunks.iter().zip(volumes) {
            world.adopt(*pos, vol);
        }
        world.pump_light_blocking();
        assert!(world.light_settled());
        chunks
    }

    #[test]
    fn incremental_light_matches_fresh_relight_after_edit_storm() {
        let dir = std::env::temp_dir().join(format!("soils-world-light-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Surface region around the spawn chunk: sky, terrain, and caves.
        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle());
        let center = IVec3::new(8, 8, 8);
        let chunks = lit_region(&mut world, center);

        // Storm of edits in the center chunk: place a light-tight slab, punch
        // holes in it, drop emissive-adjacent structure, then remove some.
        let base = center * 32;
        let mut s = 42u64;
        for i in 0..48 {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let (x, y, z) =
                (base.x + (s >> 20) as i32 % 32, base.y + 8 + i % 12, base.z + (s >> 40) as i32 % 32);
            let value = if i % 3 == 0 { 0 } else { 1 + (i % 4) as u8 };
            world.edit(x, y, z, value);
        }

        // Fresh oracle: same voxels, full relight from scratch.
        let persister2 = Persister::new();
        let mut fresh = World::new(&dir, "oracle", 0, persister2.handle());
        for &pos in &chunks {
            let vol = ChunkVolume::from_bytes(
                soils_protocol::decode_chunk(&world.serve(pos).unwrap()).unwrap().as_bytes(),
            );
            fresh.adopt(pos, vol);
        }
        fresh.light_queue.clear(); // relight the whole set in one oracle pass
        let mut lw = WorldLight {
            chunks: &mut fresh.chunks,
            levels: &fresh.light_levels,
            dirty: std::collections::HashSet::new(),
        };
        light::relight_full(&mut lw, &chunks);

        for &pos in &chunks {
            assert_eq!(
                world.chunks[&pos].light.as_dense_bytes(),
                fresh.chunks[&pos].light.as_dense_bytes(),
                "incremental light diverged from fresh relight at {pos}"
            );
        }
        persister.shutdown();
        persister2.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn darkest_walkable_query_returns_valid_dark_cells() {
        let dir = std::env::temp_dir().join(format!("soils-world-dark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle());
        // Deep underground: solid rock threaded with generated caves — the
        // natural home of dark walkable cells, even at noon.
        let center = IVec3::new(8, 4, 8);
        lit_region(&mut world, center);

        let probe = center * 32 + IVec3::splat(16);
        let found = world
            .darkest_walkable_near(probe, 1, 1.0)
            .expect("cave region should offer a dark walkable cell even at noon");
        // The candidate is genuinely walkable and genuinely dark.
        assert_eq!(world.voxel(found), 0, "cell must be air");
        assert_eq!(world.voxel(found + IVec3::Y), 0, "needs headroom");
        assert_ne!(world.voxel(found - IVec3::Y), 0, "must stand on solid ground");

        // Summaries track edits: fill the found cell; the query must not hand
        // out that exact cell again.
        assert!(world.edit(found.x, found.y, found.z, 1));
        if let Some(again) = world.darkest_walkable_near(probe, 1, 1.0) {
            assert_ne!(again, found, "filled cell must leave the candidate set");
            assert_eq!(world.voxel(again), 0);
        }

        persister.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn surface_darkness_grows_at_night() {
        let dir = std::env::temp_dir().join(format!("soils-world-night-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle());
        let center = IVec3::new(8, 8, 8);
        lit_region(&mut world, center);

        // At noon the open surface is lit; at midnight it counts as dark, so
        // the night query finds a candidate where the day query may not.
        let probe = center * 32 + IVec3::splat(16);
        let night = world.darkest_walkable_near(probe, 1, 0.0);
        assert!(night.is_some(), "night should open surface cells for spawns");

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
