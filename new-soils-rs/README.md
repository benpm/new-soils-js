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
- **Atlas texturing** — reuses the original `blocks.png` / `blocks.yaml`; per-face
  tile UVs on a nearest-filtered `StandardMaterial`.
- **First-person player** — fly/walk movement with AABB voxel collision,
  mouse-look, pointer lock.
- **Block editing** — raycast break (left click) / place (right click), applied
  optimistically and broadcast to other clients by the server.
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

# Headless client self-test (streams + meshes + renders + screenshots, then exits):
SOILS_SELFTEST=1 cargo run -p soils-client      # writes /tmp/soils-selftest.png
```

## Deliberate simplifications vs. the JS original

- Terrain uses the Rust `noise` crate, so it is equivalent in character but not
  byte-identical to the JS `alea` + `simplex-noise` output.
- The greedy mesher currently emits per-face quads (`merge = false`) so atlas
  tiles aren't stretched under the simple `StandardMaterial`. Quads are rendered
  double-sided as a slice-level shortcut.

## Planned (phase B)

- Greedy quad **merging** + a custom WGSL `AtlasMaterial` porting `atlas.frag`
  (world-space tiling, 4-tap sampling, ambient occlusion, normal tint).
- **Region-file persistence** (`flate2` sectors + header) replacing the in-memory
  chunk cache.
- Other-player rendering, sky/fog, and RLE chunk compression.
