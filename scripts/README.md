# Server scripts (AssemblyScript → WASM)

Server-side scripts run **inside the authoritative 20 Hz tick**. They read world
state and emit commands (voxel edits, entity spawn/despawn/move) that the server
applies to the world; those changes replicate to clients through the normal
snapshot + edit-broadcast path — no client or protocol changes.

## Enabling

```sh
SOILS_SCRIPTS=1 cargo run -p soils-server        # loads ./scripts
SOILS_SCRIPTS_DIR=/path cargo run -p soils-server # custom directory
```

For `.ts` scripts you also need the AssemblyScript compiler. It's pinned in the
repo's `package.json`, so from the repo root:

```sh
npm install     # puts `asc` in node_modules/.bin
```

The runtime loads three file kinds from the scripts directory:

- `*.ts`   — AssemblyScript, compiled at runtime via `asc` (needs Node + the
  `assemblyscript` package — see `npm install` above; set `SOILS_ASC` to
  override the command). If no `asc` is found, `.ts` files are skipped with a
  log. Detection tries `asc` then `npx --no-install asc`, preferring the `.cmd`
  shims on Windows (npm installs `asc`/`npx` as `.cmd`, which Rust's
  `Command` won't resolve from a bare name).
- `*.wasm` — precompiled module, loaded directly.
- `*.wat`  — WebAssembly text, loaded directly (handy for tests/fixtures).

Compiled `.ts` output is cached under `scripts/.cache/` (git-ignored).

## Hot reload

Save a `.ts`/`.wasm`/`.wat` file and the server recompiles/reloads it without a
restart. Compilation runs on a background thread so it never stalls the tick.
Deleting a file unloads its script.

## Writing a script

Import the host ABI from [`soils.ts`](./soils.ts) and export any of the
lifecycle hooks (all optional):

```ts
import * as soils from "./soils";

export function on_init(): void {}                 // once, at load
export function on_tick(tick: i32, dt: f32): void {}
export function on_edit(x: i32, y: i32, z: i32, old: i32, new_: i32, by: i32): void {}
export function on_player_join(netid: i32): void {}
export function on_player_leave(netid: i32): void {}
```

See [`example.ts`](./example.ts) for a working script.

### Rules & limits

- **Scalar ABI only** (i32/f32) — no strings or arrays cross the boundary.
- **Determinism**: use `rng()` (seeded from world seed + tick), never wall-clock
  randomness — the server is authoritative and replays must match.
- **Reads are live** during a call (`get_voxel`, `entity_*`); **writes are
  buffered** and applied after the call returns, so ordering within a tick is:
  reactions (`on_edit`/join/leave) then `on_tick`, in script order.
- **Fuel budget**: each callback has a bounded instruction budget; a runaway
  loop traps and the script is disabled (its partial output for that tick is
  discarded) until you edit and reload it. Memory is capped per instance.
- `on_init` runs with no live world view — do world setup in `on_tick`.
- Script-originated edits do **not** re-trigger `on_edit` (no recursion).
