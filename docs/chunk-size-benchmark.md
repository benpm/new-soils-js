# Chunk-size experiment

## Scope

This experiment compares compile-time `16³`, `32³`, and `64³` chunks in the
protocol and CPU world generator. Each variant was built in an isolated
worktree from the same release commit after removing the saved world at
`data\singleplayer`. Criterion medians are reported below; ranges are omitted
for readability, but the raw logs are retained in the session artifacts.

The current client renderer is still 32³-specific: its WGSL mesher, voxel pool
stride, generation shader, light buffers, and LOD neighbour rules all encode
32³ assumptions. Therefore these are not GPU frame-time comparisons. A
different renderer variant would need to be implemented and validated before
using these numbers as a rendering decision.

## World-generation medians

| Benchmark | 16³ | 32³ | 64³ |
|---|---:|---:|---:|
| `wave48` | 1.59 ms | 6.91 ms | 23.94 ms |
| `surface_chunk` | 177.8 us | 587.4 us | 2.34 ms |
| `solid_chunk` | 177.3 us | 948.3 us | 4.60 ms |
| `air_chunk` | 177.3 us | 533.5 us | 3.31 us |

The 64³ air result is an early-out: the selected benchmark coordinate is
above the terrain ceiling, so it does not pay the 64³ voxel loop. It should
not be interpreted as a general 64³ generation advantage. The surface and
solid cases are the useful workload comparison.

## Protocol and occupancy medians

The codec benchmark uses the same benchmark shapes at each compile-time chunk
size. The sparse case contains 256 occupied samples, so its payload grows with
the occupancy bitset as chunk size increases.

| Benchmark | 16³ | 32³ | 64³ |
|---|---:|---:|---:|
| `encode_surface` | 8.95 us | 79.75 us | 564.1 us |
| `decode_surface` | 5.82 us | 49.03 us | 387.1 us |
| `encode_sparse_occupancy` | 4.59 us | 28.60 us | 201.7 us |
| `decode_sparse_occupancy` | 4.23 us | 35.07 us | 278.6 us |
| `build_occupancy` | 1.93 us | 14.35 us | 94.54 us |
| sparse payload | 144 B | 80 B | 193 B |
| surface payload | 55 B | 113 B | 453 B |

## Decision

Keep 32³ as the production chunk size. It is the only size wired through the
current GPU renderer and gives a substantially lower chunk-count overhead than
16³ without the 4–5x per-chunk CPU/codec costs of 64³. Smaller chunks could
help localized streaming and edit granularity, but would require retuning pool
capacity, slot-table pressure, light propagation, and draw submission. Larger
chunks reduce chunk count but make worst-case generation and codec latency
grow too quickly.

If a future experiment revisits 16³, it should first parameterize all GPU
strides and workgroup dimensions, then compare matched radius, resident voxel
count, mesh count, light backlog, and steady-state frame time. Changing only
the protocol/worldgen constants is insufficient evidence for a rendering
change.
