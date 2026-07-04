// GPU greedy voxel mesher (compute). Port of crates/soils-worldgen/src/greedy.rs.
//
// Dispatched as one workgroup per (axis d in 0..3, plane in 0..32) = (3, 33, 1)
// workgroups of size 1. Each invocation runs the serial 32x32 signed-mask +
// AO-aware greedy sweep for its slice and appends merged quads to a shared
// output buffer via an atomic counter, reproducing the CPU output.

const CHUNK: i32 = 32;
const MAX_QUADS: u32 = 8192u;

struct QuadGpu {
    base: vec3<f32>, tile: u32,
    du: vec3<f32>,   nx: f32,
    dv: vec3<f32>,   ny: f32,
    ao: vec4<f32>,
    nz: f32, pad0: f32, pad1: f32, pad2: f32,
};

struct QuadBuffer {
    count: atomic<u32>,
    p0: u32, p1: u32, p2: u32,
    quads: array<QuadGpu>,
};

// Matches wgpu's DrawIndirectArgs; consumed by the chunk's draw_indirect call.
struct IndirectArgs {
    vertex_count: u32,
    instance_count: u32,
    first_vertex: u32,
    first_instance: u32,
};

@group(0) @binding(0) var<storage, read>       voxels: array<u32>;
@group(0) @binding(1) var<storage, read_write> out_buf: QuadBuffer;
@group(0) @binding(2) var<storage, read>       block_faces: array<vec4<u32>>;
@group(0) @binding(3) var<storage, read_write> indirect: IndirectArgs;

// Per-slice scratch (one thread per workgroup, so effectively private).
var<workgroup> mask: array<i32, 1024>;
var<workgroup> aokey: array<u32, 1024>; // 4 corner occlusion levels packed 8 bits each

fn vox(x: i32, y: i32, z: i32) -> u32 {
    if (x < 0 || x >= CHUNK || y < 0 || y >= CHUNK || z < 0 || z >= CHUNK) {
        return 0u;
    }
    let idx = (y + z * CHUNK) * CHUNK + x;
    let w = voxels[u32(idx) >> 2u];
    return (w >> ((u32(idx) & 3u) * 8u)) & 0xffu;
}

fn solid(p: array<i32, 3>) -> bool {
    return vox(p[0], p[1], p[2]) != 0u;
}

fn occlusion(s1: bool, s2: bool, c: bool) -> i32 {
    if (s1 && s2) { return 0; }
    return 3 - (i32(s1) + i32(s2) + i32(c));
}

fn tile_for_normal(block_id: u32, n: array<i32, 3>) -> u32 {
    let faces = block_faces[block_id]; // x=sides, y=top, z=bottom
    let hash = u32(((n[0] + 1) * 3 + (n[1] + 1) * 2 + (n[2] + 1)) % 6);
    // CPU: idx = hash.wrapping_sub(1); table[min(idx,4)] with table
    // [sides, top, sides, bottom, sides].
    var idx: u32 = 4u;
    if (hash != 0u) { idx = hash - 1u; }
    if (idx > 4u) { idx = 4u; }
    if (idx == 1u) { return faces.y; }
    if (idx == 3u) { return faces.z; }
    return faces.x;
}

fn sel(axis: i32, comp: i32) -> i32 {
    if (axis == comp) { return 1; }
    return 0;
}

fn ao_at(vp: array<i32, 3>, norm: array<i32, 3>, cx: array<i32, 3>, cy: array<i32, 3>, o: vec2<i32>) -> bool {
    var p = array<i32, 3>(
        vp[0] + norm[0] + cx[0] * o.x + cy[0] * o.y,
        vp[1] + norm[1] + cx[1] * o.x + cy[1] * o.y,
        vp[2] + norm[2] + cx[2] * o.x + cy[2] * o.y,
    );
    return solid(p);
}

fn level_bright(packed: u32, w: u32) -> f32 {
    let level = (packed >> (w * 8u)) & 0xffu;
    return 0.1 + f32(level) * 0.3;
}

fn emit(base: array<i32, 3>, du: array<i32, 3>, dv: array<i32, 3>, norm: array<i32, 3>, block_id: u32, ao_packed: u32) {
    let slot = atomicAdd(&out_buf.count, 1u);
    if (slot >= MAX_QUADS) { return; }

    var q: QuadGpu;
    q.base = vec3<f32>(f32(base[0]), f32(base[1]), f32(base[2]));
    q.du = vec3<f32>(f32(du[0]), f32(du[1]), f32(du[2]));
    q.dv = vec3<f32>(f32(dv[0]), f32(dv[1]), f32(dv[2]));
    q.nx = f32(norm[0]);
    q.ny = f32(norm[1]);
    q.nz = f32(norm[2]);
    q.tile = tile_for_normal(block_id, norm);
    q.ao = vec4<f32>(
        level_bright(ao_packed, 0u),
        level_bright(ao_packed, 1u),
        level_bright(ao_packed, 2u),
        level_bright(ao_packed, 3u),
    );
    q.pad0 = 0.0; q.pad1 = 0.0; q.pad2 = 0.0;
    out_buf.quads[slot] = q;
}

@compute @workgroup_size(1)
fn clear_counter() {
    atomicStore(&out_buf.count, 0u);
}

// Runs after mesh_slice (dispatches in one compute pass are ordered): clamps
// the overflowed count and publishes the draw args, so the render pass draws
// exactly count*6 vertices instead of a fixed worst-case dummy mesh.
@compute @workgroup_size(1)
fn finalize_mesh() {
    let n = min(atomicLoad(&out_buf.count), MAX_QUADS);
    atomicStore(&out_buf.count, n);
    indirect.vertex_count = n * 6u;
    indirect.instance_count = 1u;
    indirect.first_vertex = 0u;
    indirect.first_instance = 0u;
}

@compute @workgroup_size(1)
fn mesh_slice(@builtin(global_invocation_id) gid: vec3<u32>) {
    let d = i32(gid.x);
    if (d > 2) { return; }
    let plane = i32(gid.y); // 0..32 inclusive
    if (plane > CHUNK) { return; }
    let xd = plane - 1; // CPU iterates x[d] from -1..31, then increments to `plane`
    let u = (d + 1) % 3;
    let v = (d + 2) % 3;

    let corner_uv = array<vec2<i32>, 4>(
        vec2<i32>(0, 0), vec2<i32>(1, 0), vec2<i32>(1, 1), vec2<i32>(0, 1),
    );
    let ao_offsets = array<vec2<i32>, 4>(
        vec2<i32>(-1, 0), vec2<i32>(-1, -1), vec2<i32>(0, -1), vec2<i32>(0, 0),
    );

    // --- Build the signed mask for this slice. ---
    var n = 0;
    for (var jv = 0; jv < CHUNK; jv = jv + 1) {
        for (var iu = 0; iu < CHUNK; iu = iu + 1) {
            var xa = array<i32, 3>(0, 0, 0);
            xa[d] = xd; xa[u] = iu; xa[v] = jv;
            var a = 0u;
            if (xd >= 0) { a = vox(xa[0], xa[1], xa[2]); }
            var b = 0u;
            if (xd < CHUNK - 1) {
                b = vox(xa[0] + sel(d, 0), xa[1] + sel(d, 1), xa[2] + sel(d, 2));
            }
            var m = 0;
            if ((a != 0u) == (b != 0u)) { m = 0; }
            else if (a != 0u) { m = i32(a); }
            else { m = -i32(b); }
            mask[n] = m;
            n = n + 1;
        }
    }

    // --- Per-cell ambient occlusion (4 corner levels packed into aokey). ---
    n = 0;
    for (var jv = 0; jv < CHUNK; jv = jv + 1) {
        for (var iu = 0; iu < CHUNK; iu = iu + 1) {
            let c = mask[n];
            if (c != 0) {
                let positive = c > 0;
                var norm = array<i32, 3>(0, 0, 0);
                if (positive) { norm[d] = 1; } else { norm[d] = -1; }
                var cx = array<i32, 3>(0, 0, 0);
                var cy = array<i32, 3>(0, 0, 0);
                if (positive) { cy[(d + 2) % 3] = 1; cx[(d + 1) % 3] = 1; }
                else          { cx[(d + 2) % 3] = 1; cy[(d + 1) % 3] = 1; }
                var base = array<i32, 3>(0, 0, 0);
                base[d] = plane; base[u] = iu; base[v] = jv;

                var packed = 0u;
                for (var w = 0; w < 4; w = w + 1) {
                    let ab = corner_uv[w];
                    var vp = array<i32, 3>(
                        base[0] + cx[0] * ab.x + cy[0] * ab.y,
                        base[1] + cx[1] * ab.x + cy[1] * ab.y,
                        base[2] + cx[2] * ab.x + cy[2] * ab.y,
                    );
                    let s1 = ao_at(vp, norm, cx, cy, ao_offsets[w]);
                    let s2 = ao_at(vp, norm, cx, cy, ao_offsets[(w + 2) % 4]);
                    let cc = ao_at(vp, norm, cx, cy, ao_offsets[(w + 1) % 4]);
                    let lvl = occlusion(s1, s2, cc);
                    packed = packed | (u32(lvl) << (u32(w) * 8u));
                }
                aokey[n] = packed;
            }
            n = n + 1;
        }
    }

    // --- Greedy merge + emit (AO-aware). ---
    var j = 0;
    loop {
        if (j >= CHUNK) { break; }
        var i = 0;
        loop {
            if (i >= CHUNK) { break; }
            let nn = j * CHUNK + i;
            let c = mask[nn];
            if (c != 0) {
                let base_key = aokey[nn];
                var width = 1;
                loop {
                    if (i + width >= CHUNK) { break; }
                    if (mask[nn + width] != c || aokey[nn + width] != base_key) { break; }
                    width = width + 1;
                }
                var height = 1;
                var stop = false;
                loop {
                    if (j + height >= CHUNK || stop) { break; }
                    var k = 0;
                    loop {
                        if (k >= width) { break; }
                        let idx = nn + k + height * CHUNK;
                        if (mask[idx] != c || aokey[idx] != base_key) { stop = true; break; }
                        k = k + 1;
                    }
                    if (!stop) { height = height + 1; }
                }

                let positive = c > 0;
                var block_id = c;
                if (!positive) { block_id = -c; }

                var du = array<i32, 3>(0, 0, 0);
                var dv = array<i32, 3>(0, 0, 0);
                var norm = array<i32, 3>(0, 0, 0);
                if (positive) {
                    dv[v] = height; du[u] = width; norm[d] = 1;
                } else {
                    du[v] = height; dv[u] = width; norm[d] = -1;
                }

                var base = array<i32, 3>(0, 0, 0);
                base[d] = plane; base[u] = i; base[v] = j;

                emit(base, du, dv, norm, u32(block_id), base_key);

                for (var l = 0; l < height; l = l + 1) {
                    for (var kk = 0; kk < width; kk = kk + 1) {
                        mask[nn + kk + l * CHUNK] = 0;
                    }
                }
                i = i + width;
            } else {
                i = i + 1;
            }
        }
        j = j + 1;
    }
}
