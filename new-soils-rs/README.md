# new-soils (Rust + Bevy port)

A Rust/[Bevy](https://bevyengine.org) port of the original Node.js + Three.js
`new-soils` voxel sandbox. This is a **playable vertical slice** with a
client/server split: a headless authoritative server generates and serves
terrain, and a Bevy client streams, meshes, renders, and edits it.

What works today:

- **Terrain generation** — multi-octave simplex heightmap, soil gradient, rock
  outcrops, and 3D-noise caves, ported from `server.js`'s `Chunk.generate`.
- **Client/server networking** — WebSocket transport with a clean bincode
  protocol; the client streams chunks in a load radius around the player.
- **Greedy meshing** — ported from `mesher_worker.js`, run off-thread on Bevy's
  `AsyncComputeTaskPool`.
- **Atlas texturing** — reuses the original `blocks.png` / `blocks.yaml`. A custom
  WGSL material (`assets/shaders/atlas.wgsl`, ported from `atlas.frag`) does
  world-space per-face tiling, ambient occlusion, and a normal brightness tint.
- **Greedy quad merging** — AO-aware: faces only merge when block id *and* corner
  AO match, so flat areas collapse to big quads without smearing AO.
- **Ambient occlusion** — per-vertex, from the canonical 3-sample corner formula.
- **Region-file persistence** — chunks are zlib-compressed into 16³ region files
  (`data/worlds/default/regions/`) and reloaded on restart; edits are saved.
- **First-person player** — fly/walk movement with AABB voxel collision,
  mouse-look, pointer lock.
- **Block editing** — raycast break (left click) / place (right click), applied
  optimistically and broadcast to other clients by the server.
- **Multiplayer actors** — clients report their position; the server broadcasts
  everyone's positions and each client renders the others as interpolated bodies.
- **Day/night** — server-driven time of day swinging a directional light.

## Workspace layout

| Crate | Role |
|-------|------|
| `soils-protocol` | Shared chunk coords, voxel storage, and the bincode wire protocol. No Bevy/tokio. |
| `soils-worldgen` | Block registry, terrain generation, greedy mesher. Pure, heavily unit-tested. |
| `soils-server`   | tokio WebSocket server: generates/caches chunks, applies & broadcasts edits, ticks time. |
| `soils-client`   | Bevy app: networking bridge, chunk streaming, async meshing, rendering, player, editing. |

## Running

Start the server, then the client (each from the workspace root):

```sh
cargo run -p soils-server          # listens on ws://127.0.0.1:9001
cargo run -p soils-client          # opens the game window
```

> Run the client with `cargo run` (not the bare binary) so Bevy resolves the
> `assets/` folder via `CARGO_MANIFEST_DIR`. To run the binary directly, set
> `BEVY_ASSET_ROOT=crates/soils-client`.

Controls: **WASD** move, **mouse** look, **Shift** sprint, **Space/Ctrl** up/down
(fly) or jump, **F** toggle fly/walk, **left/right click** break/place, **Esc**
release cursor.

### Linux build dependencies

Bevy needs the usual system libs: `libwayland-dev libxkbcommon-dev
libasound2-dev libudev-dev` (and `libxkbcommon-x11-0` at runtime for X11).

## Tests & headless verification

```sh
cargo test --workspace                          # protocol + worldgen unit tests

# End-to-end server check (run the server first):
cargo run -p soils-server --example smoke       # logs in, requests chunks, asserts terrain

# Persistence check (write an edit, restart the server, then verify):
cargo run -p soils-server --example editcheck -- write
cargo run -p soils-server --example editcheck -- verify

# Actor check: a headless peer the client will render as another player.
cargo run -p soils-server --example peer

# Headless client self-test (streams + meshes + renders + screenshots, then exits):
SOILS_SELFTEST=1 cargo run -p soils-client      # writes /tmp/soils-selftest.png
```

## Deliberate simplifications vs. the JS original

- Terrain uses the Rust `noise` crate, so it is equivalent in character but not
  byte-identical to the JS `alea` + `simplex-noise` output.
- Region saves are append-only: rewriting a chunk appends a fresh compressed
  block and repoints the header, leaking the old block until a future compaction
  pass. Quads are rendered double-sided rather than fixing per-quad winding.

## Planned (later)

- A sky/atmosphere shader and distance fog; nicer actor avatars (nameplates,
  orientation, animation).
- RLE chunk compression and region compaction.
- Chunk demote/unload timers to cap server memory.
