# The server tick

Everything the authoritative server does to a world happens inside one Bevy
schedule run at `TICK_HZ` (64 Hz, 15.6 ms). This note covers the parts of
`crates/soils-server/src/app.rs` whose *reasons* do not fit in the code:
the ordering rules that make a tick reproducible, and why login is the one
thing that is allowed to leave the tick thread.

## The four phases of `drain_inboxes`

| Phase | What it does |
|---|---|
| 0 | Collects logins that finished on a worker thread |
| 1 | Refills the per-client rate buckets and empties every inbox |
| 2 | Applies messages, in a deterministic order |
| 3 | Reaps disconnects, after their final messages have been applied |

Phase 3 runs last on purpose: a client that sends an edit and immediately
closes the socket should still have that edit applied.

## Determinism rules

Two servers fed identical input must reach identical state — otherwise the
replay tests are checking nothing, and a desync can't be reproduced from a
recording. Two rules buy that, and both look like pointless caution until
they are removed.

### Messages are sorted by client id

`Clients` is a `HashMap`, so the order clients come out of the phase-1 loop is
randomized per process by the hash seed. Phase 2 therefore sorts by client id
before applying anything. The sort must stay **stable** (`sort_by_key` is), so
that each client's own messages keep their arrival order — per-client FIFO is
the one ordering guarantee the transport gives us, and it is the one clients
rely on when they send an input and an edit in the same frame.

### Peer positions are frozen at the tick boundary

Player-vs-player collision resolves against `peer_snapshot`, a copy of every
player's position taken *before* phase 2 begins and not refreshed as players
move through the loop.

Refreshing it looks more accurate and is worse in two ways:

* **It makes the outcome depend on message order.** Two players walking
  head-on: whichever is stepped second collides against the other's
  already-updated position and stops further back. The result then depends on
  which order the messages happened to arrive in, which is exactly what the
  sort above exists to stop mattering.
* **The client cannot reproduce it.** A predicting client knows every peer's
  tick-boundary position — that is what a snapshot *is* — and knows nothing
  about the server's intra-tick ordering. Freezing gives it something it can
  actually recompute, so prediction and authority agree.

## Login runs off the tick thread

Argon2id at its default parameters is deliberately expensive — roughly
100–200 ms, or ten ticks. Verifying a password inline stalls the whole world
for every player, and a flood of failed logins is then a trivial denial of
service.

So the first `Login` message only *starts* the check:

```text
Login ─▶ AuthPool (bounded queue, N workers) ─▶ (verify, ~200 ms) ─┐
                                                                    ▼
        phase 2 (join path) ◀── replayed Login ◀── phase 0 ◀── AuthQueue
```

Workers come from a fixed pool (`AUTH_WORKERS`), not one thread per login.
Argon2id costs ~19 MB per hash, so "a thread per pending login" would hand
anyone who can open sockets an unbounded memory and CPU amplifier — the tick
would survive, and the machine would not.

`AUTH_BACKLOG` is the queue in front of that pool, and is deliberately
generous. It is *not* a second security control: a queued request is a name
and a password, three orders of magnitude cheaper than a hash. Sizing it
tightly only refuses honest bursts — a hundred players joining at once is a
normal Saturday, and the first version of this cap failed exactly that test.

The worker pushes an `AuthDone` onto the shared `AuthQueue`; phase 0 drains it
and, on success, pushes a synthetic `Login` back into the same message list
with `auth_verified` already set. The join path — spawning the entity, seeding
the world, sending the first manifest — is long, and this keeps exactly one
copy of it rather than one for the fast path and one for the slow.

Two failure modes are load-bearing and easy to reintroduce:

* `auth_inflight` must be left clear on every path that does *not* queue a
  check. Setting it and then failing to submit means that connection can never
  log in again, because every subsequent `Login` sees a check already in
  flight.
* A database that times out is **not** a rejection. Only "no such account" and
  "wrong password" are; anything else falls back to the local account file, so
  an unreachable SpacetimeDB costs the lobby and not the game. See
  [`debug.md`](debug.md#spacetimedb).

### Testing it

A timing test that measures *total* elapsed time over N ticks does not detect
this bug — chunk streaming dominates, and connection setup spreads the cost
outside the window. The test has to measure the **longest gap between
consecutive ticks**, with every connection established before any password is
sent. `logins_do_not_stall_the_tick` in
`crates/soils-server/tests/stdb_auth.rs` does that; with verification inlined
it reports a stall of about 2 s against a 250 ms budget.
`crates/soils-server/tests/auth_flood.rs` makes the same measurement without a
database, and adds the other half: every client in a 96-login flood must get an
answer rather than be turned away or left hanging.

The concurrency cap itself cannot be seen from outside the process, so it is
asserted where it lives: `the_auth_pool_bounds_concurrent_hashing` in `app.rs`
tracks the peak number of simultaneous hashes and fails if it exceeds
`AUTH_WORKERS`, alongside a check that `AUTH_WORKERS` itself stays inside a
memory budget — the peak assertion alone would be satisfied by raising the
constant.

Any timing test here should be checked by reintroducing the bug it guards
against — two earlier versions of this one passed with the bug present.

## Demo fixtures

`spawn_physics_demo` builds the prop pile used by the physics load tests and
the recorded demos. Its layout is chosen against two failure modes:

* **A perfectly aligned grid barely interacts.** It drops straight into a
  stable lattice, so a load test of settling bodies measures almost nothing.
  Each body gets a small deterministic jitter (from
  [`soils_protocol::mix`](../../crates/soils-protocol/src/rng.rs), so the pile
  is identical every run) and a spin, which also makes replicated orientation
  non-trivial.
* **A cubic pile is a wall.** A few hundred bodies stacked cubically is taller
  than a player, so anyone who walks in is simply buried. Three layers, spread
  wide, keeps it waist-high and walkable.

Spacing is 1.35 against a 1.0 cube, so nothing starts interpenetrating —
bodies that begin overlapping get launched apart by the solver and the
recording opens with an explosion.
