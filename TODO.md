# Tasks

> **Open work lives in [`Tasks.md`](Tasks.md).** This file is the historical
> record: what shipped in each phase, what was measured, and what was deferred
> with the reasoning. Checkoffs here are not re-litigated; they are evidence.
>
> `soils-terrainlab` keeps its own:
> [`crates/soils-terrainlab/TODO.md`](crates/soils-terrainlab/TODO.md).

<!-- As you complete tasks, add descriptions of your implementation to CHANGELOG.md, and then reference them here, removing their descriptions from here. -->
<!-- For each of these, do them one at a time, but first looking for dependencies. If there are dependencies, reorder the TODO items to be in the order they need to be implemented in due to dependency (but keeping the sections). When you complete a task, mark it off. Add the current date, commit hash, and branch to the end of each. Then, commit and push. Before working, check if the task is already complete. Also, make sure to understand perfectly what the user actually wants by asking questions. --> 

## UI

### Inventory

Design intent (the source of truth for the plan below):

User interface is not like Minecraft's very much.
- No hotbar, just a ring indicating all tools and weapons and consumables in your inventory.
- You can use various hotkeys to navigate the UI or pressing Alt to release the mouse cursor for UI interaction.
- Inventory screen comes up by pressing E, I, Tab, or Escape.
- A backpack icon with a cirled "E" on it is shown in the left corner of screen, indicating how to open inventory.

**Revised 2026-08-28: there is a hotbar, and it is not Minecraft's.** The first
bullet no longer holds. Working from the mockup in `scratch/`, the ring was
replaced by an eight-key bar that holds **references** to item kinds rather than
items: assigning costs no message and moves nothing, the item stays listed in
the inventory (dimmed, badged with its key), and when the item runs out the key
rebinds itself to another of the same category, function and effect — a spent
Cobblestone becomes Moss Stone, an eaten fruit becomes another healing
consumable. With no like item to hand the key goes empty and wiggles when
pressed. That is the part Minecraft does not do, and it is why a bar is worth
having here: it is a set of standing intentions, not eight more slots to
micromanage. The ring and the one-slot held-item indicator are gone; the
inventory screen groups by category instead of showing raw slots.

Phasing, rationale and per-phase test gates: [plan-ui.md](docs/plan-ui.md).

**Shipped 2026-08-25 (`ui-inventory`).** The loop works: breaking a block drops
it as a world entity, walking into the drop collects it, placing spends it, and
the server owns the inventory (protocol v4). What is left is listed under
[plan-ui.md §9](docs/plan-ui.md#9-what-is-left) — persistence across logout, the
radial *shape* of the ring, and a TTL for uncollected drops.

- [x] **Decide the Escape binding.** The intent above lists Escape as an
      inventory key, but Escape is currently the cursor release and therefore
      the pause menu; both cannot hold. Recommendation and the alternative:
      [plan-ui.md §6](docs/plan-ui.md#6-open-decisions). Blocks phase 0.
- [x] **Phase 0 — `UiMode` state.** The client has no UI state machine: the
      pause menu is shown whenever the cursor is released (`pause.rs:211`), and
      any click re-grabs it (`player.rs:313`), so an inventory screen cannot be
      clicked without dismissing itself. Replace the inferred state with a real
      one, give cursor grab a single owner, and add Alt as a modifier over it.
      Ships alone with no new UI. Gate: transitions both ways, plus a test that
      a click in `Inventory` does not re-grab.
- [x] **Phase 1 — item model in `soils-sim`.** `ItemKind` / `ItemStack` /
      `Inventory` as plain data, so the server can reuse it unchanged when
      inventory becomes authoritative. Gate: stacking and count-conservation
      tests, including insert-into-full returning the remainder.
- [x] **Phase 2 — item icons.** `blocks.png` is an 8x8 grid of 16x16 tiles and
      `BlockDef.faces[1]` is the top face, so block icons need no new art.
      Placeholder tiles for tools/weapons/consumables. Gate: every block in
      `blocks.yaml` resolves to an in-range tile.
- [x] **Phase 3 — the ring** (partial). `Hotbar` retired; the strip shows the
      carried tools/weapons/consumables. Radial layout and hold-to-open
      selection deferred until tools exist — tracked in `Tasks.md`.
- [x] **Phase 4 — inventory screen.** E/I/Tab into `UiMode::Inventory`, slot
      grid, click-to-pick/click-to-place, and the backpack affordance with the
      circled "E". Gate: open/close per binding; moving a stack conserves it;
      closing with a stack in hand returns it rather than voiding it.
- [x] **Phase 5 — authority** (persistence deferred). Protocol v4, server-owned
      inventory, client mirrors only. Persistence across logout is tracked in
      `Tasks.md`.

## BigRefactor

Focus on this refactor unles instructed otherwise. Commit for each, make sure to pull as well.

Keep the notes updated with proper documentation.

Linear implementation sequence for the plans in `docs/` (`analysis.md`, `plan-game-systems.md`,
`plan-rendering.md`). Each phase is intended to be shippable and test-gated before the next.

**Status: all 14 phases complete (2026-07-04).** Each checkoff below records what shipped,
what was measured, and what was deferred with rationale. Current-state documentation lives in
`docs/architecture.md`; the performance narrative and the ranked next-optimizations list in
`docs/perf-report.md`.

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
- [x] 13. **Pathfinding** — `soils-sim/nav.rs`, pure over `VoxelSampler` with oracle tests:
      per-chunk `WalkGrid` bit-set (borders sample vertical neighbors; unloaded = unwalkable);
      budgeted A* (lateral / 1-up jump with takeoff clearance / drops ≤ 3, Path|NoPath|Budget)
      plus `resolve_walkable` endpoint snapping (bodies hover and stand on block edges);
      HPA*: step-connected regions per chunk (drops deliberately absent — under-connects only)
      → border-sweep portal edges → abstract A* refined leg-by-leg by the flat search; flow
      fields (reverse Dijkstra, one-way drops point the right way, shared per goal). Server:
      nav cache keyed by (own, below, above) edit-version triple (neighbor bumps would break
      the light write-back guard), pruned on eviction; critters seek players via A* with an
      HPA* fallback on Budget, waypoints validated against live voxels each tick (scenario:
      converges to the player's column in ~2 s). Deferred: async task-pool pathing (synchronous
      budgeted repaths are staggered and fine at current critter counts) and a flow-field
      consumer (the named one is the mob spawner, which doesn't exist yet;
      `darkest_walkable_near` and `flow_field` are both ready for it). (game-systems §10)
- [x] 14. **Transport upgrade** — two-lane semantics first: snapshots moved to a latest-wins
      lane (backed-up links replace unsent snapshots, never queue them — correct because
      deltas encode against *acked* baselines). Then a WebTransport/QUIC endpoint (wtransport)
      on UDP at the game port, producing the same app-side connection shape as a websocket:
      reliable ordered lane = one client-opened bi stream of length-framed bincode (login,
      chunks, edits, control; decode-bomb frame caps both sides), unreliable lane = real QUIC
      datagrams for snapshots (server→client) and inputs (client→server; loss-tolerant since
      every `Inputs` bundles the last 3 frames). Client picks the transport by URL scheme
      (`wt://` vs `ws://`, bare addresses via `SOILS_WT=1`); TLS is a per-boot self-signed
      identity with client verification skipped (LAN-play trust, same as `ws://`; the
      cert-hash pinning path exists for a future wasm client). Gated by a WT scenario driving
      login → datagram inputs → datagram snapshots → server-integrated movement, the whole
      WS suite unchanged, and a full fresh-world selftest over `SOILS_WT=1` (729 chunks, 62
      fps steady, pixel-consistent with the WS reference shots). Deferred: WS remains the
      default while WT soaks; no formal transport trait (the two-lane `NewConn` shape *is*
      the seam — a third backend implements the same two channels). (game-systems §3, M8)


## Networked physics (Avian) — behind `SOILS_PHYSICS`
- [x] `soils-physics` crate wrapping Avian 0.6 (pinned to bevy 0.18); drop-and-settle + voxel-collider-alignment unit tests.
- [x] Snapshot codec: optional quantized orientation quaternion (`MASK_ROT`), free for yaw-only entities; round-trip test.
- [x] Server authoritative Avian world; `KIND_PHYSICS_CUBE` props replicated via the existing interest/snapshot pipeline; real per-chunk `Collider::voxels` terrain around live bodies (rebuilt on edit `version`). Tests: fall+rotation, two-client rest convergence.
- [x] Client local Avian world: predicts props, rebases to server snapshots past an epsilon; client terrain colliders from `ChunkMap`. Rendered from the predicted transform. (In-game validated.)
- [x] Kinematic player proxy (server + client) so props are shoved by the player, movement feel unchanged.
- [x] `spawn`/`cube` console command → `ClientMsg::SpawnCube` (reach-checked, rate-limited); test.
- [x] Angular velocity replicated (`MASK_ANGVEL` / `BodyAngVel`), so the client
      predicts a prop's spin between snapshots. Remaining follow-ups (two-way
      player via an Avian character controller, sub-tick interpolation) are in
      `Tasks.md`.

## SpacetimeDB (`stdb/soils-module`, `soils-stdb`) — behind `SOILS_STDB_URI`

Hybrid by design: SpacetimeDB owns cold/relational/persistent/social state,
`soils-server` stays authoritative for movement, chunks, entities and physics.
Mirroring is strictly opt-in — unset `SOILS_STDB_URI` and the server is exactly
as it was, region files only. Setup and schema notes in
[`stdb/README.md`](stdb/README.md).

### Done

- [x] Module: 9 tables + reducers, every world-mutating one gated on a
      registered server identity (TOFU `grant_server` bootstrap).
- [x] `chunk_key` shared with the server so the two cannot drift.
- [x] `soils-stdb`: worker thread + channels shaped like the existing `NewConn`
      transport seam. Bindings checked in, so a normal build needs no CLI.
- [x] Mirrored end-to-end and live-verified: world registration, edited-chunk
      blobs (only after a successful disk write), server registry heartbeat,
      logout profile save, login/logout presence.
- [x] Presence stays alive while a player is online (`heartbeat_presence`
      refreshes the roster in one transaction per world and prunes leavers).
- [x] `link_identity` is server-only — it was player-callable with a check that
      was vacuous for an unlinked account.
- [x] Read path: `player_profile` subscription + synchronous cache lookup;
      startup waits (bounded) for the first snapshot so a login cannot race it.
- [x] A returning player resumes where they logged out.
- [x] `chunk_blob.version` is the chunk's edit counter, not a wall clock, and
      the stale-write guard compares versions only within one `writer_epoch`.
      The counter lives in memory and restarts at 0 when a chunk is evicted and
      reloaded, so comparing across server processes silently rejected every
      edit to a reloaded chunk — permanently, since the region file is
      authoritative and nothing retries.
- [x] Edit journal removed: built on both sides, unreachable, and its
      `edits_through` column was hardcoded to 0 in every row it wrote.
- [x] Accounts live in SpacetimeDB, hashed with Argon2id and per-account salts,
      replacing the `DefaultHasher`-and-fixed-salt scheme. Offline play keeps
      the local file; existing accounts migrate on their next login.
- [x] `account` is a **private** table and verification happens inside the
      module (`verify_login`/`register_account`/`set_password`). It was public,
      which in SpacetimeDB means every connected client could subscribe and
      read every Argon2 verifier; limiting our own client's subscription list
      was politeness, not a boundary. Row-level security would be the natural
      tool and is not usable in 2.7.1 — `client_visibility_filter` is behind
      the `unstable` feature and documented in its own source as not enforced.
- [x] Logins run on a worker thread. Argon2id inline on the 64 Hz tick froze
      every player for 1.95 s under a 40-login flood — measured, and now
      guarded by `logins_do_not_stall_the_tick`, which watches the longest gap
      between ticks rather than total elapsed time.
- [x] The one-off `chunk_blob` restore subscription is released when it is
      done. Dropping the handle does not unsubscribe, so it stayed live for the
      process lifetime and held the whole stored world in memory — exactly what
      taking a one-off subscription was meant to avoid.
- [x] Client-side layer: `soils-client` depends on `soils-stdb`, with an
      optional non-blocking connection, a server browser merging the registry
      with LAN discovery, chat (`/say` + HUD), and identity linking through a
      new `ClientMsg::LinkIdentity` (protocol 3).
- [x] `World.daytime` refreshed on the heartbeat instead of sitting at 0.0.
- [x] The link reconnects with capped backoff after a database restart.

- [x] A server with an empty region directory rehydrates from the database
      (`World::restore_from_stdb`), which is what makes the mirror worth its
      cost for a fresh deployment rather than a write-only backup.
- [x] Chat reads `<ben>`, not `<a1b2c3>`: the speaker's account name is
      denormalised into `chat_message.sender_name` at write time, because
      clients cannot resolve an identity to a name themselves.

### Remaining

Moved to [`Tasks.md`](Tasks.md) — the known limits of the hybrid split
(world-readable `player_profile`/`chunk_blob`, `send_chat` world trust, the
new-world join stall, database-down logins, `grant_server` trust-on-first-use)
and the cross-cutting notes that shape work elsewhere on the list.

## The First Content Expansion

Moved to [`Tasks.md`](Tasks.md): Minecraft-parity block types, biomes, and
neural tile/structure generation.

---
*After `ui-inventory` is merged and all tasks above are put into the changelog and removed from this file...*

## Light Upgrade 2.0

**Planned, not started.** The detailed plan is written:
[plan-better-lighting.md](docs/plan-better-lighting.md). Four phases, each
test-gated, with the payoff concentrated in the first.

Two findings from writing it are worth having here, because they change what
the section is asking for:

- Three of the idea's five bullets already hold. Fine lighting is GPU-only and
  never copied back, and it already runs only near the player — phase 12 did
  that. What is left is shipping the *coarse* grid rather than recomputing it,
  smoothing it, and ordering the queue.
- That first item is not a micro-optimisation. The client re-floods every chunk
  it receives, and the client frame is light-bound: a fresh join spends four to
  five minutes at ~46 fps against a 116 fps steady state, redoing work the
  server did moments earlier and discarded.

The reference implementation the idea names, `~/projects/voxel_radiance_cascades`,
turns out to be missing its cascade passes — all six shader files are
byte-identical copies of `common.glsl` (md5-verified 2026-08-29, and its own
`CLAUDE.md` warns of exactly this). The plan records the part that does
survive, which is the probe layout, and explains why it is worth reading for
ideas rather than porting: this repo already has a working, oracle-tested
radiance-cascades implementation using a different direction encoding, and
none of the five bullets is about direction encoding.

## Draw Distance Upgrade 2.0

**Deferred — not enough detail to build.** Self-labelled WIP, and the two
bullets name outcomes rather than designs:

- Chunk compression over the network and before the copy to GPU memory
- Chunk LOD, also over the network and on the GPU

Both need a plan doc before they need code. Notes toward one:

- Chunks are *already* compressed on the wire — palette + LZ4, which took the
  join burst from 23 MB to 498 KB and has a 2 MB regression gate. So the
  network half of bullet one is done, and what is actually being asked for is
  compression **of the GPU-side copy**, which is a different problem with a
  different constraint: it has to be decompressible by the mesher, not by the
  CPU.
- The cheaper prerequisite is already listed in
  [`Tasks.md`](Tasks.md): *cull all-air chunks*. Occlusion culling withholds
  sealed chunks but still sends every empty chunk above the terrain, and
  roughly half the cube is sky. That is the largest single win available here
  and it needs no new format.
- LOD interacts with the lighting plan: a downsampled chunk needs downsampled
  light, and whether that is derived on the client or shipped is the same
  question [plan-better-lighting.md](docs/plan-better-lighting.md) phase A
  asks about full-resolution light. Decide them together.

##  GitHub Actions

- [ ] Create GitHub Actions workflow for creating builds released through the Releases section
- [ ] GitHub Pages page created showing videos of all the test cases that can be shown via video. On the Pages site, have different tabs for different branches of the repository. Trigger this workflow whenever there are changes to the repository, including branches other than master.