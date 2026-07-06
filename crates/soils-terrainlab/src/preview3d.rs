//! Live 3D terrain preview. A grid mesh is displaced entirely on the GPU: the
//! terrain material's vertex shader — generated from the node graph by
//! [`crate::wgsl_gen::generate_material`] — evaluates `height_out(x,z)` per
//! vertex. Node parameters ride in a storage buffer, so dragging a slider only
//! rewrites that buffer; the shader is regenerated only when the graph
//! *structure* changes (detected by hashing the generated WGSL, which contains
//! no param values). An orbit camera frames the terrain.

use std::hash::{Hash, Hasher};

use bevy::input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin};
use bevy::prelude::*;
use bevy::mesh::{Indices, MeshVertexBufferLayoutRef, PrimitiveTopology};
use bevy::asset::{RenderAssetUsages, uuid_handle};
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;
use bevy::render::storage::ShaderStorageBuffer;
use bevy_egui::{EguiContexts, PrimaryEguiContext};
use soils_worldgen::graph::TerrainGraph;

use crate::wgsl_gen;

/// Preview grid resolution (vertices per side).
const RES: u32 = 160;
/// World span the preview covers (matches the 2D preview), centred on origin.
const SPAN: f32 = 2048.0;
/// Vertical exaggeration of the displaced terrain.
const HSCALE: f32 = 2.0;

/// Fixed handle for the generated terrain material shader; its source is
/// overwritten whenever the graph structure changes.
const TERRAIN_SHADER: Handle<Shader> = uuid_handle!("7e44a1b2-c0de-4001-8002-000000000003");

/// Which view fills the central area.
#[derive(Resource, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Graph,
    Terrain3d,
}

/// The graph + colour range the preview should render, published by the egui
/// layer each frame.
#[derive(Resource, Default)]
pub struct PreviewInput {
    pub graph: Option<TerrainGraph>,
    pub hmin: f32,
    pub hmax: f32,
}

#[derive(Clone, ShaderType)]
struct PreviewParams {
    /// (res, origin, step, hscale)
    a: Vec4,
    /// (hmin, hmax, _, _)
    b: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone)]
struct TerrainMaterial {
    #[storage(0, read_only)]
    params: Handle<ShaderStorageBuffer>,
    #[uniform(1)]
    pv: PreviewParams,
}

impl Material for TerrainMaterial {
    fn vertex_shader() -> ShaderRef {
        TERRAIN_SHADER.into()
    }
    fn fragment_shader() -> ShaderRef {
        TERRAIN_SHADER.into()
    }
    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // The grid mesh carries POSITION only (ignored by the vertex shader,
        // which computes everything from vertex_index); declaring it keeps the
        // bound vertex buffer slot valid.
        let vertex_layout = layout.0.get_layout(&[Mesh::ATTRIBUTE_POSITION.at_shader_location(0)])?;
        descriptor.vertex.buffers = vec![vertex_layout];
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

#[derive(Component)]
struct Orbit {
    yaw: f32,
    pitch: f32,
    dist: f32,
    target: Vec3,
}

/// The terrain material + params buffer handles (the entity is always visible,
/// so it needn't be tracked).
#[derive(Resource)]
struct Terrain3dState {
    material: Handle<TerrainMaterial>,
    params: Handle<ShaderStorageBuffer>,
}

pub struct TerrainPreviewPlugin;

impl Plugin for TerrainPreviewPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<TerrainMaterial>::default())
            .insert_resource(ViewMode::Graph)
            .init_resource::<PreviewInput>()
            .init_resource::<Terrain3dStateHash>()
            .add_systems(Startup, setup)
            .add_systems(Update, (sync_terrain, orbit_camera));
    }
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TerrainMaterial>>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut shaders: ResMut<Assets<Shader>>,
) {
    // Seed the shader asset with a trivial flat shader; the first sync replaces
    // it with one generated from the graph.
    let flat = TerrainGraph::default_soils();
    let _ = shaders.insert(
        &TERRAIN_SHADER,
        Shader::from_wgsl(wgsl_gen::generate_material(&flat), "terrain_preview.wgsl"),
    );

    let params = buffers.add(ShaderStorageBuffer::from(vec![0.0f32]));
    let step = SPAN / RES as f32;
    let material = materials.add(TerrainMaterial {
        params: params.clone(),
        pv: PreviewParams {
            a: Vec4::new(RES as f32, -SPAN * 0.5, step, HSCALE),
            b: Vec4::new(0.0, 1.0, 0.0, 0.0),
        },
    });
    let mesh = meshes.add(grid_mesh(RES));
    commands.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material.clone()),
        Transform::default(),
        // Always visible: it renders behind the (transparent) node canvas in
        // Graph mode and fills the view in 3D mode.
        Visibility::Visible,
        bevy::camera::visibility::NoFrustumCulling,
    ));

    commands.insert_resource(Terrain3dState { material, params });

    // Camera (owns the primary egui context) + orbit rig + light.
    commands.spawn((
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(Color::srgb(0.55, 0.68, 0.85)),
            ..default()
        },
        Projection::from(PerspectiveProjection { far: 20000.0, ..default() }),
        Transform::default(),
        Orbit { yaw: 0.7, pitch: -0.55, dist: 2800.0, target: Vec3::new(0.0, 150.0, 0.0) },
        PrimaryEguiContext,
    ));
    commands.spawn((
        DirectionalLight { illuminance: 12000.0, shadows_enabled: false, ..default() },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.9, 0.6, 0.0)),
    ));
}

/// Regenerate the material shader on structural change and refresh params +
/// colour range every frame. The terrain is always visible (behind the node
/// canvas in Graph mode, full-screen in 3D mode).
fn sync_terrain(
    input: Res<PreviewInput>,
    state: Res<Terrain3dState>,
    mut shaders: ResMut<Assets<Shader>>,
    mut materials: ResMut<Assets<TerrainMaterial>>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut state_mut: ResMut<Terrain3dStateHash>,
) {
    let Some(graph) = &input.graph else { return };

    let src = wgsl_gen::generate_material(graph);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut hasher);
    let h = hasher.finish();
    if h != state_mut.0 {
        state_mut.0 = h;
        let _ = shaders.insert(&TERRAIN_SHADER, Shader::from_wgsl(src, "terrain_preview.wgsl"));
    }

    // Params buffer (values only).
    let mut params = wgsl_gen::collect_params(graph);
    if params.is_empty() {
        params.push(0.0);
    }
    if let Some(buf) = buffers.get_mut(&state.params) {
        buf.data = Some(bytemuck::cast_slice(&params).to_vec());
    }

    // Colour range.
    if let Some(mat) = materials.get_mut(&state.material) {
        mat.pv.b.x = input.hmin;
        mat.pv.b.y = input.hmax.max(input.hmin + 1.0);
    }
}

/// Separate the mutable structure hash so `sync_terrain` can keep `state`
/// immutable (its handles/entity never change).
#[derive(Resource, Default)]
pub struct Terrain3dStateHash(u64);

/// Drag to orbit, scroll to zoom — only in 3D mode and when egui isn't using
/// the pointer.
fn orbit_camera(
    mode: Res<ViewMode>,
    mut contexts: EguiContexts,
    motion: Res<AccumulatedMouseMotion>,
    scroll: Res<AccumulatedMouseScroll>,
    buttons: Res<ButtonInput<MouseButton>>,
    input: Res<PreviewInput>,
    mut q: Query<(&mut Orbit, &mut Transform)>,
) {
    if *mode != ViewMode::Terrain3d {
        return;
    }
    let egui_wants = contexts.ctx_mut().map(|c| c.wants_pointer_input()).unwrap_or(false);

    let Ok((mut orbit, mut tf)) = q.single_mut() else { return };
    // Keep the target near the terrain's mid height.
    orbit.target.y = (input.hmax - input.hmin).max(0.0) * HSCALE * 0.5;

    if !egui_wants {
        if buttons.pressed(MouseButton::Left) {
            orbit.yaw -= motion.delta.x * 0.005;
            orbit.pitch = (orbit.pitch - motion.delta.y * 0.005).clamp(-1.5, -0.05);
        }
        if scroll.delta.y != 0.0 {
            orbit.dist = (orbit.dist * (1.0 - scroll.delta.y * 0.1)).clamp(300.0, 12000.0);
        }
    }

    let rot = Quat::from_euler(EulerRot::YXZ, orbit.yaw, orbit.pitch, 0.0);
    let pos = orbit.target + rot * Vec3::new(0.0, 0.0, orbit.dist);
    *tf = Transform::from_translation(pos).looking_at(orbit.target, Vec3::Y);
}

/// A flat grid of `res*res` vertices with triangle indices. Positions are zero
/// (the vertex shader derives everything from `vertex_index`).
fn grid_mesh(res: u32) -> Mesh {
    let n = (res * res) as usize;
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; n]);
    let mut idx: Vec<u32> = Vec::with_capacity(((res - 1) * (res - 1) * 6) as usize);
    for z in 0..res - 1 {
        for x in 0..res - 1 {
            let i = z * res + x;
            idx.extend_from_slice(&[i, i + res, i + 1, i + 1, i + res, i + res + 1]);
        }
    }
    mesh.insert_indices(Indices::U32(idx));
    mesh
}
