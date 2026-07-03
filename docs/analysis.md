# Codebase analysis

Scope: the Rust workspace at the repo root (`crates/`). The legacy JS version has been removed
from the tree (see git history) and only matters as a behavioral reference.

## Layout and dependency direction

```
soils-protocol   coords, ChunkVolume, bincode wire enums, LAN discovery types. No Bevy/tokio.
soils-worldgen   BlockRegistry (blocks.yaml, embedded), TerrainGen, CPU greedy mesher,
                 CPU radiance-cascades oracle. Pure, unit-tested.
soils-server     tokio + tungstenite WebSocket server. Depends on protocol + worldgen.
soils-client     Bevy 0.18 app. Depends on protocol + worldgen + soils-server (embedded SP).
```

Dependencies point strictly downward; `soils-protocol` and `soils-worldgen` are engine-free. This
layering is the codebase's best structural asset and the reorganization plan preserves it.

## Data model

- Chunk = dense 32³ `u8` block ids (`ChunkVolume`), index `(y + z*32)*32 + x`, 32,768 B raw.
- Block ids come from `blocks.yaml` declaration order, embedded via `include_str!` in
  `soils-worldgen/src/lib.rs:16` — client and server registries are guaranteed identical at
  compile time (the copy in `soils-client/assets/blocks.yaml` is unused by code).
- Persistence: 16³-chunk region files; 16 KB u32 header (0=absent, 1=empty, else offset), data
  blocks are `len + zlib(voxels)`. Appends-only; rewrites leak the old block (`region.rs`).

## Server (`soils-server/src/lib.rs`, 572 lines)

Purely **reactive**: there is no tick loop and no simulation. Everything happens in per-connection
tokio tasks handling one message at a time, against shared state in `Arc<Mutex<...>>` webs:

- `Worlds = Arc<Mutex<HashMap<String, Arc<Mutex<World>>>>>` — chunk cache + terrain gen per world.
- `Players = Arc<Mutex<HashMap<u16, PlayerEntry>>>` — world name + last `ActorState` (pos, vel).
- `Clock = Arc<Mutex<f32>>` — one global daytime, advanced by a 1 s interval task.
- `Broadcast = broadcast::Sender<(u16 sender, String world, ServerMsg)>` — fan-out to every
  connection's forwarder task, filtered by world string; `"*"` = all. Every broadcast clones a
  `String` and a `ServerMsg` per subscriber.

Background tasks: day clock (1 Hz `Time` broadcast), actor sync (10 Hz `ActorUpdate` of **all**
players in a world to everyone in it), LAN discovery UDP responder (toggleable via `watch`).

Authority model is thin:
- Movement is client-authoritative. The only check is distance-per-`Move` ≤ `MAX_STEP = 64`
  (`lib.rs:541`) — at the client's 20 Hz send rate that permits ~1,280 u/s, so it stops only
  teleports, not speed hacks. Velocity is trusted verbatim. No server-side collision.
- Edits are applied with no reach/permission/rate validation (`lib.rs:525`). If the target chunk
  isn't in the server's cache, `World::edit` returns `false` and the edit is **silently dropped**
  (`world.rs:68`) — while the editing client has already applied it optimistically → persistent
  desync until that chunk is re-streamed.
- Auth is an acknowledged stub (`auth.rs`): `DefaultHasher` with a constant salt, whole account
  map rewritten per signup.

Persistence behavior: every single-voxel edit re-zlib-compresses and appends the whole 32 KB chunk
(`world.rs:72`). Generated chunks are also saved at first generation, so *exploring* writes to
disk. The in-memory chunk cache never evicts; memory grows with union of all players' exploration.

Wire protocol (`soils-protocol/src/messages.rs`): one bincode enum each way, each message a binary
WS frame. Chunk payloads are the **raw dense 32,768 bytes** (only all-air chunks are elided via
`empty`). No wire compression, no palette/RLE, no protocol version handshake, no tick numbers, no
acks. `Bundle` (16 chunks) is the only batching. At the default load radius 4 a fresh login
streams up to 9³ = 729 chunks ≈ 24 MB uncompressed worst case.

## Client (`soils-client`, ~3,400 lines + 3 WGSL shaders)

- **Net bridge** (`net.rs`): dedicated thread + tokio runtime; outgoing tokio unbounded channel,
  incoming crossbeam channel drained per frame. Deferred connect (login screen picks the URL).
  Clean, and worth keeping.
- **Message dispatch** (`main.rs:300` `net_receive`): a single god-system that matches every
  `ServerMsg` and touches chunks, GPU buffers, materials, actors, player transform, login state,
  world time — already at Bevy's 16-system-param limit (worked around with `SystemParam`
  bundling). Every new message type grows this function.
- **Chunk streaming** (`player.rs:203`): when the player crosses a chunk boundary, request all
  not-yet-loaded chunks in a cubic radius, sorted nearest-first. Chunks are never unloaded;
  `ChunkMap` and GPU buffers grow without bound.
- **Meshing** (`gpu_mesh.rs` + `voxel_mesh.wgsl`): per chunk, a compute pass regenerates the whole
  quad buffer whenever `pending > 0` (edit or re-stream). Dispatch is `(3, 33, 1)` workgroups of
  **size 1** — 99 serial threads per chunk, i.e. very low GPU occupancy; correctness came first
  (CPU `greedy.rs` is the tested oracle). Output: fixed 8192-quad buffer (655 KB) per chunk.
  Overflow silently drops quads (`voxel_mesh.wgsl:85`) and `count` can exceed `MAX_QUADS`, which
  the vertex shader compares against unclamped (`atlas.wgsl:124`) → out-of-bounds-clamped reads
  render garbage in the overflow case.
- **Drawing** (`material.rs` + `atlas.wgsl`): vertex pulling from the quad buffer via a shared
  49,152-vertex dummy mesh. Every chunk draws all 49k vertices every frame regardless of actual
  quad count (surplus collapse to degenerate points), all chunks have `NoFrustumCulling`, and
  cull mode is off (double-sided) to dodge winding. One material *instance* per chunk (needed for
  its quad buffer binding), so per-chunk bind groups.
- **Lighting**: terrain is **unlit**. A constant `TERRAIN_BRIGHTNESS = 45_000` lux stands in for
  sun+ambient, day/night is EV100 exposure interpolation plus sun-disc rotation, fog is manual
  exp². Shadows are explicitly disabled on the sun. AO is per-vertex from the mesher. Without GI,
  caves are as bright as noon surfaces (modulo AO).
- **GI** (`gi.rs`, `radiance.wgsl`): opt-in radiance cascades. 64³ occupancy window around the
  player, CPU-blitted from chunk copies and re-uploaded (262 KB) on recenter and every 30 frames;
  4 cascades (16³×4² … 2³×32² = 122,880 rays) traced every 6th frame, fixed-step (0.5 voxel) ray
  march, no DDA, sampled per-fragment as a 16-direction cosine sum from the nearest probe. Solid
  engineering (CPU oracle + headless GPU-vs-oracle test in `tests/gi_gpu.rs`) but it is a *local*
  effect (30-voxel interval reach) and the only source of darkness/emissive light in the game.
- **Physics/input** (`player.rs`): per-frame variable-`dt` integration in `Update` (not
  `FixedUpdate`), axis-separated AABB sweep against `voxel_at` ECS lookups. Fine for one player;
  useless for prediction/replay as-is because it is frame-rate-dependent and lives only client-side.
- **Actors** (`actor.rs`): remote players are cuboids lerped toward the latest 10 Hz position at a
  fixed rate (`t = dt*12`) — effectively exponential smoothing, no velocity extrapolation, no
  snapshot buffer, no orientation.
- **Edits** (`edit.rs`): Amanatides–Woo raycast, optimistic local apply + `Edit` to server. No
  sequence numbers, no rollback path if the server disagrees (and the server never tells you).
- **Singleplayer** (`singleplayer.rs`): embedded `soils_server::spawn()` on a loopback ephemeral
  port — one code path for SP/MP. This is a keeper.

Small duplications worth folding when touched: `read_voxel` (edit.rs) vs `voxel_at` (chunk.rs)
differ only in query mutability; `ease10` exists in both `main.rs` and `gi.rs`.

## Testing & validation culture

Strong, and the pattern to protect during any rewrite:
- Pure crates unit-tested (protocol round-trips, region round-trip, terrain invariants, greedy
  mesher, radiance math).
- **Oracle pattern**: CPU reference implementations (`greedy.rs`, `radiance.rs`) are the tested
  source of truth; GPU ports are validated entry-for-entry against them on real hardware
  (`tests/gi_gpu.rs`, auto-skips without GPU).
- End-to-end: headless self-test client (stream→mesh→render→screenshot→assert), server smoke /
  editcheck / peer examples, CI renders screenshots into releases.

## Strengths to preserve

1. Crate layering with engine-free protocol/worldgen cores.
2. Oracle pattern + headless E2E validation.
3. GPU-resident meshing/vertex-pulling architecture (fix its edges; keep the shape).
4. Singleplayer = embedded server (one networking/auth/streaming path).
5. Small, documented, boring wire protocol — easy to replace wholesale.

## Structural limits (what blocks the roadmap)

| Limit | Where | Why it blocks growth |
|---|---|---|
| No server tick/simulation; reactive handlers on mutexes | `lib.rs` | Entities, NPCs, server physics, lag compensation all need an authoritative fixed-rate loop |
| Players are the only "entity"; full-state 10 Hz broadcast to all | `lib.rs:299` | O(players²) traffic; no kinds, no components, no interest management, no deltas |
| Client-authoritative movement, unvalidated edits | `lib.rs:525,532` | Authoritative-server model requires input-based movement + server-side sim |
| Movement/collision code exists only in the client, on variable dt | `player.rs` | Server can't simulate; client can't predict/replay deterministically |
| Dense uncompressed chunk wire format | `messages.rs:52` | ~24 MB joins; no room for entity traffic budgets |
| No protocol versioning/ticks/acks | `messages.rs` | Delta replication needs baselines, acks, and evolvable framing |
| Unbounded chunk caches both sides; per-edit whole-chunk persistence | `world.rs`, `player.rs` | Memory and disk-write growth with play time |
| God-system message dispatch at the param limit | `main.rs:300` | Every feature adds messages; needs event-per-type routing |
| Lighting = constant brightness + optional local GI; nothing baked, nothing server-side | `material.rs`, `gi.rs` | See `plan-rendering.md` |
| Meshing/draw inefficiencies (fixed buffers, no culling, no indirect draw, size-1 workgroups) | `gpu_mesh.rs` | Caps world size/perf; see `plan-rendering.md` |

The two companion documents build directly on this table:
- `plan-game-systems.md` — entities, authoritative server, delta-compressed replication, and
  paths for pathfinding + client-side workload sharing.
- `plan-rendering.md` — baked+dynamic lighting split, cheap server-side macroscopic lighting.
