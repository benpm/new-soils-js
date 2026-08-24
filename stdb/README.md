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

## What is stored

| Table | Keyed by | Written by | Readable by |
|---|---|---|---|
| `world` | server-chosen `world_id` (`world_id_for(name)`) | server, on world open | everyone |
| `chunk_blob` | packed `chunk_key` | server, after a successful region write | everyone |
| `player_profile` | **account name** | server, on logout | everyone |
| `presence` | **account name** | server, on login, every 5 s, and on logout | everyone |
| `game_server` | `server_id` (hash of name+bind) | server, every 5 s | everyone |
| `chat_message` | auto id | players | everyone |
| `account` | account name | server, via reducers | **nobody** — private |
| `server_identity` | `Identity` | `grant_server` | **nobody** — private |

### `public` is a permission, not a label

SpacetimeDB has exactly two visibilities. `public` means *any* connected
identity may subscribe to that table and read every row — restricting what our
own client subscribes to (`CLIENT_SUBSCRIPTIONS`) is politeness, and a hostile
client is under no obligation to be polite.

So `account` is **private**, and password verifiers never leave the database.
That has a consequence worth stating plainly: a private table generates no
client-side accessor at all, so the *game server* cannot read it either.
Verification therefore happens where the row lives — the `verify_login`,
`register_account` and `set_password` reducers do the Argon2id work inside the
module, and only a yes/no comes back.

Sending a password to a reducer is safe here specifically because SpacetimeDB
2.0 stopped broadcasting reducer arguments: an outcome is delivered only to the
connection that called it. Under 1.0 semantics this design would have published
every password to every client.

Row-level security would be the natural tool for the tables that must stay
readable by servers but not players (`player_profile` carries every player's
last known position). As of 2.7.1 it is not available: `client_visibility_filter`
is gated behind the `unstable` feature and the bindings' own source says the
filters are "currently unimplemented, and are not enforced". Declaring one would
look like a boundary while being decoration, so this is left as a known
limitation rather than a fake mitigation.

`player_profile` and `presence` are keyed by *account*, not `Identity`: players
authenticate to the game server with a name/password, so the account is what
durably identifies them. `player_profile.identity` is filled in by the
`link_identity` reducer, which is **server-only**: the module has no way to
tell whether a caller owns an account, because ownership is proved by a
password to the *game server*, which is the party that then vouches for it. An earlier version let the claiming client call it and checked only
that any *existing* link matched the sender — vacuous for an unlinked account,
which is every account's normal state, so any client could claim any account by
name.

Presence is refreshed on the server's 5 s heartbeat, not just written at login.
The reaper deletes any presence row older than `LIVENESS_TTL` (30 s), so a
login-only row would vanish while the player was still connected.
`heartbeat_presence` refreshes the whole roster in one transaction per world and
prunes rows for anyone no longer on that server, which also covers unclean
disconnects sooner than the reaper would.

## What is read back

The server subscribes to `player_profile` and reads it from the SDK's local
cache, which makes a lookup synchronous — the login path runs inside the tick
and cannot wait on a round trip. Startup blocks (bounded, non-fatal) for the
first snapshot: a login racing it would fall back to the world spawn point and
quietly lose that player's saved position.

Nothing else is subscribed. Chunks are served from region files, which stay
authoritative, so subscribing to `chunk_blob` would stream the whole stored
world into memory for no benefit.

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
- **`chunk_blob.version` is the chunk's edit counter, not a clock.** The module
  rejects a write whose version is below the stored one, so a wall-clock stamp
  would fail every write after a backwards clock step (NTP, VM resume) — and
  since the region file is authoritative and the chunk is no longer dirty by
  then, those edits would never be retried.

## Client credentials

A client connects with its **own** identity, not the server's. `soils-client`
reads `SOILS_STDB_CLIENT_TOKEN` — deliberately a different variable from the
server's `SOILS_STDB_TOKEN`, which is a credential in the module's allowlist:
handing it to a client would let that client call every server-only reducer.

Unset means an anonymous identity, which is the right default — a player is
identified by their game account, and `link_identity` binds the two after the
server has checked the password.

Two clients sharing one identity is a real failure mode and looks confusing
rather than broken: both accounts link to the same identity, `send_chat`
attributes every line to whichever linked last, and the per-account chat
cooldown starts throttling both. Child processes inherit the environment, so a
launcher that exports `SOILS_STDB_TOKEN` has to clear it explicitly.

Note also that `link_identity` refuses to rebind an account already linked to a
*different* identity. That is deliberate, but it means a database carrying links
from an earlier run will reject new ones — republish with `--delete-data` when
starting over.

## Not wired yet

Tracked in `TODO.md`. Built on both sides and currently unreachable:

Nothing, as of the client layer landing. `account`, `chat_message` and
`game_server` all have consumers now; see `TODO.md` for what is left.
