//! GPU frustum culling + the demand queue (stream pipeline phase 4).
//!
//! Every frame a compute pass frustum-tests all resident mesh slots and writes
//! their indirect `instance_count`s (replacing Bevy's per-entity CPU culling,
//! which died with the per-chunk entities), then scans the desired window
//! around the camera for unmapped chunks and appends 16-byte demand records —
//! the GPU asking the CPU for chunks. The demand buffer is read back
//! asynchronously (`bevy::render::gpu_readback`) into [`DemandedChunks`];
//! records repeat every frame until fulfilled, so consumers are self-healing.

use bevy::camera::primitives::Frustum;
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_graph::{self, RenderGraph, RenderLabel};
use bevy::render::render_resource::binding_types::{
    storage_buffer_read_only_sized, storage_buffer_sized, uniform_buffer_sized,
};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries, Buffer,
    BufferDescriptor, BufferUsages, CachedComputePipelineId, ComputePassDescriptor,
    ComputePipelineDescriptor, PipelineCache, ShaderStages,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue};
use bevy::render::storage::{GpuShaderStorageBuffer, ShaderStorageBuffer};
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};
use soils_protocol::CHUNK_BIT;

use crate::player::{Player, Streaming};
use crate::pool::ChunkPools;

/// Capacity of the demand buffer (records); must match cull_demand.wgsl.
const DEMAND_CAP: usize = 8192;
/// Demand buffer bytes: 16-byte header + records.
const DEMAND_BYTES: usize = 16 + DEMAND_CAP * 16;

/// The frustum + window the cull pass tests, mirrored from the camera each
/// frame (main world) and uploaded as a uniform.
#[derive(Resource, Clone, Copy, ExtractResource)]
pub struct CullParams {
    pub planes: [Vec4; 6],
    pub camera_chunk: IVec3,
    pub radius: i32,
}

impl Default for CullParams {
    fn default() -> Self {
        Self { planes: [Vec4::ZERO; 6], camera_chunk: IVec3::ZERO, radius: 0 }
    }
}

impl CullParams {
    fn as_bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        for (i, p) in self.planes.iter().enumerate() {
            for (j, v) in p.to_array().iter().enumerate() {
                b[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        for (j, v) in [self.camera_chunk.x, self.camera_chunk.y, self.camera_chunk.z, self.radius]
            .iter()
            .enumerate()
        {
            b[96 + j * 4..96 + j * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
}

/// The GPU's chunk wishlist from the last completed readback: unmapped
/// positions in the desired window, nearest-first. Phase 6 turns these into
/// gen/stream residency; until then they are informational.
#[derive(Resource, Default)]
pub struct DemandedChunks {
    pub positions: Vec<IVec3>,
    /// True count the GPU reported (may exceed what fit in the buffer).
    pub total: u32,
}

/// Handle to the demand buffer asset (main world, for the Readback entity).
#[derive(Resource, Clone, ExtractResource)]
pub struct DemandBuffer(pub Handle<ShaderStorageBuffer>);

pub struct CullPlugin;

impl Plugin for CullPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CullParams>()
            .init_resource::<DemandedChunks>()
            .add_plugins(ExtractResourcePlugin::<CullParams>::default())
            .add_plugins(ExtractResourcePlugin::<DemandBuffer>::default())
            .add_systems(Startup, setup_demand_buffer)
            // After Bevy refreshes camera frusta so the cull sees THIS frame's
            // view (reading in Update lags a frame and visibly races
            // teleports/turns).
            .add_systems(
                PostUpdate,
                update_cull_params
                    .after(bevy::camera::visibility::VisibilitySystems::UpdateFrusta),
            );

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app
            .add_systems(
                RenderStartup,
                (
                    init_pipeline,
                    // The mesher's node must exist before our edge references it.
                    add_render_graph_node.after(crate::gpu_mesh::add_render_graph_node),
                ),
            )
            .add_systems(Render, prepare_cull.in_set(RenderSystems::PrepareBindGroups));
    }
}

fn setup_demand_buffer(mut commands: Commands, mut buffers: ResMut<Assets<ShaderStorageBuffer>>) {
    let mut buf = ShaderStorageBuffer::with_size(DEMAND_BYTES, Default::default());
    buf.buffer_description.usage = BufferUsages::STORAGE | BufferUsages::COPY_SRC | BufferUsages::COPY_DST;
    let handle = buffers.add(buf);
    commands.insert_resource(DemandBuffer(handle.clone()));
    // One persistent readback: fires ReadbackComplete every frame with a
    // coherent post-scan snapshot (Bevy encodes readback copies after the
    // render graph in the same submission).
    commands.spawn(Readback::buffer(handle)).observe(on_demands);
}

fn on_demands(event: On<ReadbackComplete>, mut demanded: ResMut<DemandedChunks>) {
    let bytes: &[u8] = &event.data;
    if bytes.len() < 16 {
        return;
    }
    let total = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let n = (total as usize).min(DEMAND_CAP).min((bytes.len() - 16) / 16);
    let mut records: Vec<(u32, IVec3)> = (0..n)
        .map(|i| {
            let b = &bytes[16 + i * 16..32 + i * 16];
            let v = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
            (u32::from_le_bytes(b[12..16].try_into().unwrap()), IVec3::new(v(0), v(4), v(8)))
        })
        .collect();
    records.sort_unstable_by_key(|(d2, _)| *d2);
    demanded.total = total;
    demanded.positions = records.into_iter().map(|(_, p)| p).collect();
}

/// Mirror the camera frustum + window into [`CullParams`] (main world).
fn update_cull_params(
    mut params: ResMut<CullParams>,
    streaming: Res<Streaming>,
    camera: Query<(&Frustum, &Transform), With<Player>>,
) {
    let Ok((frustum, transform)) = camera.single() else { return };
    for (i, hs) in frustum.half_spaces.iter().enumerate() {
        params.planes[i] = hs.normal_d();
    }
    let p = transform.translation.floor().as_ivec3();
    params.camera_chunk = IVec3::new(p.x >> CHUNK_BIT, p.y >> CHUNK_BIT, p.z >> CHUNK_BIT);
    params.radius = streaming.load_radius;
}

// ---------------- Render world ----------------

#[derive(Resource)]
struct CullPipeline {
    layout: BindGroupLayoutDescriptor,
    cull: CachedComputePipelineId,
    scan: CachedComputePipelineId,
    uniform: Buffer,
}

#[derive(Resource, Default)]
struct CullJob {
    bind_group: Option<BindGroup>,
    radius: i32,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
struct CullLabel;

fn init_pipeline(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    device: Res<RenderDevice>,
    pipeline_cache: Res<PipelineCache>,
) {
    let layout = BindGroupLayoutDescriptor::new(
        "cull_layout",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                uniform_buffer_sized(false, None),           // params
                storage_buffer_read_only_sized(false, None), // mesh_info
                storage_buffer_sized(false, None),           // indirect (rw)
                storage_buffer_read_only_sized(false, None), // desc
                storage_buffer_read_only_sized(false, None), // slot table
                storage_buffer_sized(false, None),           // demands (rw)
            ),
        ),
    );
    let shader = asset_server.load("shaders/cull_demand.wgsl");
    let pipe = |entry: &'static str| ComputePipelineDescriptor {
        label: Some(format!("cull_{entry}").into()),
        layout: vec![layout.clone()],
        shader: shader.clone(),
        entry_point: Some(entry.into()),
        ..default()
    };
    let cull = pipeline_cache.queue_compute_pipeline(pipe("cull"));
    let scan = pipeline_cache.queue_compute_pipeline(pipe("demand_scan"));
    let uniform = device.create_buffer(&BufferDescriptor {
        label: Some("cull_params"),
        size: 128,
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    commands.insert_resource(CullPipeline { layout, cull, scan, uniform });
    commands.insert_resource(CullJob::default());
}

fn add_render_graph_node(mut render_graph: ResMut<RenderGraph>) {
    render_graph.add_node(CullLabel, CullNode);
    // After the mesher (finalize writes instance_count 1; cull's verdict must
    // win) and before the camera draws.
    render_graph.add_node_edge(crate::gpu_mesh::VoxelMeshLabel, CullLabel);
    render_graph.add_node_edge(CullLabel, bevy::render::graph::CameraDriverLabel);
}

#[allow(clippy::too_many_arguments)]
fn prepare_cull(
    job: Option<ResMut<CullJob>>,
    pipeline: Option<Res<CullPipeline>>,
    pools: Option<Res<ChunkPools>>,
    params: Option<Res<CullParams>>,
    demand: Option<Res<DemandBuffer>>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    buffers: Res<RenderAssets<GpuShaderStorageBuffer>>,
    pipeline_cache: Res<PipelineCache>,
) {
    let (Some(mut job), Some(pipeline)) = (job, pipeline) else { return };
    job.bind_group = None;
    let (Some(pools), Some(params), Some(demand)) = (pools, params, demand) else { return };
    let Some(demand_buf) = buffers.get(&demand.0) else { return };
    if params.radius == 0 {
        return; // no camera yet
    }
    queue.write_buffer(&pipeline.uniform, 0, &params.as_bytes());
    let layout = pipeline_cache.get_bind_group_layout(&pipeline.layout);
    job.bind_group = Some(device.create_bind_group(
        None,
        &layout,
        &BindGroupEntries::sequential((
            pipeline.uniform.as_entire_buffer_binding(),
            pools.mesh_info.as_entire_buffer_binding(),
            pools.indirect.as_entire_buffer_binding(),
            pools.desc.as_entire_buffer_binding(),
            pools.table.as_entire_buffer_binding(),
            demand_buf.buffer.as_entire_buffer_binding(),
        )),
    ));
    job.radius = params.radius;
}

struct CullNode;

impl render_graph::Node for CullNode {
    fn run(
        &self,
        _graph: &mut render_graph::RenderGraphContext,
        render_context: &mut RenderContext,
        world: &World,
    ) -> Result<(), render_graph::NodeRunError> {
        let pipeline_cache = world.resource::<PipelineCache>();
        let pipeline = world.resource::<CullPipeline>();
        let Some(job) = world.get_resource::<CullJob>() else { return Ok(()) };
        let Some(bind_group) = &job.bind_group else { return Ok(()) };
        let (Some(cull), Some(scan)) = (
            pipeline_cache.get_compute_pipeline(pipeline.cull),
            pipeline_cache.get_compute_pipeline(pipeline.scan),
        ) else {
            return Ok(());
        };
        // Reset the demand counter, then cull + scan in one pass.
        let demand = world.resource::<RenderAssets<GpuShaderStorageBuffer>>();
        if let Some(db) = world.get_resource::<DemandBuffer>()
            && let Some(buf) = demand.get(&db.0)
        {
            render_context.command_encoder().clear_buffer(&buf.buffer, 0, Some(16));
        }
        let mut pass = render_context
            .command_encoder()
            .begin_compute_pass(&ComputePassDescriptor { label: Some("cull_demand"), ..default() });
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_pipeline(cull);
        pass.dispatch_workgroups((crate::pool::N_MESH).div_ceil(64), 1, 1);
        pass.set_pipeline(scan);
        let side = (job.radius * 2 + 1) as u32;
        let groups = side.div_ceil(4);
        pass.dispatch_workgroups(groups, groups, groups);
        Ok(())
    }
}
