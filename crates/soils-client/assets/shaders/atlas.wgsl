// Terrain draw over the POOLED chunk caches: one multi_draw_indirect renders
// every resident chunk. The vertex stage pulls packed greedy quads from the
// shared quad pool (slot = vertex_index / (QUADS_PER_SLOT * 6)); the fragment
// stage shades with the original atlas logic (world-space per-face tiling,
// ambient occlusion, normal tint), sampling L0 light across chunk borders
// through the slot table — no per-chunk buffers, materials, or bind groups.

#import bevy_pbr::{
    mesh_view_bindings::view,
    view_transformations::position_world_to_clip,
}

struct WorldParams {
    ambient_occlusion: f32,
    // Effective illuminance applied to the (otherwise unlit) terrain so it sits
    // in the same exposure regime as the physically-bright atmosphere sky.
    brightness: f32,
    // Exponential-squared distance fog (JS `FogExp2`): density per world unit,
    // colour in the same lux regime as `brightness` so it dims with exposure.
    fog_density: f32,
    fog_color: vec3<f32>,
    // Radiance-cascades GI (see gi.rs): world-voxel corner of the volume, and a
    // >0.5 enable flag.
    gi_origin: vec3<f32>,
    gi_enabled: f32,
    // Baked L0 light grid (see light.rs): day-scaled illuminance of a fully
    // sky-lit surface, and a >0.5 enable flag (off = flat `brightness`).
    sky_term: f32,
    light_enabled: f32,
};

struct ChunkSlot {
    cpos: vec3<i32>,
    mesh_slot: u32,
    flags: u32,
    flags_gpu: u32,
    quad_count: u32,
    pad: u32,
};

@group(2) @binding(0) var<storage, read> quads: array<u32>;          // packed, N_MESH × QPS × 2
@group(2) @binding(1) var<storage, read> mesh_info: array<vec4<i32>>; // per mesh slot: cpos, light slot
@group(2) @binding(2) var<storage, read> desc: array<ChunkSlot>;      // per unified slot
@group(2) @binding(3) var<storage, read> light_pool: array<u32>;      // N_SLOTS × 8192 words
@group(2) @binding(4) var<storage, read> slot_table: array<u32>;      // 32³ wrap-window map
@group(2) @binding(5) var<storage, read> gi_probes: array<vec4<f32>>;
// Block textures: one 1024² layer per tile index (assets/blocks_mega.png via
// scripts/gen_textures.mjs), each repeating over TILE_PERIOD blocks. Linear +
// repeat sampler, so no fract() is needed on the face coordinate.
@group(2) @binding(6) var atlas_tex: texture_2d_array<f32>;
@group(2) @binding(7) var atlas_sampler: sampler;
@group(2) @binding(8) var<uniform> params: WorldParams;

// Blocks per texture repeat (1024 px / 64 px per block).
const TILE_PERIOD: f32 = 16.0;

// Must match pool::QUADS_PER_SLOT / voxel_mesh.wgsl.
const QUADS_PER_SLOT: u32 = 4096u;
const TABLE_EMPTY: u32 = 0xffffffffu;
// Illuminance of a level-15 blocklight surface (lux regime), warm-tinted.
const BLOCK_LUX: f32 = 35000.0;
const BLOCK_TINT: vec3<f32> = vec3<f32>(1.0, 0.82, 0.6);

// Resolve a world chunk coordinate to its unified slot through the
// wrap-window table, validating against the descriptor (stale cells are
// expected; validation makes them read as "unloaded").
fn slot_of(cpos: vec3<i32>) -> u32 {
    let m = vec3<i32>(31);
    let c = cpos & m;
    let slot = slot_table[u32(c.x + c.y * 32 + c.z * 1024)];
    if (slot == TABLE_EMPTY) { return TABLE_EMPTY; }
    let d = desc[slot];
    if (any(d.cpos != cpos)) { return TABLE_EMPTY; }
    return slot;
}

// Packed L0 light byte for the air voxel just in front of a fragment's face.
// Crosses chunk borders through the slot table; unloaded space reads dark.
fn light_at(world_pos: vec3<f32>, n: vec3<f32>) -> u32 {
    let v = vec3<i32>(floor(world_pos + n * 0.5));
    let slot = slot_of(v >> vec3<u32>(5u));
    if (slot == TABLE_EMPTY) { return 0u; }
    let l = v & vec3<i32>(31);
    let idx = u32((l.y + l.z * 32) * 32 + l.x);
    let w = light_pool[slot * 8192u + (idx >> 2u)];
    return (w >> ((idx & 3u) * 8u)) & 0xffu;
}

// Cascade-0 layout (must match radiance.wgsl / gi_irradiance.wgsl / gi.rs).
const GI_DIM: f32 = 64.0;
const GI_PROBES0: i32 = 16;
const GI_SPACING0: f32 = 4.0;
// Scales GI irradiance into the terrain's lux exposure regime.
const GI_LUX: f32 = 3500.0;

// One probe's ambient cube evaluated at normal `n`: blend the three faces the
// normal leans on by its squared components (they sum to 1).
fn gi_cube(pidx: u32, n: vec3<f32>) -> vec3<f32> {
    let base = pidx * 6u;
    let n2 = n * n;
    let fx = select(1u, 0u, n.x >= 0.0);
    let fy = select(3u, 2u, n.y >= 0.0);
    let fz = select(5u, 4u, n.z >= 0.0);
    return n2.x * gi_probes[base + fx].rgb
        + n2.y * gi_probes[base + fy].rgb
        + n2.z * gi_probes[base + fz].rgb;
}

// Trilinearly interpolated ambient-cube irradiance about normal `n`.
fn gi_irradiance(world_pos: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    // Sample one probe-spacing off the surface along the normal: a probe sitting
    // exactly on the surface is embedded in the solid voxel and traces only that
    // (black), so nudge into the air where the lit probes are.
    let local = world_pos + n * GI_SPACING0 - params.gi_origin;
    let pf = local / GI_SPACING0 - vec3<f32>(0.5);
    let pr = round(pf);
    if (pr.x < 0.0 || pr.y < 0.0 || pr.z < 0.0 ||
        pr.x >= f32(GI_PROBES0) || pr.y >= f32(GI_PROBES0) || pr.z >= f32(GI_PROBES0)) {
        return vec3<f32>(0.0);
    }
    let p0 = vec3<i32>(floor(pf));
    let fr = pf - floor(pf);
    var acc = vec3<f32>(0.0);
    for (var k = 0u; k < 8u; k += 1u) {
        let o = vec3<i32>(i32(k & 1u), i32((k >> 1u) & 1u), i32((k >> 2u) & 1u));
        let pc = clamp(p0 + o, vec3<i32>(0), vec3<i32>(GI_PROBES0 - 1));
        let wv = mix(vec3<f32>(1.0) - fr, fr, vec3<f32>(o));
        let pidx = u32((pc.y * GI_PROBES0 + pc.z) * GI_PROBES0 + pc.x);
        acc += (wv.x * wv.y * wv.z) * gi_cube(pidx, n);
    }
    return acc;
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
fn vertex(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    // For non-indexed indirect draws vertex_index includes first_vertex, so it
    // carries the slot's global quad offset already.
    let slot = vertex_index / (QUADS_PER_SLOT * 6u);
    let rem = vertex_index % (QUADS_PER_SLOT * 6u);
    let q = rem / 6u;
    let corner = CORNERS[rem % 6u];

    // Unpack (see voxel_mesh.wgsl emit): w0 = x|y|z|w|h|axis, w1 = sign|tile|ao.
    let at = (slot * QUADS_PER_SLOT + q) * 2u;
    let w0 = quads[at];
    let w1 = quads[at + 1u];
    let base = vec3<f32>(
        f32(w0 & 63u),
        f32((w0 >> 6u) & 63u),
        f32((w0 >> 12u) & 63u),
    );
    let qw = i32((w0 >> 18u) & 63u);
    let qh = i32((w0 >> 24u) & 63u);
    let d = (w0 >> 30u) & 3u;
    let positive = (w1 & 1u) == 1u;
    let u_axis = (d + 1u) % 3u;
    let v_axis = (d + 2u) % 3u;

    var du = vec3<i32>(0);
    var dv = vec3<i32>(0);
    if (positive) { du[u_axis] = qw; dv[v_axis] = qh; }
    else          { du[v_axis] = qh; dv[u_axis] = qw; }
    var normal = vec3<f32>(0.0);
    normal[d] = select(-1.0, 1.0, positive);

    var p = base;
    if (corner == 1u) { p = base + vec3<f32>(du); }
    else if (corner == 2u) { p = base + vec3<f32>(du) + vec3<f32>(dv); }
    else if (corner == 3u) { p = base + vec3<f32>(dv); }

    let origin = vec3<f32>(mesh_info[slot].xyz * 32);
    let world_position = origin + p;
    out.clip_position = position_world_to_clip(world_position);
    out.local_position = p;
    out.world_position = world_position;
    out.normal = normal;
    out.tile = (w1 >> 1u) & 0xffu;
    let ao_lvl = (w1 >> (9u + corner * 2u)) & 3u;
    out.ao = 0.1 + f32(ao_lvl) * 0.3;
    return out;
}

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

    // Sample the tile's layer over a TILE_PERIOD-block repeat. The face
    // coordinate is chunk-local (0..32) and 16 divides 32, so the pattern is
    // continuous across chunk borders; the orientation fix-ups above keep
    // each 64 px band's row 0 at the top of its block on side faces.
    var color = textureSample(atlas_tex, atlas_sampler, tile_uv / TILE_PERIOD, i32(in.tile));

    if (params.ambient_occlusion > 0.5) {
        color = vec4<f32>(color.rgb * in.ao, color.a);
    }

    // Subtle brightness boost on side faces (matches the JS normal tint).
    let tint = 1.0 + abs(n.x + n.y) * 0.2;
    color = vec4<f32>(color.rgb * tint, color.a);

    // Lift the terrain into the atmosphere's physical-light exposure regime.
    // The base term comes from the baked L0 grid: skylight (day-scaled
    // `sky_term`, squared falloff so caves darken quickly) plus warm blocklight
    // from emissive blocks, over a small floor so nothing reads pure black.
    // With the grid disabled it falls back to the flat `brightness` (GI demo).
    // The radiance-cascades GI then adds coloured bounce on top.
    var lit = vec3<f32>(params.brightness);
    if (params.light_enabled > 0.5) {
        let l = light_at(in.world_position, n);
        let skyf = f32(l >> 4u) / 15.0;
        let blockf = f32(l & 15u) / 15.0;
        let sky_l = params.sky_term * skyf * skyf;
        let block_l = BLOCK_TINT * (BLOCK_LUX * pow(blockf, 1.4));
        lit = vec3<f32>(sky_l + params.brightness * 0.015) + block_l;
    }
    var gi = vec3<f32>(0.0);
    if (params.gi_enabled > 0.5) {
        gi = gi_irradiance(in.world_position, n) * GI_LUX;
    }
    color = vec4<f32>(color.rgb * (lit + gi) * view.exposure, color.a);

    // Exponential-squared distance fog toward the (exposure-scaled) horizon
    // colour, blending the chunk-load boundary into the atmosphere haze.
    let dist = length(in.world_position - view.world_position);
    let fog = 1.0 - exp(-pow(dist * params.fog_density, 2.0));
    color = vec4<f32>(mix(color.rgb, params.fog_color * view.exposure, fog), color.a);

    return color;
}
