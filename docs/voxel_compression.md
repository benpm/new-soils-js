# Voxel Compression in New Soils

The original concept proposes a one-bit occupancy volume for meshing and a
deferred material lookup after a depth/ID pass. The occupancy idea is viable;
the complete deferred renderer is not yet a good default for New Soils.

New Soils currently uses a GPU greedy mesher. It needs material IDs during face
mask construction, atlas tile selection, ambient occlusion, greedy merging, and
neighbour/LOD sampling. A visibility-buffer material pass would therefore be a
new renderer, not a local optimization. It would also add an ID attachment,
material-brick cache management, and extra synchronization to a path where
terrain rasterization is already negligible compared with client lighting.

The implemented first stage is conservative:

- `ChunkVolume::occupancy_words()` derives a 1-bit-per-voxel sidecar without
  increasing resident chunk memory.
- Sparse edited chunks can use codec tag `3`: a 4096-byte occupancy bitset plus
  non-air material IDs, LZ4-compressed.
- Existing uniform, palette, and raw-dense formats are unchanged.
- Decoding still produces the dense authoritative volume, preserving edits,
  lighting, collision, and GPU meshing behavior.

See `docs/plan-voxel-compression.md` for the staged GPU plan and benchmark
acceptance criteria. The next rendering experiment should upload occupancy
only as a derived GPU sidecar and measure the complete meshing pass before
considering deferred material shading.
