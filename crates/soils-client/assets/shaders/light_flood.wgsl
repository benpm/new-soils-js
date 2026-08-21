// GPU L0 light flood over the pooled chunk caches — the compute replacement
// for the CPU `soils_sim::light` flood (which cost ~29 ms/frame and took
// minutes to drain a radius-8 join). Semantics mirror `soils_sim::light`
// exactly; the light_gpu oracle test byte-compares converged output against
// `relight_full`.
//
// Model (matches Channel::step): packed byte per voxel, sky hi nibble, block
// lo. Light spreads into non-solid, in-domain cells at level-1 per step,
// except a sky-15 falls downward losslessly (the beam). Solid cells carry
// pack(0, emission) — emitters radiate outward but nothing propagates in.
// "In domain" = the chunk resolves through the slot table; an unmapped chunk
// above means optimistic open sky (corrected when it maps — the CPU reseeds
// the column below any newly mapped chunk).
//
// Passes per scheduling round (CPU-ordered):
//   1. `reseed` (dirty core): zero + seeds (emission; sky 15 at the top layer
//      under an unmapped chunk).
//   2. `beam` (dirty core, one dispatch per chunk-y layer, top-down): per
//      column, fall the sky beam from the above chunk's bottom (or 15 when
//      unmapped) through the 32 cells. Ordered layers make beams exact in one
//      sweep; laterals can't mint new 15s.
//   3. `relax` × N (core + 1-ring): Jacobi raise-only max over the 6
//      neighbors with step semantics, crossing chunk borders through the
//      slot table. Monotone, so racy cross-slot reads only slow convergence;
//      15 rounds bound any attenuated path.

struct ChunkSlot {
    cpos: vec3<i32>,
    mesh_slot: u32,
    flags: u32,
    flags_gpu: u32,
    quad_count: u32,
    pad: u32,
};

/// Per-job data: the slot being lit and its chunk coordinate.
struct LightJob {
    cpos: vec3<i32>,
    slot: u32,
    mesh_slot: u32,
    _p0: u32,
    _p1: u32,
    _p2: u32,
};

@group(0) @binding(0) var<storage, read_write> light_pool: array<u32>; // N_SLOTS × 8192 words
@group(0) @binding(1) var<storage, read> voxel_pool: array<u32>;       // N_MESH × 8192 words
@group(0) @binding(2) var<storage, read> desc: array<ChunkSlot>;
@group(0) @binding(3) var<storage, read> slot_table: array<u32>;
@group(0) @binding(4) var<storage, read> emitters: array<u32>;         // block id → emission (u32 rows)
@group(0) @binding(5) var<storage, read> jobs: array<LightJob>;

const TABLE_EMPTY: u32 = 0xffffffffu;
const NO_MESH: u32 = 0xffffffffu;
const MAX_LIGHT: u32 = 15u;

fn slot_of(cpos: vec3<i32>) -> u32 {
    let c = cpos & vec3<i32>(31);
    let slot = slot_table[u32(c.x + c.y * 32 + c.z * 1024)];
    if (slot == TABLE_EMPTY) { return TABLE_EMPTY; }
    if (any(desc[slot].cpos != cpos)) { return TABLE_EMPTY; }
    return slot;
}

fn voxel_at(mesh_slot: u32, l: vec3<i32>) -> u32 {
    // Mesh slot 0 is the shared all-air sentinel; air chunks use it.
    let idx = u32((l.y + l.z * 32) * 32 + l.x);
    let w = voxel_pool[mesh_slot * 8192u + (idx >> 2u)];
    return (w >> ((idx & 3u) * 8u)) & 0xffu;
}

fn light_byte(slot: u32, l: vec3<i32>) -> u32 {
    let idx = u32((l.y + l.z * 32) * 32 + l.x);
    let w = light_pool[slot * 8192u + (idx >> 2u)];
    return (w >> ((idx & 3u) * 8u)) & 0xffu;
}

/// Read the packed light of an arbitrary world voxel through the table.
/// (0, false) when the containing chunk is unmapped.
fn world_light(v: vec3<i32>) -> vec2<u32> {
    let cpos = v >> vec3<u32>(5u);
    let slot = slot_of(cpos);
    if (slot == TABLE_EMPTY) { return vec2<u32>(0u, 0u); }
    return vec2<u32>(light_byte(slot, v & vec3<i32>(31)), 1u);
}

/// Solidity of an arbitrary world voxel; unmapped chunks read air (out of
/// domain — the relax never *writes* there, and reads contribute 0 anyway).
fn world_solid(v: vec3<i32>) -> bool {
    let cpos = v >> vec3<u32>(5u);
    let slot = slot_of(cpos);
    if (slot == TABLE_EMPTY) { return false; }
    let mesh = desc[slot].mesh_slot;
    if (mesh == NO_MESH) { return false; }
    return voxel_at(mesh, v & vec3<i32>(31)) != 0u;
}

// The mesher/other passes write whole bytes; light words are only written by
// this shader, one word (4 x-adjacent voxels) per thread in reseed/beam and
// one CELL per thread in relax — so relax threads must pack read-modify-write
// per word. To avoid intra-dispatch races on shared words, relax processes
// one word (4 cells) per thread too.

@compute @workgroup_size(64)
fn reseed(@builtin(global_invocation_id) gid: vec3<u32>) {
    // One thread per word: 8192 words per slot → dispatch (128, jobs, 1).
    let word = gid.x;
    if (word >= 8192u) { return; }
    let job = jobs[gid.y];
    // Word covers cells (x0..x0+4, y, z).
    let base_idx = word * 4u;
    let y = i32((base_idx / 32u) % 32u);
    let z = i32(base_idx / 1024u);
    let x0 = i32(base_idx % 32u);
    // Is the chunk above unmapped (optimistic open sky)?
    let above_open = slot_of(job.cpos + vec3<i32>(0, 1, 0)) == TABLE_EMPTY;

    var out = 0u;
    for (var k = 0; k < 4; k = k + 1) {
        let l = vec3<i32>(x0 + k, y, z);
        var b = 0u;
        let id = select(0u, voxel_at(job.mesh_slot, l), job.mesh_slot != NO_MESH);
        if (id != 0u) {
            // Solid: pack(0, emission).
            b = emitters[id] & 15u;
        } else {
            let e = emitters[id] & 15u; // air emission is 0; kept for parity
            var s = 0u;
            if (y == 31 && above_open) {
                s = MAX_LIGHT;
            }
            b = (s << 4u) | e;
        }
        out = out | (b << (u32(k) * 8u));
    }
    light_pool[job.slot * 8192u + word] = out;
}

@compute @workgroup_size(64)
fn beam(@builtin(global_invocation_id) gid: vec3<u32>) {
    // One thread per (x-word, z) column strip: 8 words × 32 z = 256 threads
    // per chunk → dispatch (4, jobs, 1).
    let strip = gid.x;
    if (strip >= 256u) { return; }
    let job = jobs[gid.y];
    let xw = i32(strip % 8u);
    let z = i32(strip / 8u);

    // Feed per column: the above chunk's bottom sky, or 15 when unmapped.
    let above = slot_of(job.cpos + vec3<i32>(0, 1, 0));
    var feed = vec4<u32>(MAX_LIGHT);
    if (above != TABLE_EMPTY) {
        for (var k = 0; k < 4; k = k + 1) {
            feed[k] = light_byte(above, vec3<i32>(xw * 4 + k, 0, z)) >> 4u;
        }
    }
    // Walk down, raise-only merging into the word.
    for (var y = 31; y >= 0; y = y - 1) {
        let word = u32((y + z * 32) * 8 + xw);
        var w = light_pool[job.slot * 8192u + word];
        for (var k = 0; k < 4; k = k + 1) {
            let l = vec3<i32>(xw * 4 + k, y, z);
            let id = select(0u, voxel_at(job.mesh_slot, l), job.mesh_slot != NO_MESH);
            if (id != 0u) {
                feed[k] = 0u; // beam blocked; solid keeps its seed byte
                continue;
            }
            // Level entering this cell from above (lossless at 15).
            var enter = feed[k];
            if (enter != MAX_LIGHT && enter > 0u) {
                enter = enter - 1u;
            }
            let cur = (w >> (u32(k) * 8u)) & 0xffu;
            let s = max(cur >> 4u, enter);
            let nb = (s << 4u) | (cur & 15u);
            w = (w & ~(0xffu << (u32(k) * 8u))) | (nb << (u32(k) * 8u));
            feed[k] = s; // the next cell falls from what this cell now holds
        }
        light_pool[job.slot * 8192u + word] = w;
    }
}

@compute @workgroup_size(64)
fn relax(@builtin(global_invocation_id) gid: vec3<u32>) {
    // One thread per word (4 x-adjacent cells) → dispatch (128, jobs, 1).
    let word = gid.x;
    if (word >= 8192u) { return; }
    let job = jobs[gid.y];
    let base_idx = word * 4u;
    let y = i32((base_idx / 32u) % 32u);
    let z = i32(base_idx / 1024u);
    let x0 = i32(base_idx % 32u);
    let origin = job.cpos * 32;

    var w = light_pool[job.slot * 8192u + word];
    var changed = false;
    for (var k = 0; k < 4; k = k + 1) {
        let l = vec3<i32>(x0 + k, y, z);
        let id = select(0u, voxel_at(job.mesh_slot, l), job.mesh_slot != NO_MESH);
        if (id != 0u) {
            continue; // solid cells never receive propagation
        }
        let cur = (w >> (u32(k) * 8u)) & 0xffu;
        var s = cur >> 4u;
        var b = cur & 15u;
        let v = origin + l;
        // 6 neighbors; sky falls losslessly from directly above at 15.
        for (var d = 0; d < 6; d = d + 1) {
            var dir = vec3<i32>(0);
            dir[d / 2] = 1 - 2 * (d % 2); // +x,-x,+y,-y,+z,-z
            let nl = world_light(v + dir);
            if (nl.y == 0u) { continue; }
            let ns = nl.x >> 4u;
            let nb = nl.x & 15u;
            // Contribution from neighbor to us: step(level, dir n→v).
            var cs = select(ns - 1u, ns, dir.y == 1 && ns == MAX_LIGHT);
            if (ns == 0u) { cs = 0u; }
            s = max(s, cs);
            b = max(b, max(nb, 1u) - 1u);
        }
        let nbyte = (s << 4u) | b;
        if (nbyte != cur) {
            w = (w & ~(0xffu << (u32(k) * 8u))) | (nbyte << (u32(k) * 8u));
            changed = true;
        }
    }
    if (changed) {
        light_pool[job.slot * 8192u + word] = w;
    }
}
