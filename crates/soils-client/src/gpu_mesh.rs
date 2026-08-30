//! GPU-resident chunk meshing over the pooled caches. A compute shader
//! (`voxel_mesh.wgsl`) greedy-meshes each dirty mesh slot's voxels into its
//! region of the shared packed-quad pool and publishes per-slot indirect draw
//! args — no CPU meshing, no Bevy meshes, no per-chunk buffers or bind groups
//! (see `pool.rs` for the buffers and `world_draw.rs` for the draw).

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

use crate::chunk::Blocks;
use crate::pool::{ChunkPools, ExtractedPoolOps};

/// Keeps the atlas texture asset alive for the session (the render world
/// resolves it through `world_draw::ExtractedAtlas`).
#[derive(Resource)]
pub struct AtlasAssets {
    #[allow(dead_code)]
    pub texture: Handle<Image>,
}

/// The block-faces table buffer (`vec4<u32>` rows), extracted to the render world.
#[derive(Resource, Clone, ExtractResource)]
pub struct FacesTable(pub Handle<ShaderBuffer>);

pub struct GpuMeshPlugin;

impl Plugin for GpuMeshPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractResourcePlugin::<FacesTable>::default())
            .add_systems(Startup, setup_gpu_assets);

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .add_systems(RenderStartup, init_pipeline)
            .add_systems(Render, prepare_jobs.in_set(RenderSystems::PrepareBindGroups))
            // Bevy 0.19 replaced the render graph with schedules: passes are
            // plain systems in the root `RenderGraph` schedule, ordered by
            // system relations rather than node edges. Meshing must land
            // before the cameras draw.
            .add_systems(
                RenderGraph,
                voxel_mesh_pass.in_set(RenderGraphSystems::Render).before(camera_driver),
            );
    }
}

/// Build the atlas texture and the faces-table buffer.
fn setup_gpu_assets(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
    blocks: Res<Blocks>,
) {
    // Nearest filtering comes from `ImagePlugin::default_nearest()` in `main`,
    // not from settings here: the UI loads this same path too, and per-load
    // settings only apply to whichever `load` happens to run first.
    let texture = asset_server.load("blocks.png");

    let faces: Vec<UVec4> = blocks.0.faces_table().into_iter().map(UVec4::from_array).collect();
    let faces_buf = buffers.add(ShaderBuffer::from(faces));

    commands.insert_resource(crate::world_draw::ExtractedAtlas(texture.clone()));
    commands.insert_resource(AtlasAssets { texture });
    commands.insert_resource(FacesTable(faces_buf));
}

// ---------- Render world ----------

#[derive(Resource)]
pub(crate) struct VoxelMeshPipeline {
    layout: BindGroupLayoutDescriptor,
    clear: CachedComputePipelineId,
    mesh: CachedComputePipelineId,
    finalize: CachedComputePipelineId,
}

/// This frame's remesh batch: the jobs buffer holds the mesh-slot ids, the
/// bind group binds it with the pools.
#[derive(Resource, Default)]
pub(crate) struct VoxelMeshJobs {
    jobs: Option<RawBufferVec<u32>>,
    bind_group: Option<BindGroup>,
    count: u32,
}

fn init_pipeline(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    pipeline_cache: Res<PipelineCache>,
) {
    let layout = BindGroupLayoutDescriptor::new(
        "voxel_mesh_layout",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                storage_buffer_read_only_sized(false, None), // voxel pool
                storage_buffer_sized(false, None),           // quad pool (rw)
                storage_buffer_read_only_sized(false, None), // block faces
                storage_buffer_sized(false, None),           // indirect args (rw, atomic)
                storage_buffer_read_only_sized(false, None), // jobs
            ),
        ),
    );
    let shader = asset_server.load("shaders/voxel_mesh.wgsl");
    let pipe = |entry: &'static str| ComputePipelineDescriptor {
        label: Some(format!("voxel_{entry}").into()),
        layout: vec![layout.clone()],
        shader: shader.clone(),
        entry_point: Some(entry.into()),
        ..default()
    };
    let clear = pipeline_cache.queue_compute_pipeline(pipe("clear_counter"));
    let mesh = pipeline_cache.queue_compute_pipeline(pipe("mesh_slice"));
    let finalize = pipeline_cache.queue_compute_pipeline(pipe("finalize_mesh"));
    commands.insert_resource(VoxelMeshPipeline { layout, clear, mesh, finalize });
    commands.insert_resource(VoxelMeshJobs::default());
}

/// Upload this frame's dirty-slot list and build the batch bind group.
#[allow(clippy::too_many_arguments)]
fn prepare_jobs(
    mut jobs: ResMut<VoxelMeshJobs>,
    mut ops: ResMut<ExtractedPoolOps>,
    pipeline: Res<VoxelMeshPipeline>,
    pools: Option<Res<ChunkPools>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    pipeline_cache: Res<PipelineCache>,
    faces: Option<Res<FacesTable>>,
    buffers: Res<RenderAssets<GpuShaderBuffer>>,
) {
    jobs.bind_group = None;
    jobs.count = 0;
    let Some(pools) = pools else { return };
    let Some(faces) = faces else { return };
    if ops.1.is_empty() {
        return;
    }
    let Some(faces_buf) = buffers.get(&faces.0) else {
        return; // faces table not resident yet; the dirty list carries over
    };
    // Never consume jobs the node can't dispatch: during startup the compute
    // pipelines compile asynchronously, and a batch taken before they're
    // ready would be silently dropped — chunks that never mesh (the join
    // burst races pipeline compilation on warm servers).
    if pipeline_cache.get_compute_pipeline(pipeline.clear).is_none()
        || pipeline_cache.get_compute_pipeline(pipeline.mesh).is_none()
        || pipeline_cache.get_compute_pipeline(pipeline.finalize).is_none()
    {
        return;
    }
    let dirty = std::mem::take(&mut ops.1);

    let jobs_vec = jobs.jobs.get_or_insert_with(|| RawBufferVec::new(BufferUsages::STORAGE));
    jobs_vec.clear();
    for slot in &dirty {
        jobs_vec.push(*slot);
    }
    jobs_vec.write_buffer(&render_device, &render_queue);

    let layout = pipeline_cache.get_bind_group_layout(&pipeline.layout);
    let bind_group = render_device.create_bind_group(
        None,
        &layout,
        &BindGroupEntries::sequential((
            pools.voxels.as_entire_buffer_binding(),
            pools.quads.as_entire_buffer_binding(),
            faces_buf.buffer.as_entire_buffer_binding(),
            pools.indirect.as_entire_buffer_binding(),
            jobs.jobs.as_ref().unwrap().buffer().unwrap().as_entire_buffer_binding(),
        )),
    );
    jobs.bind_group = Some(bind_group);
    jobs.count = dirty.len() as u32;
}

/// Greedy-mesh every dirty slot into the shared quad pool.
pub(crate) fn voxel_mesh_pass(
    mut render_context: RenderContext,
    pipeline_cache: Res<PipelineCache>,
    pipeline: Res<VoxelMeshPipeline>,
    jobs: Option<Res<VoxelMeshJobs>>,
) {
    let Some(jobs) = jobs else { return };
    let Some(bind_group) = &jobs.bind_group else {
        return;
    };
    let (Some(clear), Some(mesh), Some(finalize)) = (
        pipeline_cache.get_compute_pipeline(pipeline.clear),
        pipeline_cache.get_compute_pipeline(pipeline.mesh),
        pipeline_cache.get_compute_pipeline(pipeline.finalize),
    ) else {
        return;
    };

    let mut pass = render_context
        .command_encoder()
        .begin_compute_pass(&ComputePassDescriptor { label: Some("voxel_mesh"), ..default() });
    pass.set_bind_group(0, bind_group, &[]);
    pass.set_pipeline(clear);
    pass.dispatch_workgroups(jobs.count, 1, 1);
    pass.set_pipeline(mesh);
    pass.dispatch_workgroups(3, 33, jobs.count);
    pass.set_pipeline(finalize);
    pass.dispatch_workgroups(jobs.count, 1, 1);
}
