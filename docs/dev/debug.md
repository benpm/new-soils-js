# Debugging field guide

Bugs this codebase actually produced, organised by **symptom**, because that is
what you arrive with. Each entry says what it looked like, what it really was,
and how to tell the difference quickly.

Companion docs: [`architecture.md`](../architecture.md) (how it works),
[`server-tick.md`](server-tick.md) (the tick's ordering rules and off-thread
login), [`perf-report.md`](../perf-report.md) (where the time goes),
[`stdb/README.md`](../../stdb/README.md) (SpacetimeDB setup and gotchas).

---

## Triage: the first five minutes

Run these before forming a theory. Several bugs below cost hours because a
plausible theory got tested before the cheap checks.

| Check | Command | Catches |
|---|---|---|
| Client asset errors | `grep -c "Path not found" <client log>` | The single most expensive trap in this repo (below) |
| Is it *stuck* or *slow*? | `tasklist /v \| grep <exe>` twice, 20 s apart, compare CPU time | Flat CPU = a genuine block, not slow progress |
| Streaming actually finished? | HUD line `generating N (M%)` | Reading fps or "missing terrain" before 100% is meaningless |
| Real failure or the known flake? | `grep "os error 10013"` | WebTransport UDP bind; environmental, passes on rerun |
| Test hung or panicked? | Look for `has been running for over 60 seconds` | A deadlocked barrier hides the *real* assertion failure |

**Rule that would have saved the most time:** when a symptom survives a clean
rebuild of a known-good baseline, suspect the *harness*, not the code. A
baseline launched the same wrong way reproduces the same wrong result.

---

## Rendering and the client

### Terrain is an empty void, but the self-test PASSES and reports mesh slots

**This is the asset-path trap. Check it first, every time.**

Bevy resolves assets relative to the *executable* when the binary is launched
directly. Running `./target/release/soils-client.exe` finds no shaders, so
chunk meshes exist and simply never draw. The self-test still passes — it
counts mesh slots, not pixels — and the scene looks like a plausible rendering
regression.

```sh
grep -c "Path not found" client.log     # must be 0
```

Fix: run via `cargo run -p soils-client`, or set the asset root explicitly.
Note assets live under the **client crate**, not the workspace root:

```sh
BEVY_ASSET_ROOT=<repo>/crates/soils-client ./target/release/soils-client.exe
```

This is unavoidable when spawning the client as a child process (recording
tests do). Both `demo.rs` and `props_demo.rs` set it.

Why it burns time: the symptom is identical on a stashed clean baseline,
because you launch the baseline the same way. Grep the log *before* bisecting.

### Two windows of the same binary, and a capture tool grabs the same one twice

Window title carries the player name (`SOILS_NAME` → `new-soils [ana]`)
precisely so external tools can tell instances apart. Match on title, not
executable, when there is more than one.

### A camera-parking system fights another system for the transform

`mouse_look` writes rotation every frame from accumulated look state. A system
that parks the camera must be ordered `.after(player::mouse_look)` as well as
`.after(player::sync_camera)`, or it wins or loses depending on schedule order.
See `spectator_camera` in `main.rs`.

### "does not describe a valid system configuration"

Bevy's system tuples cap at **20 elements**. Split into a second
`.add_systems(Update, (...))` call. The error does not say this.

---

## Networking and replication

### A wait on entity state hangs forever, with zero CPU

**Delta snapshots omit entities that did not change.** There is no message to
wait for, so "wait until this player's position arrives" never returns for a
player standing still — and standing still is exactly what a test asks of a
player it wants to observe.

Fixes used in `tests/common/mod.rs`:

- Cache last-known positions per entity (`known`), seeded from `EntitySpawn`,
  which carries a position.
- Poll `current_self_pos` / `await_self_where` instead of blocking on the next
  snapshot mentioning you.
- Treat "no update" as "unchanged", *but only after confirming fresh snapshots
  actually arrived* — see the next entry.

### A falling player is reported as landed

"No snapshot for us" is ambiguous between *did not move* and *the reply is
still in flight*. On a 240 ms round trip, a 150 ms wait always reads as
stillness. Wait on **observed server ticks** (`await_server_ticks`), not
wall-clock, before concluding nothing changed.

### A wait for message X consumes message Y that a later wait needs

`recv_until` **discards** messages that do not match its predicate. Waiting for
an entity spawn threw away the chunk manifest that a later `await_chunk`
needed — and the server sends each chunk exactly once, so that wait hung
forever.

The fix is what a real client does: record durable facts from *every* message
as it passes (`Client::record` keeps `seen_chunks`, `seen_spawns`, `known`),
and check the recorded state before waiting.

Note this only reproduced once latency shifted message ordering. Ordering-
dependent bugs hide until something perturbs the order — which is a reason to
run tests under `SOILS_NETSIM` deliberately.

### An input edge event is silently dropped

`jump` and `toggle_fly` are **latches**: set by input gathering, consumed and
cleared by the fixed tick. A rendered frame does not always contain a fixed
tick. Assigning a whole `PlayerInput` every frame wipes a latch set moments
earlier, so the player intermittently never jumps and never leaves fly mode.

Write held state (axes, yaw) by assignment; OR edge events in. The keyboard
path does exactly this — `bot.rs::hold` copies it.

### The server never moves a player

The server steps a player **only on ticks it receives input for**. A silent
client is frozen, not falling. "Stand still" means sending idle frames.

### Rubber-banding whenever another player is nearby

Reconciliation must replay each historical tick against **that tick's** state,
not current state. Replaying the whole input ring against one current peer set
can never reproduce the server's history while a peer is moving, so prediction
stays outside `RECONCILE_EPSILON` and rewinds on every snapshot. Peers are
recorded per tick in `InputRing::history`.

Same class of bug, different flavour: it self-corrects when peers stand still,
which is exactly what a naive test does — so it passes.

---

## Determinism

### Same inputs, different outcome between runs

`Clients` is a `HashMap`, so the order messages come out of it is randomised
per process. Anything order-sensitive downstream inherits that.

Two rules that fixed peer collision:

1. **Sort cross-client work by client id** (stably, so each client's own
   messages keep arrival order).
2. **Freeze shared state for the whole tick.** The peer snapshot in
   `drain_inboxes` is taken once at the tick boundary and *not* refreshed as
   players move through the loop. Refreshing looks more accurate but makes the
   outcome depend on processing order — and it gives the client something it
   cannot reproduce, since the client knows tick-boundary positions and nothing
   about intra-tick ordering.

If `soils-sim` has a determinism test and the server layer still drifts, the
non-determinism is in the *server*, not the sim.

---

## Persistence and versioning

### Writes are silently rejected forever after a clock change

A wall-clock stamp used as a version looks monotonic and is not. The
SpacetimeDB module rejects a write whose version is below the stored one, so a
backwards step (NTP correction, VM resume) fails **every** write until the
clock catches up — and because region files are authoritative and the chunk is
no longer dirty by then, those edits are never retried. The mirror silently
holds stale data.

Use a real counter. `chunk_blob.version` is the chunk's own edit counter.

### ...and then rejected forever after an eviction

The counter that fixes the clock bug has the same shape of bug hiding in it.
It lives in memory and is reset to 0 by `World::entry`, which runs every time a
chunk is evicted and read back from its region file. Comparing versions across
two server processes therefore rejects every edit made to a reloaded chunk
until its counter climbs past its own previous high-water mark — silently,
permanently, and in the ordinary case of a long-lived world.

The guard is now scoped by `writer_epoch`, a per-process value with no ordering
to get wrong; across epochs the later writer wins, since only one server owns a
world at a time. `a_write_from_a_new_epoch_is_not_stale` covers it.

The general lesson: **a monotonic counter is only monotonic within the lifetime
of whatever holds it.** Before comparing two versions, ask what happens when
they come from different processes, and what resets the counter.

Consequence to remember: an edit counter is **deterministic across runs**, so a
test that detects "a fresh write happened" by comparing versions will see an
identical value on a repeat run. Compare `updated_at` instead.

---

## Concurrency and tests

### A test binary hangs and the real failure is invisible

A peer thread that panics never reaches its barrier, so its partner waits
forever and the harness reports a timeout instead of the assertion. **Every
barrier wait gets a deadline:**

```rust
async fn sync(b: &Barrier, what: &str) {
    tokio::time::timeout(Duration::from_secs(60), b.wait()).await
        .unwrap_or_else(|_| panic!("timed out at barrier {what:?}: a peer failed or stalled"));
}
```

### A barrier that can never be met

Joining the first thread before spawning the second. `join()` blocks, so the
second thread does not exist yet. Spawn every participant, *then* join.

### `thread '<unknown>' has overflowed its stack`

Two causes, both real here:

- **Rayon does not name its workers**, which is why the thread is unnamed.
  Configure the global pool with `.thread_name(...)` so future overflows are
  attributable.
- **Debug builds do not shrink async state machines.** A `Client` future
  nested through login/stream/settle overflows the 1 MB Windows default once
  many run at once. Peer threads use `.stack_size(8 * 1024 * 1024)`.

### An infinite loop in a message drain

Popping a non-matching message off a holding buffer and pushing it straight
back spins forever. Drain from the *source* only; the holding buffer is
append-only during a drain.

### Something slow is on the tick thread

Symptom: everyone on the server freezes together, for a fraction of a second to
a couple of seconds, correlated with someone logging in.

`drain_inboxes` runs on the tick thread. Anything it calls synchronously —
Argon2, a database round trip, a file write — stops the world. Argon2id is
*deliberately* expensive (that is what makes a stolen verifier costly to
crack), so it is the worst possible thing to run there, and a client sending
logins in a loop turns it into a denial of service needing no bandwidth.

The pattern used here: dispatch to a worker, return immediately, and let the
result re-enter through a queue that replays the original message. The success
path then stays in exactly one place instead of being duplicated for the
deferred case. Bound each connection to one outstanding check so the queue
cannot be flooded.

**How to test it, and how not to.** Total elapsed time is not discriminating:
the first version of `logins_do_not_stall_the_tick` measured how long 20 ticks
took under a flood, and passed happily with the hashing back on the tick thread
— connection setup spread the cost outside the window. Measure the **longest
gap between consecutive ticks** instead, and establish all the connections
before firing the logins so the cost lands at once. That version reports a
1.95 s stall inline and passes off-thread.

Always check a timing test by reintroducing the bug. If it still passes, it is
measuring the wrong thing.

### Tests that pass without testing anything

Four real examples, all of which looked fine:

| Test | Why it passed vacuously |
|---|---|
| presence lifecycle | Only asserted presence was *absent* after logout — true whether or not it ever existed |
| shove the pile | `await_settled` returned mid-settle, so residual motion satisfied "something moved" |
| snapshot cost | Ceiling was 32 kB against a ~620 B measurement and a 410 B budget — nothing could trip it |
| chunk mirroring | Residue from a previous run satisfied the assertion |

Ask of every assertion: *what value would make this fail?* If the answer is
"none reachable", it is decoration.

---

## SpacetimeDB

Setup, schema and gotchas live in [`stdb/README.md`](../../stdb/README.md).
Debug-specific notes:

### A table is readable by people who should not read it

`public` on a `#[table]` is a **client read permission**, not documentation.
Any connected identity may subscribe to a public table and read every row of
it. Limiting what our own SDK subscribes to (`CLIENT_SUBSCRIPTIONS`) is our
code being polite; a hostile client is under no obligation to be. `account`
held Argon2 verifiers in a public table for exactly this reason — the
subscription list looked like a boundary.

Row-level security is the natural tool and is **not usable in 2.7.1**:
`client_visibility_filter` is behind the `unstable` feature and the bindings'
own source says the filters are "currently unimplemented, and are not
enforced". A declared filter would compile, read as a security control, and do
nothing.

The available answer is `private`, with a consequence that is easy to miss:

> A private table generates **no client-side accessor at all**. Regenerate the
> bindings and `<table>_table.rs` is deleted. The database *owner* can still
> query it with `spacetime sql`, but no SDK connection — the game server's
> included — can read it.

So making a table private means moving every read of it into a reducer. For
`account` that meant hashing and verification move into the module.

Related: passing a password to a reducer is safe **only** because SpacetimeDB
2.0 stopped broadcasting reducer arguments (a reducer's outcome goes to the
calling connection alone). Under 1.0 semantics the same code would publish
every password to every client. If you ever see 1.0-era advice about reducer
callbacks, check this before trusting it.

### Reads return nothing, or every password is rejected

Reads come from the SDK's **local cache**, which is empty until the
subscription delivers its first snapshot. "Cache not warm" and "no such row"
are indistinguishable unless you track readiness — treat them differently or
you will reject every existing account on startup. `StdbLink::wait_ready`
exists for this, and startup blocks (bounded) on it.

### A player can read chat but not send it

`send_chat` finds the sender by scanning `account` for `identity == sender`, so
a client whose identity is not bound to its account gets *"no account linked to
this identity"* — while still seeing everyone else's lines, because reads come
from a subscription and need no binding. Almost always a stale link: an
anonymous client is issued a **fresh identity on every reconnect**, and the
client only sends `LinkIdentity` once per session.

`link_identity` used to refuse to rebind an account already linked to a
different identity, which turned every lobby reconnect into a permanent mute
and made a database carrying links from an earlier run reject new ones (the
old advice here was to republish with `--delete-data=always`). Rebinding is now
allowed — `require_server` is what makes it safe — and the client re-sends
`LinkIdentity` on every `Connected` event.

### Every chat line is attributed to one player

Two clients sharing one identity. Usually because a **child process inherited
`SOILS_STDB_TOKEN`** from the launching shell — `Command` inherits the
environment, so clearing it requires `.env_remove()`. Clients read
`SOILS_STDB_CLIENT_TOKEN`; the server's token is a credential in the module's
allowlist and must never reach a client.

Confirm with:

```sh
spacetimedb-cli sql --server local soils "SELECT name, identity FROM account"
```

Two accounts, one identity → this bug.

### A row vanishes while the player is still online

Anything with a liveness TTL needs refreshing, not just writing once.
`LIVENESS_TTL` is 30 s and `REAP_INTERVAL` 10 s, against a 5 s server
heartbeat. A row written at login and never refreshed disappears mid-session.

### Known exposures, so you do not rediscover them as bugs

Two tables are readable by any client and should not be:

- `player_profile` — every player's last known position.
- `chunk_blob` — the stored (edited) world.

Both must be public because the *game server* reads them through the SDK cache,
and a private table has no client accessor at all. Row-level security is the
right tool and is unimplemented in 2.7.1. This is recorded in `TODO.md`; it is
a known limitation, not an oversight to re-report.

Likewise `send_chat` trusts the caller's `world_id`, and `grant_server` is
trust-on-first-use.

### Regenerating bindings deletes files

`spacetimedb-cli generate` prompts to remove files it considers stale and
`--yes` accepts. Check `git status` afterwards. Removing a table or reducer
from the module *and* regenerating is two separate deletions — verify the
reducer you still need is still there.

### A reducer needs to answer a question

Reducers are fire-and-forget by default, but the generated bindings also emit a
`<reducer>_then` variant taking a callback that receives the module's own
`Result<(), String>`. That is a private request/response channel — correlation
is per call, so there is no need to match on arguments. `StdbLink::ask` wraps
it into a blocking call.

Blocking on it from the ECS tick thread is the trap; see the Argon2 entry under
*Concurrency*.

Other SDK facts that produce confusing failures:

- Reducers generate as **one snake_case trait each**; import individually.
- The SDK connects via `tokio::task::block_in_place`, which panics on a
  current-thread runtime → tests need `#[tokio::test(flavor = "multi_thread")]`.
- `try_identity()` is empty until the handshake completes, which is *after*
  `build()` returns. Poll; do not treat the first `None` as failure.

---

## Recording (OBS)

### `obs-cli` cannot talk to OBS

`muesli/obs-cli` is pinned to obs-websocket **protocol 4** (master still
requires `goobs v0.8.0`, default port 4444). OBS 28+ serves **protocol 5**
only, and fails the handshake:

```
error: Failed auth: Client/server version mismatch?
  "obsWebSocketVersion":"5.7.3","rpcVersion":1
```

Use `grigio/obs-cmd` (protocol 5, same command shape). `scripts/obs_record.py`
wraps it.

### A two-pane comparison silently records one pane

OBS scene items store a **relative** position alongside the absolute one, and
prefers it. Relative space spans `x ∈ [-aspect, +aspect]`, `y ∈ [-1, 1]` — so a
value copied from a 16:9 layout puts every pane in the same place on a 32:9
canvas. Compute it: `rel_x = 2 * x / canvas_h - aspect`.

The same trap in bounds form: `bounds_rel` of `2.0` on a 16:9 canvas renders at
0.5625 scale, not full width.

### OBS starts but the websocket never comes up

A force-killed OBS leaves a modal "OBS Studio Crash Detected" dialog that
blocks websocket startup, and `--disable-shutdown-check` does **not** suppress
it on OBS 32. `obs_record.py` dismisses it via UI Automation.

### The raw file is enormous

OBS writes at the canvas resolution and a quality bitrate: a 30 s two-pane
2560x720 take is ~173 MB. That is the right thing for a master. Everything
downstream — the share folder, the dashboard, a PR — wants a second pass:

```sh
ffmpeg -ss 3 -i take.mp4 -t 27 -vf scale=1400:-2   -c:v libx264 -preset slow -crf 36 -pix_fmt yuv420p -an   -movflags +faststart out.mp4      # 173 MB -> 1.7 MB
```

`-ss 3` drops the settling seconds at the head, `-an` drops a silent audio
track, and `+faststart` moves the moov atom to the front so the file plays
before it has fully downloaded. Check the result is actually smaller: a source
already encoded harder than your settings will *grow*, and you will have paid a
second generation of loss for the privilege (`deploy_dashboard.py` keeps
whichever is smaller for this reason).

### The recording opens on an empty world

Do not guess a warm-up delay. The client signals `SOILS_READY_FILE` once
streaming has settled; the recorder waits for that. Stream-in time depends on
view radius, disk cache, and whether the server is a debug build — every fixed
guess is wrong somewhere.

Related: **a debug-built server cannot generate and light chunks fast enough**
for the world to be on screen at all. Record in release.

### The video shows judder that is not in the game

If frames are captured by writing images, encoding cost perturbs the frame
clock. Muxing at an assumed constant rate then stamps fabricated judder onto
exactly the property you are trying to judge. OBS captures at a real 60 fps
without touching the render loop — prefer it. (The old frame-dump path recorded
true timestamps and muxed with per-frame durations for this reason.)

---

## Windows and toolchain

| Symptom | Cause | Fix |
|---|---|---|
| Compiler ICE, `OS error 1450` (`ERROR_NO_SYSTEM_RESOURCES`) | Full-parallelism workspace build exhausts the paged pool | `cargo build -j 3` |
| `LNK1104: cannot open file ...exe` | A previous run still holds the binary | Kill leftover `soils-*`, `demo-*`, `*_demo-*` processes |
| `os error 10013` (WSAEACCES) on a UDP bind | Ephemeral port inside a Windows reserved range | Environmental; reruns pass |
| HUD fps pinned at ~8–9 | Window presenting on a virtual display | Environmental, not a perf regression |
| Unfocused window locked to ~16 ms frames | DWM composites unfocused windows and vsyncs them | Use `SOILS_NOFOCUS=1`, and do not read perf from an unfocused window |
| `CRLF will be replaced by LF` on every commit | Working tree is CRLF | Cosmetic |

`cargo clippy --workspace` is currently **red on `master`** for two
pre-existing `approx_constant` errors in `soils-protocol/src/snapshot.rs` test
code. Not yours; do not chase it.

---

## Editing this codebase safely

Most of the self-inflicted damage this session came from *edit tooling*, not
from reasoning.

### Scripted deletions cut too much

A delete script that spans "from marker A to the next marker B" removed
`save_profile` along with the reducer it was targeting, because `save_profile`
happened to sit between them. It compiled — the reducer simply vanished, and
only a missing generated binding revealed it.

After any scripted deletion:

```sh
grep -c "pub fn " <file>          # before and after
cargo check                       # then verify the *intended* item is gone
git diff --stat                   # and nothing else is
```

### Shell mangling in patch scripts

- Backticks inside a double-quoted `python -c "..."` are command substitution;
  Rust doc comments full of `` `code` `` get silently emptied.
- Heredocs with quoted delimiters are safer, but apostrophes in prose still
  break some shells.
- `git show` output decoded as cp1252 raises `UnicodeDecodeError` on em dashes —
  capture bytes and `.decode('utf-8')`.

Write the patch to a `.py` file and run it, rather than inlining.

### Staging only part of a file

To commit your change to a file that also holds someone else's work in
progress: reconstruct the file from `HEAD` plus your section, `git add`, then
restore the working copy. Then verify:

```sh
git show :FILE | grep -c "<marker unique to their work>"   # want 0
```

A plain `git add FILE` after that reconstruction will re-stage the whole
working copy and quietly re-include their edits.

### Cargo commits everything staged

A `git add` you did earlier is still staged when you commit something else
later. Check `git status` before each commit, not just before the first.

---

## Useful environment variables

| Variable | Effect |
|---|---|
| `BEVY_ASSET_ROOT` | Asset root; needed when running the binary directly |
| `SOILS_NETSIM=lat,jitter,loss[,seed]` | Simulated bad link; seeded and reproducible |
| `SOILS_RADIUS` | View radius (2–8); small values stream in far faster |
| `SOILS_DAYTIME` | Pin time of day so a long take does not drift into night |
| `SOILS_NOFOCUS=1` | Visible window that does not steal focus |
| `SOILS_HEADLESS=1` | Unmapped window — **not** for perf numbers |
| `SOILS_PROPS=n` | Drop n rigid-body props near spawn (implies physics) |
| `SOILS_BOT=a\|b`, `SOILS_BOT_START` | Scripted client input, released by a shared file |
| `SOILS_READY_FILE` | Client writes it once the world is on screen |
| `SOILS_STDB_URI` / `_DB` / `_TOKEN` | Server-side database (opt-in) |
| `SOILS_STDB_CLIENT_TOKEN` | Client identity — never the server's token |
| `RUST_MIN_STACK` | Raises default thread stacks; useful to *confirm* a stack diagnosis |
