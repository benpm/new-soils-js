//! GPU radiance-cascades global illumination.
//!
//! A world-space cube of voxel occupancy around the player is kept in a GPU
//! buffer; a compute shader (`radiance.wgsl`) traces a hierarchy of probe
//! cascades against it and merges them top-down into a single incoming-radiance
//! field (cascade 0). The chunk material samples that field to light the
//! terrain, so emissive blocks and the sky bounce light dynamically. See
//! `soils-worldgen/src/radiance.rs` for the reference math this mirrors.
//!
//! The only CPU→GPU transfer is the occupancy volume (block ids); all probes,
//! rays, and merged radiance stay on the GPU — meshing likewise stays GPU-only.

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_graph::{self, RenderGraph, RenderLabel};
use bevy::render::render_resource::binding_types::{
    storage_buffer_read_only_sized, storage_buffer_sized,
};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
    CachedComputePipelineId, ComputePassDescriptor, ComputePipelineDescriptor, PipelineCache,
    ShaderStages,
};
use bevy::render::renderer::{RenderContext, RenderDevice};
use bevy::render::storage::{GpuShaderStorageBuffer, ShaderStorageBuffer};
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};
use soils_protocol::CHUNK_BIT;

use crate::chunk::{Blocks, ChunkMap, WorldTime};

/// World volume side, in voxels. Must match `GI_DIM` in radiance.wgsl. Sized
/// for integrated GPUs (see the note in radiance.wgsl).
pub const GI_DIM: i32 = 64;
/// Number of cascades. Must match `CASCADES` in radiance.wgsl.
const CASCADES: usize = 4;
/// Probes per axis, directions-per-axis, per cascade (mirror radiance.wgsl).
const PROBES: [u32; CASCADES] = [16, 8, 4, 2];
const DIRRES: [u32; CASCADES] = [4, 8, 16, 32];
/// Recenter the volume when the player drifts this many voxels from its middle.
/// Kept under half the (smaller) volume so the player stays well inside it.
const RECENTER_SLACK: i32 = 12;
/// Re-trace the cascades every Nth rendered frame (GI needn't update per frame;
/// spreading the work keeps it clear of GPU watchdog timeouts).
const GI_TRACE_INTERVAL: u32 = 6;

/// Entries (probe × direction) in cascade `c`.
fn cascade_entries(c: usize) -> u64 {
    let p = PROBES[c] as u64;
    let d = DIRRES[c] as u64;
    p * p * p * d * d
}

/// Per-component arithmetic right shift (glam has no scalar `>>` for `IVec3`).
fn shr(v: IVec3, bit: i32) -> IVec3 {
    IVec3::new(v.x >> bit, v.y >> bit, v.z >> bit)
}

/// Per-component left shift.
fn shl(v: IVec3, bit: i32) -> IVec3 {
    IVec3::new(v.x << bit, v.y << bit, v.z << bit)
}

/// Little-endian bytes of an f32 slice, for storage-buffer uploads.
fn f32_bytes(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

/// User toggle (pause menu / console), mirrors `RenderToggles`.
#[derive(Resource)]
pub struct GiSettings {
    pub enabled: bool,
}

impl Default for GiSettings {
    fn default() -> Self {
        // Off by default: the radiance-cascades trace is an experimental,
        // GPU-heavy pass that can destabilise some drivers, so it is opt-in via
        // the pause menu or `/gi on`. Enable at startup with `SOILS_GI=1`.
        Self { enabled: false }
    }
}

/// The GPU buffers backing GI. Handles are shared: the chunk material binds the
/// same `cascade0` and `params` buffers the compute pass writes.
#[derive(Resource, Clone, ExtractResource)]
pub struct GiAssets {
    world_vox: Handle<ShaderStorageBuffer>,
    emission: Handle<ShaderStorageBuffer>,
    cascades: [Handle<ShaderStorageBuffer>; CASCADES],
    metas: [Handle<ShaderStorageBuffer>; CASCADES],
    params: Handle<ShaderStorageBuffer>,
    /// Pending GPU occupancy refill: chunk voxel buffers (GPU-resident for the
    /// mesher) to blit into the volume, with their offsets relative to the
    /// volume origin. Populated on refill frames, consumed by the render node
    /// (a clear pass runs first so unloaded space reads as air).
    refill: Vec<(Handle<ShaderStorageBuffer>, IVec3)>,
    /// Placeholder bound to the unused `far` slot during trace (can't reuse the
    /// write target — a buffer may not be both read-write and read-only in one
    /// dispatch).
    dummy: Handle<ShaderStorageBuffer>,
    /// Center voxel the volume is currently built around (`None` until first
    /// fill), so we only rebuild when the player drifts far enough.
    center: Option<IVec3>,
    /// Extracted each frame so the render node can skip work when GI is off.
    enabled: bool,
    /// Last `(origin, enabled)` pushed into chunk materials, so we only touch
    /// them (and rebuild their bind groups) when it actually changes.
    applied: Option<(IVec3, bool)>,
}

impl GiAssets {
    /// The cascade-0 radiance-field buffer the chunk material samples. Its GPU
    /// buffer is written only by the compute shader and never recreated, so it
    /// is safe to hold in a cached material bind group.
    pub fn cascade0(&self) -> Handle<ShaderStorageBuffer> {
        self.cascades[0].clone()
    }

    /// Force a volume refill and a re-push of origin/enable into every chunk
    /// material next frame — for when the scene changes in a way the periodic
    /// refill/sync would otherwise miss for a frame (e.g. a chunk injected
    /// directly into `ChunkMap`, as the GI demo does).
    pub fn mark_scene_dirty(&mut self) {
        self.center = None;
        self.applied = None;
    }

    /// Current `(volume origin as world voxel coords, enable flag)` for a chunk
    /// material's `AtlasParams`.
    pub fn apply_params(&self) -> (Vec3, f32) {
        let origin = self.center.map(|c| c - IVec3::splat(GI_DIM / 2)).unwrap_or(IVec3::ZERO);
        (origin.as_vec3(), if self.enabled { 1.0 } else { 0.0 })
    }
}

pub struct GiPlugin;

impl Plugin for GiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GiSettings>()
            .add_plugins(ExtractResourcePlugin::<GiAssets>::default())
            .add_systems(Startup, setup_gi_assets)
            .add_systems(Update, (selftest_disable_gi, update_gi_volume).chain());

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .add_systems(RenderStartup, (init_pipeline, add_render_graph_node))
            .add_systems(Render, prepare_bind_groups.in_set(RenderSystems::PrepareBindGroups));
    }
}

/// Allocate all GI buffers up front (fixed sizes).
fn setup_gi_assets(
    mut commands: Commands,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    blocks: Res<Blocks>,
) {
    let usage = RenderAssetUsages::default();

    // One byte (block id) per voxel; the shader reads it as packed u32s.
    let world_vox =
        buffers.add(ShaderStorageBuffer::with_size((GI_DIM * GI_DIM * GI_DIM) as usize, usage));

    // Per-block emitted radiance as vec4<f32> rows, indexed by block id.
    let emission_rows: Vec<Vec4> =
        blocks.0.emission_table().into_iter().map(Vec4::from_array).collect();
    let emission = buffers.add(ShaderStorageBuffer::from(emission_rows));

    let cascades = std::array::from_fn(|c| {
        buffers.add(ShaderStorageBuffer::with_size(cascade_entries(c) as usize * 16, usage))
    });
    let metas = std::array::from_fn(|c| {
        buffers.add(ShaderStorageBuffer::from(vec![c as u32]))
    });

    // 12 f32 = origin(3)+day, zenith(3)+lux, horizon(3)+enabled.
    let params = buffers.add(ShaderStorageBuffer::from(vec![0.0f32; 12]));
    let dummy = buffers.add(ShaderStorageBuffer::with_size(16, usage));

    commands.insert_resource(GiAssets {
        world_vox,
        emission,
        cascades,
        metas,
        params,
        refill: Vec::new(),
        dummy,
        center: None,
        enabled: false,
        applied: None,
    });
}

/// Apply the `SOILS_GI` startup override (GI is off by default otherwise).
fn selftest_disable_gi(mut settings: ResMut<GiSettings>, mut done: Local<bool>) {
    if *done {
        return;
    }
    *done = true;
    if let Ok(v) = std::env::var("SOILS_GI") {
        settings.enabled = v != "0";
    }
}

/// Recenter + refill the occupancy volume around the player and refresh the
/// per-frame params (day/sky/enabled). Only rebuilds the volume when the player
/// has drifted past [`RECENTER_SLACK`] from the current center.
#[allow(clippy::too_many_arguments)]
fn update_gi_volume(
    settings: Res<GiSettings>,
    world_time: Res<WorldTime>,
    map: Res<ChunkMap>,
    gpu_chunks: Query<&crate::gpu_mesh::GpuChunk>,
    player: Query<&Transform, With<crate::player::Player>>,
    mut gi: ResMut<GiAssets>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut materials: ResMut<Assets<crate::material::ChunkMeshMaterial>>,
    mut refill: Local<u32>,
) {
    gi.enabled = settings.enabled;
    // Refill requests live for exactly one frame (the extract after this
    // system clones them into the render world).
    gi.refill.clear();

    // When GI is off, do zero GPU work — just make sure chunk materials aren't
    // still flagged to sample a (now un-updated) radiance field, then bail. This
    // keeps the disabled path free of the volume fill and per-frame buffer
    // churn, i.e. as close to the pre-GI renderer as possible.
    if !settings.enabled {
        if gi.applied.map(|(_, on)| on) != Some(false) {
            gi.applied = Some((IVec3::ZERO, false));
            for (_, m) in materials.iter_mut() {
                m.params.gi_enabled = 0.0;
            }
        }
        return;
    }

    // Daylight factor: same curve as `day_night` in main.rs (daytime 0 = noon =
    // bright, 0.5 = midnight = dark), floored so nights keep a little sky bounce.
    let day = soils_sim::ease10(world_time.daytime * 2.0 - 1.0).max(0.03);

    let Ok(pt) = player.single() else { return };
    let player_vox = pt.translation.floor().as_ivec3();

    // Decide (re)center: snap origin to the chunk grid so refills reuse whole
    // chunks. Origin is the volume's (0,0,0) corner in world voxel coords.
    let half = GI_DIM / 2;
    let need_recenter = match gi.center {
        None => true,
        Some(c) => (player_vox - c).abs().max_element() > RECENTER_SLACK,
    };

    let origin = if need_recenter {
        let center = player_vox;
        let origin = shl(shr(center - IVec3::splat(half), CHUNK_BIT), CHUNK_BIT);
        gi.center = Some(origin + IVec3::splat(half));
        origin
    } else {
        gi.center.unwrap() - IVec3::splat(half)
    };
    // Refill the occupancy volume on recenter, and periodically otherwise, so
    // geometry that streams in (or is edited) while the player stands still is
    // still picked up by the trace — not just on movement. The fill itself is
    // GPU-side (plan §1 L2 item 1): queue the overlapping chunks' resident
    // voxel buffers; the render node clears the volume and blits them.
    *refill = refill.wrapping_add(1);
    if need_recenter || *refill % 30 == 0 {
        let c0 = shr(origin, CHUNK_BIT);
        let c1 = shr(origin + IVec3::splat(GI_DIM - 1), CHUNK_BIT);
        for cy in c0.y..=c1.y {
            for cz in c0.z..=c1.z {
                for cx in c0.x..=c1.x {
                    let cpos = IVec3::new(cx, cy, cz);
                    let Some(&e) = map.map.get(&cpos) else { continue };
                    // Air chunks have no GPU buffer; the clear pass covers them.
                    let Ok(gc) = gpu_chunks.get(e) else { continue };
                    gi.refill.push((gc.voxels.clone(), shl(cpos, CHUNK_BIT) - origin));
                }
            }
        }
    }
    // The compute shader's own params buffer is rewritten every frame; that's
    // fine because its bind group is rebuilt every frame. (It must NOT be bound
    // by the chunk material, whose bind group is cached — see material.rs.)
    write_params(&gi, &mut buffers, origin, day, settings.enabled);

    // Push origin/enable into chunk materials only when they change, so we don't
    // dirty every material (and rebuild every bind group) each frame.
    let state = (origin, settings.enabled);
    if gi.applied != Some(state) {
        gi.applied = Some(state);
        let origin_v = origin.as_vec3();
        let flag = if settings.enabled { 1.0 } else { 0.0 };
        for (_, m) in materials.iter_mut() {
            m.params.gi_origin = origin_v;
            m.params.gi_enabled = flag;
        }
    }
}


/// Pack the params buffer (origin, daylight, sky colours, enabled flag).
fn write_params(
    gi: &GiAssets,
    buffers: &mut Assets<ShaderStorageBuffer>,
    origin: IVec3,
    day: f32,
    enabled: bool,
) {
    // Sky radiance in the GI's linear units (tuned modest; scaled to lux at
    // apply time). Horizon warmer/dimmer, zenith cooler/brighter.
    let zenith = [0.5, 0.7, 1.0];
    let horizon = [0.8, 0.85, 0.9];
    let data: Vec<f32> = vec![
        origin.x as f32, origin.y as f32, origin.z as f32, day,
        zenith[0], zenith[1], zenith[2], 0.0,
        horizon[0], horizon[1], horizon[2], if enabled { 1.0 } else { 0.0 },
    ];
    if let Some(buf) = buffers.get_mut(&gi.params) {
        buf.data = Some(f32_bytes(&data));
    }
}

// ---------------- Render world ----------------

#[derive(Resource)]
struct GiPipeline {
    layout: BindGroupLayoutDescriptor,
    trace: CachedComputePipelineId,
    merge: CachedComputePipelineId,
    blit_layout: BindGroupLayoutDescriptor,
    clear: CachedComputePipelineId,
    blit: CachedComputePipelineId,
}

/// Bind groups + dispatch sizes for one frame's cascades.
#[derive(Resource, Default)]
struct GiJobs {
    /// Occupancy refill: clear the volume, then blit each chunk.
    clear: Option<BindGroup>,
    blits: Vec<BindGroup>,
    trace: Vec<(BindGroup, u32)>,
    merge: Vec<(BindGroup, u32)>,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
struct GiLabel;

fn init_pipeline(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    pipeline_cache: Res<PipelineCache>,
) {
    let layout = BindGroupLayoutDescriptor::new(
        "gi_layout",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                storage_buffer_read_only_sized(false, None), // 0 world_vox
                storage_buffer_read_only_sized(false, None), // 1 emission
                storage_buffer_sized(false, None),           // 2 cascade (rw)
                storage_buffer_read_only_sized(false, None), // 3 params
                storage_buffer_read_only_sized(false, None), // 4 meta
                storage_buffer_read_only_sized(false, None), // 5 far
            ),
        ),
    );
    let shader = asset_server.load("shaders/radiance.wgsl");
    let trace = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("gi_trace".into()),
        layout: vec![layout.clone()],
        shader: shader.clone(),
        entry_point: Some("trace".into()),
        ..default()
    });
    let merge = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("gi_merge".into()),
        layout: vec![layout.clone()],
        shader,
        entry_point: Some("merge".into()),
        ..default()
    });

    // Occupancy blit (chunk voxels → GI volume, all GPU-side).
    let blit_layout = BindGroupLayoutDescriptor::new(
        "gi_blit_layout",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                storage_buffer_read_only_sized(false, None), // 0 chunk voxels
                storage_buffer_sized(false, None),           // 1 world_vox (rw)
                storage_buffer_read_only_sized(false, None), // 2 blit params
            ),
        ),
    );
    let blit_shader = asset_server.load("shaders/gi_blit.wgsl");
    let clear = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("gi_clear".into()),
        layout: vec![blit_layout.clone()],
        shader: blit_shader.clone(),
        entry_point: Some("clear_volume".into()),
        ..default()
    });
    let blit = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("gi_blit".into()),
        layout: vec![blit_layout.clone()],
        shader: blit_shader,
        entry_point: Some("blit_chunk".into()),
        ..default()
    });
    commands.insert_resource(GiPipeline { layout, trace, merge, blit_layout, clear, blit });
    commands.insert_resource(GiJobs::default());
}

fn add_render_graph_node(mut render_graph: ResMut<RenderGraph>) {
    render_graph.add_node(GiLabel, GiNode);
    render_graph.add_node_edge(GiLabel, bevy::render::graph::CameraDriverLabel);
}

fn prepare_bind_groups(
    mut jobs: ResMut<GiJobs>,
    pipeline: Res<GiPipeline>,
    render_device: Res<RenderDevice>,
    pipeline_cache: Res<PipelineCache>,
    gi: Option<Res<GiAssets>>,
    buffers: Res<RenderAssets<GpuShaderStorageBuffer>>,
    mut frame: Local<u32>,
) {
    jobs.clear = None;
    jobs.blits.clear();
    jobs.trace.clear();
    jobs.merge.clear();
    let Some(gi) = gi else { return };
    if !gi.enabled {
        return;
    }

    // Occupancy refill (independent of the trace throttle): clear the volume,
    // then blit each queued chunk buffer into it — all on the GPU.
    if !gi.refill.is_empty()
        && let Some(world_vox) = buffers.get(&gi.world_vox)
    {
        let blit_layout = pipeline_cache.get_bind_group_layout(&pipeline.blit_layout);
        for (handle, rel) in &gi.refill {
            let Some(chunk_buf) = buffers.get(handle) else { continue };
            let params = render_device.create_buffer_with_data(
                &bevy::render::render_resource::BufferInitDescriptor {
                    label: Some("gi_blit_params"),
                    contents: &[rel.x, rel.y, rel.z, 0i32]
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect::<Vec<u8>>(),
                    usage: bevy::render::render_resource::BufferUsages::STORAGE,
                },
            );
            let bg = render_device.create_bind_group(
                None,
                &blit_layout,
                &BindGroupEntries::sequential((
                    chunk_buf.buffer.as_entire_buffer_binding(),
                    world_vox.buffer.as_entire_buffer_binding(),
                    params.as_entire_buffer_binding(),
                )),
            );
            if jobs.clear.is_none() {
                jobs.clear = Some(bg.clone());
            }
            jobs.blits.push(bg);
        }
    }

    // Round-robin the cascade work across frames (plan §1 L2 item 5): with
    // GI_TRACE_INTERVAL = 6 and 4 cascades, frames 0-3 trace one cascade
    // each, frame 4 runs the merge chain, frame 5 idles — same steady-state
    // cadence as the old trace-everything-every-6th-frame, without the spike.
    *frame = frame.wrapping_add(1);
    let phase = *frame % GI_TRACE_INTERVAL;
    let layout = pipeline_cache.get_bind_group_layout(&pipeline.layout);

    // All shared buffers must be resident.
    let (Some(world_vox), Some(emission), Some(params)) = (
        buffers.get(&gi.world_vox),
        buffers.get(&gi.emission),
        buffers.get(&gi.params),
    ) else {
        return;
    };
    let cascades: Vec<_> = gi.cascades.iter().map(|h| buffers.get(h)).collect();
    let metas: Vec<_> = gi.metas.iter().map(|h| buffers.get(h)).collect();
    let Some(dummy) = buffers.get(&gi.dummy) else { return };
    if cascades.iter().any(Option::is_none) || metas.iter().any(Option::is_none) {
        return;
    }
    let cascade = |c: usize| cascades[c].unwrap();
    let meta = |c: usize| metas[c].unwrap();

    // Phases 0..CASCADES handle one cascade each, top-down (c3, c2, c1, c0),
    // and each cascade's *merge* runs in the same frame as its trace: the
    // merge overwrites the cascade in place, so pairing them means cascade 0
    // is never left holding a raw (near-field-only) trace for the material to
    // sample — merging c against an already-merged c+1 from this same cycle
    // keeps the far-field chain intact. Remaining phases idle.
    if (phase as usize) >= CASCADES {
        return;
    }
    let c = CASCADES - 1 - phase as usize;
    let bg = render_device.create_bind_group(
        None,
        &layout,
        &BindGroupEntries::sequential((
            world_vox.buffer.as_entire_buffer_binding(),
            emission.buffer.as_entire_buffer_binding(),
            cascade(c).buffer.as_entire_buffer_binding(),
            params.buffer.as_entire_buffer_binding(),
            meta(c).buffer.as_entire_buffer_binding(),
            dummy.buffer.as_entire_buffer_binding(),
        )),
    );
    jobs.trace.push((bg, cascade_entries(c).div_ceil(64) as u32));
    if c < CASCADES - 1 {
        let bg = render_device.create_bind_group(
            None,
            &layout,
            &BindGroupEntries::sequential((
                world_vox.buffer.as_entire_buffer_binding(),
                emission.buffer.as_entire_buffer_binding(),
                cascade(c).buffer.as_entire_buffer_binding(),
                params.buffer.as_entire_buffer_binding(),
                meta(c).buffer.as_entire_buffer_binding(),
                cascade(c + 1).buffer.as_entire_buffer_binding(),
            )),
        );
        jobs.merge.push((bg, cascade_entries(c).div_ceil(64) as u32));
    }
}

struct GiNode;

impl render_graph::Node for GiNode {
    fn run(
        &self,
        _graph: &mut render_graph::RenderGraphContext,
        render_context: &mut RenderContext,
        world: &World,
    ) -> Result<(), render_graph::NodeRunError> {
        let pipeline_cache = world.resource::<PipelineCache>();
        let pipeline = world.resource::<GiPipeline>();
        let Some(jobs) = world.get_resource::<GiJobs>() else {
            return Ok(());
        };
        if jobs.trace.is_empty() && jobs.merge.is_empty() && jobs.blits.is_empty() {
            return Ok(());
        }

        let mut pass = render_context
            .command_encoder()
            .begin_compute_pass(&ComputePassDescriptor { label: Some("gi"), ..default() });

        // Occupancy refill first, so this frame's (or the next) trace reads
        // fresh geometry: one clear over the volume, then a blit per chunk.
        if let (Some(clear_bg), Some(clear), Some(blit)) = (
            &jobs.clear,
            pipeline_cache.get_compute_pipeline(pipeline.clear),
            pipeline_cache.get_compute_pipeline(pipeline.blit),
        ) {
            pass.set_pipeline(clear);
            pass.set_bind_group(0, clear_bg, &[]);
            pass.dispatch_workgroups(((GI_DIM * GI_DIM * GI_DIM / 4) as u32).div_ceil(64), 1, 1);
            pass.set_pipeline(blit);
            for bg in &jobs.blits {
                pass.set_bind_group(0, bg, &[]);
                pass.dispatch_workgroups(1, 8, 8);
            }
        }

        if let Some(trace) = pipeline_cache.get_compute_pipeline(pipeline.trace)
            && !jobs.trace.is_empty()
        {
            pass.set_pipeline(trace);
            for (bg, groups) in &jobs.trace {
                pass.set_bind_group(0, bg, &[]);
                pass.dispatch_workgroups(*groups, 1, 1);
            }
        }
        if let Some(merge) = pipeline_cache.get_compute_pipeline(pipeline.merge)
            && !jobs.merge.is_empty()
        {
            pass.set_pipeline(merge);
            for (bg, groups) in &jobs.merge {
                pass.set_bind_group(0, bg, &[]);
                pass.dispatch_workgroups(*groups, 1, 1);
            }
        }
        Ok(())
    }
}
