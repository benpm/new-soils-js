# SpacetimeDB integration

SpacetimeDB owns the **cold, relational, persistent and social** half of the
game state. Authority stays in `soils-server`: players never write voxel or
position state here, and every world-mutating reducer requires the caller to be
a registered game server.

Region files remain the **source of truth**. The database is a mirror written
*after* a successful disk write, and a SpacetimeDB failure is logged rather
than propagated — losing the database can never lose a chunk. Mirroring is
opt-in, so single-player and offline play need no database at all.

## Layout

| Path | What |
|---|---|
| `stdb/soils-module` | The WASM module: tables + reducers. Built for `wasm32-unknown-unknown`, excluded from the workspace. |
| `crates/soils-stdb` | Native client: generated bindings + `StdbLink` (worker thread + channels, shaped like the existing `NewConn` transport seam). |
| `soils_protocol::chunk_key` | Chunk-key packing **and** `world_id_for`. Lives here because the module and the server must agree byte-for-byte. |

## Why chunks are cheap to store

Terrain is bit-exact reproducible on the client from `GenParams`, so only
*edited* chunks are ever stored — pristine ones are skipped on disk and in the
database. Payloads are the shipping `chunk_codec` bytes; anything over 992 B
lands in SpacetimeDB's BLAKE3 content-addressed, refcounted blob store, so
identical chunks dedupe for free.

Clients never subscribe to the chunk tables. Only the server does, and it is
the sole writer — which sidesteps SpacetimeDB's row-granular deltas re-pushing
a whole 32 KB blob to every subscriber on a one-voxel edit.

## Local setup

The CLI is **not** vendored. Get 2.7.x either from the official installer or,
if that is unreachable, the pinned GitHub release:

```powershell
# Windows, into a gitignored .tools/
Invoke-WebRequest -Uri https://github.com/clockworklabs/SpacetimeDB/releases/download/v2.7.1/spacetime-x86_64-pc-windows-msvc.zip -OutFile $env:TEMP\stdb.zip
Expand-Archive $env:TEMP\stdb.zip -DestinationPath .tools
```

Then:

```powershell
# 1. Run a standalone host
.\.tools\spacetimedb-cli.exe start --listen-addr 127.0.0.1:3000 --data-dir .tools\stdb-data

# 2. Register it and publish the module
.\.tools\spacetimedb-cli.exe server add --url http://127.0.0.1:3000 local
.\.tools\spacetimedb-cli.exe publish --server local -p stdb\soils-module soils

# 3. Authorise your identity to act as a game server.
#    The allowlist is trust-on-first-use: while `server_identity` is empty any
#    caller may claim it, so do this before exposing the database.
.\.tools\spacetimedb-cli.exe call --server local soils grant_server <your-identity>
```

Get your identity and token with `spacetimedb-cli login show [--token]`.

## Running the server against it

Mirroring is enabled purely by environment:

```powershell
$env:SOILS_STDB_URI   = "http://127.0.0.1:3000"
$env:SOILS_STDB_DB    = "soils"        # optional, defaults to "soils"
$env:SOILS_STDB_TOKEN = "<token>"      # identity must be in server_identity
cargo run -p soils-server
```

Unset `SOILS_STDB_URI` and the server is exactly as it was: region files only.

## Regenerating bindings

The generated bindings in `crates/soils-stdb/src/module_bindings` are checked
in, so a normal build needs no CLI. After changing the module's schema:

```powershell
.\.tools\spacetimedb-cli.exe publish --server local -p stdb\soils-module soils --delete-data=always --yes
.\.tools\spacetimedb-cli.exe generate --lang rust -p stdb\soils-module -o crates\soils-stdb\src\module_bindings
```

`--delete-data` destroys stored data; omit it and SpacetimeDB will refuse
breaking schema changes instead.

## Tests

Both suites auto-skip without a host, in the same style as the `asc`-gated
scripting test:

```powershell
$env:SOILS_STDB_URI="http://127.0.0.1:3000"; $env:SOILS_STDB_TOKEN="<token>"
cargo test -p soils-stdb                          # blob round-trip, dedup, stale writes
cargo test -p soils-server --test stdb_mirror     # a real edit reaches the database
```

## Gotchas

- **The `world_id` is chosen by the server**, not auto-assigned: it is a stable
  FNV-1a hash of the world name (`world_id_for`), so chunk keys are computable
  at startup and survive restarts. A 16-bit collision is possible but is
  *detected* — `upsert_world` refuses an id already held by another name
  rather than letting two worlds share chunk storage.
- **Reducers generate as one snake_case trait each.** They must be imported
  individually for `reducers.<name>(..)` to resolve.
- **The SDK connects via `tokio::task::block_in_place`**, which panics on a
  current-thread runtime — tests need `#[tokio::test(flavor = "multi_thread")]`.
- **`try_identity()` is empty until the handshake completes**, which is after
  `build()` returns. Poll for it rather than treating the first `None` as a
  failure.
- **`wasm-opt` is optional** but the CLI will nag; installing binaryen shrinks
  and speeds up the published module.
