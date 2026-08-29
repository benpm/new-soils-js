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
//! ([`flush_dirty`](World::flush_dirty)). Freshly *generated* chunks are NOT
//! persisted — worldgen v2 is deterministic, so pristine chunks regenerate on
//! demand and a chunk is only written once it has been
//! edited.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::IVec3;
use soils_protocol::{AIR, CHUNK_BIT, CHUNK_CLIP, CHUNK_SIZE, ChunkVolume, chunk_of};
use soils_worldgen::{BlockRegistry, TerrainGen, WorldType, default_registry};

use soils_sim::block_data::ChunkData;
use soils_sim::light::{self, ChunkLight, LightWorld};
use soils_sim::nav;

/// Block data crosses the disk boundary as bincode, the same encoding the wire
/// uses. `paged` compresses it, so this side stays a plain serialization.
impl Codec for ChunkData {
    fn encode(&self) -> Vec<u8> {
        soils_protocol::encode(self)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        soils_protocol::decode(bytes)
    }

    fn is_empty(&self) -> bool {
        ChunkData::is_empty(self)
    }
}

use crate::persist::PersistHandle;
use crate::region;
use crate::store::{Codec, Store};

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
    /// Ever edited (from the region header's EDITED_FLAG, set by `edit`).
    /// Pristine chunks stream as manifest positions; edited ones as payloads.
    edited: bool,
    /// Which of the chunk's six boundary layers are solid all the way across
    /// (see [`face_mask`]). Drives occlusion culling; recomputed on edit.
    faces: u8,
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

/// One-time (per generator identity) reclassification of persisted chunks:
/// **pristine ⇔ persisted bytes == current gen(seed, graph, world_type, pos)**.
/// Any worldgen change (algo bump, graph edit) invalidates the `gen_stamp`
/// file and re-runs the sweep, demoting now-unreproducible chunks to edited —
/// fail-safe in both directions: a wrong "pristine" is definitionally
/// impossible (it requires byte equality with what the client will generate),
/// a wrong "edited" only costs wire bytes. Crash mid-sweep → stamp absent →
/// idempotent re-run.
fn classify_regions(
    data_dir: &Path,
    name: &str,
    regions_dir: &Path,
    terrain: &TerrainGen,
    registry: &BlockRegistry,
    graph_hash: u64,
) {
    let stamp_path = data_dir.join("worlds").join(name).join("gen_stamp");
    let stamp = format!("{graph_hash} {} 0", terrain.seed());
    if std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str()) {
        return;
    }
    let t0 = Instant::now();
    let mut total = 0usize;
    let mut edited = 0usize;
    let result = region::classify_dir(regions_dir, |batch| {
        let positions: Vec<IVec3> = batch.iter().map(|(p, _)| *p).collect();
        let gens = terrain.generate_batch(&positions, registry);
        total += batch.len();
        batch
            .iter()
            .zip(&gens)
            .map(|((_, vol), generated)| {
                let is_edited = vol.as_bytes() != generated.as_bytes();
                edited += is_edited as usize;
                is_edited
            })
            .collect()
    });
    match result {
        Ok(()) => {
            let _ = std::fs::write(&stamp_path, &stamp);
            if total > 0 {
                println!(
                    "world {name}: classified {total} persisted chunks ({edited} edited) in {:?}",
                    t0.elapsed()
                );
            }
        }
        Err(e) => eprintln!("world {name}: region classification failed (will retry on open): {e}"),
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
    /// Stable id this world's chunks are keyed under when SpacetimeDB
    /// mirroring is on; `None` leaves persistence disk-only.
    stdb_world_id: Option<u16>,
    /// Handle to the background writer: chunk saves are enqueued here instead
    /// of being written on the tick path.
    persist: PersistHandle,
    /// Per-block state the voxel array cannot hold — chest contents, today.
    /// Same write-back policy as `chunks`, one page per chunk, addressed at the
    /// same slot index in a parallel file. See `store.rs`.
    block_data: Store<ChunkData>,
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
    /// Cached `soils_worldgen::graph_hash` of the active generator.
    graph_hash: u64,
    /// Precomputed inclusive shell box of the demo chamber, in absolute
    /// voxels, with the ids it is built from: `(min, max, wall, floor)`.
    /// Derived once in [`World::new`] from the terrain's own surface height,
    /// so [`World::adopt`] only has to intersect an AABB.
    chamber: Option<(IVec3, IVec3, u8, u8)>,
}

/// The six axis directions, in the order [`face_mask`] indexes its bits.
pub const FACE_DIRS: [IVec3; 6] = [
    IVec3::new(1, 0, 0),
    IVec3::new(-1, 0, 0),
    IVec3::new(0, 1, 0),
    IVec3::new(0, -1, 0),
    IVec3::new(0, 0, 1),
    IVec3::new(0, 0, -1),
];

/// Index of `dir` in [`FACE_DIRS`], or `None` if it is not an axis step.
fn face_index(dir: IVec3) -> Option<usize> {
    FACE_DIRS.iter().position(|d| *d == dir)
}

/// One bit per direction in [`FACE_DIRS`]: set when that boundary layer of the
/// chunk is solid across all 32x32 of it.
///
/// This is what occlusion culling is built on. A chunk whose neighbours all
/// present a solid layer towards it cannot be seen into and cannot be walked
/// into, whatever it contains — so the client never needs it, caves and all.
/// Testing the *neighbours'* layers rather than the chunk's own contents is
/// what makes the test useful: at depth almost every chunk has some cave air
/// in it, but the layer between two of them is usually still solid.
fn face_mask(vol: &ChunkVolume) -> u8 {
    let n = CHUNK_SIZE - 1;
    let mut mask = 0u8;
    for (bit, dir) in FACE_DIRS.iter().enumerate() {
        let solid = (0..CHUNK_SIZE).all(|a| {
            (0..CHUNK_SIZE).all(|b| {
                let (x, y, z) = match (dir.x, dir.y, dir.z) {
                    (1, 0, 0) => (n, a, b),
                    (-1, 0, 0) => (0, a, b),
                    (0, 1, 0) => (a, n, b),
                    (0, -1, 0) => (a, 0, b),
                    (0, 0, 1) => (a, b, n),
                    _ => (a, b, 0),
                };
                vol.get(x, y, z) != AIR
            })
        });
        if solid {
            mask |= 1 << bit;
        }
    }
    mask
}

/// The traditional spawn column. Only x/z are fixed; the height follows the
/// terrain.
const SPAWN_X: i32 = 282;
const SPAWN_Z: i32 = 268;
/// Eye height above the surface voxel at spawn.
///
/// 29 preserves the drop the fixed `[282, 285, 268]` spawn used to have over a
/// ~256 surface. It is not cosmetic: players spawn flying, and the open air
/// under them is where scripted tests build platforms and place blocks within
/// reach. Shrinking it puts the spawn inside whatever the terrain does there.
pub(crate) const SPAWN_CLEARANCE: f32 = 29.0;

/// Spawn eye position for a world: the generator's own surface height at the
/// spawn column, plus clearance.
///
/// Hardcoding the height stopped being viable when the continental octave
/// landed — the surface at this column can now sit anywhere across hundreds of
/// voxels, so a fixed y is either buried in rock or hundreds of blocks up in
/// the air.
fn surface_spawn(terrain: &TerrainGen, x: i32, z: i32) -> [f32; 3] {
    let h = terrain.surface_height(x, z) as f32;
    [x as f32, h + SPAWN_CLEARANCE, z as f32]
}

impl World {
    /// Turn on SpacetimeDB mirroring for this world's chunk saves.
    pub fn enable_stdb(&mut self, world_id: u16) {
        self.stdb_world_id = Some(world_id);
    }

    /// Seed the region files from SpacetimeDB when there are none.
    ///
    /// This is the case that makes the mirror worth its cost: a fresh
    /// deployment, or a host whose disk was lost, gets its edited chunks back
    /// instead of a world reset to pristine terrain. Only edited chunks were
    /// ever stored — everything else is bit-exact reproducible from
    /// `GenParams`, so a full restore is exactly the edits and nothing more.
    ///
    /// Deliberately only when the directory is empty. Region files stay
    /// authoritative, and a restore that ran over a populated directory could
    /// roll live edits backwards to whatever the mirror last received.
    pub fn restore_from_stdb(&mut self, link: &soils_stdb::StdbLink) -> usize {
        let Some(world_id) = self.stdb_world_id else { return 0 };
        let populated = std::fs::read_dir(&self.regions_dir)
            .map(|d| d.filter_map(Result::ok).any(|e| e.path().is_file()))
            .unwrap_or(false);
        if populated {
            return 0;
        }

        // Short, because this runs on the tick thread: the restore has to
        // finish before anyone can join and generate pristine terrain over the
        // chunks it is recovering, so it cannot be moved off the critical path
        // — but it must not stall the first heartbeat either.
        let (blobs, complete) = link.fetch_world_chunks(world_id, std::time::Duration::from_secs(5));
        if !complete {
            // Not the same as "this world has nothing stored". Say so: from
            // here the server generates pristine terrain, and the next flush
            // of an edited chunk overwrites whatever was really in the
            // database.
            eprintln!(
                "stdb restore: world {world_id} did not answer in time;                  continuing with {} recovered chunk(s) — stored edits may be                  overwritten once play resumes",
                blobs.len()
            );
        }
        if blobs.is_empty() {
            return 0;
        }
        let mut restored = Vec::new();
        for blob in &blobs {
            let Some(volume) = soils_protocol::decode_chunk(&blob.payload) else {
                eprintln!(
                    "stdb restore: chunk ({}, {}, {}) has an undecodable payload; skipping",
                    blob.cx, blob.cy, blob.cz
                );
                continue;
            };
            restored.push((IVec3::new(blob.cx, blob.cy, blob.cz), volume));
        }
        // Written straight to disk rather than into the resident map: the
        // normal load path picks them up from there, and a restore should look
        // to the rest of the server exactly like a world that was always there.
        let refs: Vec<(IVec3, &ChunkVolume, bool)> =
            restored.iter().map(|(p, v)| (*p, v, true)).collect();
        if let Err(e) = region::save_many(&self.regions_dir, &refs) {
            eprintln!("stdb restore: could not write region files: {e}");
            return 0;
        }
        println!("stdb restore: recovered {} edited chunks from SpacetimeDB", refs.len());
        refs.len()
    }

    /// The chunk key this position mirrors under, if mirroring is on and the
    /// position is representable.
    fn stdb_key(&self, pos: IVec3) -> Option<u64> {
        let id = self.stdb_world_id?;
        soils_protocol::chunk_key::pack_chunk_key(id, pos.x, pos.y, pos.z)
    }

    /// Create (or open) a named world under `data_dir`. Each world persists to
    /// its own region directory and generates from its own `seed`, so different
    /// names yield different terrain.
    pub fn new(
        data_dir: &Path,
        name: &str,
        seed: u32,
        persist: PersistHandle,
        chamber: Option<crate::Chamber>,
    ) -> Self {
        let regions_dir = data_dir.join("worlds").join(name).join("regions");
        // Reclaim space leaked by append-only chunk rewrites. Best-effort and
        // bounded by the leak thresholds; runs before any header is memoised.
        region::compact_dir(&regions_dir);
        let registry = Arc::new(default_registry());
        let terrain = Arc::new(TerrainGen::new(seed, WorldType::Normal));
        let graph_hash = soils_worldgen::graph_hash(terrain.graph());
        classify_regions(data_dir, name, &regions_dir, &terrain, &registry, graph_hash);
        let spawn = surface_spawn(&terrain, SPAWN_X, SPAWN_Z);
        // Anchored to the generator's own surface height rather than an
        // absolute y: with the continental octave the surface at this column
        // can sit anywhere across hundreds of voxels, so a fixed depth is the
        // only thing that means the same everywhere.
        let chamber = chamber.map(|c| {
            let s = terrain.surface_height(SPAWN_X, SPAWN_Z);
            let floor = s - c.depth;
            let wall_id = registry.id_of("Stone").unwrap_or(3);
            let floor_id = registry.id_of("Cobblestone").unwrap_or(wall_id);
            (
                IVec3::new(SPAWN_X - c.half - 1, floor, SPAWN_Z - c.half - 1),
                IVec3::new(SPAWN_X + c.half + 1, floor + c.height + 1, SPAWN_Z + c.half + 1),
                wall_id,
                floor_id,
            )
        });
        Self {
            chamber,
            light_levels: registry.light_table(),
            registry,
            terrain,
            chunks: HashMap::new(),
            refs: HashMap::new(),
            light_queue: Vec::new(),
            light_inflight: None,
            block_data: Store::new(regions_dir.clone(), "b"),
            regions_dir,
            stdb_world_id: None,
            persist,
            header_cache: HashMap::new(),
            navs: HashMap::new(),
            spawn,
            seed: seed as i64,
            graph_hash,
        }
    }

    fn entry(&mut self, pos: IVec3, volume: ChunkVolume, edited: bool) -> ChunkEntry {
        let zero_since = if self.refs.get(&pos).copied().unwrap_or(0) > 0 {
            None
        } else {
            Some(Instant::now())
        };
        self.light_queue.push(pos);
        let faces = face_mask(&volume);
        ChunkEntry {
            volume,
            light: ChunkLight::dark(),
            summary: LightSummary::default(),
            version: 0,
            dirty: false,
            edited,
            faces,
            zero_since,
        }
    }

    /// Read a persisted chunk via the memoised region-header cache, opening the
    /// region file at most once per region instead of once per chunk. Returns
    /// `None` for a chunk that has never been persisted (caller generates it).
    fn probe(&mut self, pos: IVec3) -> Option<(ChunkVolume, bool)> {
        let path = region::region_path(&self.regions_dir, pos);
        let header = self
            .header_cache
            .entry(path)
            .or_insert_with(|| region::read_header(&self.regions_dir, pos).unwrap_or(None));
        let hdr = header.as_ref()?;
        let entry = hdr[region::header_index(pos)];
        let vol = region::read_chunk(&self.regions_dir, pos, entry).unwrap_or(None)?;
        Some((vol, region::entry_edited(entry)))
    }

    /// Make `pos` resident from the in-memory cache or disk. `false` = never
    /// persisted: the caller must generate it (off-thread) and [`adopt`]
    /// (World::adopt) the result.
    pub fn ensure_resident(&mut self, pos: IVec3) -> bool {
        if self.chunks.contains_key(&pos) {
            return true;
        }
        match self.probe(pos) {
            Some((volume, edited)) => {
                let entry = self.entry(pos, volume, edited);
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
    pub fn adopt(&mut self, pos: IVec3, mut volume: ChunkVolume) {
        if !self.chunks.contains_key(&pos) {
            // Carve *before* `entry`: it computes the face mask and seeds the
            // light state from the volume it is handed.
            let carved = self.carve_chamber(pos, &mut volume);
            // Pristine chunks are no longer persisted — they're reproducible
            // from the world identity (worldgen v2 is deterministic), so they
            // hit disk only once edited (the dirty flush).
            //
            // A carved chunk is marked edited, and that is load-bearing rather
            // than bookkeeping. A pristine manifest entry tells the client to
            // *regenerate* the chunk locally from `GenParams`, which would
            // reproduce solid rock — the room would exist only on the server.
            // It also keeps the chunk in the client's CPU mirror, which is
            // what the placement raycast reads, so a pristine chamber is one
            // you cannot put a block in either.
            //
            // Not marked dirty: the carve is a pure function of the config and
            // the seed, so an evicted chamber chunk re-carves identically on
            // its way back in and never needs to reach disk on its own.
            let entry = self.entry(pos, volume, carved);
            self.chunks.insert(pos, entry);
        }
    }

    /// Stamp this chunk's slice of the configured chamber into `vol`.
    /// Returns whether it touched anything.
    ///
    /// The room is a solid shell with an air interior: without the shell a
    /// natural cave could open it to a lit column (skylight falls down a shaft
    /// with no attenuation) or simply drop the player through the floor.
    fn carve_chamber(&self, pos: IVec3, vol: &mut ChunkVolume) -> bool {
        let Some((min, max, wall, floor)) = self.chamber else { return false };
        let origin = soils_protocol::chunk_origin(pos);
        let size = soils_protocol::CHUNK_SIZE as i32;
        // Chunk-vs-box rejection first: all but a dozen chunks in a world
        // leave here without touching a voxel.
        if origin.x > max.x
            || origin.y > max.y
            || origin.z > max.z
            || origin.x + size <= min.x
            || origin.y + size <= min.y
            || origin.z + size <= min.z
        {
            return false;
        }
        let lo = min.max(origin);
        let hi = max.min(origin + IVec3::splat(size - 1));
        for wy in lo.y..=hi.y {
            for wz in lo.z..=hi.z {
                for wx in lo.x..=hi.x {
                    let inside = wx > min.x
                        && wx < max.x
                        && wy > min.y
                        && wy < max.y
                        && wz > min.z
                        && wz < max.z;
                    // Two 2x2 pillars left standing. A featureless box gives
                    // the lamplight nothing to fall on, and the whole point of
                    // the take is watching it land on something.
                    let pillar = inside
                        && (wx - SPAWN_X).abs().abs_diff(12) < 2
                        && (wz - SPAWN_Z).abs().abs_diff(12) < 2;
                    let id = if pillar {
                        wall
                    } else if inside {
                        0
                    } else if wy == min.y {
                        floor
                    } else {
                        wall
                    };
                    vol.set(wx - origin.x, wy - origin.y, wz - origin.z, id);
                }
            }
        }
        true
    }

    /// Serialize a resident chunk for the wire as a `chunk_codec` payload
    /// (palette + LZ4). `None` if not resident.
    pub fn serve(&self, pos: IVec3) -> Option<Vec<u8>> {
        Some(soils_protocol::encode_chunk(&self.chunks.get(&pos)?.volume))
    }

    /// Whether a resident chunk has ever been edited (manifest classification).
    /// `None` if not resident.
    pub fn chunk_edited(&self, pos: IVec3) -> Option<bool> {
        self.chunks.get(&pos).map(|e| e.edited)
    }

    /// The world-generation identity clients need for local generation.
    pub fn gen_params(&self) -> soils_protocol::GenParams {
        soils_protocol::GenParams {
            seed: self.seed,
            world_type: 0, // the server currently always runs WorldType::Normal
            graph_hash: self.graph_hash,
        }
    }

    /// Handles for generating chunks off-thread (generation is pure).
    pub fn gen_ctx(&self) -> (Arc<TerrainGen>, Arc<BlockRegistry>) {
        (self.terrain.clone(), self.registry.clone())
    }

    /// Whether a chunk is resident (used to freeze AI on unloaded terrain).
    pub fn has_chunk(&self, cpos: IVec3) -> bool {
        self.chunks.contains_key(&cpos)
    }

    /// A resident chunk's voxel volume and edit version, for building physics
    /// colliders (`version` bumps on edit, so a stale collider is rebuilt).
    pub fn chunk_volume(&self, cpos: IVec3) -> Option<(&ChunkVolume, u32)> {
        self.chunks.get(&cpos).map(|e| (&e.volume, e.version))
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
        entry.edited = true;
        entry.version = entry.version.wrapping_add(1);
        // An edit can punch through a boundary layer, which un-seals whichever
        // neighbour that layer was hiding. Cheaper to recompute the six masks
        // than to reason about which one the voxel belonged to.
        entry.faces = face_mask(&entry.volume);
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

    /// One chunk's block data, faulting the page in. Absent data reads as an
    /// empty `ChunkData` — a chunk nobody has put anything in is
    /// indistinguishable from one that was never written, and neither needs a
    /// caller to know the difference.
    ///
    /// The server reaches for individual blocks rather than whole pages
    /// ([`container_at`](Self::container_at)); this is the whole-page view the
    /// tests assert against and the shape a future "what is in this chunk"
    /// query would use.
    #[cfg(test)]
    pub fn block_data(&mut self, cpos: IVec3) -> &ChunkData {
        self.block_data.get(cpos)
    }

    /// The container at an absolute voxel position, or `None` if that block
    /// holds nothing. Cheap on the common answer: the page probe is a memoised
    /// pointer-table lookup, so asking about a block that has never held
    /// anything costs no I/O.
    pub fn container_at(&mut self, v: IVec3) -> Option<&soils_sim::Inventory> {
        let key = soils_sim::local_key(v.x, v.y, v.z);
        match self.block_data.get(chunk_of(v)).get(key) {
            Some(soils_sim::BlockData::Container(inv)) => Some(inv),
            None => None,
        }
    }

    /// A container's contents for the wire, padded out to the block's slot
    /// count. Read-only on purpose: opening a chest nobody has ever used must
    /// not create a page for it, and every chest starts that way.
    pub fn container_view(
        &mut self,
        v: IVec3,
        slots: usize,
    ) -> Vec<Option<soils_protocol::ItemStack>> {
        match self.container_at(v) {
            Some(inv) => inv.slots().to_vec(),
            None => vec![None; slots],
        }
    }

    /// The container at `v`, created with `slots` slots if this is the first
    /// thing anyone has put in it. Marks the page dirty, so callers that only
    /// want to *look* must use [`container_at`](Self::container_at).
    pub fn container_mut(&mut self, v: IVec3, slots: usize) -> &mut soils_sim::Inventory {
        let key = soils_sim::local_key(v.x, v.y, v.z);
        self.block_data.get_mut(chunk_of(v)).container_mut(key, slots)
    }

    /// Drop the data attached to `v` and return it — what breaking a block
    /// does. The caller owns what comes back; dropping it on the floor is how
    /// a chest's contents silently vanish.
    #[must_use = "the contents are lost unless they are spilled or re-stored"]
    pub fn take_block_data(&mut self, v: IVec3) -> Option<soils_sim::BlockData> {
        let key = soils_sim::local_key(v.x, v.y, v.z);
        let cpos = chunk_of(v);
        // Peek first: a break on an ordinary block must not dirty a page.
        if self.block_data.get(cpos).get(key).is_none() {
            return None;
        }
        let data = self.block_data.get_mut(cpos).remove(key);
        self.block_data.get_mut(cpos).prune();
        data
    }

    /// Tidy a page after a mutation and hold/release it against eviction.
    /// A page with an open viewer must stay resident: the alternative is a
    /// container that empties itself because its page left memory mid-session.
    pub fn prune_block_data(&mut self, cpos: IVec3) {
        self.block_data.get_mut(cpos).prune();
    }

    pub fn pin_block_data(&mut self, cpos: IVec3) {
        self.block_data.pin(cpos);
    }

    pub fn unpin_block_data(&mut self, cpos: IVec3) {
        self.block_data.unpin(cpos);
    }

    /// Cache counters, for the flush-interval log line and tests.
    pub fn block_data_stats(&self) -> crate::store::StoreStats {
        self.block_data.stats()
    }

    /// Block-data pages currently in memory.
    pub fn block_data_pages(&self) -> usize {
        self.block_data.len()
    }

    /// Is the boundary layer of the chunk at `pos` facing `dir` solid all the
    /// way across? `None` when the chunk is not resident, which callers must
    /// read as "not yet known", never as "no".
    pub fn face_solid(&self, pos: IVec3, dir: IVec3) -> Option<bool> {
        let entry = self.chunks.get(&pos)?;
        let bit = face_index(dir)?;
        Some(entry.faces & (1 << bit) != 0)
    }

    /// Whether every one of `pos`'s six neighbours presents a solid layer
    /// towards it, i.e. the chunk is sealed off from the rest of the world.
    ///
    /// `None` if any neighbour that `visible` accepts is not resident yet:
    /// the verdict is undecidable until it is, and guessing "not sealed" would
    /// leak chunks while guessing "sealed" would hide visible ones. `visible`
    /// answers whether a neighbour position is one the asking client would
    /// ever be sent; a neighbour outside that set counts as exposed, which is
    /// what terminates the question at the edge of a view radius.
    pub fn sealed(&self, pos: IVec3, visible: impl Fn(IVec3) -> bool) -> Option<bool> {
        for dir in FACE_DIRS {
            let n = pos + dir;
            if !visible(n) {
                return Some(false);
            }
            match self.face_solid(n, -dir) {
                None => return None,
                Some(false) => return Some(false),
                Some(true) => {}
            }
        }
        Some(true)
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
        // `stdb_key` borrows self immutably, so resolve keys before the
        // mutable iteration.
        let world_id = self.stdb_world_id;
        let dir = self.regions_dir.clone();
        for (pos, entry) in self.chunks.iter_mut() {
            if entry.dirty {
                entry.dirty = false;
                let key = world_id
                    .and_then(|id| soils_protocol::chunk_key::pack_chunk_key(id, pos.x, pos.y, pos.z));
                self.persist.enqueue(
                    dir.clone(),
                    *pos,
                    entry.volume.clone(),
                    entry.edited,
                    key,
                    entry.version,
                );
            }
        }
        // Block data rides the same writer and the same cadence: dirty in RAM
        // until a flush, never written on the tick thread.
        self.persist.enqueue_blobs(self.block_data.take_dirty());
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
                let key = self.stdb_key(pos);
                self.persist.enqueue(
                    self.regions_dir.clone(),
                    pos,
                    entry.volume,
                    entry.edited,
                    key,
                    entry.version,
                );
            }
            // The background writer will rewrite this chunk's region header;
            // the memoised copy is stale the moment the write lands.
            self.header_cache.remove(&region::region_path(&self.regions_dir, pos));
            self.navs.remove(&pos);
            // Block data outliving its voxels is a leak with extra steps —
            // unless something has it open, which `evict` respects.
            let writes = self.block_data.evict(pos);
            self.persist.enqueue_blobs(writes);
        }
        // Pages whose chunk is still resident but that nobody has touched in a
        // while: a chest read once during a walk past should not stay in RAM
        // for as long as the terrain around it.
        let writes = self.block_data.tick_lifecycle(ttl);
        self.persist.enqueue_blobs(writes);
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

    /// The lowest-corner voxel of a chunk.
    fn chunk_origin_of(cpos: IVec3) -> IVec3 {
        cpos * CHUNK_SIZE
    }

    fn generate_one(world: &World, pos: IVec3) -> ChunkVolume {
        let (terrain, registry) = world.gen_ctx();
        terrain.generate_batch(&[pos], &registry).into_iter().next().unwrap()
    }

    /// The seal test and the edit that breaks it — the invariant the whole
    /// occlusion cull rests on.
    ///
    /// A player can never trigger this by hand: `CULL_KEEP` keeps the chunks
    /// around them subscribed, and edit reach is a handful of voxels, so the
    /// nearest withheld chunk is always tens of voxels out of range. Scripts
    /// edit at arbitrary coordinates though, and so would a teleport or an
    /// explosion, which is why the exposure path exists and why it is pinned
    /// here rather than through a client.
    #[test]
    fn breaking_a_boundary_layer_unseals_the_chunk_behind_it() {
        let dir = std::env::temp_dir().join(format!("soils-world-seal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);

        // A solid target with solid neighbours all round. Deep enough that the
        // generator gives solid rock on every face.
        let target = IVec3::new(4, 2, 4);
        for dir_ in FACE_DIRS {
            let n = target + dir_;
            let vol = generate_one(&world, n);
            world.adopt(n, vol);
        }
        let vol = generate_one(&world, target);
        world.adopt(target, vol);

        let all_visible = |_: IVec3| true;
        assert_eq!(
            world.sealed(target, all_visible),
            Some(true),
            "a deep chunk with solid neighbours on every face must be sealed"
        );

        // Punch one voxel out of the neighbour above's *bottom* layer — the
        // layer that faces the target. That is the only thing between them.
        let above = target + IVec3::Y;
        let floor = chunk_origin_of(above);
        assert!(world.edit(floor.x, floor.y, floor.z, soils_protocol::AIR), "edit applies");

        assert_eq!(
            world.sealed(target, all_visible),
            Some(false),
            "one hole in the layer above must unseal it"
        );

        // A neighbour that is not resident is not evidence of anything: the
        // verdict has to be "unknown", never "sealed".
        let lonely = IVec3::new(40, 2, 40);
        assert_eq!(world.sealed(lonely, all_visible), None, "no neighbours, no verdict");

        persister.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Neighbours outside what the client would ever be sent count as open, so
    /// the outermost shell of a view radius resolves instead of waiting for a
    /// chunk nobody will generate.
    #[test]
    fn a_neighbour_outside_the_view_counts_as_exposed() {
        let dir = std::env::temp_dir().join(format!("soils-world-edge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);

        let target = IVec3::new(4, 2, 4);
        for dir_ in FACE_DIRS {
            let n = target + dir_;
            let vol = generate_one(&world, n);
            world.adopt(n, vol);
        }
        let vol = generate_one(&world, target);
        world.adopt(target, vol);

        // Pretend the chunk above is past the view radius.
        let above = target + IVec3::Y;
        assert_eq!(world.sealed(target, |n| n != above), Some(false));

        persister.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pristine_stays_off_disk_and_edits_persist() {
        let dir = std::env::temp_dir().join(format!("soils-world-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // Adopting a generated chunk caches it but must NOT persist it:
        // pristine chunks regenerate from the world identity on demand.
        let pos = IVec3::new(8, 7, 8);
        {
            let persister = Persister::new();
            let mut world = World::new(&dir, "default", 0, persister.handle(), None);
            assert!(!world.ensure_resident(pos), "fresh world: nothing on disk yet");
            world.adopt(pos, generate_one(&world, pos));
            let payload = world.serve(pos).expect("adopted chunk is resident");
            let vol = soils_protocol::decode_chunk(&payload).expect("payload decodes");
            assert!(!vol.is_empty(), "below-surface chunk should be non-empty");
            persister.shutdown(); // flush the writer
        }
        let regions = dir.join("worlds").join("default").join("regions");
        assert!(
            region::load(&regions, pos).ok().flatten().is_none(),
            "pristine chunk must not persist on adopt"
        );

        // An edit dirties it; the flush persists it (edited flag set) and a
        // fresh world then loads it from disk instead of regenerating.
        let edited_payload = {
            let persister = Persister::new();
            let mut world = World::new(&dir, "default", 0, persister.handle(), None);
            world.adopt(pos, generate_one(&world, pos));
            let v = pos * 32 + IVec3::new(1, 2, 3);
            assert!(world.edit(v.x, v.y, v.z, 3));
            world.flush_dirty();
            let payload = world.serve(pos).expect("resident");
            persister.shutdown();
            payload
        };
        let persister2 = Persister::new();
        let mut world2 = World::new(&dir, "default", 0, persister2.handle(), None);
        assert!(world2.ensure_resident(pos), "edited chunk should load from disk");
        assert_eq!(world2.serve(pos).unwrap(), edited_payload);
        assert_eq!(world2.chunk_edited(pos), Some(true));
        persister2.shutdown();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The chamber is carved on adoption, and the chunks it touches are
    /// marked *edited*.
    ///
    /// That flag is the whole test. A pristine manifest entry tells the client
    /// to regenerate the chunk locally from `GenParams`, which reproduces
    /// solid rock — so an unmarked carve is a room that exists on the server
    /// and nowhere else, and that the player cannot even place a block in
    /// (placement raycasts the client's CPU mirror, which only retains chunks
    /// the server sent as payloads).
    #[test]
    fn a_chamber_is_carved_on_adoption_and_marked_edited() {
        let dir = std::env::temp_dir().join(format!("soils-world-chamber-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let persister = Persister::new();
        let chamber = crate::Chamber::DEMO;
        let mut world = World::new(&dir, "default", 0, persister.handle(), Some(chamber));

        // Mid-room, and a voxel in the floor directly below it.
        let surface = world.terrain.surface_height(SPAWN_X, SPAWN_Z);
        let floor_y = surface - chamber.depth;
        let air = IVec3::new(SPAWN_X, floor_y + chamber.height / 2, SPAWN_Z);
        let cpos = soils_protocol::chunk_of(air);

        world.adopt(cpos, generate_one(&world, cpos));
        assert_eq!(
            world.chunk_edited(cpos),
            Some(true),
            "a carved chunk must ship as a payload, not as a position the client regenerates"
        );
        assert_eq!(world.voxel(air), 0, "the interior must be hollow");

        // The floor plane sits at the box minimum, one below the lowest air.
        let floor = IVec3::new(SPAWN_X, floor_y, SPAWN_Z);
        let fpos = soils_protocol::chunk_of(floor);
        if fpos != cpos {
            world.adopt(fpos, generate_one(&world, fpos));
        }
        assert_ne!(world.voxel(floor), 0, "the player must have something to stand on");

        // Far from spawn is ordinary terrain: the carve is a box test, not a
        // world-wide rewrite.
        let away = cpos + IVec3::new(6, 0, 6);
        world.adopt(away, generate_one(&world, away));
        assert_eq!(world.chunk_edited(away), Some(false), "untouched chunks stay pristine");

        persister.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adopt_never_clobbers_a_resident_chunk() {
        let dir = std::env::temp_dir().join(format!("soils-world-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let pos = IVec3::new(8, 7, 8);
        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);
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
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);

        // A surface chunk plus its vertical neighbors (the grid samples them).
        // The surface height at this column depends on the worldgen tuning, so
        // find the straddling chunk instead of hardcoding it.
        let cy = {
            let (terrain, registry) = world.gen_ctx();
            (0..16)
                .rev()
                .find(|&cy| !terrain.generate(IVec3::new(8, cy, 8), &registry).is_empty())
                .expect("some chunk in the column is solid")
        };
        let cpos = IVec3::new(8, cy, 8);
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
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);
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
        let mut fresh = World::new(&dir, "oracle", 0, persister2.handle(), None);
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
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);
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
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);
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
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);
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
        let mut world2 = World::new(&dir, "default", 0, persister2.handle(), None);
        assert!(world2.ensure_resident(pos), "evicted chunk reloads from disk");
        let vol = soils_protocol::decode_chunk(&world2.serve(pos).unwrap()).unwrap();
        assert_eq!(vol.get(0, 0, 0), 9, "edit survived eviction via save-if-dirty");

        persister2.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Block data rides the same cache policy as voxels: dirty in memory, out
    /// on eviction, back off disk on demand. The failure this guards is the
    /// interesting one — a chest that empties itself because its page left
    /// memory before its bytes did.
    #[test]
    fn block_data_survives_eviction_and_comes_back_off_disk() {
        let dir = std::env::temp_dir().join(format!("soils-world-blockdata-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let cpos = IVec3::new(8, 7, 8);
        let v = cpos * 32 + IVec3::new(3, 4, 5);
        let stack = soils_sim::ItemStack::new(soils_sim::ItemKind::Block(4), 17).unwrap();

        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);
        world.adopt(cpos, generate_one(&world, cpos));
        assert!(world.container_mut(v, 27).insert(stack).is_none());
        assert_eq!(world.block_data(cpos).len(), 1);

        // Eviction is what writes it out; nothing has flushed yet.
        world.tick_lifecycle(Duration::ZERO);
        assert_eq!(world.resident(), 0);
        drop(world);
        persister.shutdown();

        let persister2 = Persister::new();
        let mut world2 = World::new(&dir, "default", 0, persister2.handle(), None);
        let inv = world2.container_at(v).expect("the chest came back");
        assert_eq!(inv.count_of(soils_sim::ItemKind::Block(4)), 17);
        assert_eq!(world2.block_data_stats().loads, 1, "and it came from the file, not from nowhere");

        // A block with nothing in it costs no I/O: the pointer table says
        // absent, and that answer is memoised for the whole region.
        let before = world2.block_data_stats();
        for i in 0..8 {
            assert!(world2.container_at(cpos * 32 + IVec3::new(i, 0, 0)).is_none());
        }
        assert_eq!(world2.block_data_stats().loads, before.loads, "empty blocks must not inflate");

        // Emptying it clears the slot rather than storing a row of `None`s.
        assert_eq!(world2.container_mut(v, 27).remove(soils_sim::ItemKind::Block(4), 17), 17);
        world2.prune_block_data(cpos);
        world2.flush_dirty();
        drop(world2);
        persister2.shutdown();

        let persister3 = Persister::new();
        let mut world3 = World::new(&dir, "default", 0, persister3.handle(), None);
        assert!(world3.container_at(v).is_none(), "an emptied chest leaves nothing to reload");
        assert_eq!(world3.block_data_stats().loads, 0);

        persister3.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An open container pins its page: eviction must not pull the world out
    /// from under a player who is looking at it.
    #[test]
    fn a_pinned_page_outlives_its_chunk() {
        let dir = std::env::temp_dir().join(format!("soils-world-pinned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let cpos = IVec3::new(2, 2, 2);
        let v = cpos * 32 + IVec3::splat(1);
        let persister = Persister::new();
        let mut world = World::new(&dir, "default", 0, persister.handle(), None);
        world.adopt(cpos, generate_one(&world, cpos));
        let stack = soils_sim::ItemStack::new(soils_sim::ItemKind::Block(4), 3).unwrap();
        assert!(world.container_mut(v, 27).insert(stack).is_none());
        world.pin_block_data(cpos);

        world.tick_lifecycle(Duration::ZERO);
        assert_eq!(world.resident(), 0, "the chunk itself still evicts");
        assert_eq!(
            world.container_at(v).map(|i| i.count_of(soils_sim::ItemKind::Block(4))),
            Some(3),
            "but the pinned page is still in memory, unwritten and intact"
        );
        assert_eq!(world.block_data_stats().loads, 0, "it was never re-read");

        world.unpin_block_data(cpos);
        world.tick_lifecycle(Duration::ZERO);
        assert_eq!(world.block_data_stats().evictions, 1);

        persister.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
