# Changelog
****
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
