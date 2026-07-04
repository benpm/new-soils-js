# Todo

Linear implementation sequence for the plans in `docs/` (`analysis.md`, `plan-game-systems.md`,
`plan-rendering.md`). Each phase is intended to be shippable and test-gated before the next.

- [x] 1. **Extract `soils-sim`** — shared movement/collision/raycast/edit-rule functions; client
      physics moves to `FixedUpdate` on it; split the `net_receive` god-system into per-message
      event systems. (game-systems M1)
- [x] 2. **Baked light grid (L0)** — skylight+blocklight nibble grid in `soils-sim` with
      full-relight oracle + incremental flood, baked only on world modification; client shades
      with it so caves darken with GI off. (rendering §1, §4.1; region-file persistence of light
      deferred to phase 9 when the server adopts the grid)
- [x] 3. **Renderer hygiene** — indirect draws from the GPU quad count, per-chunk AABB frustum
      culling, quad-overflow clamp, mesher workgroup occupancy, backface culling (winding was
      already consistent), all gated by a new GPU-vs-CPU mesher equality test
      (`tests/mesher_gpu.rs`). Overflow logging/CPU-fallback remesh deferred with the pooled
      quad-memory idea. Measured (RTX 5070, radius 8, vsync off, release): 11.4 → 10.4 ms;
      the frame is now bounded by per-chunk draw submission (~5k bind groups) + atmosphere,
      not terrain geometry — pooled quad memory / merged draws is the next lever. (rendering §2)
- [x] 4. **Worldgen performance** — criterion benches (`soils-worldgen/benches/terrain.rs`),
      then: cave noise on a 9³ lattice with trilinear interpolation, all-air/rock-top early
      outs, palette hoisted per batch. Wave of 48 chunks 9.05 → 3.46 ms (release); air chunks
      ~543× faster. Restored caves lost in the JS port (threshold 0.7 → 0.55 vs noise-crate
      simplex range, pinned by a density-band test). Server now generates outside the world
      lock (concurrent edits/loads during waves) and logs wave timings; fresh-world burst of
      810 chunks ≈ 65 ms total gen time, verified via selftest screenshot.
- [x] 5. **Server as headless Bevy ECS app** — 20 Hz fixed tick (`SERVER_TICK_HZ` in `soils-sim`),
      connection tasks are pure inbox/outbox pumps, mutex web → ECS resources (`app.rs`).
      Chunk pipeline: waves probe cache/disk on the tick, generate on rayon off it, ≤8 waves
      in flight per client, delivery in request order. Fresh 729-chunk burst: 187 ms
      (tick-quantized; old per-connection loop 85 ms — invisible behind client apply pacing,
      path redone in phase 6). Gated by tests/{scenarios,streaming,embedded,discovery}.rs +
      examples/msgcount.rs; client A/B old-vs-new server: identical selftest results.
      (game-systems M2)
- [ ] 6. **Chunk streaming v2** — server-driven subscribe/unload with hysteresis, palette+LZ4
      encoding (~32 KB → 1–4 KB), server chunk refcount/evict, coalesced dirty-chunk saves +
      region compaction. (game-systems M3, §5) NOTE: the client's chunk-apply budget is
      count-based (8/frame, `CHUNK_APPLY_BUDGET`) and collapses when burst frames run long
      (~60 chunks/s observed at ~8 fps → fresh-world fill takes >10 s regardless of server);
      make it time-budgeted (or cheapen per-apply cost) as part of this phase.
- [ ] 7. **Server authority** — `InputMsg` replaces `Move`, server simulates players via
      `soils-sim`; edits validated server-side with seq/ack/rollback (fixes the unloaded-chunk
      edit desync). (game-systems M4, §6)
- [ ] 8. **Entity model** — `NetId`, `entities.yaml` registry, spawn/despawn replication,
      interest management via chunk-column buckets; decision point: hand-rolled vs
      `bevy_replicon`. (game-systems M5, §2, §7)
- [ ] 9. **Server-side lighting queries** — server runs the shared L0 flood + per-chunk darkness
      summaries (dark-cell counts, reservoir samples, column heightmap); spawn-query API for
      "darkest walkable spot near player". Nothing per-voxel on the wire. (rendering §3)
- [ ] 10. **Delta snapshot pipeline** — per-client quantized baselines in a 64-tick ring,
      zigzag-varint/bit-packed deltas, priority accumulator under a byte budget, LZ4 over
      threshold, acks piggybacked on inputs. (game-systems M6, §4)
- [ ] 11. **Prediction & reconciliation** — own-entity rewind/replay, remote snapshot-buffer
      interpolation + capped extrapolation, optimistic-edit rollback, lag-compensated
      interactions; validated on a simulated 150 ms / 2 % loss link. (game-systems M7, §9)
- [ ] 12. **Radiance-cascades GI upgrades** — GPU-side occupancy fill (kill the CPU refill), seed
      from L0, 3D-texture + DDA marching, per-probe SH/ambient-cube irradiance; flip default-on
      where stable. (rendering §1 L2)
- [ ] 13. **Pathfinding** — walkability grid → budgeted local A* → HPA* chunk-portal graph →
      flow fields for crowds; chunk-version invalidation throughout. (game-systems §10)
- [ ] 14. **Transport upgrade** — WebTransport/QUIC datagrams (or UDP) behind the transport
      trait; snapshot channel goes truly unreliable/sequenced. (game-systems §3, M8)
