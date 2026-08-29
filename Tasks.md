# Tasks

Open work, one list. Finished work and its rationale live in
[`TODO.md`](TODO.md) (the 14-phase implementation log and its checkoffs) and
[`CHANGELOG.md`](CHANGELOG.md).

When a task lands: check it off here with the date, commit and branch, move the
description into `CHANGELOG.md`, and leave only the one-line result behind.

---

## Now — inventory follow-ups

The loop works (break → drop → pick up → place). What it still lacks, in the
order that hurts:

- [x] **Items can be stored in blocks.** Closed 2026-08-28 (`ui-inventory`).
      Wooden Crate (27 slots) and Clay Pot (9) hold items; contents live in a
      per-chunk block-data page addressed at the same slot index as that chunk's
      voxels, cached write-back and streamed off disk on demand. Protocol v5.
      See [plan-storage.md](docs/plan-storage.md).
- [ ] **Inventory does not survive logout.** The disk half of this is now
      cheap — the write-back discipline exists and is tested — but a player key
      is not spatial, so it wants a file per account rather than a `paged` slot;
      see [plan-storage.md §6](docs/plan-storage.md#6-phase-2--the-remaining-tenants)
      for why forcing it into the same file would be a second format wearing the
      first one's name. It is session state on the server,
      so a reconnecting player is re-stocked with the starter kit — generous
      rather than correct, and the one gap that makes the feature feel unreal.
      Copy the `player_profile` shape: authoritative in `soils-server` during a
      session, written to SpacetimeDB on logout. Needs a module schema change
      and a republish. See [plan-ui.md §9](docs/plan-ui.md#9-what-is-left).
- [ ] **Decide whether the starter kit should exist.** New players get 128 each
      of nine block kinds so building works from the first second, as it did
      before placement cost anything. That is a sandbox answer to a survival
      design (`docs/concepts.md`). Options: keep it, shrink it, or
      replace it with a "creative mode" flag. Cheap to change — one const in
      `app.rs`.
- [ ] **Nearby drops do not merge.** Uncollected items are reaped after 5 min,
      so they no longer accumulate forever, but strip mining still puts one
      replicated entity per block in the world until they expire. Merging drops
      within a voxel or two would cut that hard.
- [x] **The ring is a strip, not a ring.** Closed 2026-08-28 (`ui-inventory`) by
      deleting it. Working from the mockup in `scratch/`, the ring and the
      one-slot held-item indicator were replaced by an eight-key hotbar that
      holds *references* to item kinds: assigning moves nothing and sends
      nothing, the item stays listed in the inventory (dimmed, badged with its
      key), and a key whose item runs out rebinds itself to another of the same
      category, function and effect — or goes empty and wiggles when pressed.
      The screen groups by category instead of showing raw slots. See
      [plan-ui.md §10](docs/plan-ui.md#10-phase-6--the-hotbar-2026-08-28).
- [ ] **No crafting, durability, or drop tables.** `ItemStack::durability` is
      carried on the wire and never decremented; a broken block yields exactly
      itself. This is where item *identity* starts to matter and the
      `Tool`/`Weapon`/`Consumable` ids stop being reserved placeholders.
- [ ] **`ItemKind::Tool`/`Weapon`/`Consumable` have no content.** The registry
      landed 2026-08-28 — `crates/soils-sim/items.yaml` plus `ItemRegistry`,
      following the `entities.yaml` pattern, and blocks now carry a category,
      function, effect and weight in `blocks.yaml`. All three lists are still
      empty: what is missing is the items themselves. Authoring the first one is
      a YAML edit, and the hotbar's substitution rule is already written against
      it (a Large Fruit eaten to nothing is replaced by another
      `Consumable, Healing`).
- [ ] **A script edit that removes a container block does not spill it.**
      `run_scripts` applies voxel edits straight through `World::edit`, so a
      script that deletes a chest leaves its contents on the block-data page
      with no block in front of them — invisible, unreachable, and inherited by
      whatever is built on that voxel next. The player edit path handles this
      (`take_block_data` + spill + close viewers); the script path needs the
      same three lines, or both need to move behind one `break_block` helper.
      Landed 2026-08-28 with containers; see
      [plan-storage.md §5](docs/plan-storage.md).
- [ ] **Placing does not require the target to be air.** `validate_edit`
      checks reach and a known block id, nothing else, and `World::edit`
      unconditionally `set`s — so a modified client can overwrite any block
      without breaking it, taking no drop and spending one item. The container
      feature made this visible (building over a chest used to orphan its
      contents; that half is fixed — the spill is now keyed on "the old block
      is gone" rather than "this was a break"), but the gap itself predates it
      and is a gameplay decision, not just a check: some blocks *should* be
      replaceable in place, and which ones is a `blocks.yaml` key nobody has
      designed yet. Found in the PR #8 review, 2026-08-29.

- [ ] **A grave is nobody's.** Any player can open any container, including one
      that appears where someone died (planned in
      [plan-death-chest.md](docs/plan-death-chest.md)). Ownership is a real
      design question — timers, locks, owner-only tombstones — and guessing at
      it is worse than leaving it open on a game this size. Decide it before
      graves ship, not after.
- [ ] **Hotbar assignments do not survive logout.** Same gap as the inventory
      itself, and a smaller one: the bar is a client resource by design (it
      stores no items, so it needs no authority), which means it is also not on
      the wire. It rebuilds itself from the pack on login, so a returning player
      gets a usable bar — just not the one they arranged.

## Next — lighting

- [ ] **RGB blocklight, range 31.** Coloured light blocks and doubled range are
      the same change: one `u32` per voxel (`R5 G5 B5 S5`) replacing the packed
      byte, `N_SLOTS` halved to pay for it. Designed in full, including phasing
      and the pool-sizing risk:
      [plan-rgb-light-rework.md](docs/plan-rgb-light-rework.md). A sketch of
      step 1 is in a git stash (`wip: rgb light format`) — read the plan, not
      the stash.
- [ ] **Author the lamp blocks.** `blocks.yaml` has exactly two emissive blocks
      (Diamond Ore, Ruby Ore), which is not enough to see colour blending at
      all. A hue-circle set plus atlas tiles, and a `SOILS_LIGHT_DEMO` room in
      the shape of `gi_demo.rs` to look at them in.
- [ ] **Semidirectional sunlight.** Skylight has no direction, so overhangs are
      lit identically at dawn and noon and a wall lights the same on both sides.
      A second 5-bit channel in the light word's spare bits, flooded along a
      quantized sun direction:
      [plan-sun.md](docs/plan-sun.md). The re-flood cadence is the whole gamble
      — measure before committing.

## Next — streaming

- [ ] **Cull all-air chunks.** Occlusion culling withholds sealed chunks
      (26% of a radius-8 subscription) but sends every empty chunk above the
      terrain, and roughly half the cube is sky. An all-air chunk renders
      nothing and collides with nothing, so withholding it should be free —
      but check what a missing chunk means to the client's `voxel_at` and to
      `reseed`'s "chunk above unmapped = open sky" heuristic before assuming it.
      This is also what buys the headroom the RGB pool halving spends.

## Next — hardening

- [ ] **The 300-prop pile settles slowly on the new terrain.** The continental
      octave changed the microtopography the pile lands on, and
      `hundreds_of_props_stay_synced_across_two_clients` went from ~51 s to
      ~90 s; its settle budget was doubled to 120 x 700 ms to compensate. The
      comparison was also unsound — each peer decided "settled" from its own
      delta stream and the two readings could describe different instants — and
      now samples from a common barrier. Both are treatments, not a diagnosis:
      worth finding out whether the pile is genuinely creeping on a slope or
      whether the snapshot budget is starving prop updates under load.

- [ ] **`forced_misprediction_reconciles_behind_the_wall` is load-sensitive.**
      It asserts `max_divergence > 0.5` after walking a stale predictor into a
      carved tunnel. Under full-suite contention it has landed on exactly 0.5
      and failed; run alone it passes every time. The misprediction *does*
      happen — the threshold is what is marginal, and 0.5 is an arbitrary
      "half a block" rather than anything the scenario guarantees.
      Two candidate fixes, neither of them lowering the bar: drive the walk
      phase until the server is measurably N blocks ahead instead of for a
      fixed 150 ticks, or measure divergence relative to the walk distance
      actually achieved. Do **not** weaken the assertion to make a suite green
      — see `docs/dev/debug.md` on tests that pass without testing anything.
      Note this got closer to the edge when breaking a block started spawning
      a dropped item: that test breaks 27 of them, so it now replicates and
      steps 27 extra entities during the measurement.

- [ ] **Opening a *new* world on login blocks the tick** for up to 5 s while the
      restore runs. Correct as written — pristine terrain must not be generated
      over chunks being recovered — but it is a stall one joining player imposes
      on everyone. Wants asynchronous world creation with joins held until it
      completes, not a shorter timeout.
- [ ] **A database-only account cannot log in while the database is down.** The
      local account file is a cache written when *this* server registers or
      migrates an account, so a player who signed up elsewhere has no local
      record here. Keeping a local verifier for every successful database login
      would fix it, at the cost of scattering verifiers across every server a
      player touches — a deliberate trade, not an oversight.
- [ ] **`grant_server` is trust-on-first-use.** Whoever claims an empty
      allowlist first becomes a server. Fine for a local database, a race on a
      public one; seed the first identity from a trusted console instead.
- [ ] **`send_chat` trusts the caller's `world_id`.** A client can post into any
      world's channel. The check wants a presence row for the sender's account
      in that world, which needs the client to know its real
      `world_id_for(name)` first — today `Social::chat_world` defaults to 0.
      Worth doing together with per-world chat channels rather than alone.
- [ ] **`player_profile` and `chunk_blob` are world-readable.** They have to be:
      the game server reads profiles from the SDK cache on login, and a private
      table has no client accessor at all in 2.7.1. Row-level security is the
      fix and is unimplemented upstream — revisit when it lands rather than
      declaring a filter that does nothing.
- [ ] **Physics follow-ups.** A full two-way player via an Avian character
      controller (ride on / be pushed by props), and an interpolation plugin for
      sub-tick smoothing. *(Angular-velocity replication, previously listed here,
      already shipped — `MASK_ANGVEL` / `BodyAngVel`.)*
- [ ] **Decision gate:** re-evaluate whether the SpacetimeDB hybrid split still
      earns its keep, or whether more should move across.

## Later — the first content expansion

- [ ] **Block types with data parity to Minecraft.** Match the blocks in
      [Unity-Modded](https://github.com/Unity-Resource-Pack/Unity-Modded), using
      the pack's data files to work out which tile is what, and replace the
      existing textures. Interacts with inventory: every new block is a new
      `ItemKind::Block`, and the icon atlas is currently a fixed 8x8 grid of
      16x16 tiles — a wider palette needs that generalised.
- [ ] **Biomes**, blending organically into each other. Grass colour shifts in a
      subtle gradient stored as individual block data. Some biomes have tall
      trees, some short, some none; some have rain, clouds, storms; others a
      barren moon-like landscape. Transitions can carry unique structures — a
      sparse pond between wetland and desert. Some are extreme: the Crater
      Forest, whose boundary is a sheer cliff into dense canopy. Structures
      generate at quantity along chunk boundaries via blue-noise "seeds", which
      complete only when every chunk they intersect is otherwise generated;
      generate in 2x2 chunk blocks to limit incomplete structures.
- [ ] **Neural tile/structure generation.** Constraint-based tile generation
      inferring structural relationships and probabilities from example. Ingest
      Minecraft builds, parse them, convert to a compressed structure format,
      and make them editable in a new **structure design** mode that shows the
      generated output of your structure as you edit it, with a tweakable
      ruleset derived from the source map.

## Docs and hygiene

- [ ] **Organize [`docs/concepts.md`](docs/concepts.md).** Much of it refers to
      Minecraft; research Minecraft's mechanics and take notes before
      interpreting and rewriting it. Note there are now two design docs —
      `concepts.md` (philosophy, mechanics) and `conceptual_design.md` (story,
      setting) — and it is worth deciding whether that split is intended.
- [ ] **Prune `TODO.md`.** Its 14-phase checkoffs are the historical record and
      worth keeping, but the finished descriptions belong in `CHANGELOG.md` with
      only the result left behind.
- [ ] **Stale worktree.** `.claude/worktrees/light-pad-cache` is a 7.8 GB second
      checkout of the repo on the unmerged `worktree-light-pad-cache` branch,
      with uncommitted `Cargo.toml` edits. It doubles every repo-wide search.
      Land it or remove it.

---

## Cross-cutting notes

Kept from `TODO.md` because they change how the tasks above should be built.

- **Biomes / structures ↔ SpacetimeDB.** Blue-noise structure seeds spanning
  chunk boundaries are exactly the sparse relational state SpacetimeDB is good
  at, and a structure spanning chunks needs a claim that survives a restart.
  Decide before the generator is written whether seed ownership lives in region
  files or the database.
- **Wider block palettes ↔ chunk payloads.** A larger registry changes chunk
  payload size, and `chunk_blob` payloads above 992 bytes land in SpacetimeDB's
  content-addressed blob store. Re-measure dedupe rates once palettes widen.
- **Physics state stays out of the database.** Prop state is hot and per-tick.
  Anything persisted should be the *resting* state at unload, if at all.
- **Inventory is the `player_profile` shape.** Item stacks are cold relational
  state and a natural fit for a table, but inventory is latency-sensitive during
  play — hence authoritative in `soils-server`, persisted on logout.
