//! GPU-resident chunk meshing. A compute shader (`voxel_mesh.wgsl`) fills a
//! per-chunk quad storage buffer that the chunk's `ChunkMeshMaterial` vertex-pulls
//! — no CPU meshing, no Bevy mesh attributes, no readback.

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageLoaderSettings, ImageSampler};
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
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
use bevy::camera::visibility::NoFrustumCulling;
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};
use soils_protocol::{CHUNK_SIZE, ChunkVolume};

use crate::chunk::{Blocks, VoxelChunk};
use crate::material::{AtlasParams, ChunkMeshMaterial};

/// Max greedy quads per chunk (must match `MAX_QUADS` in voxel_mesh.wgsl).
pub const MAX_QUADS: u32 = 8192;
/// Bytes per `QuadGpu` (must match the std430 layout in the shaders).
const QUAD_BYTES: u64 = 80;
/// Output buffer = 16-byte header + MAX_QUADS quads.
const QUAD_BUFFER_BYTES: u64 = 16 + MAX_QUADS as u64 * QUAD_BYTES;
/// Vertices the dummy mesh issues per chunk (6 per quad).
const DUMMY_VERTS: usize = MAX_QUADS as usize * 6;
/// Frames a chunk stays dirty after a change (gives buffers time to upload and
/// the compute to run; re-meshing is idempotent).
const PENDING_FRAMES: u8 = 4;

/// Shared assets for chunk rendering: the atlas texture and the dummy draw mesh.
#[derive(Resource)]
pub struct AtlasAssets {
    pub texture: Handle<Image>,
    pub dummy_mesh: Handle<Mesh>,
}

/// The block-faces table buffer (`vec4<u32>` rows), extracted to the render world.
#[derive(Resource, Clone, ExtractResource)]
pub struct FacesTable(pub Handle<ShaderStorageBuffer>);

/// Per-chunk GPU state: input voxels + output quads + a dirty countdown.
#[derive(Component, Clone, ExtractComponent)]
pub struct GpuChunk {
    pub voxels: Handle<ShaderStorageBuffer>,
    pub quads: Handle<ShaderStorageBuffer>,
    pub pending: u8,
}

pub struct GpuMeshPlugin;

impl Plugin for GpuMeshPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<ChunkMeshMaterial>::default())
            .add_plugins(ExtractComponentPlugin::<GpuChunk>::default())
            .add_plugins(ExtractResourcePlugin::<FacesTable>::default())
            .add_systems(Startup, setup_gpu_assets)
            .add_systems(PostUpdate, tick_gpu_chunks);

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .add_systems(RenderStartup, (init_pipeline, add_render_graph_node))
            .add_systems(Render, prepare_jobs.in_set(RenderSystems::PrepareBindGroups));
    }
}

/// Build the atlas texture, the shared dummy mesh, and the faces-table buffer.
fn setup_gpu_assets(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    blocks: Res<Blocks>,
) {
    let texture = asset_server.load_with_settings("blocks.png", |s: &mut ImageLoaderSettings| {
        s.sampler = ImageSampler::nearest();
    });

    let mut dummy = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
    dummy.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; DUMMY_VERTS]);
    let dummy_mesh = meshes.add(dummy);

    let faces: Vec<UVec4> =
        blocks.0.faces_table().into_iter().map(UVec4::from_array).collect();
    let faces_buf = buffers.add(ShaderStorageBuffer::from(faces));

    commands.insert_resource(AtlasAssets { texture, dummy_mesh });
    commands.insert_resource(FacesTable(faces_buf));
}

/// Decrement each chunk's dirty countdown once per frame (after Update).
fn tick_gpu_chunks(mut chunks: Query<&mut GpuChunk>) {
    for mut gc in &mut chunks {
        if gc.pending > 0 {
            gc.pending -= 1;
        }
    }
}

/// Spawn a fully-rendered GPU chunk entity (voxel + quad buffers, material,
/// dummy mesh). Returns the entity so the caller can register it in `ChunkMap`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_gpu_chunk(
    commands: &mut Commands,
    buffers: &mut Assets<ShaderStorageBuffer>,
    materials: &mut Assets<ChunkMeshMaterial>,
    atlas: &AtlasAssets,
    cpos: IVec3,
    volume: ChunkVolume,
    params: AtlasParams,
    gi_cascade0: Handle<ShaderStorageBuffer>,
) -> Entity {
    let voxels = buffers.add(ShaderStorageBuffer::new(volume.as_bytes(), RenderAssetUsages::default()));
    let quads =
        buffers.add(ShaderStorageBuffer::with_size(QUAD_BUFFER_BYTES as usize, RenderAssetUsages::default()));
    let material = materials.add(ChunkMeshMaterial {
        quads: quads.clone(),
        atlas: atlas.texture.clone(),
        params,
        gi_cascade0,
    });
    let origin = (cpos * CHUNK_SIZE).as_vec3();
    commands
        .spawn((
            VoxelChunk { pos: cpos, volume },
            GpuChunk { voxels, quads, pending: PENDING_FRAMES },
            Mesh3d(atlas.dummy_mesh.clone()),
            MeshMaterial3d(material),
            Transform::from_translation(origin),
            Visibility::default(),
            NoFrustumCulling,
        ))
        .id()
}

/// Re-upload a chunk's voxel buffer and mark it dirty (after a server resend or
/// an edit).
pub fn refresh_gpu_chunk(
    buffers: &mut Assets<ShaderStorageBuffer>,
    gc: &mut GpuChunk,
    volume: &ChunkVolume,
) {
    if let Some(buf) = buffers.get_mut(&gc.voxels) {
        buf.data = Some(volume.as_bytes().to_vec());
    }
    gc.pending = PENDING_FRAMES;
}

// ---------- Render world ----------

#[derive(Resource)]
struct VoxelMeshPipeline {
    layout: BindGroupLayoutDescriptor,
    clear: CachedComputePipelineId,
    mesh: CachedComputePipelineId,
}

#[derive(Resource, Default)]
struct VoxelMeshJobs(Vec<BindGroup>);

#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
struct VoxelMeshLabel;

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
                storage_buffer_read_only_sized(false, None), // voxels
                storage_buffer_sized(false, None),           // quads (read_write)
                storage_buffer_read_only_sized(false, None), // block faces
            ),
        ),
    );
    let shader = asset_server.load("shaders/voxel_mesh.wgsl");
    let clear = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("voxel_clear".into()),
        layout: vec![layout.clone()],
        shader: shader.clone(),
        entry_point: Some("clear_counter".into()),
        ..default()
    });
    let mesh = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("voxel_mesh".into()),
        layout: vec![layout.clone()],
        shader,
        entry_point: Some("mesh_slice".into()),
        ..default()
    });
    commands.insert_resource(VoxelMeshPipeline { layout, clear, mesh });
    commands.insert_resource(VoxelMeshJobs::default());
}

fn add_render_graph_node(mut render_graph: ResMut<RenderGraph>) {
    render_graph.add_node(VoxelMeshLabel, VoxelMeshNode);
    render_graph.add_node_edge(VoxelMeshLabel, bevy::render::graph::CameraDriverLabel);
}

/// Build one compute bind group per dirty chunk whose GPU buffers are ready.
fn prepare_jobs(
    mut jobs: ResMut<VoxelMeshJobs>,
    pipeline: Res<VoxelMeshPipeline>,
    render_device: Res<RenderDevice>,
    pipeline_cache: Res<PipelineCache>,
    faces: Option<Res<FacesTable>>,
    buffers: Res<RenderAssets<GpuShaderStorageBuffer>>,
    chunks: Query<&GpuChunk>,
) {
    jobs.0.clear();
    let Some(faces) = faces else { return };
    let Some(faces_buf) = buffers.get(&faces.0) else { return };
    let layout = pipeline_cache.get_bind_group_layout(&pipeline.layout);

    for gc in &chunks {
        if gc.pending == 0 {
            continue;
        }
        let (Some(vox), Some(quad)) = (buffers.get(&gc.voxels), buffers.get(&gc.quads)) else {
            continue;
        };
        let bind_group = render_device.create_bind_group(
            None,
            &layout,
            &BindGroupEntries::sequential((
                vox.buffer.as_entire_buffer_binding(),
                quad.buffer.as_entire_buffer_binding(),
                faces_buf.buffer.as_entire_buffer_binding(),
            )),
        );
        jobs.0.push(bind_group);
    }
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
        if jobs.0.is_empty() {
            return Ok(());
        }
        let (Some(clear), Some(mesh)) = (
            pipeline_cache.get_compute_pipeline(pipeline.clear),
            pipeline_cache.get_compute_pipeline(pipeline.mesh),
        ) else {
            return Ok(());
        };

        let mut pass = render_context
            .command_encoder()
            .begin_compute_pass(&ComputePassDescriptor { label: Some("voxel_mesh"), ..default() });
        for bind_group in &jobs.0 {
            pass.set_bind_group(0, bind_group, &[]);
            pass.set_pipeline(clear);
            pass.dispatch_workgroups(1, 1, 1);
            pass.set_pipeline(mesh);
            pass.dispatch_workgroups(3, 33, 1);
        }
        Ok(())
    }
}
