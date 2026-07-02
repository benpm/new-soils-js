// Chunk material: vertex-pulls greedy quads from a storage buffer the compute
// mesher (voxel_mesh.wgsl) wrote, then shades with the original atlas.frag logic
// (world-space per-face tiling, ambient occlusion, normal tint). No vertex
// buffer / Bevy Mesh attributes are used.

#import bevy_pbr::{
    mesh_functions,
    mesh_view_bindings::view,
    view_transformations::position_world_to_clip,
}

struct AtlasParams {
    ambient_occlusion: f32,
    // Effective illuminance applied to the (otherwise unlit) terrain so it sits
    // in the same exposure regime as the physically-bright atmosphere sky.
    brightness: f32,
    // Exponential-squared distance fog (JS `FogExp2`): density per world unit,
    // colour in the same lux regime as `brightness` so it dims with exposure.
    fog_density: f32,
    fog_color: vec3<f32>,
    // Radiance-cascades GI (see gi.rs): world-voxel corner of the volume, and a
    // >0.5 enable flag. Carried here (not in a shared buffer) so the material
    // bind group never references a per-frame-recreated buffer.
    gi_origin: vec3<f32>,
    gi_enabled: f32,
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

// GI radiance-cascades output (see gi.rs / radiance.wgsl): the merged cascade-0
// field, shared (read-only) across all chunk materials.
@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<storage, read> qb: QuadBuffer;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var atlas_tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var atlas_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(3) var<uniform> params: AtlasParams;
@group(#{MATERIAL_BIND_GROUP}) @binding(4) var<storage, read> gi_cascade0: array<vec4<f32>>;

// Cascade-0 layout (must match radiance.wgsl / gi.rs).
const GI_DIM: f32 = 64.0;
const GI_PROBES0: i32 = 16;
const GI_SPACING0: f32 = 4.0;
const GI_DIRRES0: u32 = 4u;
// Scales GI irradiance into the terrain's lux exposure regime.
const GI_LUX: f32 = 3500.0;

fn octa_decode(u: f32, v: f32) -> vec3<f32> {
    let ox = 2.0 * u - 1.0;
    let oy = 2.0 * v - 1.0;
    let z = 1.0 - abs(ox) - abs(oy);
    var x = ox;
    var y = oy;
    if (z < 0.0) {
        x = (1.0 - abs(oy)) * sign(ox);
        y = (1.0 - abs(ox)) * sign(oy);
    }
    return normalize(vec3<f32>(x, y, z));
}

// Cosine-weighted hemisphere integral of the nearest cascade-0 probe's incoming
// radiance about normal `n`. Returns linear RGB irradiance (0 outside volume).
fn gi_irradiance(world_pos: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    // Sample one probe-spacing off the surface along the normal: a probe sitting
    // exactly on the surface is embedded in the solid voxel and traces only that
    // (black), so nudge into the air where the lit probes are.
    let local = world_pos + n * GI_SPACING0 - params.gi_origin;
    let pf = local / GI_SPACING0 - vec3<f32>(0.5);
    let pi = vec3<i32>(i32(round(pf.x)), i32(round(pf.y)), i32(round(pf.z)));
    if (pi.x < 0 || pi.y < 0 || pi.z < 0 ||
        pi.x >= GI_PROBES0 || pi.y >= GI_PROBES0 || pi.z >= GI_PROBES0) {
        return vec3<f32>(0.0);
    }
    let dirs = GI_DIRRES0 * GI_DIRRES0;
    let pidx = u32((pi.y * GI_PROBES0 + pi.z) * GI_PROBES0 + pi.x);
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var dy = 0u; dy < GI_DIRRES0; dy += 1u) {
        for (var dx = 0u; dx < GI_DIRRES0; dx += 1u) {
            let dir = octa_decode((f32(dx) + 0.5) / f32(GI_DIRRES0), (f32(dy) + 0.5) / f32(GI_DIRRES0));
            let ndl = max(dot(dir, n), 0.0);
            if (ndl <= 0.0) { continue; }
            let e = gi_cascade0[pidx * dirs + dy * GI_DIRRES0 + dx];
            acc += e.rgb * ndl;
            wsum += ndl;
        }
    }
    if (wsum <= 0.0) { return vec3<f32>(0.0); }
    return acc / wsum;
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local_position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) @interpolate(flat) tile: u32,
    @location(3) ao: f32,
    @location(4) world_position: vec3<f32>,
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
    out.world_position = world_position.xyz;
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

    // Lift the terrain into the atmosphere's physical-light exposure regime.
    // The base `brightness` is the ambient/sun term; the radiance-cascades GI
    // adds bounced + emissive light (coloured, from nearby light sources and
    // the sky), so caves darken and lamps bleed colour onto their surroundings.
    // With GI disabled the added term is zero, reproducing the flat look.
    var gi = vec3<f32>(0.0);
    if (params.gi_enabled > 0.5) {
        gi = gi_irradiance(in.world_position, n) * GI_LUX;
    }
    color = vec4<f32>(color.rgb * (vec3<f32>(params.brightness) + gi) * view.exposure, color.a);

    // Exponential-squared distance fog toward the (exposure-scaled) horizon
    // colour, blending the chunk-load boundary into the atmosphere haze.
    let dist = length(in.world_position - view.world_position);
    let fog = 1.0 - exp(-pow(dist * params.fog_density, 2.0));
    color = vec4<f32>(mix(color.rgb, params.fog_color * view.exposure, fog), color.a);

    return color;
}
