// Per-probe irradiance projection for radiance-cascades GI (plan-rendering
// §1 L2 item 4): after the merge chain finishes, fold each cascade-0 probe's
// 16 incoming directions into a 6-face ambient cube (cosine-weighted, same
// integral as `soils_worldgen::radiance::gather_irradiance`). The terrain
// fragment shader then does a trilinear 8-probe ambient-cube fetch instead of
// re-integrating 16 directions per fragment.
//
// Output layout: probe-major, 6 vec4 rows per probe in face order
// +x, -x, +y, -y, +z, -z (probe index (y*16 + z)*16 + x, as elsewhere).

const PROBES: u32 = 16u;   // cascade-0 probes per axis (radiance.wgsl)
const DIRRES: u32 = 4u;    // cascade-0 direction resolution

@group(0) @binding(0) var<storage, read> cascade0: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> probes_out: array<vec4<f32>>;

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

fn face_normal(f: u32) -> vec3<f32> {
    switch f {
        case 0u: { return vec3<f32>(1.0, 0.0, 0.0); }
        case 1u: { return vec3<f32>(-1.0, 0.0, 0.0); }
        case 2u: { return vec3<f32>(0.0, 1.0, 0.0); }
        case 3u: { return vec3<f32>(0.0, -1.0, 0.0); }
        case 4u: { return vec3<f32>(0.0, 0.0, 1.0); }
        default: { return vec3<f32>(0.0, 0.0, -1.0); }
    }
}

// One thread per (probe, face): 16^3 * 6 = 24576 threads.
@compute @workgroup_size(64)
fn project(@builtin(global_invocation_id) gid: vec3<u32>) {
    let entries = PROBES * PROBES * PROBES * 6u;
    if (gid.x >= entries) {
        return;
    }
    let p = gid.x / 6u;
    let n = face_normal(gid.x % 6u);
    let dirs = DIRRES * DIRRES;
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var dy = 0u; dy < DIRRES; dy += 1u) {
        for (var dx = 0u; dx < DIRRES; dx += 1u) {
            let dir = octa_decode((f32(dx) + 0.5) / f32(DIRRES), (f32(dy) + 0.5) / f32(DIRRES));
            let ndl = max(dot(dir, n), 0.0);
            if (ndl <= 0.0) { continue; }
            acc += cascade0[p * dirs + dy * DIRRES + dx].rgb * ndl;
            wsum += ndl;
        }
    }
    var irr = vec3<f32>(0.0);
    if (wsum > 0.0) {
        irr = acc / wsum;
    }
    probes_out[gid.x] = vec4<f32>(irr, 1.0);
}
