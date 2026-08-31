//! GPU L0 light flood (stream pipeline phase 5): replaces the CPU
//! `soils_sim::light` flood — the measured frame bottleneck (~29 ms/frame,
//! minutes to drain a radius-8 join) — with compute passes over the pooled
//! light cache (`assets/shaders/light_flood.wgsl`).
//!
//! Scheduling (main world): [`crate::light::LightQueue`] stays the event bus
//! (chunks queued on map, voxels on edit). Each frame a budgeted batch
//! becomes the *core* set — dirtied chunks, the 3×3×3 neighborhood of edits
//! (the removal margin: light travels ≤15 < 32 voxels), and the mapped column
//! below each (sky beams cascade downward without attenuation) — which is
//! reseeded, beamed, and relaxed; the core's mapped 1-ring joins for the
//! relax rounds only (lateral bleed reaches ≤15 voxels across a border).
//! Rounds: beams repeat once per distinct chunk-y (top-down convergence),
//! relax runs a fixed 16 rounds (any attenuated path is ≤15 steps).

use bevy::platform::collections::HashSet;
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{
    storage_buffer_read_only_sized, storage_buffer_sized,
};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, BufferUsages,
    CachedComputePipelineId, ComputePassDescriptor, ComputePipelineDescriptor, PipelineCache,
    RawBufferVec, ShaderStages,
};
use bevy::core_pipeline::schedule::camera_driver;
use bevy::render::renderer::{
    RenderContext, RenderDevice, RenderGraph, RenderGraphSystems, RenderQueue,
};
use bevy::render::storage::{GpuShaderBuffer, ShaderBuffer};
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};
use soils_protocol::chunk_of;

use crate::chunk::Blocks;
use crate::light::{LightQueue, PlayerLight};
use crate::pool::{ChunkSlots, NO_MESH};

/// Dirty chunks lit per frame (the rest carry over in the LightQueue).
const CORE_BUDGET: usize = 64;
/// Jacobi relax rounds per scheduling of a core set (≥15: max attenuation).
const RELAX_ROUNDS: u32 = 16;

/// One GPU light job (mirrors `LightJob` in light_flood.wgsl, 32 B).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct GpuLightJob {
    cpos: [i32; 3],
    slot: u32,
    mesh_slot: u32,
    _pad: [u32; 3],
}

unsafe impl bytemuck::Pod for GpuLightJob {}
unsafe impl bytemuck::Zeroable for GpuLightJob {}

/// A blocklight emitter that is not a block — currently just the player (see
/// [`PlayerLight`]). Mirrors `PointLight` in light_flood.wgsl, 16 B.
///
/// The reseed pass stamps these into the light grid alongside the emissive
/// blocks, so everything downstream (beam, relax, the terrain shader's
/// `light_at`) treats them as ordinary blocklight and occludes them properly.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct GpuPointLight {
    voxel: [i32; 3],
    /// Emission level; 0 is the disabled row the binding needs when there are
    /// no emitters at all (a zero-sized storage binding is not allowed).
    level: u32,
}

unsafe impl bytemuck::Pod for GpuPointLight {}
unsafe impl bytemuck::Zeroable for GpuPointLight {}

/// This frame's planned jobs, handed to the render world.
#[derive(Resource, Clone, Default, ExtractResource)]
pub struct LightBatch {
    /// Reseed + beam + relax, sorted by chunk y descending.
    pub core: Vec<GpuLightJob>,
    /// Relax-only ring (core's mapped neighbors).
    pub relax: Vec<GpuLightJob>,
    /// Distinct chunk-y layers in `core` (beam repeat count).
    pub beam_rounds: u32,
    /// Non-block emitters to stamp in during reseed. Never empty — a disabled
    /// row stands in for "none" so the binding always has something to point
    /// at.
    pub points: Vec<GpuPointLight>,
}

/// Per-block emission levels as a GPU table (u32 rows).
#[derive(Resource, Clone, ExtractResource)]
pub struct EmittersTable(pub Handle<ShaderBuffer>);

/// Mirrored render→main once the flood pipelines have compiled: the planner
/// must not drain the LightQueue before the node can actually dispatch (see
/// memory `gpu-node-pipeline-race`).
#[derive(Resource, Clone, Copy)]
pub struct LightReady;

pub struct GpuLightPlugin;

impl Plugin for GpuLightPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LightBatch>()
            .add_plugins(ExtractResourcePlugin::<LightBatch>::default())
            .add_plugins(ExtractResourcePlugin::<EmittersTable>::default())
            .add_systems(Startup, setup_emitters);

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app
            .add_systems(RenderStartup, init_pipeline)
            .add_systems(bevy::render::ExtractSchedule, mirror_ready)
            .add_systems(Render, prepare_light.in_set(RenderSystems::PrepareBindGroups))
            // After the mesher and before the draw; voxel uploads for this
            // frame land in PrepareResources, so the flood sees this frame's
            // volumes.
            .add_systems(
                RenderGraph,
                light_pass
                    .in_set(RenderGraphSystems::Render)
                    .after(crate::gpu_mesh::voxel_mesh_pass)
                    .before(camera_driver),
            );
    }
}

fn setup_emitters(
    mut commands: Commands,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
    blocks: Res<Blocks>,
) {
    let rows: Vec<u32> = blocks.0.light_table().into_iter().map(u32::from).collect();
    commands.insert_resource(EmittersTable(buffers.add(ShaderBuffer::from(rows))));
}

/// Turn queued light events into this frame's GPU job batch (replaces the CPU
/// `process_light`). Runs after every voxel/mapping change for the frame.
pub fn plan_light_jobs(
    mut queue: ResMut<LightQueue>,
    slots: Res<ChunkSlots>,
    ready: Option<Res<LightReady>>,
    player_light: Res<PlayerLight>,
    mut batch: ResMut<LightBatch>,
) {
    batch.core.clear();
    batch.relax.clear();
    batch.beam_rounds = 0;
    // Rebuilt every frame, batch or no batch: a chunk reflooded for an
    // unrelated reason must still stamp the player's light back in, or walking
    // next to someone else's edit would punch a hole in your own glow.
    batch.points = point_lights(&player_light);
    if ready.is_none() {
        return; // pipelines still compiling; the queue carries over
    }
    if queue.chunks.is_empty() && queue.edits.is_empty() {
        return;
    }

    // Core: budgeted dirty chunks + the 3×3×3 around each edit, each with its
    // mapped column below (beam cascade).
    let mut core: HashSet<IVec3> = HashSet::default();
    let add_with_column = |core: &mut HashSet<IVec3>, cpos: IVec3| {
        if slots.get(cpos).is_none() {
            return;
        }
        core.insert(cpos);
        let mut below = cpos - IVec3::Y;
        while slots.get(below).is_some() {
            core.insert(below);
            below.y -= 1;
        }
    };
    // Top-down first so upper chunks' beams are exact for the lower ones.
    queue.chunks.sort_by_key(|c| c.y);
    let mut taken = 0;
    while taken < CORE_BUDGET {
        let Some(cpos) = queue.chunks.pop() else { break };
        add_with_column(&mut core, cpos);
        taken += 1;
    }
    for v in queue.edits.drain(..) {
        let c = chunk_of(v);
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    add_with_column(&mut core, c + IVec3::new(dx, dy, dz));
                }
            }
        }
    }
    if core.is_empty() {
        return;
    }

    // Ring: mapped face-neighbors of the core (relax participation only).
    let mut ring: HashSet<IVec3> = HashSet::default();
    for &c in &core {
        for d in [IVec3::X, IVec3::NEG_X, IVec3::Y, IVec3::NEG_Y, IVec3::Z, IVec3::NEG_Z] {
            let n = c + d;
            if !core.contains(&n) && slots.get(n).is_some() {
                ring.insert(n);
            }
        }
    }

    let job = |cpos: IVec3, slots: &ChunkSlots| {
        let s = slots.get(cpos).expect("filtered to mapped");
        GpuLightJob {
            cpos: cpos.to_array(),
            slot: s.slot,
            mesh_slot: if s.mesh == NO_MESH { u32::MAX } else { s.mesh },
            _pad: [0; 3],
        }
    };
    let mut core_sorted: Vec<IVec3> = core.iter().copied().collect();
    core_sorted.sort_by_key(|c| std::cmp::Reverse(c.y));
    let layers: HashSet<i32> = core_sorted.iter().map(|c| c.y).collect();
    batch.beam_rounds = layers.len() as u32;
    batch.core = core_sorted.iter().map(|&c| job(c, &slots)).collect();
    batch.relax = core_sorted
        .iter()
        .copied()
        .chain(ring.iter().copied())
        .map(|c| job(c, &slots))
        .collect();
}

/// The frame's non-block emitters, with a disabled row when there are none.
fn point_lights(player: &PlayerLight) -> Vec<GpuPointLight> {
    match player.voxel {
        Some(v) if player.level > 0 => {
            vec![GpuPointLight { voxel: v.to_array(), level: u32::from(player.level) }]
        }
        _ => vec![GpuPointLight::default()],
    }
}

/// Tell the main world the flood pipelines are dispatchable.
fn mirror_ready(
    mut main: ResMut<bevy::render::MainWorld>,
    pipeline: Option<Res<LightPipeline>>,
    pipeline_cache: Res<PipelineCache>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(pipeline) = pipeline else { return };
    if pipeline_cache.get_compute_pipeline(pipeline.reseed).is_some()
        && pipeline_cache.get_compute_pipeline(pipeline.beam).is_some()
        && pipeline_cache.get_compute_pipeline(pipeline.relax).is_some()
    {
        main.insert_resource(LightReady);
        *done = true;
    }
}

// ---------------- Render world ----------------

#[derive(Resource)]
pub(crate) struct LightPipeline {
    layout: BindGroupLayoutDescriptor,
    reseed: CachedComputePipelineId,
    beam: CachedComputePipelineId,
    relax: CachedComputePipelineId,
}

#[derive(Resource, Default)]
pub(crate) struct LightJobsGpu {
    core: Option<RawBufferVec<GpuLightJob>>,
    relax: Option<RawBufferVec<GpuLightJob>>,
    points: Option<RawBufferVec<GpuPointLight>>,
    core_bg: Option<BindGroup>,
    relax_bg: Option<BindGroup>,
    core_count: u32,
    relax_count: u32,
    beam_rounds: u32,
}

fn init_pipeline(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    pipeline_cache: Res<PipelineCache>,
) {
    let layout = BindGroupLayoutDescriptor::new(
        "light_flood_layout",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                storage_buffer_sized(false, None),           // light pool (rw)
                storage_buffer_read_only_sized(false, None), // voxel pool
                storage_buffer_read_only_sized(false, None), // desc
                storage_buffer_read_only_sized(false, None), // slot table
                storage_buffer_read_only_sized(false, None), // emitters
                storage_buffer_read_only_sized(false, None), // jobs
                storage_buffer_read_only_sized(false, None), // point lights
            ),
        ),
    );
    let shader = asset_server.load("shaders/light_flood.wgsl");
    let pipe = |entry: &'static str| ComputePipelineDescriptor {
        label: Some(format!("light_{entry}").into()),
        layout: vec![layout.clone()],
        shader: shader.clone(),
        entry_point: Some(entry.into()),
        ..default()
    };
    let reseed = pipeline_cache.queue_compute_pipeline(pipe("reseed"));
    let beam = pipeline_cache.queue_compute_pipeline(pipe("beam"));
    let relax = pipeline_cache.queue_compute_pipeline(pipe("relax"));
    commands.insert_resource(LightPipeline { layout, reseed, beam, relax });
    commands.insert_resource(LightJobsGpu::default());
}


#[allow(clippy::too_many_arguments)]
fn prepare_light(
    mut jobs: ResMut<LightJobsGpu>,
    pipeline: Res<LightPipeline>,
    pools: Option<Res<crate::pool::ChunkPools>>,
    batch: Option<Res<LightBatch>>,
    emitters: Option<Res<EmittersTable>>,
    device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    buffers: Res<RenderAssets<GpuShaderBuffer>>,
    pipeline_cache: Res<PipelineCache>,
) {
    jobs.core_bg = None;
    jobs.relax_bg = None;
    jobs.core_count = 0;
    let (Some(pools), Some(batch), Some(emitters)) = (pools, batch, emitters) else { return };
    if batch.core.is_empty() {
        return;
    }
    let Some(emitters_buf) = buffers.get(&emitters.0) else { return };
    // Belt-and-braces: the planner already gates on LightReady, so a batch
    // only exists when the pipelines compiled.
    if pipeline_cache.get_compute_pipeline(pipeline.reseed).is_none()
        || pipeline_cache.get_compute_pipeline(pipeline.beam).is_none()
        || pipeline_cache.get_compute_pipeline(pipeline.relax).is_none()
    {
        return;
    }

    let upload = |list: &[GpuLightJob],
                  store: &mut Option<RawBufferVec<GpuLightJob>>,
                  device: &RenderDevice,
                  queue: &RenderQueue| {
        let vec = store.get_or_insert_with(|| RawBufferVec::new(BufferUsages::STORAGE));
        vec.clear();
        for j in list {
            vec.push(*j);
        }
        vec.write_buffer(device, queue);
    };
    upload(&batch.core, &mut jobs.core, &device, &render_queue);
    upload(&batch.relax, &mut jobs.relax, &device, &render_queue);
    {
        let vec = jobs.points.get_or_insert_with(|| RawBufferVec::new(BufferUsages::STORAGE));
        vec.clear();
        for p in &batch.points {
            vec.push(*p);
        }
        // The planner always leaves a row here, but a stale empty buffer would
        // fail the binding — keep the invariant local.
        if batch.points.is_empty() {
            vec.push(GpuPointLight::default());
        }
        vec.write_buffer(&device, &render_queue);
    }
    let Some(points_buf) = jobs.points.as_ref().and_then(|v| v.buffer()).cloned() else {
        return;
    };

    let layout = pipeline_cache.get_bind_group_layout(&pipeline.layout);
    let bg = |jobs_buf: &RawBufferVec<GpuLightJob>| {
        device.create_bind_group(
            None,
            &layout,
            &BindGroupEntries::sequential((
                pools.light.as_entire_buffer_binding(),
                pools.voxels.as_entire_buffer_binding(),
                pools.desc.as_entire_buffer_binding(),
                pools.table.as_entire_buffer_binding(),
                emitters_buf.buffer.as_entire_buffer_binding(),
                jobs_buf.buffer().unwrap().as_entire_buffer_binding(),
                points_buf.as_entire_buffer_binding(),
            )),
        )
    };
    jobs.core_bg = Some(bg(jobs.core.as_ref().unwrap()));
    jobs.relax_bg = Some(bg(jobs.relax.as_ref().unwrap()));
    jobs.core_count = batch.core.len() as u32;
    jobs.relax_count = batch.relax.len() as u32;
    jobs.beam_rounds = batch.beam_rounds;
}

/// Flood L0 light over the pooled caches.
pub(crate) fn light_pass(
    mut render_context: RenderContext,
    pipeline_cache: Res<PipelineCache>,
    pipeline: Res<LightPipeline>,
    jobs: Option<Res<LightJobsGpu>>,
) {
    let Some(jobs) = jobs else { return };
    let (Some(core_bg), Some(relax_bg)) = (&jobs.core_bg, &jobs.relax_bg) else {
        return;
    };
    let (Some(reseed), Some(beam), Some(relax)) = (
        pipeline_cache.get_compute_pipeline(pipeline.reseed),
        pipeline_cache.get_compute_pipeline(pipeline.beam),
        pipeline_cache.get_compute_pipeline(pipeline.relax),
    ) else {
        return;
    };

    let mut pass = render_context
        .command_encoder()
        .begin_compute_pass(&ComputePassDescriptor { label: Some("light_flood"), ..default() });
    pass.set_bind_group(0, core_bg, &[]);
    pass.set_pipeline(reseed);
    pass.dispatch_workgroups(128, jobs.core_count, 1);
    pass.set_pipeline(beam);
    for _ in 0..jobs.beam_rounds.max(1) {
        pass.dispatch_workgroups(4, jobs.core_count, 1);
    }
    pass.set_bind_group(0, relax_bg, &[]);
    pass.set_pipeline(relax);
    for _ in 0..RELAX_ROUNDS {
        pass.dispatch_workgroups(128, jobs.relax_count, 1);
    }
}
