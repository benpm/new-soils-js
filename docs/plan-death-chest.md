# Death chests, and filming them

**Status:** plan only, nothing built. Written 2026-08-28 on `ui-inventory`,
immediately after containers landed (`docs/plan-storage.md`).

---

## 0. The thing this plan has to say first

**There is no health, no damage, and no death in this codebase.** Not partially —
at all. `grep -rn "health\|damage\|respawn"` across `soils-sim`, `soils-server`
and `soils-protocol` returns nothing but the word "bodies" in physics comments.
Players cannot be hurt, cannot die, and have no state that would let them.

So "a video test of a player dying and dropping their death chest" is not a test
to be written against existing behaviour. It is three pieces of work, and the
video is the last and smallest of them:

1. a minimal, honest death model (§1),
2. the grave itself, which is mostly already built (§2),
3. the recording (§3–§5).

Doing 3 without 1 and 2 would mean filming a scripted lie. This plan does them
in order and keeps the video subordinate to headless assertions throughout: the
recording is the *artifact*, never the oracle. A video test that fails by
producing a wrong-looking file nobody watches is not a test.

---

## 1. The smallest death that is not a toy

The temptation is to add a `/kill` command, film it, and call the feature done.
That produces a demo, not a system — and it would need ripping out the moment
anything real damages a player. The smallest thing that survives contact:

### 1.1 Health lives in `soils-sim`, beside `PlayerState`

```rust
// soils-sim
pub const MAX_HEALTH: u16 = 100;

/// Server-authoritative. On the wire only as a `HealthUpdate` push, for the
/// same reason the inventory is: it changes at human speed, and a lost delta
/// would leave the HUD lying about whether you are about to die.
pub struct Health { pub current: u16 }

/// Fall damage from an impact speed, in the shared crate so the client can
/// *predict* the number it is about to be told — but never apply it.
pub fn fall_damage(impact_speed: f32) -> u16;
```

Fall damage first because it needs no new content, no combat, no mobs, and no
balance conversation: the world already has gravity, a `grounded` transition,
and a `vel.y` the server already integrates. It is the one damage source that is
already fully simulated on the authoritative side.

Threshold and curve are a tuning constant, not a design question — start at "no
damage below 4 blocks, then quadratic", and put it in one `const` so it is
cheap to argue about later.

### 1.2 Death is a server event, not a client one

In `app.rs`, where the server already steps players:

* `apply_fall_damage` — on a grounded transition with a large enough downward
  velocity, subtract. Runs in the existing `FixedUpdate` chain, after `Inputs`.
* `handle_deaths` — at zero, in one place:
  1. take the whole `Inventory` out of the `Client`,
  2. place the grave block (§2),
  3. teleport to spawn, restore full health,
  4. push `InventoryUpdate` (now empty) and a new `ServerMsg::Died { grave }`.

One system, one ordering, no partial states. A player is never "dead" as a
condition — there is no dead-and-waiting mode to design, no respawn screen, no
question about what happens if they disconnect while dead. They are alive at
spawn with nothing on them, and their things are in a box where they fell. That
is the whole model, and it is the one that needs the least new UI.

`ServerMsg::Died { grave: [i32; 3] }` exists so the client can say *where* —
"you died at (x, y, z)", the single most useful sentence a death screen can
produce. Protocol bumps to v6.

### 1.3 What this deliberately does not include

No combat, no mobs that hurt, no drowning, no starvation, no armour, no
durability, no XP. Each is a separate design conversation and none is needed to
make a grave. `Tasks.md` already tracks durability and drop tables.

---

## 2. The grave, which containers already built

A death chest is a container block that nobody placed. Almost all of it exists:

```yaml
Grave:
  faces: [26, 25, 26]     # needs atlas tiles; see the risk in §6
  category: crafted
  weight: 0.0
  container: 36           # exactly Inventory::DEFAULT_SLOTS
```

Then in `handle_deaths`:

```rust
let inv = std::mem::take(&mut c.inventory);
let at = grave_site(world, feet_of(player));
world.edit(at.x, at.y, at.z, grave_id);
*world.container_mut(at, 36) = inv;       // one move, no per-slot loop
```

Everything after that is already written and already tested: the contents go
through the same `Store<ChunkData>` page, persist to the same `b_*.bin` file,
survive eviction and restart, open with a right-click, and spill if the block is
broken. The container tests in `tests/containers.rs` cover the machinery; the
new tests only have to cover the *transition*.

### 2.1 `grave_site` — the part that is actually hard

A grave placed naively is a grave you cannot reach. It must land somewhere the
player can stand next to and open:

* Start at the feet voxel. If it is air, take it.
* Otherwise search outward — up first, then the six neighbours, then a small
  spiral — for the nearest air voxel with a solid block under it.
* Cap the search. If nothing qualifies within a few blocks (died inside solid
  rock, died over a void), **fall back to spilling the inventory as dropped
  items**, exactly as breaking a chest does. That path is already written and
  already tested, and "your things are on the floor" is a far better failure
  than "your things are inside a wall".

Never overwrite a non-air block. A grave that eats someone's build to store
someone else's loot is a grief vector, and it is the kind of thing that is
obvious in hindsight and invisible in a demo.

### 2.2 Two deaths in one place

The second grave finds the first voxel occupied, so `grave_site`'s search moves
it. Nothing special is needed — but there must be a test, because "the second
death overwrote the first grave" is exactly the bug this design's search step
exists to prevent, and it will never show up in a hand-played demo.

### 2.3 Ownership

None, for now: any player can open any grave. Ownership is a real design
question (timers? locks? tombstones that only the owner sees?) and answering it
badly is worse than leaving it open on a single-player-shaped game. Recorded as
a follow-up rather than guessed at.

---

## 3. Headless tests first — `crates/soils-server/tests/death.rs`

These are the actual tests. They run in CI, take seconds, and fail with a
sentence rather than a file to squint at. Same shape as `containers.rs`.

| Test | What it pins |
|---|---|
| `a_long_fall_costs_health_and_a_short_one_does_not` | the damage curve exists and has a floor |
| `dying_empties_the_pack_and_fills_a_grave` | the transition, both halves, in one assertion |
| `a_grave_holds_exactly_what_the_player_was_carrying` | conservation — every stack, no more, no fewer |
| `a_grave_is_placed_somewhere_reachable` | the block is air-replacing and stands on something |
| `a_grave_never_overwrites_an_existing_block` | the grief vector |
| `a_second_death_does_not_disturb_the_first_grave` | §2.2 |
| `dying_with_nowhere_to_put_a_grave_spills_instead` | the fallback, which is the interesting failure |
| `a_grave_can_be_looted_by_anyone_and_emptied` | reuses the container path end to end |
| `a_grave_survives_a_server_restart` | it is block data, so it should — and this proves it |

`a_grave_holds_exactly_what_the_player_was_carrying` is the load-bearing one.
Death is the single largest item transfer in the game, so it is where a
conservation bug would be worth the most.

**Getting a test player to die** without a debug backdoor: fly up, stop flying,
fall. The harness already has `toggle_fly`, `land`, and `settle`, and the
existing `work_site` pattern builds a floor to land on. That exercises the real
damage path rather than a test-only shortcut — the same reason the recording bot
drives real `ButtonInput` rather than a private edit path.

---

## 4. The recording — following `inventory_demo.rs`

The repo already has this pattern and it works. `crates/soils-server/tests/inventory_demo.rs`:

* is `#[ignore]`d, so it never runs in an ordinary `cargo test`,
* launches a real GPU client with `SOILS_BOT=inv`,
* drives OBS Studio through `scripts/obs_record.py`,
* and — the part that makes it a *test* — asserts every beat of the bot's script
  fired (`EXPECTED_BEATS`), by reading the client's log. A take where the bot
  stalled fails instead of being published.

So `tests/death_demo.rs` is that file with a different script, and the design
work is entirely in the script.

### 4.1 `DEATH_BEATS` — the script

New `Role::Death` in `bot.rs`, beside `Role::Inventory`, with beats as data for
the reason the existing comment gives: *the timing is the design*. Each beat has
to leave its result on screen long enough to read.

| t (s) | Beat | What the frame shows |
|---|---|---|
| 0.5 | `OpenScreen` | a full pack — establishes the stakes before anything is lost |
| 3.0 | `CloseScreen` | |
| 4.0 | `FlyUp` | rising, ground receding |
| 9.0 | `StopFlying` | the fall — the one beat that must not be cut short |
| 11.5 | *(landing)* | health drains to zero on the HUD |
| 12.0 | *(server)* | teleport to spawn, pack empty |
| 13.0 | `OpenScreen` | **an empty inventory** — the payoff shot |
| 16.0 | `CloseScreen` | |
| 17.0 | `WalkToGrave` | approach, driven from `ServerMsg::Died`'s coordinates |
| 22.0 | `OpenGrave` | right-click: the container panel, full |
| 24.0 | `TakeAll` | shift-click every cell, pack refills |
| 28.0 | `CloseScreen` | |

Twelve beats, ~30 s, which is the same take length `inventory_demo` already
uses. `WalkToGrave` is the one beat that cannot be pure timing — it needs the
grave's actual position, which is why §1.2 puts it on the wire.

### 4.2 The HUD has to show health

A video of a player dying is unreadable if nothing on screen goes down. A health
readout is small (one bar in `hud.rs`, driven by a `Health` resource fed by
`HealthUpdate`) but it is genuinely required by this plan, not a nice-to-have —
without it the most important eleven seconds of the take are a player falling
and then, for no visible reason, standing somewhere else.

---

## 5. A GIF path that does not need OBS

`inventory_demo` needs OBS Studio and a real GPU, which is why it is `#[ignore]`d
and run by hand. Worth adding alongside — not instead of — a cheap, hermetic
capture that CI could actually run:

* `SOILS_FILM=<path>` `SOILS_FILM_FPS=<n>` `SOILS_FILM_SECS=<n>` in `main.rs`,
  beside the existing `SOILS_SELFTEST` screenshot logic. The single-shot path
  already exists (`screenshot_once`, `Screenshot::primary_window`); this is that
  on a timer, into a frame buffer.
* Encode with the pure-Rust `gif` crate on exit. No ffmpeg, no OBS, no window
  manager assumptions, deterministic output.
* Downscale to 480p and 10 fps. A 30 s take is then a few MB — small enough to
  attach to a PR, which is the actual point.

This makes the recording reproducible by anyone with a checkout, and it makes
the artifact something a reviewer sees rather than something a maintainer
produces on request. The OBS path stays for full-quality captures.

**Both remain artifacts, not oracles.** The `#[ignore]` stays. The assertions
that gate a merge are the ones in §3.

---

## 6. Risks, in the order they will bite

1. **Atlas tiles.** `blocks.png` is a fixed 8x8 grid of 16x16 tiles and it is
   already well used. A Grave needs a distinct top and side, and if there are no
   free tiles this becomes an art task blocking a systems task. **Check the free
   tile count before starting.** Fallback: reuse Wooden Crate's tiles for the
   first cut and file the art separately — the whole feature works with a crate
   that happens to appear when you die.
2. **`grave_site` in the dark.** Dying underground, in water (when there is
   water), or on a slope. The spill fallback covers it, but the search must be
   *cheap* — it runs on the tick, inside `handle_deaths`. Cap the radius, do not
   pathfind.
3. **Fall damage vs. the existing tests.** Several movement and prediction tests
   fly players around and drop them (`prediction.rs`, `movement_perf.rs`,
   `robustness.rs`). If falling starts costing health, some of them may start
   killing their subjects mid-measurement — which would look like a movement
   regression. Audit before landing; the mitigation is a `ServerConfig` flag
   that leaves damage off by default in tests.
4. **The empty-inventory frame is the whole video.** If the hotbar's
   self-healing rebind (which fills empty keys from the pack) leaves the bar
   looking populated after death, the payoff shot reads as "nothing happened".
   It should be fine — `reconcile_hotbar` can only bind kinds the pack holds,
   and the pack holds nothing — but check it on the first take, because it is
   exactly the kind of thing that is obvious in motion and invisible in a unit
   test.
5. **Scope.** §1 is a health system. It is the smallest one that is not a toy,
   but it is still the first time this project has had one, and everything in
   §2–§5 is blocked behind it. If the goal is only the recording, say so and the
   plan shrinks to a debug `/kill` — but then it is a demo, and this document
   should be re-read before anyone calls it a test.

---

## 7. Order of work

1. Check free atlas tiles (risk 1) and audit the fall-heavy tests (risk 3).
   Both are cheap and both can invalidate the plan's shape.
2. `Health` + `fall_damage` in `soils-sim`, with unit tests on the curve.
3. `apply_fall_damage` + `handle_deaths` + `grave_site` in `app.rs`;
   `ServerMsg::Died`, protocol v6.
4. `tests/death.rs` — all nine (§3). **This is the deliverable.**
5. Health bar in `hud.rs`; `Died` handling in `server_msg.rs`.
6. `Role::Death` + `DEATH_BEATS` in `bot.rs`.
7. `tests/death_demo.rs` (OBS) and `SOILS_FILM` (GIF).
