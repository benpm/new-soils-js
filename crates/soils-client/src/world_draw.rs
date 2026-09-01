//! The pooled terrain draw: ONE binned `Opaque3d` phase item covering every
//! resident chunk, executed as a single `multi_draw_indirect` over the pooled
//! indirect-args table (zero-instance args skip culled/empty slots). Replaces
//! the per-chunk `Material` + draw-function-patching architecture: no
//! per-chunk entities, materials, or bind groups exist in the render world.

use bevy::camera::visibility::{self, NoFrustumCulling, VisibilityClass};
use bevy::core_pipeline::core_3d::{CORE_3D_DEPTH_FORMAT, Opaque3d, Opaque3dBatchSetKey, Opaque3dBinKey};
use bevy::ecs::query::ROQueryItem;
use bevy::ecs::system::SystemParamItem;
use bevy::ecs::system::lifetimeless::SRes;
use bevy::pbr::{
    MeshPipelineSystems,
    MeshPipelineViewLayoutKey, MeshPipelineViewLayouts, SetMeshViewBindGroup,
    SetMeshViewBindingArrayBindGroup,
};
use bevy::prelude::*;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_phase::{
    AddRenderCommand, BinnedRenderPhaseType, DrawFunctions, InputUniformIndex, PhaseItem,
    RenderCommand, RenderCommandResult, SetItemPipeline, TrackedRenderPass,
    ViewBinnedRenderPhases,
};
use bevy::render::render_resource::binding_types::{
    sampler, storage_buffer_read_only_sized, texture_2d_array, uniform_buffer_sized,
};
use bevy::render::render_resource::{
    BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
    BindingResource, Buffer, BufferDescriptor, BufferUsages, Canonical, ColorTargetState,
    ColorWrites, CompareFunction, DepthStencilState, Face, FragmentState, PipelineCache,
    RenderPipeline, RenderPipelineDescriptor, SamplerBindingType, ShaderStages, Specializer,
    SpecializerKey, TextureFormat, TextureSampleType, Variants, VertexState,
};
use bevy::render::renderer::{RenderDevice, RenderQueue};
use bevy::render::texture::GpuImage;
use bevy::render::view::{ExtractedView, RenderVisibleEntities};
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};

use crate::gi::GiAssets;
use crate::light::SkyTerm;
use crate::material::{FOG_COLOR, FOG_DENSITY, TERRAIN_BRIGHTNESS};
use crate::pool::{ChunkPools, N_MESH};
use crate::pause::RenderToggles;

/// The single main-world entity the terrain draw hangs off.
#[derive(Clone, Component, ExtractComponent)]
#[require(VisibilityClass)]
#[component(on_add = visibility::add_visibility_class::<TerrainDraw>)]
pub struct TerrainDraw;

/// Per-frame shading params (the old per-chunk `AtlasParams`, world-global and
/// written into one uniform buffer). Mirrors `WorldParams` in atlas.wgsl.
#[derive(Resource, Clone, Copy, ExtractResource)]
pub struct TerrainParams {
    pub ambient_occlusion: f32,
    pub brightness: f32,
    pub fog_density: f32,
    pub fog_color: Vec3,
    pub gi_origin: Vec3,
    pub gi_enabled: f32,
    pub sky_term: f32,
    pub light_enabled: f32,
    /// Debug wireframe overlay (F2): outline every greedy quad and dim the
    /// faces behind it. See `debugviz.rs`.
    pub wireframe: f32,
}

impl Default for TerrainParams {
    fn default() -> Self {
        Self {
            ambient_occlusion: 1.0,
            brightness: TERRAIN_BRIGHTNESS,
            fog_density: FOG_DENSITY,
            fog_color: FOG_COLOR,
            gi_origin: Vec3::ZERO,
            gi_enabled: 0.0,
            sky_term: TERRAIN_BRIGHTNESS,
            light_enabled: 1.0,
            wireframe: 0.0,
        }
    }
}

impl TerrainParams {
    /// std140/std430 uniform bytes matching atlas.wgsl's WorldParams: three
    /// f32 + pad, fog_color vec3 + gi_enabled-slot alignment quirks — laid
    /// out explicitly to avoid guessing: WGSL packs
    /// { f32, f32, f32, vec3 (align 16), vec3, f32, f32, f32 } as
    /// ao@0 brightness@4 fog_density@8 fog_color@16 gi_origin@32 gi_enabled@44
    /// sky_term@48 light_enabled@52 wireframe@56, size 64.
    fn as_bytes(&self) -> [u8; 64] {
        let mut b = [0u8; 64];
        let mut put = |off: usize, v: f32| b[off..off + 4].copy_from_slice(&v.to_le_bytes());
        put(0, self.ambient_occlusion);
        put(4, self.brightness);
        put(8, self.fog_density);
        put(16, self.fog_color.x);
        put(20, self.fog_color.y);
        put(24, self.fog_color.z);
        put(32, self.gi_origin.x);
        put(36, self.gi_origin.y);
        put(40, self.gi_origin.z);
        put(44, self.gi_enabled);
        put(48, self.sky_term);
        put(52, self.light_enabled);
        put(56, self.wireframe);
        b
    }
}

/// Keep the world-global shading params in step with the toggles/sky/GI state
/// each frame (main world; extracted by value).
pub fn update_terrain_params(
    toggles: Res<RenderToggles>,
    viz: Res<crate::debugviz::DebugViz>,
    sky: Res<SkyTerm>,
    gi: Option<Res<GiAssets>>,
    mut params: ResMut<TerrainParams>,
) {
    params.ambient_occlusion = if toggles.ao { 1.0 } else { 0.0 };
    params.fog_density = if toggles.fog { FOG_DENSITY } else { 0.0 };
    params.light_enabled = if toggles.light { 1.0 } else { 0.0 };
    params.wireframe = if viz.wireframe_active() { 1.0 } else { 0.0 };
    params.sky_term = sky.0;
    if let Some(gi) = gi {
        let (origin, enabled) = gi.apply_params();
        params.gi_origin = origin;
        params.gi_enabled = enabled;
    }
}

pub struct WorldDrawPlugin;

impl Plugin for WorldDrawPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TerrainParams>()
            .add_plugins(ExtractComponentPlugin::<TerrainDraw>::default())
            .add_plugins(ExtractResourcePlugin::<TerrainParams>::default())
            .add_plugins(ExtractResourcePlugin::<ExtractedAtlas>::default())
            .add_systems(Startup, spawn_terrain_entity)
            .add_systems(Update, update_terrain_params);

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return };
        render_app
            .add_render_command::<Opaque3d, DrawTerrainCommands>()
            .add_systems(RenderStartup, init_terrain_pipeline.after(MeshPipelineSystems))
            .add_systems(
                Render,
                (
                    prepare_world_bind_group.in_set(RenderSystems::PrepareBindGroups),
                    queue_terrain.in_set(RenderSystems::Queue),
                ),
            );
    }
}

fn spawn_terrain_entity(mut commands: Commands) {
    commands.spawn((TerrainDraw, Visibility::default(), Transform::default(), NoFrustumCulling));
}

// ---------------- Pipeline ----------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, SpecializerKey)]
pub struct TerrainPipelineKey {
    msaa: Msaa,
    /// The format this view renders to. Bevy 0.19 deprecated
    /// `TextureFormat::bevy_default()` in favour of sourcing the real format
    /// from `ExtractedView::target_format` and specializing on it, which also
    /// covers targets that aren't the default sRGB surface.
    format: TextureFormat,
    /// The camera renders the `Atmosphere`, which grows the view bind group.
    atmosphere: bool,
}

pub struct TerrainSpecializer {
    view_layouts: MeshPipelineViewLayouts,
    world_layout: BindGroupLayoutDescriptor,
}

impl Specializer<RenderPipeline> for TerrainSpecializer {
    type Key = TerrainPipelineKey;

    fn specialize(
        &self,
        key: Self::Key,
        descriptor: &mut RenderPipelineDescriptor,
    ) -> Result<Canonical<Self::Key>, BevyError> {
        let mut view_key = MeshPipelineViewLayoutKey::empty();
        if key.msaa.samples() > 1 {
            view_key |= MeshPipelineViewLayoutKey::MULTISAMPLED;
        }
        if key.atmosphere {
            view_key |= MeshPipelineViewLayoutKey::ATMOSPHERE;
        }
        let layouts = self.view_layouts.get_view_layout(view_key);
        descriptor.layout = vec![
            layouts.main_layout.clone(),
            layouts.binding_array_layout.clone(),
            self.world_layout.clone(),
        ];
        descriptor.multisample.count = key.msaa.samples();
        if let Some(fragment) = &mut descriptor.fragment {
            fragment.targets = vec![Some(ColorTargetState {
                format: key.format,
                blend: None,
                write_mask: ColorWrites::ALL,
            })];
        }
        Ok(key)
    }
}

#[derive(Resource)]
pub struct TerrainPipeline {
    pub variants: Variants<RenderPipeline, TerrainSpecializer>,
    pub world_layout: BindGroupLayoutDescriptor,
}

/// Build the terrain pipeline.
///
/// This runs in `RenderStartup` after [`MeshPipelineSystems`] rather than as a
/// `FromWorld` in `Plugin::finish`: Bevy 0.19 moved `MeshPipelineViewLayouts`
/// creation into `RenderStartup`, so reading it at plugin-finish time panics
/// with "resource does not exist".
fn init_terrain_pipeline(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    view_layouts: Res<MeshPipelineViewLayouts>,
) {
    let terrain = {
        let shader = asset_server.load("shaders/atlas.wgsl");
        let view_layouts = view_layouts.clone();

        let world_layout = BindGroupLayoutDescriptor::new(
            "terrain_world_layout",
            &BindGroupLayoutEntries::sequential(
                ShaderStages::VERTEX_FRAGMENT,
                (
                    storage_buffer_read_only_sized(false, None), // 0 quads
                    storage_buffer_read_only_sized(false, None), // 1 mesh_info
                    storage_buffer_read_only_sized(false, None), // 2 desc
                    storage_buffer_read_only_sized(false, None), // 3 light pool
                    storage_buffer_read_only_sized(false, None), // 4 slot table
                    storage_buffer_read_only_sized(false, None), // 5 gi probes
                    texture_2d_array(TextureSampleType::Float { filterable: true }), // 6 block textures
                    sampler(SamplerBindingType::Filtering),      // 7
                    uniform_buffer_sized(false, None),           // 8 world params
                ),
            ),
        );

        let base = RenderPipelineDescriptor {
            label: Some("terrain_world_pipeline".into()),
            vertex: VertexState { shader: shader.clone(), buffers: vec![], ..default() },
            fragment: Some(FragmentState {
                shader,
                // Placeholder: `specialize` always overwrites this with the
                // view's real `target_format`.
                targets: vec![Some(ColorTargetState {
                    format: TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
                ..default()
            }),
            depth_stencil: Some(DepthStencilState {
                format: CORE_3D_DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                // Bevy uses reverse-Z.
                depth_compare: Some(CompareFunction::GreaterEqual),
                stencil: default(),
                bias: default(),
            }),
            ..default()
        };
        let mut base = base;
        base.primitive.cull_mode = Some(Face::Back);

        TerrainPipeline {
            variants: Variants::new(
                TerrainSpecializer { view_layouts, world_layout: world_layout.clone() },
                base,
            ),
            world_layout,
        }
    };
    commands.insert_resource(terrain);
}

// ---------------- Bind group + uniform ----------------

/// The static world bind group (group 2) + the uniform buffer behind it.
#[derive(Resource)]
pub struct WorldBindGroup {
    pub bind_group: BindGroup,
    pub uniform: Buffer,
}

fn prepare_world_bind_group(
    mut commands: Commands,
    existing: Option<Res<WorldBindGroup>>,
    pipeline: Option<Res<TerrainPipeline>>,
    pools: Option<Res<ChunkPools>>,
    pipeline_cache: Res<PipelineCache>,
    device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    atlas: Option<Res<ExtractedAtlas>>,
    gi: Option<Res<GiAssets>>,
    images: Res<RenderAssets<GpuImage>>,
    ssbos: Res<RenderAssets<bevy::render::storage::GpuShaderBuffer>>,
    params: Option<Res<TerrainParams>>,
) {
    // The uniform is rewritten every frame; the bind group is built once (all
    // its buffers live for the whole session).
    if let (Some(existing), Some(params)) = (&existing, &params) {
        render_queue.write_buffer(&existing.uniform, 0, &params.as_bytes());
        return;
    }
    let (Some(pipeline), Some(pools), Some(atlas), Some(gi), Some(params)) =
        (pipeline, pools, atlas, gi, params)
    else {
        return;
    };
    // Built once, so the block texture array must be resident — and actually
    // an array: `ExtractedAtlas` only appears after gpu_mesh promotes the
    // stacked image, but guard against a stale 2D upload anyway, since a 2D
    // view bound to a texture_2d_array layout is a validation panic. A
    // missing/unloadable blocks_mega.png means no terrain draws at all.
    let Some(atlas_gpu) = images.get(&atlas.0) else { return };
    if atlas_gpu.texture.depth_or_array_layers() != crate::gpu_mesh::MEGA_LAYERS {
        return;
    }
    let Some(gi_probes) = ssbos.get(&gi.irradiance()) else { return };

    let uniform = device.create_buffer(&BufferDescriptor {
        label: Some("terrain_world_params"),
        size: 64,
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    render_queue.write_buffer(&uniform, 0, &params.as_bytes());

    let layout = pipeline_cache.get_bind_group_layout(&pipeline.world_layout);
    let bind_group = device.create_bind_group(
        Some("terrain_world_bg"),
        &layout,
        &BindGroupEntries::sequential((
            pools.quads.as_entire_buffer_binding(),
            pools.mesh_info.as_entire_buffer_binding(),
            pools.desc.as_entire_buffer_binding(),
            pools.light.as_entire_buffer_binding(),
            pools.table.as_entire_buffer_binding(),
            gi_probes.buffer.as_entire_buffer_binding(),
            BindingResource::TextureView(&atlas_gpu.texture_view),
            BindingResource::Sampler(&atlas_gpu.sampler),
            uniform.as_entire_buffer_binding(),
        )),
    );
    commands.insert_resource(WorldBindGroup { bind_group, uniform });
}

/// The block texture array handle, extracted so the render world can resolve
/// its GpuImage. Inserted by `gpu_mesh::promote_block_textures` once the
/// image is an array.
#[derive(Resource, Clone, ExtractResource)]
pub struct ExtractedAtlas(pub Handle<Image>);

// ---------------- Queue + draw ----------------

type DrawTerrainCommands = (
    SetItemPipeline,
    SetMeshViewBindGroup<0>,
    SetMeshViewBindingArrayBindGroup<1>,
    SetWorldBindGroup<2>,
    MultiDrawTerrain,
);

struct SetWorldBindGroup<const I: usize>;

impl<P: PhaseItem, const I: usize> RenderCommand<P> for SetWorldBindGroup<I> {
    type Param = SRes<WorldBindGroup>;
    type ViewQuery = ();
    type ItemQuery = ();

    fn render<'w>(
        _: &P,
        _: ROQueryItem<'w, '_, Self::ViewQuery>,
        _: Option<ROQueryItem<'w, '_, Self::ItemQuery>>,
        world_bg: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        pass.set_bind_group(I, &world_bg.into_inner().bind_group, &[]);
        RenderCommandResult::Success
    }
}

struct MultiDrawTerrain;

impl<P: PhaseItem> RenderCommand<P> for MultiDrawTerrain {
    type Param = SRes<ChunkPools>;
    type ViewQuery = ();
    type ItemQuery = ();

    fn render<'w>(
        _: &P,
        _: ROQueryItem<'w, '_, Self::ViewQuery>,
        _: Option<ROQueryItem<'w, '_, Self::ItemQuery>>,
        pools: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        // Every mesh slot has an args entry; unallocated/culled slots hold
        // instance_count 0 and cost nothing.
        pass.multi_draw_indirect(&pools.into_inner().indirect, 0, N_MESH);
        RenderCommandResult::Success
    }
}

fn queue_terrain(
    pipeline_cache: Res<PipelineCache>,
    mut pipeline: ResMut<TerrainPipeline>,
    world_bg: Option<Res<WorldBindGroup>>,
    mut opaque_phases: ResMut<ViewBinnedRenderPhases<Opaque3d>>,
    draw_functions: Res<DrawFunctions<Opaque3d>>,
    views: Query<(
        &ExtractedView,
        &RenderVisibleEntities,
        &Msaa,
        Has<bevy::pbr::ExtractedAtmosphere>,
    )>,
) {
    if world_bg.is_none() {
        return; // pools/atlas not ready yet
    }
    let draw_terrain = draw_functions.read().id::<DrawTerrainCommands>();

    for (view, visible, msaa, atmosphere) in views.iter() {
        let Some(phase) = opaque_phases.get_mut(&view.retained_view_entity) else { continue };
        // 0.19 split the visible-entity list into CPU- and GPU-culled tables.
        // The terrain entity carries `NoFrustumCulling` (not `NoCpuCulling`),
        // so it lives in the CPU-culled list.
        let Some(class) = visible.get::<TerrainDraw>() else { continue };
        for &entity in &class.entities_cpu_culling {
            let Ok(pipeline_id) = pipeline.variants.specialize(
                &pipeline_cache,
                TerrainPipelineKey { msaa: *msaa, format: view.target_format, atmosphere },
            ) else {
                continue;
            };
            phase.add(
                Opaque3dBatchSetKey {
                    draw_function: draw_terrain,
                    pipeline: pipeline_id,
                    material_bind_group_index: None,
                    lightmap_slab: None,
                    // Non-mesh item: slab 0 is correct because the terrain is
                    // a single multi-drawn entity (see MeshSlabs docs).
                    slabs: default(),
                },
                Opaque3dBinKey { asset_id: AssetId::<Mesh>::invalid().untyped() },
                entity,
                InputUniformIndex::default(),
                BinnedRenderPhaseType::NonMesh,
            );
        }
    }
}
