//! GPU chunk generation (stream pipeline phase 6b): non-mirror pristine
//! demands are born directly in the pooled voxel cache — the worldgen v2
//! kernels (`soils_worldgen::wgsl::generate_chunk`, bit-exact with the CPU)
//! run as a compute node before the mesher, so a demanded chunk generates,
//! meshes, and lights in one frame with zero CPU voxel bytes.
//!
//! Occupancy is unknown until the kernel runs, so every GPU-gen chunk
//! optimistically takes a mesh slot; the kernel counts non-air cells per job
//! into a small tagged buffer that reads back asynchronously, and all-air
//! chunks demote to air slots (mesh slot freed) a couple of frames later.
//! In-flight optimism is bounded by [`GEN_BUDGET`] jobs/frame, so the mesh
//! pool can't be flooded by sky chunks during a join burst.
//!
//! The main world only queues jobs once [`GenReady`] mirrors back from the
//! render world (see memory `gpu-node-pipeline-race`).

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{
    storage_buffer_read_only_sized, storage_buffer_sized, uniform_buffer_sized,
};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, Buffer,
    BufferDescriptor, BufferUsages, CachedComputePipelineId, ComputePassDescriptor,
    ComputePipelineDescriptor, PipelineCache, RawBufferVec, ShaderStages,
};
use bevy::render::renderer::{
    RenderContext, RenderDevice, RenderGraph, RenderGraphSystems, RenderQueue,
};
use bevy::render::storage::{GpuShaderBuffer, ShaderBuffer};
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};

use crate::chunk::{Blocks, ChunkMap};
use crate::demand::ChunkDirectory;
use crate::pool::{ChunkSlots, DirtyMesh, PoolOpQueue};
use crate::server_msg::ClientGen;

/// Max GPU gen jobs dispatched per frame (also sizes the lattice scratch and
/// the occupancy readback).
pub const GEN_BUDGET: usize = 16;

const LATTICE_WORDS: u64 = 9 * 9 * 9;

/// The world-identity half of the kernel inputs: generated WGSL (graph-only)
/// plus the per-world uniform/params data. Rebuilt when the server's
/// [`soils_protocol::GenParams`] change.
#[derive(Resource, Clone, Default, ExtractResource)]
pub struct GenShaderInfo(pub Option<GenShader>);

#[derive(Clone)]
pub struct GenShader {
    pub shader: Handle<Shader>,
    pub p: Vec<i32>,
    /// flags, seed, pal0, pal1 (mirrors the WGSL GenView).
    pub view: [u32; 4],
    /// (seed, world_type) this was built for.
    key: (i64, u8),
}

/// Positions process_demands routed to GPU gen this frame (with their
/// freshly allocated mesh slots).
#[derive(Resource, Default)]
pub struct GpuGenQueue(pub Vec<(IVec3, u32)>);

/// This frame's dispatch, handed to the render world.
#[derive(Resource, Clone, Default, ExtractResource)]
pub struct GpuGenBatch {
    /// origin.xyz, mesh slot — mirrors the WGSL jobs array.
    pub jobs: Vec<[i32; 4]>,
}

/// Dispatched batches awaiting their occupancy readback, by tag.
#[derive(Resource, Default)]
pub struct GenBatches {
    next_id: u32,
    pending: HashMap<u32, Vec<(IVec3, u32)>>,
}

/// Handle to the tagged occupancy buffer ([tag, per-job counts]).
#[derive(Resource, Clone, ExtractResource)]
pub struct GenOccBuffer(pub Handle<ShaderBuffer>);

/// Mirrored render→main once the gen pipelines have compiled.
#[derive(Resource, Clone, Copy)]
pub struct GenReady;

pub struct GpuGenPlugin;

impl Plugin for GpuGenPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GenShaderInfo>()
            .init_resource::<GpuGenQueue>()
            .init_resource::<GpuGenBatch>()
            .init_resource::<GenBatches>()
            .add_plugins(ExtractResourcePlugin::<GenShaderInfo>::default())
            .add_plugins(ExtractResourcePlugin::<GpuGenBatch>::default())
            .add_plugins(ExtractResourcePlugin::<GenOccBuffer>::default())
            .add_systems(Startup, setup_occ_buffer)
            .add_systems(Update, build_gen_shader);

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app
            .add_systems(RenderStartup, init_gen)
            // Voxels must exist before the mesher (and thus the light flood).
            .add_systems(
                RenderGraph,
                gen_pass
                    .in_set(RenderGraphSystems::Render)
                    .before(crate::gpu_mesh::voxel_mesh_pass),
            )
            .add_systems(bevy::render::ExtractSchedule, mirror_ready)
            .add_systems(
                Render,
                (
                    queue_gen_pipelines.in_set(RenderSystems::Prepare),
                    prepare_gen.in_set(RenderSystems::PrepareBindGroups),
                ),
            );
    }
}

fn setup_occ_buffer(mut commands: Commands, mut buffers: ResMut<Assets<ShaderBuffer>>) {
    let mut buf = ShaderBuffer::from(vec![0u32; 1 + GEN_BUDGET]);
    buf.buffer_description.usage = BufferUsages::STORAGE | BufferUsages::COPY_SRC | BufferUsages::COPY_DST;
    let handle = buffers.add(buf);
    commands.insert_resource(GenOccBuffer(handle.clone()));
    commands.spawn(Readback::buffer(handle)).observe(on_gen_occ);
}

/// (Re)build the generated WGSL + params when the world identity changes.
/// The WGSL depends only on the graph; seed/flags/palette travel as data.
pub fn build_gen_shader(
    cgen: Res<ClientGen>,
    blocks: Res<Blocks>,
    mut info: ResMut<GenShaderInfo>,
    mut shaders: ResMut<Assets<Shader>>,
) {
    let Some(params) = cgen.params else { return };
    if !cgen.hash_ok {
        if info.0.is_some() {
            info.0 = None;
        }
        return;
    }
    let key = (params.seed, params.world_type);
    if info.0.as_ref().is_some_and(|g| g.key == key) {
        return;
    }
    let Some(terrain) = cgen.terrain() else { return };
    let graph = terrain.graph();
    let src = match soils_worldgen::wgsl::generate_chunk(graph) {
        Ok(s) => s,
        Err(e) => {
            warn!("GPU gen unavailable (WGSL codegen failed: {e}); CPU gen only");
            info.0 = None;
            return;
        }
    };
    let mut p = soils_worldgen::wgsl::collect_params(graph);
    if p.is_empty() {
        p.push(0); // WGSL arrays can't bind zero-sized
    }
    let id = |name: &str| blocks.0.id_of(name).unwrap_or(0) as u32;
    let pal0 = id("Grass") | (id("Slate") << 8) | (id("Stone") << 16) | (id("Rocky Dirt") << 24);
    let pal1 = id("Tough Dirt") | (id("Dirt") << 8);
    let flags: u32 = if params.world_type == 1 { 1 } else { 0 };
    let shader = shaders.add(Shader::from_wgsl(src, "generated://soils_gen_chunk.wgsl"));
    info.0 = Some(GenShader {
        shader,
        p,
        view: [flags, params.seed as u32, pal0, pal1],
        key,
    });
}

/// Turn the frame's queued GPU gen jobs into a tagged batch: zero+tag the
/// occupancy buffer and remember the job list for the readback.
pub fn flush_gen_batch(
    mut queue: ResMut<GpuGenQueue>,
    mut batch: ResMut<GpuGenBatch>,
    mut batches: ResMut<GenBatches>,
    occ: Option<Res<GenOccBuffer>>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
) {
    batch.jobs.clear();
    if queue.0.is_empty() {
        return;
    }
    let Some(occ) = occ else { return };
    let list: Vec<(IVec3, u32)> = queue.0.drain(..).collect();
    batches.next_id += 1;
    let tag = batches.next_id;
    let mut data = vec![0u32; 1 + GEN_BUDGET];
    data[0] = tag;
    if let Some(mut buf) = buffers.get_mut(&occ.0) {
        buf.set_data(data);
    }
    for (cpos, mesh) in &list {
        let o = *cpos * 32;
        batch.jobs.push([o.x, o.y, o.z, *mesh as i32]);
    }
    batches.pending.insert(tag, list);
    // Drop stale batches whose readback never matched (warp races).
    let cur = batches.next_id;
    batches.pending.retain(|id, _| cur.wrapping_sub(*id) < 64);
}

/// Occupancy readback: demote all-air GPU-genned chunks to air slots. Guards
/// against the world moving on underneath (unload, edit, CPU materialize).
fn on_gen_occ(
    event: On<ReadbackComplete>,
    mut batches: ResMut<GenBatches>,
    mut slots: ResMut<ChunkSlots>,
    mut pool_ops: ResMut<PoolOpQueue>,
    mut dirty_mesh: ResMut<DirtyMesh>,
    map: Res<ChunkMap>,
    dir: Res<ChunkDirectory>,
) {
    let bytes: &[u8] = &event.data;
    if bytes.len() < 4 * (1 + GEN_BUDGET) {
        return;
    }
    let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    let tag = word(0);
    let Some(list) = batches.pending.remove(&tag) else { return };
    for (i, (cpos, mesh)) in list.into_iter().enumerate() {
        if word(1 + i) != 0 {
            continue; // has terrain; keeps its mesh slot
        }
        // All air: free the mesh slot unless something else claimed the chunk
        // meanwhile (edit overlay, CPU mirror bytes, unload/remap).
        if slots.get(cpos).map(|s| s.mesh) != Some(mesh)
            || map.map.contains_key(&cpos)
            || dir.edited.contains(&cpos)
            || dir.overlay.contains_key(&cpos)
        {
            continue;
        }
        slots.demote_mesh(&mut pool_ops, &mut dirty_mesh, cpos);
    }
}

/// Tell the main world the gen pipelines are dispatchable.
fn mirror_ready(
    mut main: ResMut<bevy::render::MainWorld>,
    pipeline: Option<Res<GenPipelines>>,
    pipeline_cache: Res<PipelineCache>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(p) = pipeline else { return };
    let (Some(lattice), Some(fill)) = (p.lattice, p.fill) else { return };
    if pipeline_cache.get_compute_pipeline(lattice).is_some()
        && pipeline_cache.get_compute_pipeline(fill).is_some()
    {
        main.insert_resource(GenReady);
        *done = true;
    }
}

// ---------------- Render world ----------------

#[derive(Resource)]
pub(crate) struct GenPipelines {
    layout: BindGroupLayoutDescriptor,
    lattice: Option<CachedComputePipelineId>,
    fill: Option<CachedComputePipelineId>,
    view: Buffer,
    lattice_buf: Buffer,
    view_written: bool,
}

#[derive(Resource, Default)]
pub(crate) struct GenJobsGpu {
    p: Option<RawBufferVec<i32>>,
    jobs: Option<RawBufferVec<i32>>,
    bind_group: Option<BindGroup>,
    count: u32,
}

fn init_gen(mut commands: Commands, device: Res<RenderDevice>) {
    let layout = BindGroupLayoutDescriptor::new(
        "gpu_gen_layout",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                uniform_buffer_sized(false, None),           // GenView
                storage_buffer_read_only_sized(false, None), // P
                storage_buffer_sized(false, None),           // lattice (rw)
                storage_buffer_sized(false, None),           // out_voxels (rw)
                storage_buffer_read_only_sized(false, None), // jobs
                storage_buffer_sized(false, None),           // occ (rw)
            ),
        ),
    );
    let view = device.create_buffer(&BufferDescriptor {
        label: Some("gen_view"),
        size: 16,
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let lattice_buf = device.create_buffer(&BufferDescriptor {
        label: Some("gen_lattice"),
        size: GEN_BUDGET as u64 * LATTICE_WORDS * 4,
        usage: BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    commands.insert_resource(GenPipelines {
        layout,
        lattice: None,
        fill: None,
        view,
        lattice_buf,
        view_written: false,
    });
    commands.insert_resource(GenJobsGpu::default());
}


/// Queue the compute pipelines once the generated shader arrives (the source
/// is runtime-built from the server's world identity).
fn queue_gen_pipelines(
    mut pipelines: ResMut<GenPipelines>,
    info: Option<Res<GenShaderInfo>>,
    pipeline_cache: Res<PipelineCache>,
) {
    if pipelines.lattice.is_some() {
        return;
    }
    let Some(info) = info else { return };
    let Some(g) = &info.0 else { return };
    let pipe = |entry: &'static str, layout: &BindGroupLayoutDescriptor, shader: &Handle<Shader>| {
        ComputePipelineDescriptor {
            label: Some(format!("gen_{entry}").into()),
            layout: vec![layout.clone()],
            shader: shader.clone(),
            entry_point: Some(entry.into()),
            ..default()
        }
    };
    pipelines.lattice = Some(
        pipeline_cache.queue_compute_pipeline(pipe("gen_lattice", &pipelines.layout, &g.shader)),
    );
    pipelines.fill = Some(
        pipeline_cache.queue_compute_pipeline(pipe("gen_fill", &pipelines.layout, &g.shader)),
    );
}

#[allow(clippy::too_many_arguments)]
fn prepare_gen(
    mut jobs: ResMut<GenJobsGpu>,
    mut pipelines: ResMut<GenPipelines>,
    pools: Option<Res<crate::pool::ChunkPools>>,
    batch: Option<Res<GpuGenBatch>>,
    info: Option<Res<GenShaderInfo>>,
    occ: Option<Res<GenOccBuffer>>,
    device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    buffers: Res<RenderAssets<GpuShaderBuffer>>,
    pipeline_cache: Res<PipelineCache>,
) {
    jobs.bind_group = None;
    jobs.count = 0;
    let (Some(pools), Some(batch), Some(info), Some(occ)) = (pools, batch, info, occ) else {
        return;
    };
    let Some(g) = &info.0 else { return };
    if batch.jobs.is_empty() {
        return;
    }
    let Some(occ_buf) = buffers.get(&occ.0) else { return };
    let (Some(lattice_id), Some(fill_id)) = (pipelines.lattice, pipelines.fill) else { return };
    // The main world gates job creation on GenReady, so this only fails if a
    // batch somehow raced pipeline setup — which would silently drop chunks;
    // scream instead (their entries are already gone from the directory).
    if pipeline_cache.get_compute_pipeline(lattice_id).is_none()
        || pipeline_cache.get_compute_pipeline(fill_id).is_none()
    {
        error!("gen batch dropped: pipelines not ready despite GenReady");
        return;
    }

    if !pipelines.view_written {
        render_queue.write_buffer(&pipelines.view, 0, bytemuck::cast_slice(&g.view));
        let p = jobs.p.get_or_insert_with(|| RawBufferVec::new(BufferUsages::STORAGE));
        p.clear();
        for v in &g.p {
            p.push(*v);
        }
        p.write_buffer(&device, &render_queue);
        pipelines.view_written = true;
    }
    let jb = jobs.jobs.get_or_insert_with(|| RawBufferVec::new(BufferUsages::STORAGE));
    jb.clear();
    for j in &batch.jobs {
        for v in j {
            jb.push(*v);
        }
    }
    jb.write_buffer(&device, &render_queue);

    let layout = pipeline_cache.get_bind_group_layout(&pipelines.layout);
    jobs.bind_group = Some(device.create_bind_group(
        None,
        &layout,
        &BindGroupEntries::sequential((
            pipelines.view.as_entire_buffer_binding(),
            jobs.p.as_ref().unwrap().buffer().unwrap().as_entire_buffer_binding(),
            pipelines.lattice_buf.as_entire_buffer_binding(),
            pools.voxels.as_entire_buffer_binding(),
            jobs.jobs.as_ref().unwrap().buffer().unwrap().as_entire_buffer_binding(),
            occ_buf.buffer.as_entire_buffer_binding(),
        )),
    ));
    jobs.count = batch.jobs.len() as u32;
}

/// Generate voxels for GPU-born chunks straight into the pooled voxel cache.
pub(crate) fn gen_pass(
    mut render_context: RenderContext,
    pipeline_cache: Res<PipelineCache>,
    pipelines: Option<Res<GenPipelines>>,
    jobs: Option<Res<GenJobsGpu>>,
) {
    let Some(pipelines) = pipelines else { return };
    let Some(jobs) = jobs else { return };
    let Some(bind_group) = &jobs.bind_group else { return };
    let (Some(lattice_id), Some(fill_id)) = (pipelines.lattice, pipelines.fill) else {
        return;
    };
    let (Some(lattice), Some(fill)) = (
        pipeline_cache.get_compute_pipeline(lattice_id),
        pipeline_cache.get_compute_pipeline(fill_id),
    ) else {
        return;
    };
    let mut pass = render_context
        .command_encoder()
        .begin_compute_pass(&ComputePassDescriptor { label: Some("gpu_gen"), ..default() });
    pass.set_bind_group(0, bind_group, &[]);
    pass.set_pipeline(lattice);
    pass.dispatch_workgroups(1, jobs.count, 1);
    pass.set_pipeline(fill);
    pass.dispatch_workgroups(1, 4, jobs.count);
}
