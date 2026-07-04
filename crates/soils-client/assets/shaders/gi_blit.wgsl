// GPU occupancy fill for radiance-cascades GI (plan-rendering §1 L2 item 1):
// blits chunk voxel buffers (already GPU-resident for the mesher) into the
// GI occupancy volume, replacing the old 262 KB CPU rebuild + upload.
//
// Both layouts store block-id bytes packed little-endian in u32 words with x
// consecutive (chunk: (y + z*32)*32 + x; volume: (y*64 + z)*64 + x), and the
// volume origin is chunk-aligned — so whole u32 words (4 voxels) map 1:1.

const CHUNK: i32 = 32;
const GI_DIM: i32 = 64;

struct BlitParams {
    // Chunk corner minus volume origin, in voxels (multiples of 32).
    rel: vec3<i32>,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> chunk_vox: array<u32>;
@group(0) @binding(1) var<storage, read_write> world_vox: array<u32>;
@group(0) @binding(2) var<storage, read> params: BlitParams;

// Zero the whole volume (dispatched once before a batch of blits, so space
// with no resident chunk reads as air).
@compute @workgroup_size(64)
fn clear_volume(@builtin(global_invocation_id) gid: vec3<u32>) {
    let words = u32(GI_DIM * GI_DIM * GI_DIM) / 4u;
    if (gid.x < words) {
        world_vox[gid.x] = 0u;
    }
}

// Copy one chunk into the volume, one u32 word (4 voxels along x) per thread.
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
    let src = ((y + z * CHUNK) * CHUNK + wxw * 4) / 4;
    let dst = ((vy * GI_DIM + vz) * GI_DIM + vx) / 4;
    world_vox[u32(dst)] = chunk_vox[u32(src)];
}
