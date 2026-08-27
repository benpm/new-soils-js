# Plan: Better Lighting

## Basic Idea

Instead of computing all the lighting client-side, coarse (per-voxel) lighting should be stored with the world data and cached on the client. The coarse lighting should be saved alongside the voxels as chunk data, and the fine lighting should be computed client-side only entirely on the GPU, never being copied to CPU or stored. 

The coarse lighting corresponds