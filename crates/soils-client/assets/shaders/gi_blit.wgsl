// GPU occupancy + light fill for radiance-cascades GI (plan-rendering §1 L2
// items 1 and 2): blits chunk voxel and padded L0 light buffers (both already
// GPU-resident for the mesher/material) into the GI world volumes, replacing
// the old 262 KB CPU rebuild + upload.
//
// Voxel layouts store block-id bytes packed little-endian in u32 words with x
// consecutive (chunk: (y + z*32)*32 + x; volume: (y*64 + z)*64 + x), and the
// volume origin is chunk-aligned — so whole u32 words (4 voxels) map 1:1.
// The light source is the material's padded 34³ volume (interior voxel at
// +1 per axis), whose rows aren't word-aligned, so light bytes are gathered
// individually and repacked per output word.

const CHUNK: i32 = 32;
const GI_DIM: i32 = 64;
// Must match `gpu_mesh::LIGHT_PAD` / LPAD in atlas.wgsl.
const LPAD: i32 = 34;

struct BlitParams {
    // Chunk corner minus volume origin, in voxels (multiples of 32).
    rel: vec3<i32>,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> chunk_vox: array<u32>;
@group(0) @binding(1) var<storage, read_write> world_vox: array<u32>;
@group(0) @binding(2) var<storage, read> params: BlitParams;
@group(0) @binding(3) var<storage, read> chunk_light: array<u32>;
@group(0) @binding(4) var<storage, read_write> world_light: array<u32>;

// L0 light byte (sky nibble hi, block nibble lo) for chunk-local voxel v.
fn pad_light(v: vec3<i32>) -> u32 {
    let idx = u32(((v.y + 1) + (v.z + 1) * LPAD) * LPAD + (v.x + 1));
    return (chunk_light[idx >> 2u] >> ((idx & 3u) * 8u)) & 0xffu;
}

// Reset the whole volume (dispatched once before a batch of blits): occupancy
// to air, light to full skylight — space with no resident chunk is open sky
// (air chunks have no GPU buffers), so escaped rays keep seeing the sky there.
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
    let src = ((y + z * CHUNK) * CHUNK + wxw * 4) / 4;
    let dst = ((vy * GI_DIM + vz) * GI_DIM + vx) / 4;
    world_vox[u32(dst)] = chunk_vox[u32(src)];
    var light = 0u;
    for (var i = 0; i < 4; i += 1) {
        light |= pad_light(vec3<i32>(wxw * 4 + i, y, z)) << u32(i * 8);
    }
    world_light[u32(dst)] = light;
}
