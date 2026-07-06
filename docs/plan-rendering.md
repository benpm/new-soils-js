# Plan: rendering & lighting — baked + dynamic split, cheap server-side lighting

> **Status (2026-07-04): implemented.** L0 baked light, renderer hygiene, server-side
> lighting queries, and RC GI upgrades 1/2/4/5 all shipped (TODO phases 2–3, 9, 12);
> RC items 3 (3D-texture + DDA) and default-on are deferred with rationale in
> `TODO.md`. Current state: `architecture.md`; measurements: `perf-report.md`.
> The "current state" described below is the *pre-plan* snapshot, kept for context.

Separate note from `plan-game-systems.md`. Current state (details in `analysis.md`): terrain is
unlit with a constant 45 klux brightness, day/night is exposure-only, shadows are off, AO is
meshed-in, and the *only* real light transport is the opt-in radiance-cascades (RC) GI — a 64³
window around the player, CPU-refilled, traced every 6th frame. Caves without GI are noon-bright.

## 1. Lighting architecture: three layers instead of one

The fix is not "more GI"; it is splitting lighting by rate-of-change so each layer uses the
cheapest adequate technique:

| Layer | Technique | Updates | Owner |
|---|---|---|---|
| L0 baked | per-voxel **light grid**: skylight u4 + blocklight u4 | on chunk gen/edit only (incremental flood) | shared (`soils-sim`), CPU |
| L1 global dynamic | sun direction/intensity, sky color, exposure | per frame from `daytime` | client |
| L2 local dynamic | radiance cascades (kept, demoted to enhancement) | every Nth frame, near camera | client GPU |

### L0 — the baked light grid (the workhorse)

Minecraft-proven and exactly matches the voxel data model:

- **Skylight**: per column, full light from sky down to first occluder, then BFS attenuation
  (−1 per step) sideways/down into overhangs and caves. **Blocklight**: BFS flood from emissive
  blocks (emission already lives in `blocks.yaml`/`BlockDef`), −1 per step, radius ≤ 15.
- Storage: one `u8` per voxel (hi nibble sky, lo nibble block) alongside `ChunkVolume` — 32 KB
  per chunk; persisted in region files (new section per chunk, versioned) so relight isn't paid
  on every load.
- **Incremental** updates on edit: standard two-queue add/remove flood (removal BFS collecting
  darkened cells, then re-propagation from bright borders). O(affected cells), microseconds for a
  single block. Cross-chunk propagation uses the chunk cache; a chunk's light is only "settled"
  once its neighbors exist — same dependency discipline the mesher will need for seam AO anyway.
- Implementation home: `soils-sim` (pure functions + tests), because **both client and server run
  the identical flood** (see §3). Keep the oracle pattern: a naive full-relight reference, with
  property tests asserting incremental == full after random edit sequences.
- Client consumption: the mesher already reads the voxel buffer; upload light as a second buffer
  (or widen voxels to u16 = id + light) and sample per-vertex in `voxel_mesh.wgsl`, per-fragment
  smoothed like AO. Shading becomes:

  ```
  color × ( skylight/15 × sun_term(daytime)          // L1 drives the sun term
          + blocklight/15 × warm_emissive_curve
          + RC_GI_term (if enabled) )                 // L2
  ```

  Caves go dark, torches work, and the flat `TERRAIN_BRIGHTNESS` constant becomes the L1 sun
  term instead of a global fudge — **with GI off**, which is the default today.

### L1 — global dynamic

Keep what exists (atmosphere sky, sun swing, EV100 interpolation) — it's cheap and looks right.
Add: sun term multiplies *skylight* rather than all terrain (night actually darkens caves-vs-
surface correctly), and optionally a single cascaded shadow map for entities only (terrain
self-shadowing is already approximated by skylight + AO; blocky CSM on terrain buys little for
its cost — revisit only if visuals demand it).

### L2 — radiance cascades, demoted and optimized

RC stays as the "wow" layer: colored bounce, emissive bleed, sky-directional ambience. Because
L0 now guarantees plausible base lighting everywhere, RC can be tuned purely for quality near the
camera and remain optional on weak GPUs. Targeted optimizations, in payoff order:

1. **Kill the CPU refill** (`gi.rs:fill_volume`, 262 KB upload + 64³ CPU loop every 30 frames):
   chunk voxels are already GPU-resident per chunk — a small compute pass blits overlapping chunk
   buffers into the occupancy volume on recenter/edit. CPU cost → zero; enables a larger window.
2. Seed cascade tracing with the L0 grid (skylight as the sky-visibility term) so intervals can
   shorten — fewer march steps for the same look, or larger volume for the same cost.
3. Occupancy as a **3D texture + mips** and DDA (or coarse-fine stepping through mips) instead of
   fixed 0.5-voxel stepping in a storage buffer — fewer, cheaper samples per ray.
4. Precompute per-probe **irradiance** (9-coeff SH or 6-face ambient cube) in the merge pass, so
   the fragment shader does one interpolated fetch instead of the current 16-direction loop
   (`atlas.wgsl:gi_irradiance`), and trilinear-blend probes instead of nearest.
5. Temporal amortization is already there (6-frame throttle); add cascade-level round-robin
   (trace one cascade per frame) to flatten the spike.

Validation stays oracle-based: extend `radiance.rs` reference + `gi_gpu.rs` comparisons to every
change; L0 gets its own CPU oracle; screenshot self-tests gain a "cave at noon is dark, torch lit"
scene like the existing `gi_demo`.

## 2. Renderer hygiene (independent of lighting)

- **Indirect draws**: the mesher already writes `count`; write `count*6` into an indirect-args
  buffer and use `draw_indirect` instead of drawing 49,152 dummy vertices per chunk per frame.
  Removes the vertex-shader tax on empty space and the dummy mesh entirely.
- **Culling**: replace blanket `NoFrustumCulling` with a correct per-chunk AABB (chunk cube) so
  Bevy frustum-culls normally. Later: coarse occlusion (skip chunks fully enclosed — derivable
  from the L0 skylight/openness data or a chunk-face-connectivity solve à la Minecraft's
  visibility graph).
- **Quad overflow**: clamp `count` to `MAX_QUADS` in the vertex shader (current out-of-bounds
  reads render garbage past 8192 quads), log overflow, and re-mesh that chunk on the CPU fallback
  path (oracle mesher) with a real allocation. Long-term: pooled/suballocated quad memory sized
  by measured quad histograms instead of 655 KB × N fixed buffers.
- **Mesher occupancy**: `(3,33,1)` dispatches of workgroup size 1 = 99 threads. Restructure so a
  workgroup handles a slice with 32 lanes cooperating (or at minimum `@workgroup_size(32)` over
  rows with a serial merge step). Keep the CPU oracle equality test as the gate.
- **Winding**: emit correct per-face winding in the mesher (the data is there: normal sign) and
  restore backface culling — halves fragment work on dense scenes; removes the double-sided hack.
- Fold `read_voxel`/`voxel_at` duplication and `ease10` duplication when touching these files.

## 3. Server-side lighting (macroscopic, non-renderable, cheap by design)

Requirement restated: the server should understand lighting **for gameplay** — where it is dark
(spawning), how bright a cell is (growth, mob burn), world-scale changes (time of day, weather) —
while spending near-zero CPU and **no** GPU, and communicating only compact, slow-changing data.

Design:

- The server runs exactly the **L0 flood** from `soils-sim` — the same code the client runs. It is
  integer BFS on data the server already owns, incremental per edit; cost is O(edited cells), and
  chunks light themselves once at generation (amortized into the existing gen step).
- **Nothing per-voxel is replicated.** Light is *derived* data: clients recompute the identical
  grid from replicated voxels + the shared algorithm. The only lighting on the wire is what's
  already there (`daytime`) plus future world-scale inputs (weather/season factors, global events
  like an eclipse) — a few bytes, seconds-cadence, on the control channel. This is the
  trade: the server's lighting is *coarse truth* (macroscopic, time-insensitive), not renderable
  radiance; clients spend their own budget making it pretty (L1/L2).
- **Gameplay queries** get per-chunk summaries maintained alongside the grid (updated in the same
  incremental pass, so always coherent):
  - `dark_cells: u16` count of walkable-air cells with effective light < spawn threshold
    (effective = max(blocklight, skylight × current sun term) — evaluated with the *server's*
    daytime at query time so night opens the surface for spawns naturally);
  - a small reservoir sample (e.g. up to 8) of dark walkable cell indices per chunk, so a spawner
    picks "a darkest spot near player P" by scanning chunk summaries in the interest ring and
    sampling — O(chunks), no voxel scans at spawn time;
  - `max_skylight_column: [u8; 32×32]`-derived heightmap (already implicit in skylight) reusable
    by weather (snow accumulation), AI ("is outdoors"), and worldgen decoration.
- Spawning pipeline sketch: candidate chunks = interest ring ∩ `dark_cells > 0` → weighted pick →
  validate the sampled cell (headroom via walkability grid from the pathfinding plan §10.1 — same
  derived-data-with-chunk-version pattern) → spawn entity → it replicates like anything else.
- If profiling ever shows full L0 too hot server-side (unlikely; it's Minecraft-server-class
  work), the documented fallback is skylight-only + emissive-count summaries — the API above
  doesn't change, only fidelity. Never RC, never GPU, on the server.

Consistency guarantee: because client and server share one flood implementation and one input
(voxels), divergence is impossible modulo un-replicated edits in flight — and those converge with
the edit protocol (`plan-game-systems.md` §6). Property test in CI: server grid == fresh client
grid after a randomized edit storm over the loopback harness.

## 4. Sequencing

1. L0 grid in `soils-sim` + oracle tests; client meshes/shades with it (biggest visual win, no
   protocol change).
2. Renderer hygiene batch (indirect draw, culling, overflow clamp) — independent, do opportunistically.
3. Server adopts L0 + chunk summaries; expose spawn-query API (pairs with entity milestone M5).
4. RC optimizations 1–2 (GPU refill, L0 seeding); flip GI default-on where stable.
5. RC optimizations 3–5 as polish; entity shadow map if/when entities matter visually.
