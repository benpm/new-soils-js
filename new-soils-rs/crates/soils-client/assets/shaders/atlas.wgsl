// Custom atlas material for chunk meshes, porting the original `atlas.frag`.
//
// Each block face is textured by computing a world/local-space tile coordinate
// from the vertex position and face normal (so greedy-merged quads tile their
// atlas tile across the whole quad rather than stretching it), then sampling the
// 8x8 tile atlas. Per-vertex ambient occlusion and a normal-based brightness
// tint are applied, matching the look of the JS shader.

#import bevy_pbr::{
    mesh_functions,
    view_transformations::position_world_to_clip,
}

struct AtlasParams {
    ambient_occlusion: f32,
};

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var atlas_tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var atlas_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var<uniform> params: AtlasParams;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) tile: u32,
    @location(3) ao: f32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local_position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) @interpolate(flat) tile: u32,
    @location(3) ao: f32,
};

@vertex
fn vertex(v: Vertex) -> VertexOutput {
    var out: VertexOutput;
    let world_from_local = mesh_functions::get_world_from_local(v.instance_index);
    let world_position = mesh_functions::mesh_position_local_to_world(
        world_from_local,
        vec4<f32>(v.position, 1.0),
    );
    out.clip_position = position_world_to_clip(world_position.xyz);
    // Tiling uses the local (chunk-space) position, like the JS shader.
    out.local_position = v.position;
    out.normal = v.normal;
    out.tile = v.tile;
    out.ao = v.ao;
    return out;
}

const ATLAS_COLS: f32 = 8.0;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let n = in.normal;

    // Per-face 2D coordinate that advances by 1 per voxel along the face.
    var tile_uv = vec2<f32>(dot(n.zxy, in.local_position), dot(n.yzx, in.local_position));

    // Orientation fix-ups, ported from atlas.frag so tiles read upright.
    if (n.z < 0.0) {
        tile_uv.y = 1.0 - tile_uv.y;
    }
    if (n.x < 0.0) {
        let r = tile_uv.x;
        tile_uv.x = 1.0 - tile_uv.y;
        tile_uv.y = 1.0 - r;
    } else if (n.x > 0.0) {
        let r = tile_uv.x;
        tile_uv.x = 1.0 - tile_uv.y;
        tile_uv.y = r;
    }

    // Map into the atlas: pick the tile cell, repeat within it via fract().
    let col = f32(in.tile % 8u);
    let row = f32(in.tile / 8u);
    let within = fract(tile_uv);
    let atlas_uv = (vec2<f32>(col, row) + within) / ATLAS_COLS;

    var color = textureSample(atlas_tex, atlas_sampler, atlas_uv);

    if (params.ambient_occlusion > 0.5) {
        color = vec4<f32>(color.rgb * in.ao, color.a);
    }

    // Subtle brightness boost on side faces (matches the JS normal tint).
    let tint = 1.0 + abs(n.x + n.y) * 0.2;
    color = vec4<f32>(color.rgb * tint, color.a);

    return color;
}
