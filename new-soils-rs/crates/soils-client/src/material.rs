//! Custom atlas material (phase B): a `Material` backed by `assets/shaders/
//! atlas.wgsl` that does world-space per-face tiling, ambient occlusion, and a
//! normal brightness tint — the port of the original `atlas.frag`. This is what
//! lets greedy-merged quads texture correctly.

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageLoaderSettings, ImageSampler};
use bevy::mesh::{Indices, MeshVertexAttribute, MeshVertexBufferLayoutRef, PrimitiveTopology};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError, VertexFormat,
};
use bevy::shader::ShaderRef;
use soils_worldgen::{BlockRegistry, MeshData};

/// Per-vertex atlas tile index (which 16px cell of `blocks.png` to sample).
pub const ATTRIBUTE_TILE: MeshVertexAttribute =
    MeshVertexAttribute::new("Tile", 0x50_11_7011, VertexFormat::Uint32);
/// Per-vertex ambient-occlusion brightness.
pub const ATTRIBUTE_AO: MeshVertexAttribute =
    MeshVertexAttribute::new("AmbientOcclusion", 0x50_11_7012, VertexFormat::Float32);

/// Uniform parameters for the atlas shader.
#[derive(Clone, Default, ShaderType)]
pub struct AtlasParams {
    /// >0.5 enables AO multiply.
    pub ambient_occlusion: f32,
}

/// The chunk material: the atlas texture plus shader parameters.
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct AtlasMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub atlas: Handle<Image>,
    #[uniform(2)]
    pub params: AtlasParams,
}

impl Material for AtlasMaterial {
    fn vertex_shader() -> ShaderRef {
        "shaders/atlas.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "shaders/atlas.wgsl".into()
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        let vertex_layout = layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_NORMAL.at_shader_location(1),
            ATTRIBUTE_TILE.at_shader_location(2),
            ATTRIBUTE_AO.at_shader_location(3),
        ])?;
        descriptor.vertex.buffers = vec![vertex_layout];
        // The greedy mesher emits some quads wound opposite to Bevy's front-face
        // convention; render both sides.
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

/// Handle to the shared chunk material.
#[derive(Resource)]
pub struct ChunkMaterial(pub Handle<AtlasMaterial>);

/// Load the atlas texture (nearest filtered) and build the chunk material.
pub fn setup_material(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut materials: ResMut<Assets<AtlasMaterial>>,
) {
    let atlas = asset_server.load_with_settings("blocks.png", |s: &mut ImageLoaderSettings| {
        s.sampler = ImageSampler::nearest();
    });
    let handle = materials.add(AtlasMaterial { atlas, params: AtlasParams { ambient_occlusion: 1.0 } });
    commands.insert_resource(ChunkMaterial(handle));
}

/// Convert mesher output into a renderable Bevy mesh with custom tile + AO
/// attributes. The atlas tile per quad is resolved from the block registry and
/// the face normal (matching the JS `applyMesh` tile selection).
pub fn build_mesh(data: &MeshData, registry: &BlockRegistry) -> Mesh {
    let vert_count = data.positions.len();
    let mut tiles: Vec<u32> = vec![0; vert_count];

    for (q, &block_id) in data.block_ids.iter().enumerate() {
        let n = data.normals[q * 4];
        let normal = [n[0] as i32, n[1] as i32, n[2] as i32];
        let tile = registry.get(block_id).map(|b| b.tile_for_normal(normal)).unwrap_or(0) as u32;
        for v in 0..4 {
            tiles[q * 4 + v] = tile;
        }
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, data.positions.clone());
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, data.normals.clone());
    mesh.insert_attribute(ATTRIBUTE_TILE, tiles);
    mesh.insert_attribute(ATTRIBUTE_AO, data.ao.clone());
    mesh.insert_indices(Indices::U32(data.indices.clone()));
    mesh
}
