# Plan: game-systems reorganization — entities, authoritative server, delta replication

Goal: restructure so the server owns simulation of arbitrary **entities**, replicates world-state
**deltas** to clients in compact packets under an authoritative-server model, and leaves clean
seams for pathfinding and client-side prediction. Companion: `plan-rendering.md`.

## 1. Target crate layout

```
crates/
  soils-protocol   wire format ONLY: framing, channels, versioned messages, quantization helpers
  soils-sim        NEW — shared gameplay core, engine-light (bevy_ecs types allowed, no rendering):
                   entity kinds/components, movement + AABB voxel collision, raycast,
                   edit rules (reach/validity), fixed-tick stepping, light-grid flood (see rendering note)
  soils-worldgen   unchanged role: terrain, blocks, meshing/lighting CPU oracles
  soils-server     headless Bevy App (MinimalPlugins): net ingest, sim schedule, replication, persistence
  soils-client     Bevy App: presentation, prediction, interpolation, UI
```

Key move: everything both sides must agree on (integration step, collision, edit legality, light
flood) lives in `soils-sim` as plain functions over plain data (`ChunkVolume`, positions, inputs),
with thin ECS wrappers on each side. Exact float determinism is *not* required — the server is
authoritative and prediction only needs closeness + reconciliation — but identical code keeps
mispredictions rare.

Server becomes a Bevy ECS app for real (not just tokio): `MinimalPlugins` + `FixedUpdate` at a
fixed tick (start at 20 Hz; constant `TICK_HZ` in `soils-sim`). Connections stay on tokio tasks;
they only push decoded messages into per-client inboxes (mpsc) drained at tick start, and drain
per-client outboxes after replication. All `Arc<Mutex<HashMap>>` state dissolves into ECS
resources/components owned by the sim world. Multiple named worlds = one ECS world with a
`WorldId` component on chunks/entities (cheaper than N ECS worlds, and interest management
already partitions by world).

## 2. Entity model

- `NetId(u32)` allocated by the server; never reused within a session. Client keeps
  `HashMap<NetId, Entity>` (generalizing today's `ActorMap`).
- Data-driven kinds, mirroring `blocks.yaml`: `entities.yaml` → `EntityRegistry`
  (name, kind id `u16`, half-extents, base speed, replicated-component set, render info).
  Embedded via `include_str!` like blocks, so both binaries agree at compile time.
- Components (initial replicated set): `Kind`, `Pos` (Vec3), `Vel`, `Facing` (yaw/pitch),
  `Health`. Server-only: `Inputs`, `AiState`, `InterestSet`. Client-only: render/interp state.
- Players become ordinary entities with a `PlayerControlled(ClientId)` component — the special
  actor path in server `lib.rs` and client `actor.rs` disappears.
- Spawning/despawning is exclusively server-side; clients learn via replication (§4).

## 3. Protocol v2

Framing (per WS binary frame, before any transport upgrade):

```
u8  protocol_version      (bump on breaking change; Login rejects mismatch)
u8  channel               (0 control, 1 chunks, 2 snapshot, 3 edits)
u8  flags                 (bit0: payload is LZ4-compressed)
payload
```

- **Control** (reliable, rare): Login/Init/Warp/Time/errors — stays bincode, unchanged spirit.
- **Chunks** (bulk): palette-compressed chunk payloads (§5).
- **Snapshot** (hot path, tick-stamped): entity deltas (§4), hand-packed, not bincode.
- **Edits** (reliable, ordered): voxel edit batches with sequence numbers (§6).

Client→server hot path changes from `Move{pos,velocity}` to **inputs**:

```
InputMsg { tick: u32, seq: u32, buttons: u8 (WASD/jump/sprint/fly), yaw: u16, pitch: u16 }
```

sent every client tick (with the last N=3 inputs bundled for loss/ordering robustness — cheap,
~10 B each). The server applies inputs through `soils-sim::step_player` against *server-loaded*
chunks. This deletes the `MAX_STEP` heuristic: movement cheating becomes structurally impossible,
and `ServerMsg::Position` snap-back is replaced by normal reconciliation data (§4 header).

Transport: keep WebSocket now, but hide it behind a small trait (`send_reliable(channel, bytes)`,
`send_latest(bytes)` where WS implements `send_latest` as reliable). The snapshot channel is
designed for unreliable/sequenced delivery so a later **WebTransport/QUIC-datagram** (wasm-ready)
or UDP (renet) backend is a drop-in that removes TCP head-of-line blocking. Do not build on
"reliable ordered everything" assumptions outside the control/edit channels.

Ecosystem checkpoint (evaluated 2026-07): `bevy_replicon` provides exactly this shape —
change-detection replication, per-client visibility, `PriorityMap`, custom per-component
serialization (`RuleFns`), pluggable messaging backends; `lightyear` now *uses* replicon for
replication and adds prediction/interpolation/transport. Once the server is a Bevy app (§1),
adopting replicon + a custom WS/WebTransport backend is a serious alternative to the hand-rolled
pipeline in §4 — same concepts, less code to own. Decision point is at milestone M5; the
quantization layer below is needed either way (as replicon `RuleFns`/serialize fns).

## 4. Delta replication pipeline (the core)

Server, per tick, after simulation:

1. **Collect changes** via bevy_ecs change detection (`Changed<Pos>` etc.) into a per-tick change
   set; spawns/despawns recorded explicitly.
2. **Interest filter** per client (§7): only entities in the client's interest set; entering
   entities → full spawn record, leaving → despawn record.
3. **Delta vs acked baseline**: per client keep `last_acked_tick` plus a ring buffer of the last
   64 ticks of **quantized** component values per entity. Encode against the baseline the client
   acked (falling back to a full snapshot if the baseline aged out of the ring). Storing the
   *quantized* form makes deltas integer subtractions and kills requantization drift.
4. **Quantize + bit-pack** (in `soils-protocol`, unit-tested):
   - pos: fixed-point 1/256 voxel, per-axis zigzag varint of the delta (typically 1–2 B/axis
     moving, 0 when still);
   - vel: only when changed beyond epsilon, f16 per axis;
   - yaw/pitch: u16/u8 absolute (cheap, avoids error accumulation);
   - per-entity header: varint NetId delta (ids sorted ascending) + component-changed bitmask u8;
   - packet header: `tick: u32`, `baseline_tick: u32`, `last_input_seq: u32` (reconciliation
     anchor for the receiving client's own entity), varint entity count.
5. **Budget & prioritize**: per-client byte budget per tick (default ~8 KB/s for snapshots).
   Priority accumulator per (client, entity): grows each tick by `base_priority / distance²`,
   reset on send. Fill the packet in priority order until budget; starved entities catch up
   automatically (Overwatch/replicon scheme). Nearby players update at full tick rate, far mobs
   at a few Hz — this is what makes 100+ entities affordable, not compression alone.
6. **Compress**: if payload > ~200 B, LZ4 (`lz4_flex`) and set the flag bit. Delta+bitpack output
   is high-entropy; expect LZ4 to matter mainly for spawn bursts and chunk frames.
7. Client acks by echoing `tick` in its next `InputMsg` (`ack_tick: u32` field) — no separate ack
   message.

Client receive side:
- Own entity: compare server state @ `last_input_seq` against the predicted history; if within
  epsilon do nothing, else rewind + replay pending inputs (§9 prediction; ship interpolation-only
  first).
- Remote entities: push `(tick, state)` into a per-entity snapshot buffer; render at
  `server_time − interp_delay` (~2 ticks / 100 ms) with interpolation between buffered snapshots
  and capped extrapolation from `Vel` beyond the buffer. Replaces the fixed-rate lerp in
  `actor.rs:80`.

Envelope math (sanity target): 20 moving entities in interest ≈ 20 × ~8 B ≈ 160 B + header at
20 Hz ≈ **3–4 KB/s** steady state — versus today's 26 B × N players × 10 Hz full-state broadcast
with no interest cut.

## 5. Chunk streaming v2

- **Server-driven**: subscription = f(player chunk, radius) computed server-side; server pushes
  on enter, sends `ChunkUnload` on leave (hysteresis of +1 chunk to avoid thrash). Deletes the
  client request path (`player.rs:203`) and gives the server an exact per-client resident set —
  which is also the entity interest set (§7) and the chunk-cache refcount (§8).
- **Encoding**: palette per chunk (unique ids, typically ≤16) → bit-packed indices
  (32,768 × log₂|palette| bits), then LZ4 over the frame. Terrain chunks land ~1–4 KB (vs 32 KB).
  Single-id chunks (air, solid stone) collapse to the 1-entry-palette fast path — subsumes the
  `empty` flag. Keep a `RawDense` fallback variant for pathological palettes.
- Order pushes nearest-first (server knows the player position), a few chunks per tick so
  snapshots never starve behind a join burst.
- Validation: golden-bytes tests for the encoder; property test `decode(encode(v)) == v` over
  randomized volumes; a bandwidth-regression test that encodes a fixed generated region and
  asserts total bytes under a checked-in threshold.

## 6. Edits under authority

- Client sends `EditMsg { seq: u32, pos, value }`; still applies **optimistically** (immediacy),
  but records `seq → (pos, prev_value)` in a pending list.
- Server validates in `soils-sim::validate_edit`: chunk loaded (load it if in any client's
  interest — never silently drop; fixes the `world.rs:68` desync), reach from the player's
  *server* position, value legal, rate cap. Applies, bumps the chunk's **version counter**, and
  broadcasts `EditAccepted { seq (to the editor), pos, value }` / `EditRejected { seq }` to the
  editor and plain edits to others in interest.
- On reject the client rolls back via its pending list. Aggregate multiple edits per tick per
  chunk into one frame.
- Persistence: mark chunk dirty; a save system flushes dirty chunks on an interval (e.g. 30 s) and
  on unload/shutdown — replaces compress-whole-chunk-per-voxel-edit (`world.rs:72`). Region
  compaction (rewrite file dropping leaked blocks) runs on world open when leaked bytes exceed a
  ratio.

## 7. Interest management

- Spatial index: `HashMap<IVec2 chunk-column, SmallVec<Entity>>` maintained by a system when
  entities cross column borders (cheap; entities also carry their current column).
- Per client: interest = chunk-radius box around their entity (same radius as chunk subscription)
  ∩ same `WorldId`. Recompute only when the client crosses a chunk boundary; hysteresis on exit.
- Output feeds §4 step 2 and §5. Warp = interest set swap; the existing drop-all-and-restream
  client behavior stays valid.

## 8. Server world lifecycle

- Chunk cache entries carry a refcount = number of subscribing clients (+ pins from AI/pathfinding
  users). Zero refs → unload timer (e.g. 60 s) → save-if-dirty + evict. Bounds server memory.
- Per-chunk `version: u32` bumped on every edit — the invalidation signal for meshes (client),
  nav data (§10), and light data (rendering note).

## 9. Path: client-side prediction & workload sharing (build later, design for now)

Everything in §3–§4 already carries what prediction needs (input seqs, `last_input_seq` echo,
fixed tick). Order of adoption:

1. **Interpolation only** (ship with M6): own entity rendered from server state — feels laggy,
   proves the pipeline.
2. **Own-entity prediction**: client runs `soils-sim::step_player` in `FixedUpdate` on local
   inputs immediately; keeps a history ring of (input, resulting state); on each snapshot, rewind
   to `last_input_seq`, compare, replay if diverged. Requires client physics to move from
   variable-dt `Update` (`player.rs:97`) to the shared fixed-tick step — do that move in M1 even
   though prediction lands later.
3. **Optimistic edits** already exist; §6 upgrades them from "hope" to "rollback-correct".
4. **Remote smoothing**: snapshot-buffer interpolation (§4) + velocity extrapolation caps.
5. **Lag compensation** (server): the per-client baseline ring (§4.3) doubles as a position
   history; for hit/interaction validation, rewind targets to the client's view tick.
6. **Cosmetic client simulation**: particles, sounds, item-bob, block-break cracks run purely
   client-side triggered by predicted/replicated events — never replicated back.
7. **Delegable non-authoritative work**: clients already own meshing + render lighting. The same
   principle extends to any expensive *presentation* derived from replicated state (e.g. local
   light rebake §rendering, ambience). Rule: the server may *hint*, clients may *compute*, but
   nothing client-computed ever writes authoritative state.

Validation: an integration test with a simulated 150 ms/2% loss link (tokio + delay queue)
asserting (a) predicted-vs-authoritative divergence stays under epsilon during straight-line
movement, (b) forced misprediction (server-side wall insertion) reconciles within one RTT without
visible teleport beyond the correction itself.

## 10. Path: entity pathfinding (server-side, later)

Layered so each stage is independently useful:

1. **Walkability grid** per chunk, derived on load/edit: bit-set of "air with 2-high headroom
   above solid". Stored beside the chunk, invalidated by chunk `version`. Pure function in
   `soils-sim` with oracle-style unit tests.
2. **Local A\*** on the walkability grid (jump/fall costs like Minecraft mobs) for short paths
   (< ~32 voxels). Enough for melee mobs; budgeted per tick (N expansions), async via task pool,
   results cached keyed by (start-cell, goal-cell, chunk versions touched).
3. **Hierarchical layer (HPA\*)**: per chunk, connected components of walkable cells → region
   nodes; portals where regions touch across chunk borders → edges. Global search runs on the
   portal graph (hundreds of nodes, not millions of voxels), then stage-2 A\* refines within the
   corridor chunk by chunk. Incremental: an edit only rebuilds its chunk's regions/portals.
4. **Flow fields** for crowds: one field per (goal, region) shared by all agents heading there
   (spawn waves → player). Cost integrates the walkability grid; reuse across agents makes 100s
   of movers cheap.
5. Client never pathfinds authoritatively, but may run stage-2 locally for *cosmetic* agents and
   for immediacy hints (e.g. preview a commanded unit's path instantly while the server computes
   the real one) — consistent with §9's trust rule.

## 11. Migration milestones (each shippable, tests first)

| M | Change | Proves |
|---|---|---|
| M1 | Extract `soils-sim` (movement/collision/raycast/edit rules); client physics → `FixedUpdate` using it; split `net_receive` into per-message systems via Bevy events | No behavior change; god-system gone |
| M2 | Server → headless Bevy app with 20 Hz FixedUpdate; connections feed inboxes; old protocol adapted on top; mutex webs → ECS | Same protocol, new engine room |
| M3 | Chunk v2: server-driven subscribe/unload + palette+LZ4 encoding; server chunk refcount/evict; coalesced saves | Join bandwidth ÷ ~10; bounded memory |
| M4 | Inputs replace `Move`; server simulates players via `soils-sim`; interpolation-only rendering of self | True server authority |
| M5 | Entity model + registry + spawn/despawn replication + interest management (decision point: hand-rolled vs bevy_replicon) | NPC-capable |
| M6 | Delta snapshot pipeline: baselines, quantization, priority accumulator, budget, acks | The "hyper-efficient packets" milestone |
| M7 | Prediction + reconciliation; optimistic-edit rollback; lag-compensated interactions | Feels instant at 150 ms |
| M8+ | Pathfinding stages 1–4; transport upgrade (WebTransport/QUIC datagrams) | Living world |

Cross-cutting validation (per milestone, extending the existing culture): protocol golden-bytes +
fuzz-decode (`decode` must never panic on arbitrary bytes — it's the attack surface); loopback
sim harness (1 server + 2 headless clients in-process) asserting state convergence after N ticks;
bandwidth counters asserted against thresholds in CI; keep the smoke/editcheck/peer examples
working at every milestone.
