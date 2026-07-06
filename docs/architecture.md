# Architecture

How the system works as implemented — all 14 phases of `TODO.md` are
complete. This is the "what is" companion to the "what should be" plans
(`plan-rendering.md`, `plan-game-systems.md`) and the measurements in
`perf-report.md`. File references are starting points, not an API contract.

## Workspace

| Crate | Role |
|-------|------|
| `soils-protocol` | Wire types + codecs: `ClientMsg`/`ServerMsg` (bincode), the palette+LZ4 chunk codec (`chunk_codec.rs`), the quantized delta-snapshot codec + `SnapshotTracker` (`snapshot.rs`), chunk/voxel coordinates. No Bevy, no tokio. |
| `soils-worldgen` | Block registry, terrain generation (lattice-interpolated cave noise, early-outs), and the *CPU oracles*: reference greedy mesher (`greedy.rs`) and radiance-cascades math (`radiance.rs`). Pure, unit-tested, criterion-benched. |
| `soils-sim` | The shared simulation both sides run: player movement/collision (`step_player`), input packing, edit validation, the L0 light flood (`light.rs`), entity registry (`entities.yaml`), and pathfinding (`nav.rs`: walk grids, budgeted A*, HPA*, flow fields). Engine-free, everything over a `VoxelSampler` trait (unloaded space reads as air). |
| `soils-server` | Headless authoritative server: a Bevy ECS app (`app.rs`) at a 20 Hz fixed tick behind a tokio network edge (`lib.rs`), world/chunk lifecycle + persistence (`world.rs`, `region.rs`, `persist.rs`), accounts (`auth.rs`). |
| `soils-client` | The Bevy game: net bridge (`net.rs`), chunk streaming + GPU meshing (`server_msg.rs`, `gpu_mesh.rs`, `indirect_draw.rs`), materials + L0 shading (`material.rs`, `light.rs`), radiance-cascades GI (`gi.rs`), prediction (`player.rs`), remote-entity interpolation (`actor.rs`), optimistic edits (`edit.rs`), UI. |

One rule holds everything together: **client and server share one simulation**
(`soils-sim`) and one set of codecs (`soils-protocol`), so predicted movement,
server authority, lighting, and replicated state can't drift by construction —
the remaining risks are pinned by oracle tests.

## Data flow

```
             ┌────────────────────── soils-server ──────────────────────┐
 tokio edge  │  ECS app (20 Hz FixedUpdate)                             │
┌─────────┐  │  accept → drain_inboxes → wander_critters →              │
│ WS pump │──┼─▶ inbox   (auth, inputs, edits, view radius)             │
│ WT pump │◀─┼── outbox  (reliable: chunks/edits/control)               │
└─────────┘  │   snapshot lane (latest-wins watch channel)              │
             │  pump_chunk_jobs ── rayon: worldgen waves, light jobs    │
             │  replicate_entities ─ interest diff + delta snapshots    │
             │  world_lifecycle ── refcounts, evict, flush, compaction  │
             └───────────────────────────────────────────────────────────┘
                     ▲ inputs (datagram/WS)        │ chunks, snapshots
                     │                             ▼
             ┌────────────────────── soils-client ──────────────────────┐
             │ net thread (ws:// or wt://) → NetEvent channel           │
             │ apply_chunks (ordered stream, time-boxed)                │
             │   → GPU voxel buffers → compute mesher → indirect draws  │
             │   → L0 light flood → padded light buffers → material     │
             │   → GI volume blit → trace/merge round-robin → probes    │
             │ snapshots → SnapshotTracker → reconcile self / buffer    │
             │             remote actors (interp @ 2-tick delay)        │
             │ FixedUpdate 64 Hz: predict_and_send (shared step_player) │
             └───────────────────────────────────────────────────────────┘
```

## Protocol

- **Encoding**: bincode for message envelopes; two hand-packed hot paths.
- **Chunk payloads** (`chunk_codec.rs`): per-chunk palette, bit-packed
  indices, LZ4; tiers for uniform (air/solid), paletted, and raw-dense
  chunks. Fuzz-tested to never panic on arbitrary bytes.
- **Snapshots** (`snapshot.rs`): positions quantized to 1/256 voxel, encoded
  as zigzag-varint deltas against the **acked baseline tick** (carried on the
  wire as `baseline_tick`); velocity/yaw only when changed; LZ4 above 200 B.
  `SnapshotTracker` keeps per-entity (tick, state) rings so any baseline the
  server might use is reconstructable — this is what makes the unreliable
  snapshot lane safe.
- **Inputs**: packed frames (buttons/flags/quantized yaw), the last 3 bundled
  per send for loss tolerance, `ack_tick` piggybacked for snapshot acking.

## Transports

Two lanes with different semantics, one app-side shape (`NewConn`):

- **Reliable ordered**: login, chunk data/unloads, edits + acks, entity
  spawn/despawn, time, warp.
- **Latest-wins / unreliable**: snapshots (server→client) and inputs
  (client→server). Server-side this is a `watch` channel — a backed-up link
  replaces the unsent snapshot rather than queuing it.

Backends: **WebSocket** (default; both lanes share the socket) and
**WebTransport/QUIC** (`wt://`, opt-in via `SOILS_WT=1`): the reliable lane
is a client-opened bi stream of length-framed bincode, and snapshots/inputs
are real QUIC datagrams. TLS is a per-boot self-signed identity with client
verification skipped — LAN trust, same as `ws://`; the cert-hash pinning
path exists for a future wasm client. The ECS app cannot tell transports
apart.

## Server

- **Tick** (`FixedUpdate`, 20 Hz): accept new connections → drain inboxes
  (auth, input token bucket at the 64 Hz sim rate ×32 burst, edit validation:
  seq, rate bucket, reach, block id, residency) → critter AI → chunk job
  pumping → entity replication → clock → world lifecycle. Player movement
  integrates client inputs through the shared `step_player` at the client dt,
  so speed-hacking is structurally impossible (scenario-verified).
- **Chunk lifecycle**: subscriptions are server-owned boxes around each
  client (radius + hysteresis); wanted chunks are probed cache→disk on the
  tick and generated on rayon in nearest-first waves (≤8 in flight per
  client), delivered in request order. Residency is refcounted; zero-ref
  chunks evict after 60 s (save-if-dirty through the background persister);
  region files compact on open past a 25% leak ratio.
- **Lighting**: the shared L0 flood (skylight + blocklight nibbles) runs
  against dense cloned regions on rayon, version-guarded on write-back;
  edits relight inline (small), and per-chunk summaries (dark walkable-air
  counts + sampled cells) power gameplay queries like
  `darkest_walkable_near` without touching voxels on the wire.
- **Replication**: interest = chunk-column buckets within the subscription
  radius, diffed into spawn/despawn a few times a second; state goes out
  every tick as delta snapshots under a 410 B budget with a per-entity
  priority accumulator (base/dist², players boosted, reset on send).
- **Pathfinding**: per-chunk `(WalkGrid, ChunkNav)` cached keyed by the
  (own, below, above) edit-version triple (walk grids sample vertical
  neighbors). Critters seek nearby players via budgeted A*, fall back to
  HPA* (region-portal graph + flat refinement) when the budget can't reach,
  and validate each waypoint against live voxels so edits force repaths.

## Client

- **Streaming**: chunk data and unloads share one ordered queue, applied
  under a per-frame wall-time budget (count budgets collapse on slow frame
  clocks). Each chunk gets GPU voxel buffers; meshing is a compute pass
  (greedy, AO-aware, oracle-matched) whose finalize step writes indirect
  draw args — chunks render via `draw_indirect` with exact AABBs, backface
  culling, and no CPU meshing or readback anywhere.
- **Shading**: the material samples the padded per-chunk L0 light volume
  (sky nibble scaled by the day curve + warm blocklight) plus meshed-in AO;
  with GI on it adds the radiance-cascades term.
- **GI** (opt-in): a 64³ occupancy+light volume around the player is
  refilled by a GPU blit of resident chunk buffers; 4 probe cascades trace
  against it (top-cascade escapes gated by baked skylight), merge top-down
  — one cascade per frame, trace and merge paired — and project into
  per-probe ambient cubes that fragments fetch trilinearly. CPU oracles
  cover trace, blit, and projection entry-for-entry.
- **Prediction**: 64 Hz `FixedUpdate` steps the shared sim, records
  (seq, input, state) in a ring, sends bundled inputs. Each snapshot
  rewinds to the server's state at `last_input_seq` and replays pending
  inputs; remote entities render from per-entity snapshot buffers at a
  2-tick interpolation delay with capped extrapolation. Edits apply
  optimistically with rollback on rejection.

## Testing

- **Oracles**: GPU mesher vs CPU greedy mesher (sorted multiset equality);
  GI trace/blit/irradiance vs `radiance.rs`/CPU replicas; incremental light
  vs fresh relight; nav paths validated move-by-move.
- **Codecs**: golden bytes, round-trips, fuzzed panic-free decode.
- **Scenarios** (`soils-server/tests/`): an embedded server + scripted
  protocol clients pin movement authority, edit acks/rejects, subscriptions,
  persistence across restarts, world isolation, critter seek, bandwidth
  budgets, and the WebTransport datagram loop. Tests serialize on a static
  gate — parallel embedded servers starve the shared rayon pool.
- **Prediction**: a TCP delay proxy (75 ms each way, 2% input loss) drives a
  headless client twin through convergence and forced-misprediction cases.
- **Visual**: `SOILS_SELFTEST=1` renders, screenshots, and asserts terrain
  presence headlessly; the GI demo scene isolates the bounce for eyeballing;
  CI renders release screenshots under Mesa lavapipe.

## Known deferrals

Each `TODO.md` checkoff records its own; the ones that shape future work:
pooled quad memory / merged draws (the current frame bound), GI 3D-texture +
DDA marching and default-on, async pathfinding + a flow-field consumer (the
mob spawner), snapshot MTU packing, wasm client with cert-hash pinning, and
the `bevy_replicon` re-evaluation if entity kinds multiply. Details and
rationale: `perf-report.md` §"What's left on the table".
