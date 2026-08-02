// GPU occupancy + light fill for radiance-cascades GI: blits chunk voxel and
// L0 light data straight out of the POOLED caches into the GI world volumes.
//
// Voxel layouts store block-id bytes packed little-endian in u32 words with x
// consecutive (chunk: (y + z*32)*32 + x; volume: (y*64 + z)*64 + x), and the
// volume origin is chunk-aligned — so whole u32 words (4 voxels) map 1:1 for
// both voxels and (now unpadded) light.

const CHUNK: i32 = 32;
const GI_DIM: i32 = 64;

struct BlitParams {
    // Chunk corner minus volume origin, in voxels (multiples of 32).
    rel: vec3<i32>,
    // Mesh slot in the voxel pool (0 = the all-zero air sentinel).
    mesh_slot: u32,
    // Unified slot in the light pool.
    light_slot: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<storage, read> voxel_pool: array<u32>;
@group(0) @binding(1) var<storage, read_write> world_vox: array<u32>;
@group(0) @binding(2) var<storage, read> params: BlitParams;
@group(0) @binding(3) var<storage, read> light_pool: array<u32>;
@group(0) @binding(4) var<storage, read_write> world_light: array<u32>;

// Reset the whole volume (dispatched once before a batch of blits): occupancy
// to air, light to full skylight — space with no resident chunk is open sky,
// so escaped rays keep seeing the sky there.
@compute @workgroup_size(64)
fn clear_volume(@builtin(global_invocation_id) gid: vec3<u32>) {
    let words = u32(GI_DIM * GI_DIM * GI_DIM) / 4u;
    if (gid.x < words) {
        world_vox[gid.x] = 0u;
        world_light[gid.x] = 0xf0f0f0f0u;
    }
}

// Copy one chunk into the volumes, one u32 word (4 voxels along x) per thread.
// Thread space: (8 words, 32 y, 32 z) per chunk = dispatch (1, 8, 8).
@compute @workgroup_size(8, 4, 4)
fn blit_chunk(@builtin(global_invocation_id) gid: vec3<u32>) {
    let wxw = i32(gid.x); // word index along x (0..8)
    let y = i32(gid.y);
    let z = i32(gid.z);
    if (wxw >= CHUNK / 4 || y >= CHUNK || z >= CHUNK) {
        return;
    }
    let vx = params.rel.x + wxw * 4;
    let vy = params.rel.y + y;
    let vz = params.rel.z + z;
    if (vx < 0 || vx + 3 >= GI_DIM || vy < 0 || vy >= GI_DIM || vz < 0 || vz >= GI_DIM) {
        return;
    }
    let src = u32(((y + z * CHUNK) * CHUNK + wxw * 4) / 4);
    let dst = u32(((vy * GI_DIM + vz) * GI_DIM + vx) / 4);
    world_vox[dst] = voxel_pool[params.mesh_slot * 8192u + src];
    world_light[dst] = light_pool[params.light_slot * 8192u + src];
}
