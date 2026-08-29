# Plan: Better Lighting (Cached Voxel Radiance Cascades)

## Idea

Instead of computing all the lighting client-side, coarse (per-voxel) lighting should be stored with the world data and cached on the client. The coarse lighting should be saved alongside the voxels as chunk data, and the fine lighting should be computed client-side only entirely on the GPU, never being copied to CPU or stored. 

- Use the implementation in `~/projects/voxel_radiance_cascades` as a starting point 
- Make sure to add notes to this plan that would make it no longer necessary to be able to access that implementation in its entirety
- Cache and store the coarse lighting information. 
- Use bilinear interpolation or better to interpolate the light values across voxels to acheive a smoother lighting effect
- If a chunk is likely to be culled from drawing entirely (surrounded on all sides by other chunks that have a high density of voxels (measure this upon chunk gen using an atomic counter), prioritize its lighting to be done later
- Only compute higher detailed lighting, dynamic light, etc, for chunks that are near the player, and ONLY ON CLIENT

## Detailed Plan
<!-- AGENT: put detailed plan here -->