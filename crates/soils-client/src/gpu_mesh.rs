//! GPU-resident chunk meshing over the pooled caches. A compute shader
//! (`voxel_mesh.wgsl`) greedy-meshes each dirty mesh slot's voxels into its
//! region of the shared packed-quad pool and publishes per-slot indirect draw
//! args — no CPU meshing, no Bevy meshes, no per-chunk buffers or bind groups
//! (see `pool.rs` for the buffers and `world_draw.rs` for the draw).

use bevy::image::{ImageLoaderSettings, ImageSampler};
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_graph::{self, RenderGraph, RenderLabel};
use bevy::render::render_resource::binding_types::{
    storage_buffer_read_only_sized, storage_buffer_sized,
};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, BufferUsages,
    CachedComputePipelineId, ComputePassDescriptor, ComputePipelineDescriptor, PipelineCache,
    RawBufferVec, ShaderStages,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue};
use bevy::render::storage::{GpuShaderStorageBuffer, ShaderStorageBuffer};
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
pub struct FacesTable(pub Handle<ShaderStorageBuffer>);

pub struct GpuMeshPlugin;

impl Plugin for GpuMeshPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractResourcePlugin::<FacesTable>::default())
            .add_systems(Startup, setup_gpu_assets);

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .add_systems(RenderStartup, (init_pipeline, add_render_graph_node))
            .add_systems(Render, prepare_jobs.in_set(RenderSystems::PrepareBindGroups));
    }
}

/// Build the atlas texture and the faces-table buffer.
fn setup_gpu_assets(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    blocks: Res<Blocks>,
) {
    let texture = asset_server.load_with_settings("blocks.png", |s: &mut ImageLoaderSettings| {
        s.sampler = ImageSampler::nearest();
    });

    let faces: Vec<UVec4> = blocks.0.faces_table().into_iter().map(UVec4::from_array).collect();
    let faces_buf = buffers.add(ShaderStorageBuffer::from(faces));

    commands.insert_resource(crate::world_draw::ExtractedAtlas(texture.clone()));
    commands.insert_resource(AtlasAssets { texture });
    commands.insert_resource(FacesTable(faces_buf));
}

// ---------- Render world ----------

#[derive(Resource)]
struct VoxelMeshPipeline {
    layout: BindGroupLayoutDescriptor,
    clear: CachedComputePipelineId,
    mesh: CachedComputePipelineId,
    finalize: CachedComputePipelineId,
}

/// This frame's remesh batch: the jobs buffer holds the mesh-slot ids, the
/// bind group binds it with the pools.
#[derive(Resource, Default)]
struct VoxelMeshJobs {
    jobs: Option<RawBufferVec<u32>>,
    bind_group: Option<BindGroup>,
    count: u32,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
pub struct VoxelMeshLabel;

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

pub fn add_render_graph_node(mut render_graph: ResMut<RenderGraph>) {
    render_graph.add_node(VoxelMeshLabel, VoxelMeshNode);
    render_graph.add_node_edge(VoxelMeshLabel, bevy::render::graph::CameraDriverLabel);
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
    buffers: Res<RenderAssets<GpuShaderStorageBuffer>>,
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

struct VoxelMeshNode;

impl render_graph::Node for VoxelMeshNode {
    fn run(
        &self,
        _graph: &mut render_graph::RenderGraphContext,
        render_context: &mut RenderContext,
        world: &World,
    ) -> Result<(), render_graph::NodeRunError> {
        let pipeline_cache = world.resource::<PipelineCache>();
        let pipeline = world.resource::<VoxelMeshPipeline>();
        let Some(jobs) = world.get_resource::<VoxelMeshJobs>() else {
            return Ok(());
        };
        let Some(bind_group) = &jobs.bind_group else {
            return Ok(());
        };
        let (Some(clear), Some(mesh), Some(finalize)) = (
            pipeline_cache.get_compute_pipeline(pipeline.clear),
            pipeline_cache.get_compute_pipeline(pipeline.mesh),
            pipeline_cache.get_compute_pipeline(pipeline.finalize),
        ) else {
            return Ok(());
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
        Ok(())
    }
}
