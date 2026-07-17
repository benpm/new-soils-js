//! Client side of the baked L0 light grid (see `soils_sim::light` for the
//! algorithms). Chunks queue here when they stream in or get edited; a
//! budgeted system runs the incremental floods over the loaded ECS world,
//! then re-uploads the padded per-chunk light volumes the terrain material
//! samples.
//!
//! GPU note: the padded light buffers are CPU-recreated, and the chunk
//! material's bind group is cached — so after touching a buffer we also touch
//! its material (`materials.get_mut`) to force the bind group to rebuild.

use bevy::platform::collections::HashSet;
use bevy::prelude::*;
use bevy::render::storage::ShaderStorageBuffer;
use soils_protocol::{CHUNK_CLIP, CHUNK_SIZE, chunk_of, chunk_origin, local_of};
use soils_sim::light::{self, LightWorld};

use crate::chunk::{Blocks, ChunkMap, VoxelChunk, WorldTime};
use crate::gpu_mesh::{GpuChunk, LIGHT_BYTES, LIGHT_PAD};
use crate::material::{ChunkMeshMaterial, TERRAIN_BRIGHTNESS};

/// Chunks to (re)light this frame budget allows, and voxels whose block
/// changed. Fed by `server_msg::apply_chunks` / edit paths.
#[derive(Resource, Default)]
pub struct LightQueue {
    pub chunks: Vec<IVec3>,
    pub edits: Vec<IVec3>,
    /// Chunks whose light changed but whose padded GPU volume hasn't been
    /// re-uploaded yet. A set, so a chunk re-dirtied by wave after wave of
    /// neighbor floods uploads once per drain instead of once per touch —
    /// during a join burst the same chunk otherwise re-pads dozens of times.
    pending_pads: HashSet<IVec3>,
}

impl LightQueue {
    /// Outstanding work: (chunks to flood, voxel relights, pads to re-upload).
    /// Diagnostics only — `process_light` early-outs when all three are zero,
    /// so a non-zero reading at "steady state" means the pipeline is still
    /// draining and any fps sampled there is a backlog number, not steady.
    pub fn backlog(&self) -> (usize, usize, usize) {
        (self.chunks.len(), self.edits.len(), self.pending_pads.len())
    }

    /// Drop all queued work (warp: the whole world went away).
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.edits.clear();
        self.pending_pads.clear();
    }

    /// Drop work queued for one chunk (it left the subscription). Queued
    /// voxel-edit relights are safe to keep: they no-op on unloaded space.
    pub fn unload(&mut self, pos: IVec3) {
        self.chunks.retain(|c| *c != pos);
        self.pending_pads.remove(&pos);
    }
}

/// Backstop on chunks (re)lit per frame; [`FLOOD_MS`] is the real limiter.
/// High enough that a vsync-limited frame clock (e.g. a ~10 Hz USB display)
/// still floods a join burst in a few seconds.
const CHUNK_BUDGET: usize = 64;
/// Stop flooding once this much of the frame has been spent lighting.
const FLOOD_MS: f32 = 5.0;
/// Backstop on padded light volumes re-uploaded per frame; [`PAD_MS`] is the
/// real limiter. Each is a ~43 KB rebuild plus a bind-group invalidation, so
/// an unbounded drain during a burst is what used to stretch frames to ~125 ms.
const PAD_BUDGET: usize = 64;
/// Stop re-uploading pads once this much of the frame has been spent on them.
const PAD_MS: f32 = 4.0;

/// The current day-scaled skylight illuminance, mirrored into every chunk
/// material's `sky_term` when its quantized value changes.
#[derive(Resource)]
pub struct SkyTerm(pub f32);

impl Default for SkyTerm {
    fn default() -> Self {
        Self(TERRAIN_BRIGHTNESS)
    }
}

/// `soils_sim::light::LightWorld` over the loaded ECS chunks. Records which
/// chunks' light changed (including padded-buffer neighbors) in `dirty`.
struct EcsWorld<'a, 'w, 's> {
    map: &'a ChunkMap,
    chunks: &'a mut Query<'w, 's, &'static mut VoxelChunk>,
    levels: &'a [u8],
    dirty: HashSet<IVec3>,
}

impl EcsWorld<'_, '_, '_> {
    fn voxel(&self, v: IVec3) -> u8 {
        let Some(&e) = self.map.map.get(&chunk_of(v)) else { return 0 };
        let Ok(chunk) = self.chunks.get(e) else { return 0 };
        let l = local_of(v);
        chunk.volume.get(l.x, l.y, l.z)
    }
}

impl LightWorld for EcsWorld<'_, '_, '_> {
    fn solid(&self, v: IVec3) -> bool {
        self.voxel(v) != 0
    }

    fn emission(&self, v: IVec3) -> u8 {
        self.levels.get(self.voxel(v) as usize).copied().unwrap_or(0)
    }

    fn light(&self, v: IVec3) -> u8 {
        let Some(&e) = self.map.map.get(&chunk_of(v)) else { return 0 };
        let Ok(chunk) = self.chunks.get(e) else { return 0 };
        let l = local_of(v);
        chunk.light.get(l.x, l.y, l.z)
    }

    fn set_light(&mut self, v: IVec3, packed: u8) {
        let c = chunk_of(v);
        let Some(&e) = self.map.map.get(&c) else { return };
        let Ok(mut chunk) = self.chunks.get_mut(e) else { return };
        let l = local_of(v);
        chunk.light.set(l.x, l.y, l.z, packed);
        // Dirty this chunk, plus any neighbor whose padded volume sees `v`.
        self.dirty.insert(c);
        for i in 0..3 {
            let mut axis = IVec3::ZERO;
            axis[i] = 1;
            if l[i] == 0 {
                self.dirty.insert(c - axis);
            } else if l[i] == CHUNK_CLIP {
                self.dirty.insert(c + axis);
            }
        }
    }

    fn in_domain(&self, v: IVec3) -> bool {
        self.map.map.contains_key(&chunk_of(v))
    }

    fn open_sky_above(&self, _v: IVec3) -> bool {
        // Only consulted when the chunk above isn't loaded: assume open sky;
        // corrected by `reconcile_sky_below` when it loads.
        true
    }
}

/// Run queued lighting work, then refresh the GPU light volumes of every
/// chunk whose light changed.
#[allow(clippy::too_many_arguments)]
pub fn process_light(
    mut queue: ResMut<LightQueue>,
    map: Res<ChunkMap>,
    // `'static` data lifetime so `&mut Query` fits `EcsWorld`'s field (mutable
    // references are invariant over the query's data type).
    mut chunks: Query<&'static mut VoxelChunk>,
    gpu: Query<(&GpuChunk, &MeshMaterial3d<ChunkMeshMaterial>)>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut materials: ResMut<Assets<ChunkMeshMaterial>>,
    blocks: Res<Blocks>,
    mut levels: Local<Vec<u8>>,
) {
    if queue.chunks.is_empty() && queue.edits.is_empty() && queue.pending_pads.is_empty() {
        return;
    }
    if levels.is_empty() {
        *levels = blocks.0.light_table();
    }

    let mut world =
        EcsWorld { map: &map, chunks: &mut chunks, levels: &levels, dirty: HashSet::default() };

    // Light from the top of each column down: a chunk under a loaded-but-unlit
    // chunk gets no sky seed of its own, so processing the topmost first lets
    // its beam flood the whole loaded column in one propagation.
    queue.chunks.sort_by_key(|c| c.y);
    let t0 = std::time::Instant::now();
    for done in 0..queue.chunks.len().min(CHUNK_BUDGET) {
        if done > 0 && t0.elapsed().as_secs_f32() * 1000.0 > FLOOD_MS {
            break; // slow frame: keep at least one flood of progress, defer the rest
        }
        let cpos = queue.chunks.pop().expect("bounded by len");
        light::light_new_chunk(&mut world, cpos);
        light::reconcile_sky_below(&mut world, cpos);
    }
    for v in queue.edits.drain(..) {
        light::apply_voxel_change(&mut world, v);
    }

    // Coalesce dirt into the pending set, then upload a bounded, deduped batch.
    let dirty = world.dirty;
    queue.pending_pads.extend(dirty);
    let batch: Vec<IVec3> = queue.pending_pads.iter().copied().take(PAD_BUDGET).collect();
    let pads_t0 = std::time::Instant::now();
    let mut padded_count = 0;
    for cpos in batch {
        if padded_count > 0 && pads_t0.elapsed().as_secs_f32() * 1000.0 > PAD_MS {
            break; // deferred pads stay in the set for next frame
        }
        padded_count += 1;
        queue.pending_pads.remove(&cpos);
        let Some(&e) = map.map.get(&cpos) else { continue };
        let Ok((gc, mat)) = gpu.get(e) else { continue }; // empty chunks render nothing
        let padded = build_padded(&map, &chunks, cpos);
        if let Some(buf) = buffers.get_mut(&gc.light) {
            buf.data = Some(padded);
        }
        // Force the cached material bind group to pick up the new buffer.
        materials.get_mut(&mat.0);
    }
}

/// Build a chunk's padded light volume: its own 32³ plus one voxel of
/// neighbor light on every side, so border faces sample correctly.
fn build_padded(map: &ChunkMap, chunks: &Query<&'static mut VoxelChunk>, cpos: IVec3) -> Vec<u8> {
    let mut out = vec![0u8; LIGHT_BYTES];
    let idx = |x: i32, y: i32, z: i32| ((y + z * LIGHT_PAD) * LIGHT_PAD + x) as usize;

    // Interior: straight copy from this chunk's rows.
    if let Some(&e) = map.map.get(&cpos)
        && let Ok(chunk) = chunks.get(e)
    {
        let src = chunk.light.as_bytes();
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                let row = ((y + z * CHUNK_SIZE) * CHUNK_SIZE) as usize;
                let dst = idx(1, y + 1, z + 1);
                out[dst..dst + CHUNK_SIZE as usize]
                    .copy_from_slice(&src[row..row + CHUNK_SIZE as usize]);
            }
        }
    }

    // Shell: sample the six face-neighbor chunks (edges/corners of the pad are
    // left dark — no face ever reads them).
    let origin = chunk_origin(cpos);
    let mut fill = |v: IVec3, px: i32, py: i32, pz: i32| {
        let Some(&e) = map.map.get(&chunk_of(v)) else { return };
        let Ok(chunk) = chunks.get(e) else { return };
        let l = local_of(v);
        out[idx(px, py, pz)] = chunk.light.get(l.x, l.y, l.z);
    };
    let s = CHUNK_SIZE;
    for a in 0..s {
        for b in 0..s {
            fill(origin + IVec3::new(-1, a, b), 0, a + 1, b + 1);
            fill(origin + IVec3::new(s, a, b), LIGHT_PAD - 1, a + 1, b + 1);
            fill(origin + IVec3::new(a, -1, b), a + 1, 0, b + 1);
            fill(origin + IVec3::new(a, s, b), a + 1, LIGHT_PAD - 1, b + 1);
            fill(origin + IVec3::new(a, b, -1), a + 1, b + 1, 0);
            fill(origin + IVec3::new(a, b, s), a + 1, b + 1, LIGHT_PAD - 1);
        }
    }
    out
}

/// Keep every chunk material's `sky_term` in step with the day/night cycle.
/// Quantized so materials (and their bind groups) are only touched a handful
/// of times per day cycle, not per frame.
pub fn update_sky_term(
    world_time: Res<WorldTime>,
    mut sky: ResMut<SkyTerm>,
    mut materials: ResMut<Assets<ChunkMeshMaterial>>,
    mut last_q: Local<Option<f32>>,
) {
    let day = soils_sim::ease10(world_time.daytime * 2.0 - 1.0);
    // Floor keeps night surfaces moonlit-visible (exposure dims them further).
    let q = ((0.05 + 0.95 * day) * 64.0).round() / 64.0;
    if *last_q == Some(q) {
        return;
    }
    *last_q = Some(q);
    sky.0 = TERRAIN_BRIGHTNESS * q;
    for (_, m) in materials.iter_mut() {
        m.params.sky_term = sky.0;
    }
}
