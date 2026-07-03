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

/// Effective illuminance (lux-ish) applied to the unlit terrain so it sits in
/// the same exposure regime as the physically-bright atmosphere sky. Tuned so a
/// mid-albedo block lands around `albedo * 4` at the daytime exposure — bright
/// enough to read clearly through the atmosphere's in-scattering veil.
pub const TERRAIN_BRIGHTNESS: f32 = 45_000.0;

/// Exponential-squared fog density (per world unit). Tuned so terrain is crisp
/// up close and fades into the horizon haze near the chunk-load boundary.
pub const FOG_DENSITY: f32 = 0.0018;

/// Fog colour in the same lux regime as [`TERRAIN_BRIGHTNESS`] (scaled by the
/// view exposure in-shader), matched to the atmosphere's pale horizon haze so
/// the load boundary dissolves into the sky.
pub const FOG_COLOR: Vec3 = Vec3::new(23_000.0, 23_000.0, 24_000.0);

/// Uniform parameters for the atlas shader.
#[derive(Clone, ShaderType)]
pub struct AtlasParams {
    /// >0.5 enables AO multiply.
    pub ambient_occlusion: f32,
    /// Effective illuminance scaling the unlit terrain into the HDR/atmosphere
    /// exposure regime (see [`TERRAIN_BRIGHTNESS`]).
    pub brightness: f32,
    /// Exponential-squared distance-fog density (see [`FOG_DENSITY`]).
    pub fog_density: f32,
    /// Fog colour in the lux regime (see [`FOG_COLOR`]).
    pub fog_color: Vec3,
    /// World voxel coords of the GI volume's `(0,0,0)` corner (see `gi.rs`), so
    /// the shader can locate the cascade-0 probe for a fragment.
    pub gi_origin: Vec3,
    /// >0.5 enables the radiance-cascades GI term.
    pub gi_enabled: f32,
    /// Day-scaled skylight illuminance (lux regime): what a fully sky-lit
    /// (level 15) surface receives. Synced across materials by
    /// `light::update_sky_term`.
    pub sky_term: f32,
    /// >0.5 shades from the baked L0 light grid; otherwise the flat
    /// `brightness` (the pre-L0 look; the GI demo uses this path).
    pub light_enabled: f32,
}

impl Default for AtlasParams {
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
        }
    }
}

/// One material per chunk: its quad storage buffer (vertex-pulled) plus the
/// shared atlas texture and params. `gi_cascade0` is shared across all chunks
/// and points at the GI radiance-cascades output (see `gi.rs`): the merged
/// cascade-0 radiance field, sampled to light terrain. It is written only by
/// the compute shader (its GPU buffer is never recreated), so this bind group
/// stays valid; the volume origin/enable flag ride in `params` instead.
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct ChunkMeshMaterial {
    #[storage(0, read_only)]
    pub quads: Handle<ShaderStorageBuffer>,
    #[texture(1)]
    #[sampler(2)]
    pub atlas: Handle<Image>,
    #[uniform(3)]
    pub params: AtlasParams,
    #[storage(4, read_only)]
    pub gi_cascade0: Handle<ShaderStorageBuffer>,
    /// Padded per-chunk L0 light volume (see `gpu_mesh::LIGHT_PAD`). The CPU
    /// recreates this buffer's data on light changes, so `light::process_light`
    /// touches the material afterwards to rebuild the cached bind group.
    #[storage(5, read_only)]
    pub light: Handle<ShaderStorageBuffer>,
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
