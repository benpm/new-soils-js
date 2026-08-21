// GPU chunk culling + demand scan over the pooled caches, run every frame.
//
// `cull`: one thread per mesh slot — frustum-test the chunk's 32³ AABB and
// write the slot's indirect `instance_count` (0/1). Runs after the mesher in
// the graph, so its verdict is what the draw sees.
//
// `demand_scan`: one thread per position in the desired window (camera chunk ±
// radius) — a slot-table miss (or stale cell) appends a 16-byte demand record:
// the GPU telling the CPU which chunks it wants. The CPU reads the buffer back
// and turns records into gen/stream jobs (self-healing: records repeat every
// frame until the chunk is mapped).

struct CullParams {
    // View-frustum half-spaces (normal.xyz, d), pointing inward.
    planes: array<vec4<f32>, 6>,
    // Camera chunk coordinate and desired radius (chunks).
    camera_chunk: vec3<i32>,
    radius: i32,
}

struct ChunkSlot {
    cpos: vec3<i32>,
    mesh_slot: u32,
    flags: u32,
    flags_gpu: u32,
    quad_count: u32,
    pad: u32,
};

struct DemandRecord {
    cpos: vec3<i32>,
    // dist² to camera in the low bits (CPU sorts nearest-first).
    key: u32,
}

struct DemandBuffer {
    count: atomic<u32>,
    _p0: u32,
    _p1: u32,
    _p2: u32,
    records: array<DemandRecord>,
}

@group(0) @binding(0) var<uniform> params: CullParams;
@group(0) @binding(1) var<storage, read> mesh_info: array<vec4<i32>>;
@group(0) @binding(2) var<storage, read_write> indirect: array<u32>; // N_MESH × 4 words
@group(0) @binding(3) var<storage, read> desc: array<ChunkSlot>;
@group(0) @binding(4) var<storage, read> slot_table: array<u32>;
@group(0) @binding(5) var<storage, read_write> demands: DemandBuffer;

const N_MESH: u32 = 4096u;
const TABLE_EMPTY: u32 = 0xffffffffu;
const DEMAND_CAP: u32 = 8192u;

// AABB-vs-frustum: the box is fully outside if its positive vertex along any
// plane normal is behind that plane.
fn aabb_visible(mn: vec3<f32>, mx: vec3<f32>) -> bool {
    for (var i = 0; i < 6; i = i + 1) {
        let p = params.planes[i];
        let v = vec3<f32>(
            select(mn.x, mx.x, p.x > 0.0),
            select(mn.y, mx.y, p.y > 0.0),
            select(mn.z, mx.z, p.z > 0.0),
        );
        if (dot(p.xyz, v) + p.w < 0.0) {
            return false;
        }
    }
    return true;
}

@compute @workgroup_size(64)
fn cull(@builtin(global_invocation_id) gid: vec3<u32>) {
    let slot = gid.x;
    if (slot >= N_MESH) {
        return;
    }
    let info = mesh_info[slot];
    // Unallocated / freed mesh slots have their light-slot word poisoned.
    if (u32(info.w) == TABLE_EMPTY) {
        indirect[slot * 4u + 1u] = 0u;
        return;
    }
    let mn = vec3<f32>(info.xyz * 32);
    let visible = aabb_visible(mn, mn + vec3<f32>(32.0));
    indirect[slot * 4u + 1u] = select(0u, 1u, visible);
}

@compute @workgroup_size(4, 4, 4)
fn demand_scan(@builtin(global_invocation_id) gid: vec3<u32>) {
    let side = u32(params.radius * 2 + 1);
    if (gid.x >= side || gid.y >= side || gid.z >= side) {
        return;
    }
    let offs = vec3<i32>(gid) - vec3<i32>(params.radius);
    let cpos = params.camera_chunk + offs;
    // Resolve through the wrap-window table; a stale or vacant cell means the
    // chunk isn't mapped.
    let m = vec3<i32>(31);
    let c = cpos & m;
    let slot = slot_table[u32(c.x + c.y * 32 + c.z * 1024)];
    if (slot != TABLE_EMPTY && all(desc[slot].cpos == cpos)) {
        return;
    }
    let n = atomicAdd(&demands.count, 1u);
    if (n >= DEMAND_CAP) {
        return; // count still reports the truth; CPU clamps
    }
    let d2 = dot(offs, offs);
    demands.records[n] = DemandRecord(cpos, u32(d2));
}
