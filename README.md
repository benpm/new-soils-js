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
- **GPU-resident greedy meshing** — chunk triangle meshes are generated entirely
  on the GPU by a compute shader (`assets/shaders/voxel_mesh.wgsl`, a port of the
  CPU `greedy.rs`): one workgroup per (axis, plane) runs the AO-aware greedy sweep
  and appends merged quads to a per-chunk storage buffer via an atomic counter. No
  CPU meshing, no readback. (The CPU `greedy_mesh` in `soils-worldgen` is kept as
  the reference/oracle and stays unit-tested.)
- **Vertex pulling** — each chunk's `ChunkMeshMaterial` (`assets/shaders/
  atlas.wgsl`) pulls quads straight from the compute output buffer via
  `vertex_index` (a shared dummy mesh just sets the draw count). The fragment is
  the original `atlas.frag` port: world-space per-face tiling of `blocks.png`,
  ambient occlusion, and a normal brightness tint.
- **Ambient occlusion** — per-vertex 3-sample corner occlusion, computed in the
  compute shader; greedy merging is AO-aware (faces merge only when block id *and*
  corner AO match).
- **GPU radiance-cascades global illumination** — a compute shader
  (`assets/shaders/radiance.wgsl`) traces a hierarchy of world-space probe
  cascades against a voxel-occupancy volume around the player and merges them
  top-down into a single incoming-radiance field, which the terrain material
  samples to light itself. Emissive blocks (e.g. Diamond/Ruby Ore) and the sky
  bleed coloured light onto nearby surfaces, and caves fall dark. Fully
  GPU-resident: only the occupancy volume is uploaded; probes, rays, and merged
  radiance never leave the GPU. The math has a CPU oracle
  (`soils-worldgen/src/radiance.rs`) that the shader is validated against.
  **Experimental and off by default** — the per-frame trace is GPU-heavy and can
  destabilise some drivers; enable it in the pause menu, with `/gi on`, or at
  startup with `SOILS_GI=1`.
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
| `soils-worldgen` | Block registry, terrain generation, reference CPU greedy mesher. Pure, unit-tested. |
| `soils-server`   | tokio WebSocket server: generates/caches chunks, applies & broadcasts edits, ticks time. |
| `soils-client`   | Bevy app: networking bridge, chunk streaming, GPU compute meshing + vertex-pulling render, player, editing. |

## Running

**Single player**: just run the client and click **Singleplayer** on the login
screen — an internal server instance starts inside the client process (ephemeral
port) and you're logged in automatically. Saves live under `data/singleplayer/`,
separate from a dedicated server's `data/`.

A single-player world can be opened to the local network Minecraft-style: the
pause menu (Esc) has a **LAN discovery** toggle, off by default. Turning it on
makes the world show up in other clients' server lists (UDP 9002); turning it
off stops advertising immediately. Note the embedded server listens on all
interfaces either way (with discovery off it's just unadvertised — anyone
joining still needs to log in).

For multiplayer, start the server, then the client (each from the workspace
root):

```sh
cargo run -p soils-server          # listens on ws://127.0.0.1:9001
cargo run -p soils-client          # opens the game window
```

> Run the client with `cargo run` (not the bare binary) so Bevy resolves the
> `assets/` folder via `CARGO_MANIFEST_DIR`. To run the binary directly, set
> `BEVY_ASSET_ROOT=crates/soils-client`.

On launch a **login/signup screen** appears: click Singleplayer, or pick a
username (and optional password) and Log in or Sign up against a server.

Controls: **WASD** move, **mouse** look, **Shift** sprint, **Space/Ctrl** up/down
(fly) or jump, **F** toggle fly/walk, **left/right click** break/place,
**1-9** pick the placement block, **F3** toggle the debug overlay, **/** open the
command console, **Esc** release the cursor (shows the pause/settings menu).

Console commands: `tp x y z`, `warp <world>`, `daytime t`, `loadradius n`,
`fog on|off`, `ao on|off`, `gi on|off`.

### Linux build dependencies

Bevy needs the usual system libs: `libwayland-dev libxkbcommon-dev
libasound2-dev libudev-dev` (and `libxkbcommon-x11-0` at runtime for X11).

## Tests & headless verification

```sh
cargo test --workspace                          # protocol + worldgen + GI unit tests

# GI is validated three ways: the radiance-cascades math is unit-tested on the
# CPU (soils-worldgen radiance::tests); the compute shader is run headlessly on
# a real GPU and its cascade-0 output compared entry-for-entry against that CPU
# oracle (soils-client tests/gi_gpu.rs, auto-skips if no GPU); and both shaders
# are naga-validated whenever the client starts.

# End-to-end server check (run the server first):
cargo run -p soils-server --example smoke       # logs in, requests chunks, asserts terrain

# Persistence check (write an edit, restart the server, then verify):
cargo run -p soils-server --example editcheck -- write
cargo run -p soils-server --example editcheck -- verify

# Actor check: a headless peer the client will render as another player.
cargo run -p soils-server --example peer

# Headless client self-test (streams + meshes + renders + screenshots, then exits):
SOILS_SELFTEST=1 cargo run -p soils-client      # writes /tmp/soils-selftest.png

# GI demo scene: an enclosed dark room lit only by two ore light sources, framed
# for the camera — the clearest way to see the radiance-cascades bounce. Compare
# GI off vs on (writes /tmp/soils-selftest.png each run):
SOILS_SELFTEST=1 SOILS_GI_DEMO=1 SOILS_DAYTIME=0.0 SOILS_GI=0 cargo run -p soils-client  # dark
SOILS_SELFTEST=1 SOILS_GI_DEMO=1 SOILS_DAYTIME=0.0 SOILS_GI=1 cargo run -p soils-client  # lit by GI
```

## Deliberate simplifications vs. the JS original

- Terrain uses the Rust `noise` crate, so it is equivalent in character but not
  byte-identical to the JS `alea` + `simplex-noise` output.
- Region saves are append-only: rewriting a chunk appends a fresh compressed
  block and repoints the header, leaking the old block until a future compaction
  pass. Quads are rendered double-sided rather than fixing per-quad winding.

## Rendering & UI

- Physically-based atmosphere sky (Bevy 0.18) on an HDR/tonemapped camera, with
  a day/night cycle (the sun is rotated and the world dimmed via exposure) and
  exponential distance fog matched to the horizon haze.
- HUD: crosshair, F3 debug overlay, a wireframe selection box on the targeted
  voxel, a 1-9 block hotbar, a pause/settings menu (load radius, AO, fog, global
  illumination, LAN discovery), and a `/` command console.
- Chunks stream from the server in batched `Bundle` messages.

## Server

- Account auth: a login/signup screen; the server stores salted-hashed passwords
  (`data/accounts.bin`) and rejects all traffic until a connection authenticates.
  This is a lightweight stand-in, **not** production-grade security.
- Multiple named worlds created on demand, each with its own seed and region
  files; `/warp <world>` switches worlds (chunks/actors are world-scoped).
- Authoritative position correction: implausible movement jumps are rejected and
  the client is snapped back.

## Planned (later)

- Nicer actor avatars (nameplates, orientation, animation).
- RLE chunk compression and region compaction.
- Chunk demote/unload timers to cap server memory.
