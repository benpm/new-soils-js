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
- [x] 6. **Chunk streaming v2** — palette+LZ4 chunk codec (`soils-protocol/chunk_codec.rs`,
      join burst 23 MB → 498 KB measured, 2 MB regression gate + fuzzed panic-free decode);
      server-driven subscribe/unload (`ViewRadius`/`ChunkUnload`, +1-chunk hysteresis,
      deliveries filtered against the live sub set, data+unloads share one ordered stream
      client-side; `ReqChunks` deleted); chunk refcount → 60 s zero-ref evict (save-if-dirty);
      edits mark dirty with 30 s/evict/shutdown coalesced flushes (`shutdown_and_wait` for
      tests); region compaction on world open past a 25% leak ratio. Client applies/floods/pad
      uploads became wall-time-boxed (count budgets collapsed on slow frame clocks). New
      scenarios: move-driven restream+unload, edit persistence across restart. (game-systems
      M3, §5, §6, §8)
- [x] 7. **Server authority** — `ClientMsg::Inputs` (packed frames, seq-deduped, last-3 bundled)
      replaces `Move`; the server steps players via shared `soils_sim::step_player` at the
      client dt, with a TICK_HZ token bucket so input flooding can't speed-hack (scenario-
      verified: flood moves <8 u vs 80 if trusted; legal input integrates exactly). MAX_STEP +
      `Position` snap-back deleted; self renders from interpolated ActorUpdate echoes until
      phase 11 prediction. Edits: seq + server validation (rate bucket, reach from server pos,
      block id, residency) → EditAccepted/EditRejected; client keeps optimistic apply with
      rollback via pending list. Deferred: per-chunk edit aggregation per tick. (game-systems
      M4, §6)
- [x] 8. **Entity model** — `NetId(u32)`, compile-time `entities.yaml` → `EntityRegistry`
      (soils-sim, kinds: Player/Critter), server entities are real ECS entities
      (Kind/SimState/Yaw/InWorld/PlayerControlled); actor protocol replaced by
      EntitySpawn/EntityDespawn/EntityUpdate diffed per client from chunk-column interest
      buckets at the subscription radius. Decision point resolved: hand-rolled (replicon
      would supplant the M2 transport/message stack; revisit at M6 if delta plumbing balloons).
      `ServerConfig::critters` seeds deterministic wander-AI test critters (frozen off resident
      terrain). Scenarios: spawn/kind, integrated movement, despawn on disconnect/warp,
      critter wander; selftest framed a wandering critter. (game-systems M5, §2, §7)
- [x] 9. **Server-side lighting queries** — server runs the shared soils-sim L0 flood
      (queue-on-residency, top-first, 4 ms/tick budget; edits relight inline) with per-chunk
      summaries: dark walkable-air counts at both sun extremes + ≤8 sampled cells;
      `World::darkest_walkable_near` validates samples against live voxels. Nothing per-voxel
      on the wire; light persistence skipped (derived, rebuilt on residency). Consistency
      property pinned: incremental == fresh full relight after an edit storm. Deferred: column
      heightmap summary (no consumer yet). (rendering §3; also closes the phase-2 note on
      light persistence)
- [x] 10. **Delta snapshot pipeline** — `soils-protocol/snapshot.rs`: 1/256 fixed-point pos with
      zigzag-varint deltas vs acked baselines, changed-only vel (1/256 i16, not f16 — same
      size, no dep)/yaw, varint NetId deltas + change masks, LZ4 >200 B, fuzzed panic-free
      decode; SnapshotTracker shared by client and test harness. Server: per-(client,entity)
      64-send baseline ring, ack_tick piggybacked on Inputs (ordered transport ⇒ ack covers
      all earlier sends), priority accumulator (base/dist², players 2×, reset on send) under
      410 B/tick. Bandwidth pinned: self+3 critters average <150 B/tick (scenario assert).
      Remote-body buffer interpolation deferred to phase 11 per §9 adoption order.
      (game-systems M6, §4)
- [x] 11. **Prediction & reconciliation** — client predicts via shared sim with a (seq, input,
      state) history ring; on each snapshot: rewind to server state at last_input_seq + replay
      pending inputs (anchor rebased, fly/grounded from recorded state). Remote bodies:
      per-entity snapshot buffers at a 2-tick delay + capped extrapolation on a re-synced
      render clock. Validated headless through a 75 ms-each-way proxy with 2% input loss:
      straight flight reconciles bit-exact; an unseen terrain change forces divergence and
      converges (tests/prediction.rs). Fallout fixed along the way: Snapshot gained the §4
      baseline_tick (deltas previously applied against latest state → +60% speed at RTT);
      server light floods moved off-thread onto dense cloned regions (300 ms/column stalls);
      per-chunk edit versions added; hot member crates opt-level 3 in dev. Deferred to later
      work: lag-compensated hit interactions (no combat consumers yet). (game-systems M7, §9)
- [x] 12. **Radiance-cascades GI upgrades** — GPU-side occupancy fill (`gi_blit.wgsl` blits the
      mesher's resident chunk voxel + padded-light buffers into the volumes; the 262 KB/30-frame
      CPU rebuild is gone), L0 seeding (top-cascade escapes gated by baked skylight at the
      interval end — caves deeper than the 30-voxel march stop leaking daylight; unresident
      space defaults to open sky), cascade round-robin (trace+merge paired per frame, top-down,
      so the material never samples a raw cascade 0), and per-probe ambient-cube irradiance
      projected once per cycle with trilinear 8-probe sampling in the fragment shader (replaces
      the per-fragment 16-direction loop; kills nearest-probe blockiness). All four pinned by
      headless GPU-vs-CPU oracle tests (`tests/gi_gpu.rs`). Deferred: 3D-texture + mips + DDA
      marching (perf-only — 60 fps steady on discrete with the fixed-step march, and no
      integrated GPU here to validate the win against watchdog limits) and the default-on flip
      (single-GPU + lavapipe evidence doesn't meet "where stable"; still opt-in via
      `SOILS_GI=1` / `/gi on`). (rendering §1 L2)
- [ ] 13. **Pathfinding** — walkability grid → budgeted local A* → HPA* chunk-portal graph →
      flow fields for crowds; chunk-version invalidation throughout. (game-systems §10)
- [ ] 14. **Transport upgrade** — WebTransport/QUIC datagrams (or UDP) behind the transport
      trait; snapshot channel goes truly unreliable/sequenced. (game-systems §3, M8)
