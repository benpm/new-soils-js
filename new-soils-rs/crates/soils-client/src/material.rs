//! Atlas texture material (phase A: a `StandardMaterial` with nearest-filtered
//! atlas sampling) and the `MeshData` → Bevy `Mesh` conversion that bakes
//! per-face tile UVs.

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageLoaderSettings, ImageSampler};
use bevy::prelude::*;
use bevy::mesh::{Indices, PrimitiveTopology};
use soils_worldgen::{BlockRegistry, MeshData};

/// 8×8 grid of 16px tiles (the original `blocks.png`).
const ATLAS_COLS: f32 = 8.0;

/// Handle to the shared chunk material.
#[derive(Resource)]
pub struct AtlasMaterial(pub Handle<StandardMaterial>);

/// Load the atlas texture with nearest filtering and build the chunk material.
pub fn setup_material(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let texture = asset_server.load_with_settings("blocks.png", |s: &mut ImageLoaderSettings| {
        // Crisp voxel pixels, no bilinear smearing across tiles.
        s.sampler = ImageSampler::nearest();
    });
    let handle = materials.add(StandardMaterial {
        base_color_texture: Some(texture),
        perceptual_roughness: 1.0,
        metallic: 0.0,
        reflectance: 0.1,
        // Render both sides: the greedy mesher emits some quads wound the
        // opposite way to Bevy's front-face convention, and per-vertex normals
        // (computed explicitly) drive lighting regardless of winding.
        double_sided: true,
        cull_mode: None,
        ..default()
    });
    commands.insert_resource(AtlasMaterial(handle));
}

/// Convert mesher output into a renderable Bevy mesh, assigning atlas UVs per
/// quad from the block registry (matching the JS `applyMesh` UV logic).
pub fn build_mesh(data: &MeshData, registry: &BlockRegistry) -> Mesh {
    let quad_count = data.block_ids.len();
    let mut uvs: Vec<[f32; 2]> = vec![[0.0, 0.0]; data.positions.len()];

    for q in 0..quad_count {
        let n = data.normals[q * 4];
        let normal = [n[0] as i32, n[1] as i32, n[2] as i32];
        let tile = registry
            .get(data.block_ids[q])
            .map(|b| b.tile_for_normal(normal))
            .unwrap_or(0);

        let col = (tile % 8) as f32;
        let row = (tile / 8) as f32;
        let u0 = col / ATLAS_COLS;
        let v0 = row / ATLAS_COLS;
        let u1 = u0 + 1.0 / ATLAS_COLS;
        let v1 = v0 + 1.0 / ATLAS_COLS;

        // Vertex order from the mesher: [origin, +du, +du+dv, +dv].
        let base = q * 4;
        uvs[base] = [u0, v1];
        uvs[base + 1] = [u1, v1];
        uvs[base + 2] = [u1, v0];
        uvs[base + 3] = [u0, v0];
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, data.positions.clone());
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, data.normals.clone());
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(data.indices.clone()));
    mesh
}
