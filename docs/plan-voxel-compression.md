# Voxel compression plan for New Soils

## Viability assessment

`docs/voxel_compression.md` describes a useful occupancy/material split, but
the proposed deferred ID-buffer material pass is not a safe first change for
New Soils. The current GPU greedy mesher needs material IDs while constructing
face masks, selecting atlas tiles, computing ambient occlusion, merging quads,
and sampling equal- or mixed-LOD neighbours. Deferring every material lookup
until after depth rasterization would require a new visibility-buffer renderer,
per-pixel source-cell IDs, material-cache residency, and a different treatment
for transparent/animated blocks. It would add passes and bandwidth to a
renderer whose steady-state terrain rasterization is already about 0.07 ms GPU
on the reference RTX 5070.

The viable path is incremental:

1. Keep the authoritative `ChunkVolume` dense and unchanged for edits,
   lighting, collision, and the current mesher.
2. Add a derived one-bit occupancy sidecar API. Do not store it beside every
   chunk until measurements prove the extra 4 KiB per chunk is worthwhile.
3. Use occupancy-aware sparse wire encoding for edited/sparse chunks. The
   payload contains a 4 KiB bitset followed by only non-air material IDs,
   compressed with the existing LZ4 transport.
4. Benchmark codec size/CPU and GPU frame time separately. A network win must
   not be reported as a rendering win.
5. Only prototype GPU occupancy-first meshing after the sidecar reduces
   meshing bandwidth in a capture; retain the dense material pool as the
   fallback for AO, tiles, neighbours, and edits.

## New Soils implementation details

- `ChunkVolume::occupancy_words()` uses the existing voxel index order and
  returns 1024 little-endian `u32` words for a 32^3 chunk.
- Codec tag `3` is selected for 1..4096 occupied voxels. Its decoded form is
  bounded to 4096 occupancy bytes plus 32768 material bytes, so malformed
  payloads cannot request unbounded memory.
- Uniform, palette, and raw-dense encodings remain unchanged. This preserves
  existing golden bytes and compatibility for all established payload classes.
- The client and server still decode into dense `ChunkVolume` values. This is
  intentional: it avoids changing lighting, collision, edit overlays, GPU
  voxel uploads, or LOD neighbour semantics in the first slice.
- Criterion reports encode/decode and occupancy extraction. The existing
  release self-test/render diagnostic remains the source of truth for frame
  time; codec throughput must not be conflated with render time.

## Acceptance measurements

Run:

```text
cargo test -p soils-protocol
cargo bench -p soils-protocol --bench codec
SOILS_SELFTEST=1 SOILS_RADIUS=8 SOILS_VSYNC=0 SOILS_RENDERDIAG=1 \
  cargo run --release -p soils-client
```

Record payload sizes for air, solid, sparse, and layered surface chunks,
encode/decode medians, and the steady-state frame time after the light backlog
drains. A future occupancy-first GPU implementation must beat the dense path
in a matched render capture before it is enabled by default.
