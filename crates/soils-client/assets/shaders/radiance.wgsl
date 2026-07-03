// GPU radiance-cascades global illumination (compute). A line-by-line port of
// the CPU oracle in soils-worldgen/src/radiance.rs — see that file for the
// theory. Two entry points share one bind-group layout:
//
//   trace: for every (probe, direction) of one cascade, ray-march the world
//          voxel volume over that cascade's interval and store (radiance, vis)
//          into `cascade` (binding 2).
//   merge: fold cascade c+1 (`far`, binding 5) into cascade c (`cascade`,
//          binding 2, read_write) — angular 4:1 + nearest spatial — so after
//          merging down to 0, cascade 0 holds the full incoming radiance field.
//
// Dynamic lighting is fully GPU-resident: the only CPU input is the packed
// voxel-occupancy volume (binding 0); probes, rays, and merged radiance never
// leave the GPU. Bindings unused by a given entry point are bound to dummies.

// Sized for integrated GPUs: a 64-voxel volume (±1 chunk around the player) and
// ~122k rays/frame (~1M march samples) — ~8x cheaper than the 128³/1M-ray setup,
// to stay clear of integrated-GPU watchdog timeouts. Combined with the every-6th
// -frame throttle in gi.rs. Keep in lockstep with gi.rs / atlas.wgsl.
const GI_DIM: u32 = 64u;           // world volume side, in voxels
const CASCADES: u32 = 4u;
const STEP: f32 = 0.5;             // ray-march step, in voxels

// Per-cascade constants (see radiance.rs). Probe count per axis halves and
// direction resolution doubles each level; intervals telescope [0..30) voxels.
const PROBES = array<u32, 4>(16u, 8u, 4u, 2u);
const DIRRES = array<u32, 4>(4u, 8u, 16u, 32u);
const SPACING = array<f32, 4>(4.0, 8.0, 16.0, 32.0);
const INT_START = array<f32, 4>(0.0, 2.0, 6.0, 14.0);
const INT_END = array<f32, 4>(2.0, 6.0, 14.0, 30.0);

struct GiParams {
    origin: vec3<f32>,       // world voxel coords of the volume's (0,0,0) corner
    day: f32,                // 0..1 daylight factor (scales sky radiance)
    sky_zenith: vec3<f32>,
    _pad0: f32,
    sky_horizon: vec3<f32>,
    _pad1: f32,
};

struct Meta {
    cascade: u32,
};

@group(0) @binding(0) var<storage, read> world_vox: array<u32>;
@group(0) @binding(1) var<storage, read> emission: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> cascade: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> params: GiParams;
@group(0) @binding(4) var<storage, read> job: Meta;
@group(0) @binding(5) var<storage, read> far: array<vec4<f32>>;

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

// Direction for texel (dx,dy) of a res x res octahedral grid (texel centre).
fn dir_for_texel(dx: u32, dy: u32, res: u32) -> vec3<f32> {
    let u = (f32(dx) + 0.5) / f32(res);
    let v = (f32(dy) + 0.5) / f32(res);
    return octa_decode(u, v);
}

// Sky radiance for a direction (rays that escape the world hit this).
fn sky(dir: vec3<f32>) -> vec3<f32> {
    let up = clamp(dir.y * 0.5 + 0.5, 0.0, 1.0);
    let col = mix(params.sky_horizon, params.sky_zenith, up);
    return col * params.day;
}

// Block id at a volume voxel (packed 4 ids per u32); 0 (Air) outside bounds.
fn vox(v: vec3<i32>) -> u32 {
    if (v.x < 0 || v.y < 0 || v.z < 0 ||
        v.x >= i32(GI_DIM) || v.y >= i32(GI_DIM) || v.z >= i32(GI_DIM)) {
        return 0u;
    }
    let idx = (u32(v.y) * GI_DIM + u32(v.z)) * GI_DIM + u32(v.x);
    let w = world_vox[idx >> 2u];
    return (w >> ((idx & 3u) * 8u)) & 0xffu;
}

// March the interval [t0,t1) of a world-space ray; return (rgb, vis) where
// vis==1 means it cleared, vis==0 means it hit an opaque voxel (whose emission
// is returned as rgb). Mirrors radiance.rs::trace_interval.
fn trace_interval(ro: vec3<f32>, dir: vec3<f32>, t0: f32, t1: f32) -> vec4<f32> {
    let base = vec3<i32>(i32(params.origin.x), i32(params.origin.y), i32(params.origin.z));
    var t = t0;
    loop {
        if (t >= t1) { break; }
        let p = ro + dir * t;
        let v = vec3<i32>(i32(floor(p.x)), i32(floor(p.y)), i32(floor(p.z)));
        let id = vox(v - base);
        if (id != 0u) {
            // Opaque hit: return its emission (bounds-guarded — an id past the
            // table would be an out-of-range read, i.e. GPU UB).
            var em = vec3<f32>(0.0);
            if (id < arrayLength(&emission)) {
                em = emission[id].rgb;
            }
            return vec4<f32>(em, 0.0);
        }
        t += STEP;
    }
    return vec4<f32>(0.0, 0.0, 0.0, 1.0); // OPEN
}

@compute @workgroup_size(64)
fn trace(@builtin(global_invocation_id) gid: vec3<u32>) {
    let c = job.cascade;
    let probes = PROBES[c];
    let res = DIRRES[c];
    let dirs = res * res;
    let entries = probes * probes * probes * dirs;
    let e = gid.x;
    if (e >= entries) { return; }

    // Decode entry -> probe (px,py,pz) + direction (dx,dy).
    let pidx = e / dirs;
    let didx = e % dirs;
    let px = pidx % probes;
    let pz = (pidx / probes) % probes;
    let py = pidx / (probes * probes);
    let dx = didx % res;
    let dy = didx / res;

    let spacing = SPACING[c];
    let probe_pos = params.origin + (vec3<f32>(f32(px), f32(py), f32(pz)) + 0.5) * spacing;
    let dir = dir_for_texel(dx, dy, res);

    var r = trace_interval(probe_pos, dir, INT_START[c], INT_END[c]);
    // Top cascade: rays that escape see the sky (terminal), giving the whole
    // hierarchy an ambient sky term once merged down.
    if (c == CASCADES - 1u && r.w > 0.5) {
        r = vec4<f32>(sky(dir), 0.0);
    }
    cascade[e] = r;
}

fn far_entry(fc: u32, fpx: u32, fpy: u32, fpz: u32, fdx: u32, fdy: u32) -> vec4<f32> {
    let fprobes = PROBES[fc];
    let fres = DIRRES[fc];
    let cpx = min(fpx, fprobes - 1u);
    let cpy = min(fpy, fprobes - 1u);
    let cpz = min(fpz, fprobes - 1u);
    let pidx = (cpy * fprobes + cpz) * fprobes + cpx;
    let didx = fdy * fres + fdx;
    return far[pidx * (fres * fres) + didx];
}

// Radiance-cascades interval merge: near occludes far by its visibility.
fn merge_rad(n: vec4<f32>, f: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(n.rgb + n.w * f.rgb, n.w * f.w);
}

@compute @workgroup_size(64)
fn merge(@builtin(global_invocation_id) gid: vec3<u32>) {
    let c = job.cascade;       // near
    let fc = c + 1u;            // far
    let probes = PROBES[c];
    let res = DIRRES[c];
    let dirs = res * res;
    let entries = probes * probes * probes * dirs;
    let e = gid.x;
    if (e >= entries) { return; }

    let pidx = e / dirs;
    let didx = e % dirs;
    let px = pidx % probes;
    let pz = (pidx / probes) % probes;
    let py = pidx / (probes * probes);
    let dx = didx % res;
    let dy = didx / res;

    // Nearest far probe: the far grid is half the resolution, so this near
    // probe maps to far index round(px/2) (a standard RC spatial simplification
    // — angular merging below is exact; spatial uses nearest rather than
    // trilinear, trading a little smoothness for far fewer samples).
    let fpx = (px + (px & 1u)) >> 1u;
    let fpy = (py + (py & 1u)) >> 1u;
    let fpz = (pz + (pz & 1u)) >> 1u;

    // Angular 4:1 fold: average the four far directions (2dx+i, 2dy+j) that
    // this near direction subtends.
    var far_avg = vec4<f32>(0.0);
    for (var j = 0u; j < 2u; j += 1u) {
        for (var i = 0u; i < 2u; i += 1u) {
            far_avg += far_entry(fc, fpx, fpy, fpz, dx * 2u + i, dy * 2u + j);
        }
    }
    far_avg *= 0.25;

    cascade[e] = merge_rad(cascade[e], far_avg);
}
