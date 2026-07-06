# Performance report

Where the time went, what was done about it, and what's left on the table.
Covers the optimization arc from the first "performance is very, very bad"
profile (2026-07-02) through the completion of all 14 `TODO.md` phases
(2026-07-04). Companion: `architecture.md` (how the system works now),
`plan-rendering.md` / `plan-game-systems.md` (the original plans), and the
per-phase measurements recorded in `TODO.md`'s checkoffs.

Reference machine: RTX 5070 / Ryzen 5 3600, Windows 11, vsync off, release
builds, static self-test viewpoint unless noted.

## Headline numbers

| Metric | Before | After |
|---|---|---|
| Frame time, radius 8 (4913 chunks) | 11.4 ms (88 fps) | 10.4 ms (96 fps) |
| Frame rate, radius 4 (729 chunks) | 231 fps | 254 fps |
| Worldgen, 48-chunk wave | 9.05 ms | 3.46 ms |
| Worldgen, all-air chunk | — | ~543× faster (early-out) |
| Fresh-world join burst, server side | 849 ms (serialized waves) | 187 ms (8 waves in flight) |
| Join burst on the wire | 23 MB | 498 KB |
| Steady-state snapshot traffic (self + 3 critters) | n/a (full actor spam) | < 150 B/tick (410 B budget) |
| Server light rebuild, one edited column | ~300 ms **on the tick thread** | off-thread, tick unaffected |
| GI occupancy upload | 262 KB CPU rebuild + upload / 30 frames | zero (GPU blit of resident buffers) |
| GI irradiance in the fragment shader | 16-direction loop per fragment | one trilinear ambient-cube fetch |

The honest summary: on a desktop GPU the *renderer* was never the main
problem — the "very bad" feel at world open was the server's worldgen burst,
tick stalls from lighting, and 23 MB joins. Rendering hygiene bought a real
but modest ~10%; the order-of-magnitude wins were server- and protocol-side.

## Methodology

- **Steady-state fps** from the F3 HUD in the headless self-test
  (`SOILS_SELFTEST=1`), only after `generating … (100%)` — worldgen-burst fps
  is meaningless. Two environmental traps cost real debugging time and are
  worth restating: a client exe run directly (not via `cargo run`) fails to
  resolve `assets/` and renders empty sky *while the self-test still passes*;
  and a window presenting on a USB display caps at ~10 Hz regardless of load.
- **Worldgen** via criterion benches (`soils-worldgen/benches/terrain.rs`).
- **Wire sizes** asserted in scenario tests (`tests/streaming.rs` gates the
  join burst at < 2 MB; `tests/scenarios.rs` gates snapshots at < 150 B/tick),
  so the wins are regression-locked, not one-off measurements.
- **Correctness gates for every optimization**: CPU oracles for the GPU
  mesher, GI trace/blit/irradiance, and the light flood; golden-bytes +
  fuzzed panic-free decode for the codecs; behavior-pinning network scenarios.

## The steps

### 1. Renderer hygiene (plan-rendering §2 — TODO phase 3)

The starting state drew a shared 49,152-vertex dummy mesh for *every* chunk
every frame with frustum culling disabled (`NoFrustumCulling`) and
`cull_mode: None` — ~241 M vertex invocations/frame at radius 8 — and the
mesher's quad counter overflowed into out-of-bounds reads past 8192 quads.

- **Frustum culling**: exact per-chunk AABBs (with `NoAutoAabb` so Bevy's
  auto-calculator doesn't replace them with the dummy mesh's degenerate one).
- **Indirect draws**: the mesher's compute pass writes `quads×6` into an
  indirect-args buffer; a patched per-material draw function issues
  `draw_indirect`, deleting the dummy-vertex tax entirely. Chunk origins moved
  into the material uniform (indirect draws can't carry Bevy's per-instance
  mesh-uniform index).
- **Backface culling**: case analysis showed both meshers already emit CCW
  quads; one line (`cull_mode: Back`) halved fragment work.
- **Overflow clamp**: vertex shader and finalize pass clamp to `MAX_QUADS`,
  fixing the OOB-garbage rendering.
- **Mesher occupancy**: `mesh_slice` went from `@workgroup_size(1)` (99
  serial threads) to 32 lanes cooperating on mask fill + AO with the serial
  greedy merge on lane 0.

Measured: 11.4 → 10.4 ms at radius 8; 231 → 254 fps at radius 4. The
surprise finding: the 241 M degenerate vertices cost only ~1 ms on this class
of GPU (collapsed triangles are rejected pre-raster almost free) — the plan
overestimated that tax for desktop hardware, though it would matter on iGPUs.
After hygiene the frame is **draw-submission bound** (~5k draw calls, one
material bind group each) plus Bevy's atmosphere — see "what's left".

### 2. Worldgen (TODO phase 4)

Criterion first, then: cave noise evaluated on a 9³ lattice per chunk and
trilinearly interpolated (instead of per-voxel 3D noise), all-air and
rock-top early-outs, the palette lookup hoisted per batch. A 48-chunk wave
dropped 9.05 → 3.46 ms and air chunks became ~543× cheaper; a fresh world's
810-chunk burst totals ≈ 65 ms of generation. This work also *restored caves
lost in the JS port* (threshold retuned against the Rust noise crate's range,
pinned by a density-band test) — optimization and correctness in one pass.

### 3. Server architecture (TODO phases 5–6, 9, 11)

- The mutex-web server became a headless Bevy ECS app at a 20 Hz fixed tick;
  connections are pure inbox/outbox pumps.
- Chunk generation runs on rayon *off* the tick, in nearest-first waves, up
  to 8 in flight per client: the serialized 849 ms join burst fell to 187 ms.
- Chunk persistence moved to a background writer (saves never block a tick);
  region files self-compact on world open past a 25% leak ratio.
- The killer stall: the trait-based light flood took ~300 ms for one edited
  column — at a 50 ms tick budget the server dropped to ~4 Hz. Floods now
  run on rayon against dense cloned regions (plain array indexing instead of
  millions of HashMap-mediated voxel reads) with per-chunk version guards on
  write-back. A property test pins incremental == full relight.

### 4. Protocol & replication (TODO phases 6–7, 10)

- **Chunk codec**: palette + bit-pack + LZ4 with uniform/paletted/raw-dense
  tiers. Join burst: 23 MB → 498 KB (fuzzed panic-free, golden-bytes tested,
  2 MB regression gate).
- **Server-driven subscriptions** with hysteresis replaced client chunk
  requests; refcounted residency + 60 s zero-ref eviction bound server memory.
- **Delta snapshots**: 1/256 fixed-point quantization, zigzag-varint deltas
  against *acked* baselines (per-client/entity 64-send rings), change masks,
  LZ4 over 200 B, and a priority accumulator (base/dist², players 2×) under a
  410 B/tick budget. Steady state measures < 150 B/tick for self + 3 moving
  critters. One regression mattered enough to record: deltas MUST decode
  against the tick the server encoded against (`baseline_tick` on the wire) —
  applying them to latest state double-counted motion by +60% at high RTT.

### 5. Client streaming & prediction (TODO phases 6, 11)

- All burst work (chunk applies, light floods, padded-light uploads) is
  wall-time-boxed per frame instead of count-budgeted — count budgets
  collapse when the frame clock is slow, which is exactly when bursts hurt.
- Prediction reuses the shared sim with a (seq, input, state) history ring;
  reconciliation rewinds to the server state at `last_input_seq` and replays.
  Validated headless through a 75 ms-each-way lossy proxy: straight flight
  reconciles bit-exact; a forced misprediction (server-side terrain change
  the predictor can't see) diverges > 0.5 u and converges after correction.

### 6. Radiance-cascades GI (TODO phase 12)

- **GPU occupancy fill**: the 262 KB CPU volume rebuild + re-upload every 30
  frames became a compute blit of the mesher's already-resident chunk voxel
  buffers (plus the padded L0 light buffers). CPU cost: zero. Byte-exact
  oracle test.
- **Round-robin with paired merges**: instead of tracing all 4 cascades in
  one spiky frame, each frame handles one cascade top-down — and its merge
  runs in the *same* frame, because merging overwrites in place and the
  material must never sample a raw cascade 0 (the naive split left it raw 4
  of 6 frames and killed the bounce).
- **L0 skylight seeding**: escaped top-cascade rays are gated by the baked
  skylight at the interval end, so enclosures deeper than the 30-voxel march
  stop leaking daylight. The flood already knew the answer; the tracer now
  asks it.
- **Per-probe ambient cubes**: the per-fragment 16-direction cosine loop
  (potentially millions of integrations per frame) became one 24,576-thread
  projection per GI cycle; fragments do a trilinear 8-probe ambient-cube
  fetch. Also a quality win: trilinear interpolation removed the hard
  probe-cell blockiness.

### 7. Transport (TODO phase 14)

- **Latest-wins snapshot lane**: on a backed-up link, unsent snapshots are
  replaced, never queued — a slow client stops accumulating a backlog of
  stale entity state. Correct by construction: deltas encode against acked
  baselines, and an undelivered snapshot is simply never acked.
- **WebTransport/QUIC**: snapshots and inputs ride real datagrams (no TCP
  head-of-line blocking); chunks/edits/control keep a reliable ordered
  stream. Same 62 fps steady state and pixel-identical self-test over
  `SOILS_WT=1`.

## Current steady state

| Scenario | Result |
|---|---|
| Radius 4 fresh world, vsync on | 60–62 fps through the whole join burst |
| Radius 4, vsync off | ~254 fps |
| Radius 8, vsync off | ~96 fps (draw-submission + atmosphere bound) |
| Fresh 729-chunk world, click-to-playable | ~3 s (gate: < 3 s in `tests/streaming.rs`) |
| Server tick | 20 Hz held during joins, edit storms, and light rebuilds |

Debug vs release client barely differs (83 vs 88 fps at radius 8 baseline):
the client is GPU/submission-bound, not CPU-bound. Hot workspace members
(`soils-sim/protocol/worldgen/server`) build at `opt-level 3` even in dev so
the embedded server and tests behave.

## What's left on the table

Ranked by expected payoff on the reference hardware:

1. **Pooled quad memory + merged draws** — the frame's dominant cost is ~5k
   per-chunk draw calls with one material bind group each. Suballocating all
   chunk quads from one pool and drawing with one bind group (multi-draw or
   a single indirect batch) attacks the actual bottleneck. This was the
   explicit "next lever" noted when phase 3 landed.
2. **Atmosphere cost** — Bevy's physically-based sky is the other steady
   line item. Options: lower LUT resolutions, or compute-once-per-daytime
   instead of per-frame.
3. **GI 3D-texture + DDA marching** (plan §1 L2 item 3, deferred) — the
   fixed-step 0.5-voxel march through a storage buffer is fine on desktop
   (60 fps with headroom) but is the piece to optimize for iGPUs: hardware
   filtering, mip-based coarse stepping, and exact DDA replace ~1M buffer
   taps per trace cycle. Pair with *shortening the cascade intervals* now
   that L0 seeding supplies far-field sky occlusion.
4. **GI default-on** — blocked on stability evidence across drivers, not
   performance (see `TODO.md` phase 12 deferral note).
5. **Snapshot MTU packing** — the snapshot budget (410 B) is conservative
   against the ~1200 B QUIC datagram floor; entity coverage per tick could
   roughly double before fragmentation risk.
6. **Async pathfinding pool** — repaths are synchronous but budgeted and
   staggered; fine at current critter counts. Move `find_path`/`hpa_path`
   onto rayon (the nav data is already version-keyed) before shipping
   hundreds of agents, and hand crowds to the already-implemented flow
   fields (one field per goal, per-agent cost = one hash lookup).
7. **Padded-light upload batching** — each light change re-uploads a ~43 KB
   padded volume per touched chunk (time-boxed, deduped). A GPU-side pad
   rebuild from neighbor buffers (same trick as the GI blit) would remove
   the CPU rebuild entirely.
8. **`bevy_replicon` checkpoint** — the hand-rolled replication pipeline is
   done and tested, but if entity kinds multiply, re-evaluate replicon with
   a custom transport backend (decision point noted in plan §3; the
   quantization layer transfers either way).
