//! Chunk material (GPU-resident path): a `Material` backed by `assets/shaders/
//! atlas.wgsl` that **vertex-pulls** greedy quads from a per-chunk storage buffer
//! the compute mesher fills, then shades with the original atlas tiling/AO/tint.

use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey};
use bevy::prelude::*;
use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::render::storage::ShaderStorageBuffer;
use bevy::shader::ShaderRef;

/// Uniform parameters for the atlas shader.
#[derive(Clone, Default, ShaderType)]
pub struct AtlasParams {
    /// >0.5 enables AO multiply.
    pub ambient_occlusion: f32,
}

/// One material per chunk: its quad storage buffer (vertex-pulled) plus the
/// shared atlas texture and params.
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct ChunkMeshMaterial {
    #[storage(0, read_only)]
    pub quads: Handle<ShaderStorageBuffer>,
    #[texture(1)]
    #[sampler(2)]
    pub atlas: Handle<Image>,
    #[uniform(3)]
    pub params: AtlasParams,
}

impl Material for ChunkMeshMaterial {
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
        // The dummy mesh only carries POSITION (ignored by the vertex shader,
        // which pulls from storage); declaring it keeps the bound vertex buffer
        // slot valid.
        let vertex_layout = layout.0.get_layout(&[Mesh::ATTRIBUTE_POSITION.at_shader_location(0)])?;
        descriptor.vertex.buffers = vec![vertex_layout];
        // Quads can be wound either way; render both sides.
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}
