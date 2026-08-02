// GPU greedy voxel mesher (compute) over the POOLED chunk caches. Port of
// crates/soils-worldgen/src/greedy.rs.
//
// Dispatched per remesh batch: `jobs` lists the mesh slots to rebuild;
// `clear_counter`/`finalize_mesh` run (jobs, 1, 1) and `mesh_slice` runs
// (3, 33, jobs) — workgroup (d, plane, job) of 32 lanes. Lanes cooperate on
// the slice's mask and AO passes (one 32-cell row each, barrier-separated);
// lane 0 then runs the serial AO-aware greedy sweep and appends merged quads
// to the slot's region of the pooled quad buffer via an atomic counter
// (parked in the slot's indirect-args word during the pass), reproducing the
// CPU output.
//
// A packed quad is two u32 words (8 B vs the old 80 B):
//   w0 = x:6 | y:6 | z:6 | w:6 | h:6 | axis:2
//   w1 = sign:1 | tile:8 | ao:8 (4 corner levels × 2 bits)
// The draw shader (atlas.wgsl) reconstructs base/du/dv/normal exactly as the
// emit below defines them.

const CHUNK: i32 = 32;
// Must match pool::QUADS_PER_SLOT.
const QUADS_PER_SLOT: u32 = 4096u;

@group(0) @binding(0) var<storage, read>       voxels: array<u32>;      // N_MESH × 8192 words
@group(0) @binding(1) var<storage, read_write> quads: array<u32>;       // N_MESH × QPS × 2 words
@group(0) @binding(2) var<storage, read>       block_faces: array<vec4<u32>>;
@group(0) @binding(3) var<storage, read_write> indirect: array<atomic<u32>>; // N_MESH × 4 words
@group(0) @binding(4) var<storage, read>       jobs: array<u32>;        // mesh slots this pass

// Per-slice scratch, filled cooperatively (one row per lane, see mesh_slice).
var<workgroup> mask: array<i32, 1024>;
var<workgroup> aokey: array<u32, 1024>; // 4 corner occlusion levels packed 8 bits each

fn vox(slot: u32, x: i32, y: i32, z: i32) -> u32 {
    if (x < 0 || x >= CHUNK || y < 0 || y >= CHUNK || z < 0 || z >= CHUNK) {
        return 0u;
    }
    let idx = (y + z * CHUNK) * CHUNK + x;
    let w = voxels[slot * 8192u + (u32(idx) >> 2u)];
    return (w >> ((u32(idx) & 3u) * 8u)) & 0xffu;
}

fn solid(slot: u32, p: array<i32, 3>) -> bool {
    return vox(slot, p[0], p[1], p[2]) != 0u;
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

fn ao_at(slot: u32, vp: array<i32, 3>, norm: array<i32, 3>, cx: array<i32, 3>, cy: array<i32, 3>, o: vec2<i32>) -> bool {
    var p = array<i32, 3>(
        vp[0] + norm[0] + cx[0] * o.x + cy[0] * o.y,
        vp[1] + norm[1] + cx[1] * o.x + cy[1] * o.y,
        vp[2] + norm[2] + cx[2] * o.x + cy[2] * o.y,
    );
    return solid(slot, p);
}

// Repack the 4×8-bit corner levels (0..3 each) into 4×2 bits.
fn pack_ao2(ao_packed: u32) -> u32 {
    return (ao_packed & 3u)
        | (((ao_packed >> 8u) & 3u) << 2u)
        | (((ao_packed >> 16u) & 3u) << 4u)
        | (((ao_packed >> 24u) & 3u) << 6u);
}

fn emit(slot: u32, base: array<i32, 3>, width: i32, height: i32, d: i32, positive: bool, block_id: u32, ao_packed: u32) {
    // The slot's vertex_count word doubles as the append counter during the
    // pass; finalize converts it to real draw args.
    let n = atomicAdd(&indirect[slot * 4u], 1u);
    if (n >= QUADS_PER_SLOT) { return; }

    var norm = array<i32, 3>(0, 0, 0);
    if (positive) { norm[d] = 1; } else { norm[d] = -1; }
    let w0 = u32(base[0]) | (u32(base[1]) << 6u) | (u32(base[2]) << 12u)
        | (u32(width) << 18u) | (u32(height) << 24u) | (u32(d) << 30u);
    let w1 = select(0u, 1u, positive)
        | (tile_for_normal(block_id, norm) << 1u)
        | (pack_ao2(ao_packed) << 9u);
    let at = (slot * QUADS_PER_SLOT + n) * 2u;
    quads[at] = w0;
    quads[at + 1u] = w1;
}

@compute @workgroup_size(1)
fn clear_counter(@builtin(workgroup_id) wg: vec3<u32>) {
    atomicStore(&indirect[jobs[wg.x] * 4u], 0u);
}

// Runs after mesh_slice (dispatches in one compute pass are ordered): clamps
// the overflowed count and publishes the slot's draw args. `first_vertex`
// points the shared vertex-pull shader at this slot's quad range;
// `first_instance` carries the slot id (read back via vertex_index math, no
// INDIRECT_FIRST_INSTANCE dependency).
@compute @workgroup_size(1)
fn finalize_mesh(@builtin(workgroup_id) wg: vec3<u32>) {
    let slot = jobs[wg.x];
    let n = min(atomicLoad(&indirect[slot * 4u]), QUADS_PER_SLOT);
    atomicStore(&indirect[slot * 4u], n * 6u);
    atomicStore(&indirect[slot * 4u + 1u], 1u);
    atomicStore(&indirect[slot * 4u + 2u], slot * QUADS_PER_SLOT * 6u);
    atomicStore(&indirect[slot * 4u + 3u], slot);
}

@compute @workgroup_size(32)
fn mesh_slice(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let slot = jobs[wg.z];
    let d = i32(wg.x);
    let plane = i32(wg.y); // 0..32 inclusive
    let lane = i32(lid.x); // this lane's mask row (jv)
    let xd = plane - 1; // CPU iterates x[d] from -1..31, then increments to `plane`
    let u = (d + 1) % 3;
    let v = (d + 2) % 3;

    let corner_uv = array<vec2<i32>, 4>(
        vec2<i32>(0, 0), vec2<i32>(1, 0), vec2<i32>(1, 1), vec2<i32>(0, 1),
    );
    let ao_offsets = array<vec2<i32>, 4>(
        vec2<i32>(-1, 0), vec2<i32>(-1, -1), vec2<i32>(0, -1), vec2<i32>(0, 0),
    );

    // --- Build the signed mask for this slice: one row per lane. ---
    {
        let jv = lane;
        var n = jv * CHUNK;
        for (var iu = 0; iu < CHUNK; iu = iu + 1) {
            var xa = array<i32, 3>(0, 0, 0);
            xa[d] = xd; xa[u] = iu; xa[v] = jv;
            var a = 0u;
            if (xd >= 0) { a = vox(slot, xa[0], xa[1], xa[2]); }
            var b = 0u;
            if (xd < CHUNK - 1) {
                b = vox(slot, xa[0] + sel(d, 0), xa[1] + sel(d, 1), xa[2] + sel(d, 2));
            }
            var m = 0;
            if ((a != 0u) == (b != 0u)) { m = 0; }
            else if (a != 0u) { m = i32(a); }
            else { m = -i32(b); }
            mask[n] = m;
            n = n + 1;
        }
    }
    workgroupBarrier();

    // --- Per-cell ambient occlusion (4 corner levels packed into aokey), the
    // hot pass (12 solid() probes per face cell): one row per lane. ---
    {
        let jv = lane;
        var n = jv * CHUNK;
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
                    let s1 = ao_at(slot, vp, norm, cx, cy, ao_offsets[w]);
                    let s2 = ao_at(slot, vp, norm, cx, cy, ao_offsets[(w + 2) % 4]);
                    let cc = ao_at(slot, vp, norm, cx, cy, ao_offsets[(w + 1) % 4]);
                    let lvl = occlusion(s1, s2, cc);
                    packed = packed | (u32(lvl) << (u32(w) * 8u));
                }
                aokey[n] = packed;
            }
            n = n + 1;
        }
    }
    workgroupBarrier();

    // --- Greedy merge + emit (AO-aware): serial scan, lane 0 only (the merge
    // is an inherently sequential sweep; the passes above are the hot part). ---
    if (lane != 0) { return; }
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

                var base = array<i32, 3>(0, 0, 0);
                base[d] = plane; base[u] = i; base[v] = j;

                emit(slot, base, width, height, d, positive, u32(block_id), base_key);

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
