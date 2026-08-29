# Changelog
****
## Five fixes from the PR #8 review

**Branch `ui-inventory`, 2026-08-29.** An independent review of the container
and hotbar work (gemini CLI over the diff, every finding then verified against
the code). Nine of its eighteen findings did not survive that check; five that
did are fixed here, and one is recorded in `Tasks.md` because it is a design
decision rather than a bug.

### Warping with a chest open unpinned the wrong world

`ClientMsg::Warp` replaced `c.world`, drained the subscription and re-spawned
the player, but never closed an open container. Two consequences, neither
visible from a client, which is how it survived being written:

* The old world's block-data page stayed pinned forever, and pinned pages are
  never evicted.
* On the next tick `close_unreachable_containers` resolved the *new* world and
  unpinned there — so if another player had that same chunk's page open in the
  destination world, their pin was decremented and their live chest's page
  became evictable underneath them.

One line, before `c.world` is replaced.

### Building over a chest orphaned its contents

Nothing on the server requires the target voxel to be air before a place, so a
client can send one straight onto a container block. The spill was keyed on
"this was a break" rather than "the old block is gone", so the page entry
stayed behind with no container in front of it — invisible, unreachable, and
inherited by whatever was built on that voxel next.

`if !placing` became `if old != 0`. That whether placement should require air
at all is a separate question — a gameplay decision about which blocks are
replaceable in place, not just a missing check — is now in `Tasks.md`.

### A page whose write was still in flight could be evicted and re-read stale

`take_dirty` clears `dirty` and hands the bytes to a channel, so eviction saw
a clean page and dropped it *and* its header memo while the write was still
queued. The next `get` re-read the pre-write file and was now resident, stale
and clean — and the next mutation wrote that stale version back over the good
one. Pages now carry `in_flight`, set at `take_dirty` and cleared at the next
one (a whole flush interval later, and the writer is one thread draining an
mpsc in order), and neither `tick_lifecycle` nor `evict` will drop one.

Found alongside it: `idle_since` was set at fault-in and never moved, so the
TTL measured *age*, not idleness — a page read every tick was evicted exactly
`ttl` after it loaded. `get`/`get_mut` now restart the timer.

### Inventory and container messages spent no rate tokens

`Edit` and `Inputs` were bucketed; the five inventory messages were not, and
`drain_inboxes` pulls the inbox in an unbounded loop. `TransferItem` in
particular does a fresh `container_view` allocation and sends a
`ContainerUpdate` to *every* viewer, immediately — so shuffling one item back
and forth forced unbounded per-tick work and outbound traffic to everyone else
in the chest. New `UI_RATE` bucket at 64/s, spent by `MoveItem`, `DropItem`,
`OpenContainer`, `CloseContainer` and `TransferItem`.

### Two smaller ones

`paged::read_block` allocated `vec![0u8; len]` straight from a `u32` read off
disk, so a truncated region file could ask for 4 GB before the `read_exact`
that would have failed anyway. It now bounds the length against the file
first; `compact` already checked the same field.

And the put-back after a refused transfer was guarded by `debug_assert!`,
which compiles out — so a failure would have silently voided items rather than
panicking. It now spills the remainder into the world and says so. Currently
unreachable (it needs a durability item with `max_stack > 1`, and there is
none), which is exactly why it was worth fixing before the first one exists.

****
## Items go in chests, and one cache serves every world structure

**Branch `ui-inventory`, 2026-08-28.** Design and reasoning in
[plan-storage.md](docs/plan-storage.md). Protocol bumps to **v5**.

### The cache was already written; it was just welded to voxels

The chunk pipeline already did all of this for terrain: a pointer table at the
head of each region file, a memoised header so a probe is a map lookup, the
block inflated into memory on a miss, mutated in RAM, marked dirty, and written
out by a background thread on a flush interval, on eviction, or at shutdown.
None of that is voxel-shaped, so it came out into two modules that know nothing
about the game:

* **`paged.rs`** — the file format. Slotted, append-and-repoint, zlib blocks,
  compaction with a caller-supplied `keep` predicate (chunks prune pristine
  terrain they can regenerate; block data keeps everything, because nothing
  regenerates a chest).
* **`store.rs`** — `Store<V>`: fault in, dirty, pin, flush, evict, with
  counters that reach the flush-interval log.

`region.rs` is now the chunk-shaped layer on top. Its six tests were not touched
by the extraction, which is the evidence that the format did not move.

### Blocks can hold things

`soils_sim::block_data` adds `BlockData::Container(Inventory)` and `ChunkData`,
keyed by position within a chunk — no id to allocate, no free list, and no way
for the side table to disagree with the world about which block it describes.
One page per chunk, at the *same slot index* as that chunk's voxels, so
`r_0_0_0.bin` slot 37 and `b_0_0_0.bin` slot 37 are one chunk seen two ways.
Evicting a chunk evicts its page; an open container pins one.

Which blocks hold things is data: `container: 27` in `blocks.yaml`. Wooden Crate
holds 27, Clay Pot 9. Presence of the key is what makes a block openable — there
is no separate boolean to disagree with it.

### Containers on the wire

`OpenContainer` / `CloseContainer` / `TransferItem` up, `ContainerUpdate` /
`ContainerClosed` down.

A transfer names **what to move, not where to put it**: the destination is
whichever side the source is not. Two players may hold one chest open, so
neither can be right about which slot the next stack lands in — and the client
therefore never needs to model the server's stacking rules. Opening is not a
lock; every viewer sees every change, whole-state, for the reason
`InventoryUpdate` is whole-state and more so.

The server decides the panel is open. The client asks; only `ContainerUpdate`
says it happened. Reach is re-checked on **every transfer**, not just on open —
otherwise a chest stays lootable for as long as the client keeps quiet about
walking away. `soils_sim::within_reach` came out of `validate_edit` so placing
and opening cannot drift apart.

Breaking a container spills everything it held as world items and closes it for
every viewer. That is the correctness cliff: without it, breaking a chest either
voids its contents or leaves orphan data for the next block built on that voxel
to inherit.

### In the client

Right-click a container block to open it (shift overrides, so a chest is still
something you can build on). The panel sits above the pack in the inventory
window; right-click moves one item across, shift-click moves the stack, and the
footer hint swaps to say so. Nothing is applied optimistically — with two
players in one chest a client that guessed would be wrong half the time and
quietly duplicate items the rest.

`route_server_messages` was at Bevy's sixteen-parameter ceiling, so its message
writers are now one `SystemParam` bundle. The next message the protocol grows
would not have fitted.

### Tested

40 new tests. The load-bearing ones are conservation
(`a_transfer_never_creates_or_destroys_an_item`), the spill
(`breaking_a_full_crate_spills_everything_and_closes_it`), the shared case
(`both_viewers_of_one_crate_see_every_change`), and the storage claim end to end
(`crate_contents_survive_a_server_restart`). Plus, in the store itself: a dirty
page is written out by eviction rather than lost, a pinned page is not evicted,
an emptied page clears its slot rather than storing a row of nothings, the
pointer table is read once per region and not once per page, and an undecodable
payload reads as empty and counts rather than panicking.

### Also written

[plan-death-chest.md](docs/plan-death-chest.md) — death chests and automated
video tests of them, planned but not built. It opens by recording that this
codebase has no health, damage or death of any kind, so the feature is a small
health system first and a recording last.

****
## The hotbar holds references, and the inventory groups by category

**Branch `ui-inventory`, 2026-08-28.** Worked from the design mockup in
`scratch/`. Reverses the "No hotbar, just a ring" line in `TODO.md` — the
reasoning is in [plan-ui.md §10](docs/plan-ui.md#10-phase-6--the-hotbar-2026-08-28).

### The bar holds references, not items

A hotbar slot holds an `ItemKind`, never an `ItemStack`. Putting Cobblestone on
key 1 moves nothing, sends nothing, and leaves the stack where the server put
it; the inventory screen still lists it, dimmed, wearing a badge with the key's
number. So there is no protocol change and no second authority to keep in sync
— `PROTOCOL_VERSION` stays at 4 and `Hotbar` is a client resource.

Binding by *kind* rather than by slot index is what makes that work. Slot
indices are not stable: the server merges, splits and relocates stacks, and one
kind routinely spans several slots — the starter kit is 128 apiece against a 64
stack cap. It also retires the old selection, an index into the derived
`placeable()` list that went stale every time a stack was spent and carried a
clamp and a dedicated test to survive it. `edit.rs` and the F3 overlay now read
`Hotbar::selected_block()`, which cannot.

### A key heals itself

When the item a key points at runs out, the key rebinds to another item of the
same `ItemClass` — category, function *and* effect. Spend the last Cobblestone
and it holds Slate; eat the last Large Fruit and it holds another healing
consumable. Never across a class: with the whole stone category gone, a key that
wanted stone goes empty rather than take the Leaves sitting unassigned beside
it. Nor does it steal a replacement already sitting on another key — that would
just move the dead key one along. An empty key wiggles when pressed, so a dead
key is visible rather than an input that appears to have been swallowed.

`reconcile` is two passes, not one: every slot must let go of what it lost
before any slot refills, or the first key takes a replacement the second had a
better claim to. Candidates come from `PlayerInventory::kinds()`, in
inventory-slot order, so the choice is deterministic and testable.

A key that has never held anything takes whatever is going — otherwise a new
player faces eight blanks over a pack the server has just filled with nine kinds
of block. A key that *has* held something only ever takes a like replacement.

### `ItemClass`, and blocks that know what they are

`ItemClass { category, function, effect }` in `soils-protocol` beside
`ItemKind`. Blocks carry theirs in `blocks.yaml` alongside a weight, parsed as
optional keys the way `emission` already was; `crates/soils-sim/items.yaml` and
`ItemRegistry` cover the tool/weapon/consumable ids, following the
`entities.yaml` pattern. All three of its lists ship empty — nothing in the game
is a tool yet — so the registry exists to let the substitution rule be written
against real data rather than a special case, and authoring the first fruit is a
YAML edit. Closed enums, not strings: a typo in `blocks.yaml` is a startup parse
error instead of an item that can never substitute for anything and no test
would notice.

The block categories were chosen so the rule is demonstrable with the default
starter kit and no new content: Stone x3, Earth x2, Crafted x2, Organic x2.

### The screen groups by category

One cell per item *kind* — 128 Cobblestone reads as one entry of 128, not two of
64 — under a row per category, empty categories omitted, with a detail panel and
a load readout. Slot positions were the only reason the flat grid existed and
are not something the player can see or act on, so rearranging leaves the UI:
`MoveItem` stays in the protocol and the server, where
`soils-server/tests/inventory.rs` still covers it, but the client no longer
sends it. The ring and the one-slot held-item indicator are gone; the bar shows
any kind and its live key *is* the held-item indicator.

`crates/soils-client/src/theme.rs` holds the mockup's palette and metrics — the
client had none, and `pause.rs` and `login.rs` had already drifted apart on
near-identical `PANEL_BG` constants. It covers the hotbar and the screen only.

Two of the mockup's touches did not port: `clip-path` chamfers and the scanline
overlay have no `bevy_ui` equivalent, and emoji category icons are impossible
for the reason already on record — the bundled font is a FiraMono subset — so
each category borrows the atlas tile of a representative block, derived from the
registry rather than hard-coded. The dimming is a compromise worth naming:
`blocks.png` tiles are fully opaque squares, so a true silhouette would turn
every assigned block into the same dark square, and the badge is what actually
says which key holds it.

### Tests

40 new, all headless. The load-bearing ones are the negative cases: binding an
item leaves `PlayerInventory` byte-identical and sends nothing (a test that only
checked the hotbar would pass while the item was quietly moved); a replacement
is never taken from another class, and never stolen from another key; an item a
key points at is still listed on the screen; every kind held appears there
exactly once — the categorized analogue of the item-void bug, where an item
belonging to no row vanishes from view while still in the pack. The hotbar's
slot entities persist and only their children are rebuilt, so an inventory
change mid-wiggle cannot despawn the animating slot; that has its own test.

## Draw distance: continental terrain, occlusion culling, and look/light fixes

**Branch `ui-inventory`, 2026-08-26.**

### Mouse-look was broken on absolute-mode pointers

The camera pinned itself to the ground and spun wildly on any mouse movement.
Not the look math — the input. winit's Windows backend gates raw mouse motion
on `MOUSE_MOVE_RELATIVE`, which is **zero**, so its `has_flag` test always
passes and a device reporting in *absolute* mode has its 0..65535 screen
coordinate delivered as if it were a delta. Reproduced live by injecting one
absolute report: yaw swung from -0.22 to -66.22 and pitch pinned to its clamp.
Wacom's driver is the local culprit; VM and remote-desktop pointers do the same.

`mouse_look` now reads individual `MouseMotion` reports instead of
`AccumulatedMouseMotion` — the accumulated resource has already folded the bad
report into the frame's sum by the time you could inspect it — and drops any
report above `MAX_RAW_DELTA` (1500 counts, well above the fastest real flick at
8000 DPI), warning once.

### Mouse sensitivity is a setting

`LookSettings`, a multiplier over the base radians-per-count, clamped 0.1-5.0:
pause-menu `[-]`/`[+]` on a 0.1 step grid, `/sens n`, or `SOILS_SENS`. Every
writer goes through `set`/`nudge`, so nothing can park it at zero (look dies) or
negative (both axes invert at once). Session-scoped, like the load radius and
the render toggles.

### The player is a light source

A blocklight emitter riding the camera's voxel, stamped into the L0 grid by the
flood's reseed pass rather than added as a shader glow — so it is occluded by
geometry exactly like a placed torch, wrapping corners and stopping at walls.
`track_player_light` queues the vacated and the occupied cell on a voxel
crossing and nothing on sub-voxel movement; the planner unions the two
neighbourhoods, so walking costs about one edit's worth of flood per voxel.
Level via `/playerlight n` (0-15, 0 off). Client-side only — no light crosses
the wire, so nothing shared diverges.

The bug worth remembering: the planner silently drops light edits for chunks
that are not resident, and at startup the player stands in their chunk for
several seconds before it streams. The emitter was registered and then lost for
good — world stays black. `PlayerLight::advance` now holds the move until the
destination chunk can actually be flooded.

### A 30-minute day

`DAY_SECONDS` 120 → 1800. Measured against a fresh server rather than trusting
the constant: `daytime` read 0.02 at 29 s and 0.14 at 261 s, i.e. 261/0.145 ≈
1800.

### Terrain: a continental octave

`default_soils` gains a 1/2000 simplex at amplitude **300**, against 115 for the
five original octaves combined — it dominates the heightmap by design, taking
the measured surface range from about ±115 to 61..433. `WORLDGEN_ALGO_VERSION`
2 → 3, `MAX_SURFACE` derived from the constant rather than restating it, goldens
re-pinned at six positions that each straddle their own column's surface (the
old four had degenerated into two all-air and two all-slate hashes).

Spawn had a hardcoded `[282, 285, 268]` against a ~256 surface, which with ±300
relief is either buried or hundreds of blocks up; it now reads the generator's
surface height at the spawn column, keeping the old 29-voxel clearance — not
cosmetic, several tests build platforms in that air.

### Occlusion culling

The server withholds chunks nothing can see into or walk into. The criterion is
**every neighbour presents a solid boundary layer towards it**, not "the chunk
is solid": at depth nearly every chunk has cave air in it, so the latter would
cull almost nothing, while the layer *between* two of them is usually still
solid.

- `face_mask` caches six bits of boundary solidity per chunk, recomputed on edit.
- `World::sealed` answers `None` when a neighbour is not resident — undecidable,
  never "no". Guessing leaks chunks or hides visible ones.
- Undecided verdicts defer per client and are re-examined each tick; neighbours
  outside the view radius count as exposed, which terminates the question at the
  shell.
- `expose_neighbours` hands a withheld chunk back when an edit breaks its seal.
- `CULL_KEEP` always sends the player's 3×3×3 — insurance against a spawn or
  warp landing inside terrain.

Measured: radius 5 withholds 18.7% of 1331 chunks; radius 8 withholds 26.2% of
4913, in 4.4 s. The ceiling is caves breaking seals and roughly half the cube
being open sky; culling all-air chunks is the obvious next win.

### Tests

`crates/soils-server/tests/draw_distance.rs`: subscribes at radius 5 (and 8
under `--ignored`), drains, then asserts the safety property from the receiving
end — for every chunk the client got, every neighbour it did *not* get is behind
a solid wall, with the face test written out independently so a bug in
`face_mask` cannot make the test agree with it — plus a floor on how much was
culled.

Breaking a seal from a client turns out to be impossible by construction
(`CULL_KEEP` plus ~8-voxel edit reach means the nearest withheld chunk is tens
of voxels out of range), so the exposure path is pinned as unit tests on `World`
instead.

Culling broke three tests that waited for chunks the server may now legitimately
never send — they hung rather than failed, which is worse.
`concurrent_requests_serve_identical_chunks` and `fresh_world_burst_streams_promptly`
asked for a fixed box and blocked on its buried members; the wire-oracle test
probed an explicitly deep chunk. The harness gains `collect_available` (drain
until the *manifest* stream is quiet — the socket never is, snapshots go out
every tick) and the burst test now asserts a delivered *band* plus the
unchanged promptness, all-pristine and manifest-size gates. `ChunkFetch` was
already the right escape hatch: it serves any subscribed, resident position,
culled or not.

`hundreds_of_props_stay_synced_across_two_clients` regressed too — the pile
settles measurably slower on the new microtopography (the suite went 51 s →
90 s) and it started failing by 1-2 units. Confirmed against a stashed baseline
rather than assumed. Its settle budget doubled, and the comparison was made
sound: each peer used to decide "settled" from its own delta stream and the two
readings could describe different instants, so both now sample from a common
barrier. The 0.5 tolerance is untouched.

Also fixed: `warp_to_a_new_world` read the client's *cached* position, still the
old world's until a snapshot lands, and so read as a reach failure once each
world's spawn followed its own surface.

### Plans

[plan-rgb-light-rework.md](docs/plan-rgb-light-rework.md) — RGB blocklight in a
`u32` (5:5:5:5) with range 31, and the pool halving that pays for it.
[plan-sun.md](docs/plan-sun.md) — semidirectional sunlight in the spare bits.

## SpacetimeDB Integration

## Inventory, dropped items, and the UI mode

**Branch `ui-inventory`, 2026-08-25.** Full detail and what is left:
[docs/plan-ui.md](docs/plan-ui.md).

The gameplay loop: breaking a block yields it as an entity lying in the world
rather than teleporting it into the inventory, and walking into that entity
collects it. Placing a block spends it and is refused when the stack is empty.
New players are stocked with the nine blocks the old hotbar hard-coded, so
building still works from the first second.

- **`soils-protocol`** gains `ItemKind`/`ItemStack` and protocol v4:
  `EntitySpawn` carries an item payload, `InventoryUpdate` pushes the whole
  inventory, and `MoveItem`/`DropItem` are requests a client can make.
- **`soils-sim`** gains `Inventory` (slots, stacking, merge-or-swap) and
  `fall_item`. Items get their own gravity step rather than `step_player`: a
  0.15 cube integrated as a 0.3x1.6 capsule wedges in one-block gaps and floats
  an eye-height off the floor, and a block broken overhead would hang there out
  of reach.
- **`soils-server`** owns the inventory. The client mirrors it and never
  decides it — a client-owned inventory is a client-owned item spawner.
- **`soils-client`** gains the inventory screen (E/I/Tab), the item strip, the
  backpack affordance, and item icons drawn from `blocks.png`. Dropped items
  wear the texture of the block they came from.
- **`UiMode`** replaces inferring UI state from the cursor. The client used to
  show the pause menu whenever the pointer was free and re-grab it on any
  click, so an inventory screen could not be clicked without dismissing itself.
  Cursor grab now has exactly one owner; Alt frees the pointer without leaving
  play; Escape backs out of whatever is open. The old `Hotbar` is gone.
