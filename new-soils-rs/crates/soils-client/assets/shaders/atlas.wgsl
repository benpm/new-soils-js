// Chunk material: vertex-pulls greedy quads from a storage buffer the compute
// mesher (voxel_mesh.wgsl) wrote, then shades with the original atlas.frag logic
// (world-space per-face tiling, ambient occlusion, normal tint). No vertex
// buffer / Bevy Mesh attributes are used.

#import bevy_pbr::{
    mesh_functions,
    view_transformations::position_world_to_clip,
}

struct AtlasParams {
    ambient_occlusion: f32,
};

struct QuadGpu {
    base: vec3<f32>, tile: u32,
    du: vec3<f32>,   nx: f32,
    dv: vec3<f32>,   ny: f32,
    ao: vec4<f32>,
    nz: f32, pad0: f32, pad1: f32, pad2: f32,
};

struct QuadBuffer {
    count: u32,
    p0: u32, p1: u32, p2: u32,
    quads: array<QuadGpu>,
};

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<storage, read> qb: QuadBuffer;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var atlas_tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var atlas_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var<uniform> params: AtlasParams;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local_position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) @interpolate(flat) tile: u32,
    @location(3) ao: f32,
};

// Two triangles per quad: corners [0,1,2, 0,2,3] over [origin, +du, +du+dv, +dv].
const CORNERS = array<u32, 6>(0u, 1u, 2u, 0u, 2u, 3u);

@vertex
fn vertex(
    @builtin(instance_index) instance_index: u32,
    @builtin(vertex_index) vertex_index: u32,
) -> VertexOutput {
    var out: VertexOutput;
    let q = vertex_index / 6u;
    let corner = CORNERS[vertex_index % 6u];

    // Collapse surplus vertices (past the generated quad count) to a clipped point.
    if (q >= qb.count) {
        out.clip_position = vec4<f32>(0.0, 0.0, 0.0, 0.0);
        return out;
    }

    let quad = qb.quads[q];
    var p = quad.base;
    if (corner == 1u) { p = quad.base + quad.du; }
    else if (corner == 2u) { p = quad.base + quad.du + quad.dv; }
    else if (corner == 3u) { p = quad.base + quad.dv; }

    let normal = vec3<f32>(quad.nx, quad.ny, quad.nz);

    let world_from_local = mesh_functions::get_world_from_local(instance_index);
    let world_position = mesh_functions::mesh_position_local_to_world(
        world_from_local,
        vec4<f32>(p, 1.0),
    );
    out.clip_position = position_world_to_clip(world_position.xyz);
    out.local_position = p;
    out.normal = normal;
    out.tile = quad.tile;
    out.ao = quad.ao[corner];
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
