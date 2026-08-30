//! CPU-side slot allocation for the pooled GPU chunk cache (stream-pipeline
//! redesign). Two slot spaces: every resident chunk (air included — sky light
//! crosses air chunks) owns a *unified* slot for its light volume + descriptor;
//! non-air chunks additionally own a *mesh* slot for voxels/quads/indirect
//! args. The GPU never allocates — systems allocate here and emit buffer
//! writes, so residency policy stays in one place.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::render::render_resource::{Buffer, BufferDescriptor, BufferUsages};
use bevy::render::renderer::{RenderDevice, RenderQueue};
use bevy::render::settings::WgpuFeatures;
use bevy::render::{ExtractSchedule, MainWorld, Render, RenderApp, RenderStartup, RenderSystems};
use soils_protocol::{CHUNK_CUBED, ChunkVolume};

/// Unified slots (light + descriptor). Covers a full r8 window (17³ = 4913)
/// with headroom for hysteresis and in-flight loads.
///
/// `N_SLOTS × SLOT_BYTES` is the light pool, and it has to fit the adapter's
/// `max_storage_buffer_binding_size` — 192 MiB at the default, which a real
/// GPU has and Mesa lavapipe (128 MiB) does not. See `small_pools`.
#[cfg(not(feature = "small_pools"))]
pub const N_SLOTS: u32 = 6144;
/// Mesh slots (voxels + quads + indirect). Only non-air chunks need one
/// (~2.5k at r8). Slot 0 is a permanently-zero sentinel so air chunks' voxel
/// reads resolve to air without branching; it is never handed out.
#[cfg(not(feature = "small_pools"))]
pub const N_MESH: u32 = 4096;

/// Pool sizes for a software rasteriser: light 96 MiB, voxels 64 MiB, quads
/// 64 MiB, all inside lavapipe's 128 MiB storage-binding limit.
///
/// CI renders headlessly under lavapipe, where the default pools trip the
/// assert in `probe_gpu_caps` and the client dies before it can draw anything.
/// A feature rather than a runtime clamp because these are `const` and feed
/// buffer sizes, free-lists and slot arithmetic all over this module; sizing
/// them dynamically is a real change to the renderer, and this is a build
/// profile. Tracked in `Tasks.md` as the thing to do properly.
///
/// The ratio is preserved (`N_SLOTS > N_MESH`): light slots cover padded
/// neighbours, so there must be more of them than mesh slots. Nothing needs
/// to change in the shaders — `N_SLOTS` appears there only in comments, and
/// `cull_demand.wgsl`'s hardcoded `N_MESH = 4096u` is an upper bound check,
/// so a smaller Rust-side pool merely never reaches it.
///
/// Capacity is the cost: this covers a radius-4 window, not radius-8. The
/// recordings set `SOILS_RADIUS=2`, so it is not the binding constraint there.
#[cfg(feature = "small_pools")]
pub const N_SLOTS: u32 = 3072;
#[cfg(feature = "small_pools")]
pub const N_MESH: u32 = 2048;
/// `Slot::mesh` value for air chunks (no mesh slot).
pub const NO_MESH: u32 = u32::MAX;
/// Slot-table axis size: chunk coord masked to `& 31` per axis. Collision-free
/// while the active window is < 32 chunks per axis (radius ≤ 8 → 17), but
/// entries left behind by movement go stale, so every lookup — GPU and CPU —
/// validates the resolved slot's chunk position.
pub const TABLE_DIM: i32 = 32;
/// Empty slot-table entry.
pub const TABLE_EMPTY: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    pub slot: u32,
    /// Mesh-space index, or [`NO_MESH`] for air chunks.
    pub mesh: u32,
}

/// Index into the wrap-window slot table for a chunk position.
pub fn table_index(c: IVec3) -> usize {
    let m = TABLE_DIM - 1;
    ((c.x & m) + (c.y & m) * TABLE_DIM + (c.z & m) * TABLE_DIM * TABLE_DIM) as usize
}

#[derive(Resource)]
pub struct ChunkSlots {
    map: HashMap<IVec3, Slot>,
    free: Vec<u32>,
    free_mesh: Vec<u32>,
    /// CPU shadow of the GPU slot table, so unmapping only vacates cells this
    /// chunk still owns (a 32-apart chunk may have overwritten the cell).
    table: Vec<u32>,
}

impl Default for ChunkSlots {
    fn default() -> Self {
        Self {
            map: HashMap::default(),
            // Popped from the back; low indices hand out first for stable tests.
            free: (0..N_SLOTS).rev().collect(),
            // Mesh slot 0 is the air sentinel — never in the free list.
            free_mesh: (1..N_MESH).rev().collect(),
            table: vec![TABLE_EMPTY; (TABLE_DIM * TABLE_DIM * TABLE_DIM) as usize],
        }
    }
}

impl ChunkSlots {
    /// Allocate a slot for `pos`. Idempotent: an already-resident chunk gets
    /// its existing slot back (upgraded with a mesh slot if newly needed).
    /// `None` = pool exhausted; the caller defers the chunk and retries (the
    /// demand queue re-issues records every frame).
    pub fn alloc(&mut self, pos: IVec3, needs_mesh: bool) -> Option<Slot> {
        if self.map.contains_key(&pos) {
            return if needs_mesh { self.ensure_mesh(pos).map(|_| self.map[&pos]) } else { Some(self.map[&pos]) };
        }
        let slot = self.free.pop()?;
        let mesh = if needs_mesh {
            match self.free_mesh.pop() {
                Some(m) => m,
                None => {
                    self.free.push(slot);
                    return None;
                }
            }
        } else {
            NO_MESH
        };
        let s = Slot { slot, mesh };
        self.map.insert(pos, s);
        Some(s)
    }

    /// Give a resident air chunk a mesh slot (first non-air edit). `None` =
    /// not resident or mesh pool exhausted.
    pub fn ensure_mesh(&mut self, pos: IVec3) -> Option<u32> {
        let s = self.map.get_mut(&pos)?;
        if s.mesh != NO_MESH {
            return Some(s.mesh);
        }
        s.mesh = self.free_mesh.pop()?;
        Some(s.mesh)
    }

    /// Release `pos`'s slots (unload/eviction). Returns what was freed so the
    /// caller can invalidate the descriptor + slot table on the GPU.
    pub fn free(&mut self, pos: IVec3) -> Option<Slot> {
        let s = self.map.remove(&pos)?;
        self.free.push(s.slot);
        if s.mesh != NO_MESH {
            self.free_mesh.push(s.mesh);
        }
        Some(s)
    }

    pub fn get(&self, pos: IVec3) -> Option<Slot> {
        self.map.get(&pos).copied()
    }

    #[allow(dead_code)] // hud/cull consumers arrive with later phases
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (&IVec3, &Slot)> {
        self.map.iter()
    }

    /// Map a chunk into the GPU caches: allocate slots, upload voxels (non-air),
    /// point the slot table at it, and queue a remesh. Idempotent for
    /// re-uploads (server resend / gen refresh). Returns the slot, or `None`
    /// on pool exhaustion (caller retries later).
    pub fn map_chunk(
        &mut self,
        ops: &mut PoolOpQueue,
        dirty: &mut DirtyMesh,
        cpos: IVec3,
        volume: &ChunkVolume,
    ) -> Option<Slot> {
        let non_air = !volume.is_empty();
        let s = self.alloc(cpos, non_air)?;
        if s.mesh != NO_MESH {
            ops.push(PoolOp::UploadVoxels { mesh: s.mesh, volume: volume.clone() });
            ops.push(PoolOp::WriteMeshInfo { mesh: s.mesh, cpos, slot: s.slot });
            dirty.0.push(s.mesh);
        }
        ops.push(PoolOp::WriteDesc { slot: s.slot, cpos, mesh: s.mesh });
        let idx = table_index(cpos) as u32;
        self.table[idx as usize] = s.slot;
        ops.push(PoolOp::WriteTable { index: idx, slot: s.slot });
        Some(s)
    }

    /// Map a chunk whose voxels the GPU generates in place: allocate slots
    /// (mesh included — occupancy is unknown until the gen readback), point
    /// the table/descriptors at it, and queue a remesh. No voxel upload.
    pub fn map_chunk_gen(
        &mut self,
        ops: &mut PoolOpQueue,
        dirty: &mut DirtyMesh,
        cpos: IVec3,
    ) -> Option<Slot> {
        let s = self.alloc(cpos, true)?;
        ops.push(PoolOp::WriteMeshInfo { mesh: s.mesh, cpos, slot: s.slot });
        dirty.0.push(s.mesh);
        ops.push(PoolOp::WriteDesc { slot: s.slot, cpos, mesh: s.mesh });
        let idx = table_index(cpos) as u32;
        self.table[idx as usize] = s.slot;
        ops.push(PoolOp::WriteTable { index: idx, slot: s.slot });
        Some(s)
    }

    /// Downgrade a resident chunk to an air slot (a GPU-genned chunk whose
    /// occupancy readback came back all-air): free the mesh slot, stop its
    /// draws. The light slot and descriptor stay (air still carries light).
    pub fn demote_mesh(&mut self, ops: &mut PoolOpQueue, dirty: &mut DirtyMesh, cpos: IVec3) {
        let Some(s) = self.map.get_mut(&cpos) else { return };
        if s.mesh == NO_MESH {
            return;
        }
        let mesh = s.mesh;
        s.mesh = NO_MESH;
        let slot = s.slot;
        self.free_mesh.push(mesh);
        ops.push(PoolOp::ClearIndirect { mesh });
        retire_mesh(ops, dirty, mesh);
        ops.push(PoolOp::WriteDesc { slot, cpos, mesh: NO_MESH });
    }

    /// Unmap a chunk (unload): free its slots, vacate its table cell (only if
    /// still owned), stop its draws, and poison its descriptor so stale table
    /// cells elsewhere fail validation.
    pub fn unmap_chunk(&mut self, ops: &mut PoolOpQueue, dirty: &mut DirtyMesh, cpos: IVec3) {
        let Some(s) = self.free(cpos) else { return };
        let idx = table_index(cpos);
        if self.table[idx] == s.slot {
            self.table[idx] = TABLE_EMPTY;
            ops.push(PoolOp::WriteTable { index: idx as u32, slot: TABLE_EMPTY });
        }
        if s.mesh != NO_MESH {
            ops.push(PoolOp::ClearIndirect { mesh: s.mesh });
            retire_mesh(ops, dirty, s.mesh);
        }
        ops.push(PoolOp::WriteDesc { slot: s.slot, cpos: IVec3::MAX, mesh: NO_MESH });
    }

    /// Drop everything (warp: the whole world went away), vacating the GPU
    /// side too.
    pub fn clear_all(&mut self, ops: &mut PoolOpQueue, dirty: &mut DirtyMesh) {
        let positions: Vec<IVec3> = self.map.keys().copied().collect();
        for cpos in positions {
            self.unmap_chunk(ops, dirty, cpos);
        }
        dirty.0.clear();
        *self = Self::default();
    }

    /// Drop everything (tests only — no GPU side to notify).
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Quads per mesh slot in the pooled quad buffer. Realistic terrain chunks
/// greedy-merge to a few hundred quads; only adversarial builds clamp (the
/// same contract the old 8192 cap had, at 8-byte packed quads instead of 80).
pub const QUADS_PER_SLOT: u32 = 4096;
/// Bytes per packed quad (two u32 words — see `voxel_mesh.wgsl` PackedQuad).
pub const QUAD_BYTES: u64 = 8;
/// Bytes per chunk descriptor (`ChunkSlot` in WGSL).
pub const DESC_BYTES: u64 = 32;
/// Bytes of one voxel/light slot (32³ bytes).
pub const SLOT_BYTES: u64 = CHUNK_CUBED as u64;

/// The pooled GPU buffers, created once at render startup — bind groups over
/// them are static, so uploads never invalidate anything.
#[derive(Resource)]
pub struct ChunkPools {
    /// `N_MESH` × 32 KB voxel volumes (u8 material ids packed in u32 words).
    pub voxels: Buffer,
    /// `N_SLOTS` × 32 KB packed light volumes (unpadded — neighbor sampling
    /// goes through the slot table).
    pub light: Buffer,
    /// `N_MESH` × `QUADS_PER_SLOT` packed quads.
    pub quads: Buffer,
    /// `N_MESH` × `DrawIndirectArgs`. Zero-initialized: unallocated slots draw
    /// nothing; the mesher's finalize writes real args per slot.
    pub indirect: Buffer,
    /// `N_SLOTS` × `ChunkSlot` descriptors.
    pub desc: Buffer,
    /// 32³ wrap-window chunk→slot map (`TABLE_EMPTY` = vacant).
    pub table: Buffer,
    /// `N_MESH` × `vec4<i32>` (cpos.xyz, unified light slot): the vertex
    /// shader's per-mesh-slot chunk origin + the fragment's own-light slot.
    pub mesh_info: Buffer,
}

impl ChunkPools {
    fn create(device: &RenderDevice) -> Self {
        let buf = |label: &str, size: u64, usage: BufferUsages| {
            // mapped_at_creation marks the whole buffer initialized (zeroed),
            // so wgpu's lazy zero-init can never race — and clobber — staged
            // write_buffer uploads on first storage use (observed on Vulkan:
            // a nondeterministic subset of first-frame desc/light writes were
            // zeroed after application).
            let b = device.create_buffer(&BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: true,
            });
            b.unmap();
            b
        };
        // COPY_SRC included for diagnostics/readback paths.
        let sc = BufferUsages::STORAGE | BufferUsages::COPY_DST | BufferUsages::COPY_SRC;
        Self {
            voxels: buf("chunk_voxel_pool", N_MESH as u64 * SLOT_BYTES, sc),
            light: buf("chunk_light_pool", N_SLOTS as u64 * SLOT_BYTES, sc),
            quads: buf("chunk_quad_pool", N_MESH as u64 * QUADS_PER_SLOT as u64 * QUAD_BYTES, sc),
            indirect: buf(
                "chunk_indirect_args",
                N_MESH as u64 * 16,
                sc | BufferUsages::INDIRECT,
            ),
            desc: buf("chunk_desc", N_SLOTS as u64 * DESC_BYTES, sc),
            table: buf("chunk_slot_table", (TABLE_DIM as u64).pow(3) * 4, sc),
            mesh_info: buf("chunk_mesh_info", N_MESH as u64 * 16, sc),
        }
    }
}

/// A write into the pooled buffers, queued by main-world systems and applied
/// in the render world via `RenderQueue::write_buffer`. Slots are CPU-owned,
/// so ops never race GPU-side allocation.
pub enum PoolOp {
    /// Full 32 KB voxel upload into a mesh slot.
    UploadVoxels { mesh: u32, volume: ChunkVolume },
    /// Single-voxel edit: rewrite the containing u32 word.
    WriteVoxelWord { mesh: u32, word_idx: u32, word: u32 },
    /// Full 32 KB light upload into a unified slot.
    /// (Re)write a slot's descriptor.
    WriteDesc { slot: u32, cpos: IVec3, mesh: u32 },
    /// Point a slot-table cell at a slot (or `TABLE_EMPTY`).
    WriteTable { index: u32, slot: u32 },
    /// (Re)write a mesh slot's info row (chunk pos + owning light slot).
    WriteMeshInfo { mesh: u32, cpos: IVec3, slot: u32 },
    /// Zero a freed mesh slot's draw args so it stops drawing.
    ClearIndirect { mesh: u32 },
}

/// The three main-world resources a pool mutation needs, bundled because Bevy
/// systems cap out at 16 params (same reason as `demand::MaterializeParams`)
/// and `apply_warp` was already sitting on the limit.
#[derive(bevy::ecs::system::SystemParam)]
pub struct PoolWrite<'w> {
    pub slots: ResMut<'w, ChunkSlots>,
    pub ops: ResMut<'w, PoolOpQueue>,
    pub dirty: ResMut<'w, DirtyMesh>,
}

/// Main-world queue of pool writes, drained into the render world each frame.
#[derive(Resource, Default)]
pub struct PoolOpQueue(pub Vec<PoolOp>);

impl PoolOpQueue {
    pub fn push(&mut self, op: PoolOp) {
        self.0.push(op);
    }
}

/// Mesh slots whose voxels changed this frame → remesh dispatch list.
#[derive(Resource, Default)]
pub struct DirtyMesh(pub Vec<u32>);

/// Retire a freed mesh slot: poison its info row and drop any remesh still
/// queued against it.
///
/// The poison is what `cull` in `cull_demand.wgsl` tests to skip freed slots —
/// without it that branch is dead and the slot keeps drawing the old chunk.
/// Dropping the dirty entry closes the matching race: `finalize_mesh` runs
/// before `cull` and would otherwise republish real draw args from the freed
/// slot's stale voxels. Scrubbing here rather than at extract time is
/// deliberate — a mesh id freed and handed straight back out by `alloc` in the
/// same frame has a *legitimate* new dirty entry, and a blanket "freed this
/// frame" filter would drop it and leave the new chunk invisible.
fn retire_mesh(ops: &mut PoolOpQueue, dirty: &mut DirtyMesh, mesh: u32) {
    ops.push(PoolOp::WriteMeshInfo { mesh, cpos: IVec3::MAX, slot: TABLE_EMPTY });
    dirty.0.retain(|m| *m != mesh);
}

fn extract_pool_ops(mut main: ResMut<MainWorld>, mut ops: ResMut<ExtractedPoolOps>) {
    if let Some(mut q) = main.get_resource_mut::<PoolOpQueue>() {
        ops.0.append(&mut q.0);
    }
    if let Some(mut d) = main.get_resource_mut::<DirtyMesh>() {
        ops.1.append(&mut d.0);
    }
}

#[derive(Resource, Default)]
pub struct ExtractedPoolOps(pub Vec<PoolOp>, pub Vec<u32>);

fn apply_pool_ops(
    mut ops: ResMut<ExtractedPoolOps>,
    pools: Res<ChunkPools>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
) {
    if ops.0.is_empty() {
        return;
    }
    // Uploads go through an explicit staging buffer + copy encoder submitted
    // immediately: `queue.write_buffer`'s staged copies proved lossy against
    // the pooled buffers on Vulkan (a nondeterministic subset of first-write
    // regions stayed zero); an explicit submission is unambiguous.
    let mut staging: Vec<u8> = Vec::new();
    let mut copies: Vec<(u64, u64, u64)> = Vec::new(); // (src off, dst off, len) into which pool
    enum Dst {
        Voxels,
        Desc,
        Table,
        MeshInfo,
        Indirect,
    }
    let mut dsts: Vec<Dst> = Vec::new();
    let stage = |bytes: &[u8], dst: Dst, dst_off: u64,
                     staging: &mut Vec<u8>,
                     copies: &mut Vec<(u64, u64, u64)>,
                     dsts: &mut Vec<Dst>| {
        let src = staging.len() as u64;
        staging.extend_from_slice(bytes);
        copies.push((src, dst_off, bytes.len() as u64));
        dsts.push(dst);
    };
    for op in ops.0.drain(..) {
        match op {
            PoolOp::UploadVoxels { mesh, volume } => {
                stage(volume.as_bytes(), Dst::Voxels, mesh as u64 * SLOT_BYTES, &mut staging, &mut copies, &mut dsts);
            }
            PoolOp::WriteVoxelWord { mesh, word_idx, word } => {
                stage(
                    &word.to_le_bytes(),
                    Dst::Voxels,
                    mesh as u64 * SLOT_BYTES + word_idx as u64 * 4,
                    &mut staging, &mut copies, &mut dsts,
                );
            }
            PoolOp::WriteDesc { slot, cpos, mesh } => {
                // Matches the WGSL ChunkSlot layout: cpos, mesh_slot, flags,
                // flags_gpu, quad_count, pad. CPU writes zero the GPU words —
                // a (re)assigned slot starts clean.
                let mut d = [0u8; DESC_BYTES as usize];
                d[0..4].copy_from_slice(&cpos.x.to_le_bytes());
                d[4..8].copy_from_slice(&cpos.y.to_le_bytes());
                d[8..12].copy_from_slice(&cpos.z.to_le_bytes());
                d[12..16].copy_from_slice(&mesh.to_le_bytes());
                stage(&d, Dst::Desc, slot as u64 * DESC_BYTES, &mut staging, &mut copies, &mut dsts);
            }
            PoolOp::WriteTable { index, slot } => {
                stage(&slot.to_le_bytes(), Dst::Table, index as u64 * 4, &mut staging, &mut copies, &mut dsts);
            }
            PoolOp::WriteMeshInfo { mesh, cpos, slot } => {
                let mut d = [0u8; 16];
                d[0..4].copy_from_slice(&cpos.x.to_le_bytes());
                d[4..8].copy_from_slice(&cpos.y.to_le_bytes());
                d[8..12].copy_from_slice(&cpos.z.to_le_bytes());
                d[12..16].copy_from_slice(&slot.to_le_bytes());
                stage(&d, Dst::MeshInfo, mesh as u64 * 16, &mut staging, &mut copies, &mut dsts);
            }
            PoolOp::ClearIndirect { mesh } => {
                stage(&[0u8; 16], Dst::Indirect, mesh as u64 * 16, &mut staging, &mut copies, &mut dsts);
            }
        }
    }
    let staging_buf = device.create_buffer_with_data(
        &bevy::render::render_resource::BufferInitDescriptor {
            label: Some("pool_op_staging"),
            contents: &staging,
            usage: BufferUsages::COPY_SRC,
        },
    );
    let mut encoder = device.create_command_encoder(
        &bevy::render::render_resource::CommandEncoderDescriptor { label: Some("pool_ops") },
    );
    for ((src, dst_off, len), dst) in copies.into_iter().zip(dsts) {
        let target = match dst {
            Dst::Voxels => &pools.voxels,
            Dst::Desc => &pools.desc,
            Dst::Table => &pools.table,
            Dst::MeshInfo => &pools.mesh_info,
            Dst::Indirect => &pools.indirect,
        };
        encoder.copy_buffer_to_buffer(&staging_buf, src, target, dst_off, len);
    }
    queue.submit([encoder.finish()]);
}

/// What the adapter actually gave us, probed once at render startup — the
/// pools above need `max_storage_buffer_binding_size` ≥ 192 MiB (the light
/// pool). Bevy's default `WgpuSettingsPriority::Functionality` already
/// requests the full adapter feature/limit set, so nothing needs forcing at
/// device creation. `multi_draw_indirect` itself is always callable in wgpu 27
/// (emulated as a `draw_indirect` loop without native support);
/// `native_multi_draw` records whether `MULTI_DRAW_INDIRECT_COUNT` says it is
/// a real single submission.
#[derive(Resource, Clone, Copy, Debug)]
pub struct GpuCaps {
    pub native_multi_draw: bool,
    pub max_storage_binding: u64,
    pub max_buffer_size: u64,
}

fn probe_gpu_caps(device: Res<RenderDevice>, mut commands: Commands) {
    let features = device.features();
    let limits = device.limits();
    let caps = GpuCaps {
        native_multi_draw: features.contains(WgpuFeatures::MULTI_DRAW_INDIRECT_COUNT),
        max_storage_binding: limits.max_storage_buffer_binding_size as u64,
        max_buffer_size: limits.max_buffer_size,
    };
    info!(
        "gpu caps: native_multi_draw={} max_storage_binding={}MiB max_buffer={}MiB",
        caps.native_multi_draw,
        caps.max_storage_binding >> 20,
        caps.max_buffer_size >> 20
    );
    let need = N_SLOTS as u64 * SLOT_BYTES;
    assert!(
        caps.max_storage_binding >= need,
        "adapter's max storage binding ({} MiB) can't hold the {} MiB light pool; \
         lower N_SLOTS/N_MESH for this device",
        caps.max_storage_binding >> 20,
        need >> 20
    );
    commands.insert_resource(ChunkPools::create(&device));
    commands.insert_resource(caps);
}

/// Mirrors [`GpuCaps`] into the main world so pool sizing (main-world systems)
/// can read it.
fn mirror_caps(caps: Option<Res<GpuCaps>>, mut main: ResMut<MainWorld>) {
    if let Some(caps) = caps
        && !main.contains_resource::<GpuCaps>()
    {
        main.insert_resource(*caps);
    }
}

pub struct PoolPlugin;

impl Plugin for PoolPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChunkSlots>()
            .init_resource::<PoolOpQueue>()
            .init_resource::<DirtyMesh>();
        let render_app = app.sub_app_mut(RenderApp);
        render_app.init_resource::<ExtractedPoolOps>();
        render_app.add_systems(RenderStartup, probe_gpu_caps);
        render_app.add_systems(ExtractSchedule, (mirror_caps, extract_pool_ops));
        render_app.add_systems(
            Render,
            apply_pool_ops.in_set(RenderSystems::PrepareResources),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_free_roundtrip() {
        let mut s = ChunkSlots::default();
        let a = s.alloc(IVec3::new(1, 2, 3), true).unwrap();
        assert_ne!(a.mesh, NO_MESH);
        assert_ne!(a.mesh, 0, "mesh sentinel must never be handed out");
        let b = s.alloc(IVec3::new(4, 5, 6), false).unwrap();
        assert_eq!(b.mesh, NO_MESH);
        assert_ne!(a.slot, b.slot);
        assert_eq!(s.len(), 2);
        assert_eq!(s.free(IVec3::new(1, 2, 3)), Some(a));
        assert_eq!(s.free(IVec3::new(1, 2, 3)), None);
        assert_eq!(s.len(), 1);
        // Freed indices come back around.
        let c = s.alloc(IVec3::new(7, 8, 9), true).unwrap();
        assert_eq!((c.slot, c.mesh), (a.slot, a.mesh));
    }

    #[test]
    fn alloc_is_idempotent() {
        let mut s = ChunkSlots::default();
        let a = s.alloc(IVec3::ZERO, false).unwrap();
        assert_eq!(s.alloc(IVec3::ZERO, false), Some(a));
        // Upgrading with needs_mesh attaches a mesh slot, same unified slot.
        let b = s.alloc(IVec3::ZERO, true).unwrap();
        assert_eq!(b.slot, a.slot);
        assert_ne!(b.mesh, NO_MESH);
        assert_eq!(s.alloc(IVec3::ZERO, true), Some(b));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn ensure_mesh_upgrades_air() {
        let mut s = ChunkSlots::default();
        s.alloc(IVec3::ONE, false).unwrap();
        let m = s.ensure_mesh(IVec3::ONE).unwrap();
        assert_ne!(m, NO_MESH);
        assert_eq!(s.get(IVec3::ONE).unwrap().mesh, m);
        assert_eq!(s.ensure_mesh(IVec3::ONE), Some(m));
        assert_eq!(s.ensure_mesh(IVec3::splat(9)), None);
    }

    #[test]
    fn exhaustion_returns_none_and_leaks_nothing() {
        let mut s = ChunkSlots::default();
        // Drain the mesh pool (N_MESH - 1 real slots).
        for i in 0..(N_MESH - 1) as i32 {
            assert!(s.alloc(IVec3::new(i, 0, 0), true).is_some(), "i={i}");
        }
        let before = s.len();
        // Mesh pool empty: mesh alloc fails and must return the unified slot.
        assert_eq!(s.alloc(IVec3::new(-1, -1, -1), true), None);
        assert_eq!(s.len(), before);
        // Air chunks still allocate until the unified pool runs dry.
        for i in 0..(N_SLOTS - N_MESH + 1) as i32 {
            assert!(s.alloc(IVec3::new(i, 1, 0), false).is_some(), "i={i}");
        }
        assert_eq!(s.alloc(IVec3::new(-2, -2, -2), false), None);
    }

    #[test]
    fn table_index_wraps() {
        assert_eq!(table_index(IVec3::ZERO), 0);
        assert_eq!(table_index(IVec3::new(32, 0, 0)), 0);
        assert_eq!(table_index(IVec3::new(-1, 0, 0)), 31);
        assert_eq!(table_index(IVec3::new(1, 1, 1)), (1 + 32 + 1024) as usize);
        // Distinct within a 17-chunk window.
        assert_ne!(table_index(IVec3::new(8, 0, 0)), table_index(IVec3::new(-8, 0, 0)));
    }

    #[test]
    fn clear_resets() {
        let mut s = ChunkSlots::default();
        s.alloc(IVec3::ONE, true).unwrap();
        s.clear();
        assert_eq!(s.len(), 0);
        assert_eq!(s.get(IVec3::ONE), None);
    }

    /// Poisoning the info row is what makes the `cull` shader's freed-slot
    /// branch reachable at all; scrubbing `DirtyMesh` stops `finalize_mesh`
    /// from republishing draw args for the chunk that just went away.
    #[test]
    fn unmap_retires_the_mesh_slot() {
        let mut s = ChunkSlots::default();
        let cpos = IVec3::new(1, 2, 3);
        let a = s.alloc(cpos, true).unwrap();
        let (mut ops, mut dirty) = (PoolOpQueue::default(), DirtyMesh(vec![a.mesh, 4242]));
        s.unmap_chunk(&mut ops, &mut dirty, cpos);
        assert!(ops.0.iter().any(|op| matches!(
            op,
            PoolOp::WriteMeshInfo { mesh, slot, .. } if *mesh == a.mesh && *slot == TABLE_EMPTY
        )));
        assert_eq!(dirty.0, vec![4242], "only the freed slot's remesh is dropped");
    }

    #[test]
    fn demote_retires_the_mesh_slot() {
        let mut s = ChunkSlots::default();
        let cpos = IVec3::new(4, 5, 6);
        let a = s.alloc(cpos, true).unwrap();
        let (mut ops, mut dirty) = (PoolOpQueue::default(), DirtyMesh(vec![a.mesh]));
        s.demote_mesh(&mut ops, &mut dirty, cpos);
        assert_eq!(s.get(cpos).unwrap().mesh, NO_MESH, "chunk stays resident as air");
        assert!(ops.0.iter().any(|op| matches!(
            op,
            PoolOp::WriteMeshInfo { mesh, slot, .. } if *mesh == a.mesh && *slot == TABLE_EMPTY
        )));
        assert!(dirty.0.is_empty());
    }

    /// Why the scrub lives at free time and not at extract time: `free_mesh` is
    /// a LIFO, so a mesh id can be freed and handed straight back out in the
    /// same frame. The new owner's remesh is legitimate — a blanket "freed this
    /// frame" filter would drop it and leave the new chunk invisible.
    #[test]
    fn same_frame_realloc_keeps_its_remesh() {
        let mut s = ChunkSlots::default();
        let old = IVec3::new(1, 0, 0);
        let a = s.alloc(old, true).unwrap();
        let (mut ops, mut dirty) = (PoolOpQueue::default(), DirtyMesh(vec![a.mesh]));
        s.unmap_chunk(&mut ops, &mut dirty, old);
        assert!(dirty.0.is_empty());
        let b = s.alloc(IVec3::new(9, 9, 9), true).unwrap();
        assert_eq!(b.mesh, a.mesh, "same mesh id comes straight back around");
        dirty.0.push(b.mesh);
        assert_eq!(dirty.0, vec![b.mesh], "the new owner's remesh survives");
    }

    #[test]
    fn clear_all_drops_every_queued_remesh() {
        let mut s = ChunkSlots::default();
        let a = s.alloc(IVec3::new(1, 1, 1), true).unwrap();
        let (mut ops, mut dirty) = (PoolOpQueue::default(), DirtyMesh(vec![a.mesh, 7]));
        s.clear_all(&mut ops, &mut dirty);
        assert_eq!(s.len(), 0);
        assert!(dirty.0.is_empty(), "the whole world went away");
    }
}
