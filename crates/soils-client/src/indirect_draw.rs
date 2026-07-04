//! Indirect drawing for GPU-meshed chunks.
//!
//! The compute mesher's finalize pass writes `DrawIndirectArgs { count*6, 1,
//! 0, 0 }` into a per-chunk buffer (`GpuChunk::indirect`), so a chunk's draw
//! costs exactly its real quad count instead of the MAX_QUADS*6 dummy-vertex
//! worst case. Bevy 0.18 has no `Material` hook for a custom draw function, but
//! `MaterialProperties` stores them per prepared material with all fields
//! public: a render-world system rewrites `ChunkMeshMaterial`'s opaque-pass
//! entry to [`DrawVoxelChunkIndirect`] — the stock [`DrawMaterial`] tail with
//! `DrawMesh` swapped for a `draw_indirect` on the chunk's args buffer.

use std::any::TypeId;
use std::sync::Arc;

use bevy::core_pipeline::core_3d::Opaque3d;
use bevy::ecs::query::ROQueryItem;
use bevy::ecs::system::SystemParamItem;
use bevy::ecs::system::lifetimeless::{Read, SRes};
use bevy::pbr::{
    MATERIAL_BIND_GROUP_INDEX, MainPassOpaqueDrawFunction, MaterialProperties, PreparedMaterial,
    SetMaterialBindGroup, SetMeshBindGroup, SetMeshViewBindGroup, SetMeshViewBindingArrayBindGroup,
};
use bevy::prelude::*;
use bevy::render::erased_render_asset::{ErasedRenderAssets, prepare_erased_assets};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_phase::{
    AddRenderCommand, DrawFunctionLabel, DrawFunctions, PhaseItem, RenderCommand,
    RenderCommandResult, SetItemPipeline, TrackedRenderPass,
};
use bevy::render::render_resource::{Buffer, BufferDescriptor, BufferUsages};
use bevy::render::renderer::RenderDevice;
use bevy::render::storage::GpuShaderStorageBuffer;
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};

use crate::gpu_mesh::GpuChunk;
use crate::material::ChunkMeshMaterial;

pub struct IndirectDrawPlugin;

impl Plugin for IndirectDrawPlugin {
    fn build(&self, app: &mut App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .add_render_command::<Opaque3d, DrawVoxelChunkIndirect>()
            .add_systems(RenderStartup, init_vertex_stub)
            .add_systems(
                Render,
                // Must run after the material is (re)prepared and before
                // queueing: binned phases cache queued entities, so a material
                // queued once with the stock draw function would keep it.
                patch_chunk_draw_function
                    .in_set(RenderSystems::PrepareAssets)
                    .after(prepare_erased_assets::<MeshMaterial3d<ChunkMeshMaterial>>),
            );
    }
}

/// Placeholder bound at vertex slot 0: the pipeline's stride-0 attribute-less
/// layout (see `ChunkMeshMaterial::specialize`) needs *a* buffer there, but
/// never reads it.
#[derive(Resource)]
struct VertexStub(Buffer);

fn init_vertex_stub(mut commands: Commands, render_device: Res<RenderDevice>) {
    let buffer = render_device.create_buffer(&BufferDescriptor {
        label: Some("chunk_vertex_stub"),
        size: 4,
        usage: BufferUsages::VERTEX,
        mapped_at_creation: false,
    });
    commands.insert_resource(VertexStub(buffer));
}

/// The stock `DrawMaterial` command sequence with `DrawMesh` replaced by the
/// indirect draw. Group 2 is unused by atlas.wgsl but the pipeline layout
/// still declares it, so it must stay bound.
type DrawVoxelChunkIndirect = (
    SetItemPipeline,
    SetMeshViewBindGroup<0>,
    SetMeshViewBindingArrayBindGroup<1>,
    SetMeshBindGroup<2>,
    SetMaterialBindGroup<MATERIAL_BIND_GROUP_INDEX>,
    DrawChunkIndirect,
);

struct DrawChunkIndirect;

impl<P: PhaseItem> RenderCommand<P> for DrawChunkIndirect {
    type Param = (SRes<RenderAssets<GpuShaderStorageBuffer>>, SRes<VertexStub>);
    type ViewQuery = ();
    type ItemQuery = Read<GpuChunk>;

    fn render<'w>(
        _item: &P,
        _view: ROQueryItem<'w, '_, Self::ViewQuery>,
        chunk: Option<ROQueryItem<'w, '_, Self::ItemQuery>>,
        (buffers, stub): SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let Some(gc) = chunk else {
            return RenderCommandResult::Skip;
        };
        let Some(ind) = buffers.into_inner().get(&gc.indirect) else {
            return RenderCommandResult::Skip;
        };
        pass.set_vertex_buffer(0, stub.into_inner().0.slice(..));
        pass.draw_indirect(&ind.buffer, 0);
        RenderCommandResult::Success
    }
}

/// Point every prepared `ChunkMeshMaterial`'s opaque-pass draw function at
/// [`DrawVoxelChunkIndirect`]. `prepare_asset` resets draw functions whenever a
/// material is (re)prepared — e.g. each time `light::process_light` touches
/// one — so this runs as a cheap idempotent per-frame scan.
fn patch_chunk_draw_function(
    mut materials: ResMut<ErasedRenderAssets<PreparedMaterial>>,
    draw_functions: Res<DrawFunctions<Opaque3d>>,
) {
    let ours = draw_functions.read().id::<DrawVoxelChunkIndirect>();
    for (id, prepared) in materials.iter_mut() {
        if id.type_id() != TypeId::of::<ChunkMeshMaterial>() {
            continue;
        }
        if prepared.properties.get_draw_function(MainPassOpaqueDrawFunction) == Some(ours) {
            continue;
        }
        let mut draw_fns = prepared.properties.draw_functions.clone();
        for entry in &mut draw_fns {
            if entry.0 == MainPassOpaqueDrawFunction.intern() {
                entry.1 = ours;
            }
        }
        // MaterialProperties has no Clone impl and sits behind an Arc, so
        // rebuild it field-by-field (all fields are public). Deliberately no
        // `..Default::default()`: a bevy upgrade adding fields must fail here.
        let old = &prepared.properties;
        prepared.properties = Arc::new(MaterialProperties {
            render_method: old.render_method,
            alpha_mode: old.alpha_mode,
            mesh_pipeline_key_bits: old.mesh_pipeline_key_bits,
            depth_bias: old.depth_bias,
            reads_view_transmission_texture: old.reads_view_transmission_texture,
            render_phase_type: old.render_phase_type,
            material_layout: old.material_layout.clone(),
            draw_functions: draw_fns,
            shaders: old.shaders.clone(),
            bindless: old.bindless,
            specialize: old.specialize,
            material_key: old.material_key.clone(),
            shadows_enabled: old.shadows_enabled,
            prepass_enabled: old.prepass_enabled,
        });
    }
}
