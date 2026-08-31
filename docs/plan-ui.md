# Plan: inventory, the item ring, and a real UI mode

> **Status (2026-08-25): phases 0-4 shipped, and phase 5's authority half with
> them.** The loop works end to end: breaking a block drops it, walking into
> the drop collects it, placing spends it.
>
> **Superseded in part (2026-08-28): the ring is gone and there is a hotbar.**
> Phase 6 below replaced the item strip and the held-item indicator with an
> eight-key bar of *references*, and turned the slot grid into a
> category-organized listing. Everything phases 0-5 say about `UiMode`, the item
> model, icons and authority still holds; §5's ring and §9's "the ring is a
> strip" do not. See [§10](#10-phase-6--the-hotbar-2026-08-28).
>
> Companion to `plan-game-systems.md` (authority, protocol) and
> `architecture.md` (current state). The remaining work is tracked in
> [`tasks.md`](tasks.md).

Goal: replace the placeholder nine-slot block hotbar with an actual inventory —
an item model, a radial selector for tools/weapons/consumables, and a full
inventory screen — without inheriting Minecraft's UI.

The design intent is recorded in `roadmap.md`:

* No hotbar. A **ring** shows every tool, weapon and consumable carried.
* Hotkeys drive the UI; **Alt** releases the cursor for mouse interaction.
* The inventory screen opens on **E**, **I**, **Tab** (and see [§6](#6-open-decisions)).
* A backpack icon with a circled "E" advertises the binding.

## 1. Where things stand

Worth reading before writing any code — three of these are load-bearing.

| Thing | Reality |
|---|---|
| Inventory | Does not exist. Nothing in any crate models an item. |
| `Hotbar` (`edit.rs:35`) | `[&'static str; 9]` of block names + `selected`. No counts, no ownership, no server involvement. Placement invents blocks from nothing. |
| UI state | **There is none.** See below. |
| Protocol | No inventory messages. `PROTOCOL_VERSION = 3`. |
| Icons | Free — see [§4](#4-icons-are-nearly-free). |

### The blocker: UI state is inferred from the cursor

`pause.rs:211` decides the pause menu is visible when
`cursor.grab_mode == CursorGrabMode::None`. Cursor-released *is* the menu.
Every other UI gate in the client (`edit.rs:83`, `edit.rs:138`,
`player.rs:329`) reads the same flag.

Two consequences, both fatal to an inventory screen:

* **Any UI that frees the cursor also opens the pause menu.** There is no way
  to express "released, but not paused".
* **A click re-grabs the cursor** (`player.rs:313`), so clicking an inventory
  slot would lock the pointer and dismiss the screen underneath it.

This is not a detail to work around inside the inventory code. One boolean is
standing in for a state machine, and it has to become one first — which is why
[§2](#2-phase-0--ui-mode) ships before anything inventory-shaped exists.

### Key availability

Taken: `WASD`, `Space`, `Shift`, `Ctrl`, `F`, `F3`, `/`, `1`-`9` (hotbar,
being replaced), `Escape` (cursor release). **Free: `E`, `I`, `Tab`, `Alt`.**

## 2. Phase 0 — UI mode

Introduce an explicit state and make cursor grab a *consequence* of it rather
than the source of truth.

```rust
#[derive(States, Default, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UiMode {
    #[default] Playing,   // cursor locked, look + edit live
    Menu,                 // pause/settings
    Inventory,            // full screen
    Console,              // existing chat/command line
}
```

Rules:

* One system owns cursor grab, driven by `UiMode` and the Alt override. Nothing
  else writes `grab_mode`.
* Gameplay systems swap `cursor.grab_mode == None` checks for
  `run_if(in_state(UiMode::Playing))`.
* `cursor_toggle`'s click-to-re-grab runs only in `Playing`.
* **Alt is a modifier, not a mode.** Held (or toggled) it frees the cursor in
  whatever mode is current, so the ring and HUD stay mouse-reachable during
  play. It must not change `UiMode`.

Ships alone, with no visible change beyond the pause menu no longer being
synonymous with a free cursor. This is a refactor of existing behaviour and is
the one phase where a regression is invisible until something else breaks —
test it on its own.

**Tests** (headless, following the `pause.rs` pattern — bare `App`, insert the
system under test, drive `Interaction`/input, assert on state):

* Each transition, both directions, including `Inventory` → `Playing`.
* A click in `Inventory` does **not** re-grab the cursor. This is the
  regression that makes the screen unusable, and it is invisible to a test that
  only checks state transitions.
* Alt frees the cursor without leaving `Playing`.

## 3. Phase 1 — the item model

In `soils-sim`, as plain data over plain functions — no ECS, no rendering — so
the server can reuse it verbatim when inventory becomes authoritative
([§7](#7-later-authority-and-persistence)).

```rust
pub enum ItemKind { Block(u8), Tool(ToolId), Weapon(WeaponId), Consumable(ConsumableId) }

pub struct ItemStack { pub kind: ItemKind, pub count: u16, pub durability: Option<u16> }

pub struct Inventory { slots: Vec<Option<ItemStack>> }
```

`Inventory` gets `insert` (merging into partial stacks first), `take`, `split`,
`swap`, and `ring_items()` — the filtered view of tools, weapons and
consumables the ring draws. Blocks are excluded from the ring: 19 block types
plus everything else would make it unreadable, and blocks belong to the
placement selector.

Categories come from `concepts.md`: tools (including the Delabbras/Maddox
hybrids), weapons, consumables, materials.

**Tests** — pure functions, so test them exhaustively rather than
representatively:

* `insert` fills partial stacks before empty slots, and never exceeds max size.
* `insert` into a full inventory returns the remainder rather than dropping it
  silently. *(The silent-drop version of this is the classic item-void bug and
  passes any test that only checks the success path.)*
* `split` conserves total count; `take` of more than held returns what exists.
* Round-trip: a sequence of inserts and takes conserves item count.

## 4. Icons are nearly free

`blocks.png` is 128x128, laid out 8 columns x 8 rows of 16x16 tiles
(`atlas.wgsl:190,214`). `BlockDef.faces[1]` is the top-face tile index. So a
block icon is:

```rust
let layout = layouts.add(TextureAtlasLayout::from_grid(UVec2::splat(16), 8, 8, None, None));
ImageNode::from_atlas_image(atlas.clone(), TextureAtlas { index: def.faces[1] as usize, layout })
```

No new art for blocks, and the same atlas the world already uploads. Tools,
weapons and consumables have no textures yet — give them a placeholder tile and
a name label until art exists; do not block the UI on it.

**Test:** every block in `blocks.yaml` resolves to a tile index inside the
8x8 grid. Cheap, and catches a widened atlas silently pointing icons at the
wrong cell.

## 5. Phases 2 and 3 — the ring, then the screen

### The ring

Radial selector over `ring_items()`, replacing the 1-9 hotbar. Opens on hold,
selects by mouse angle or by hotkey, commits on release. `edit.rs` reads the
ring's current selection instead of `Hotbar::block_name()`.

Keep `Hotbar` alive behind the ring until the screen lands, then delete it
along with `hotbar_select` and the HUD's `block [n] name` line. Deleting it
early means losing block placement for two phases.

**Tests:** angle to index mapping at each sector boundary and at the exact
12-o-clock seam (the wrap case is where these are always wrong); selection with
an empty ring does not panic; committing sets what `edit.rs` reads.

### The screen

Opens on E/I/Tab into `UiMode::Inventory`. A slot grid over `Inventory`,
click-to-pick-up and click-to-place (drag is a refinement, not the first cut),
plus the backpack affordance with the circled "E" in the corner.

**Tests:** open/close via each binding; picking up from a slot and placing into
another conserves items; closing with a stack "in hand" returns it to the
inventory rather than voiding it.

## 6. Open decisions

**Escape — decided: it backs out.** The design note lists Escape among the
inventory keys, but Escape was already the cursor release and so the pause
menu, and both could not be true. Escape now closes whatever is open and
reaches the pause menu only when nothing else is: it is the one key that must
always mean "get me out of here", and E/I/Tab already give the inventory three
bindings. Worth revisiting if that reads wrong in play.

**Alt — decided: hold.** Held, it frees the pointer without leaving the current
mode. A toggle is kinder for long interactions and is a one-line change if hold
proves annoying.

## 7. Later: authority and persistence

Out of scope for the UI work, and listed here so the phases above do not
accidentally foreclose it. `roadmap.md` already records the intended split:
authoritative in `soils-server` during a session, persisted to SpacetimeDB on
logout — the same shape as `player_profile`.

That phase adds inventory messages, bumps `PROTOCOL_VERSION` to 4, makes
breaking a block yield an item and placing one consume it, and is where the
ring stops being cosmetic. Until then the client inventory is local and
unvalidated, which is fine for building the UI and **must not ship as
multiplayer** — a client-authoritative inventory is a client-authoritative
item spawner.

## 8. Milestones

| # | Phase | Ships | Status |
|---|---|---|---|
| 0 | UI mode | `UiMode`, single cursor owner, Alt | **done** — `ui.rs`, 7 tests |
| 1 | Item model | `ItemKind`/`ItemStack`, `Inventory` | **done** — 25 tests |
| 2 | Icons | Atlas layout, in-world drop textures | **done** — `inventory.rs` |
| 3 | Ring | Item strip; `Hotbar` retired | **partly** — see below |
| 4 | Screen | E/I/Tab, slot grid, backpack affordance | **done** |
| 5 | Authority | Protocol v4, server-owned inventory | **done**, minus persistence |
| 6 | Hotbar | Reference bar, self-healing keys, categories | **done** 2026-08-28 |

Phase 0 was a refactor of code that already worked, and shipped on its own
before anything was built on it.

## 9. What is left

Tracked as tasks in [`tasks.md`](tasks.md); the reasoning is here.

* ~~**The ring is a strip, not a ring.**~~ Closed by phase 6, which deleted
  both the ring and the `HeldItem` indicator rather than making the strip
  radial. The ring never became a *selector* because nothing in the game is a
  tool, and it excluded blocks by design — so it sat next to a second strip
  showing the one thing the player actually chose between. One bar that holds
  any kind is what both were reaching for. See [§10](#10-phase-6--the-hotbar-2026-08-28).
* **Inventory does not survive logout.** It is session state on the server. The
  shape to copy is `player_profile` in SpacetimeDB — the same "authoritative in
  `soils-server` during a session, persisted on logout" split recorded in
  `roadmap.md`. Until then a reconnecting player is re-stocked with the starter
  kit, which is generous rather than correct.
* **Nearby drops do not merge.** An uncollected item is reaped after
  `DROP_TTL_TICKS` (5 min), so they no longer accumulate forever, but strip
  mining still puts one entity per block in the world until they expire.
  Merging drops within a voxel or two would cut that hard.
* **No crafting, durability, or drop tables.** `ItemStack::durability` is
  carried and never decremented, and a broken block yields exactly itself.

## 10. Phase 6 — the hotbar (2026-08-28)

Worked from the mockup in `scratch/`. Two changes, and the second is only
interesting because of the first.

### The bar holds references, not items

A slot holds an `ItemKind`, never an `ItemStack`. Putting Cobblestone on key 1
moves nothing, sends nothing, and leaves the stack where the server put it; the
inventory still lists it, dimmed, wearing a badge with the key's number. So
there is no protocol change and no second authority to keep in sync —
`PROTOCOL_VERSION` stays at 4 and `Hotbar` is a client resource.

Binding by *kind* rather than by slot index is what makes that work. Slot
indices are not stable — the server merges, splits and relocates stacks, and one
kind routinely spans several slots (the starter kit is 128 apiece against a
64 stack cap). It also retires the old selection, an index into the derived
`placeable()` list that went stale every time a stack was spent and carried a
clamp and a test to survive it.

### A key heals itself

When the item a slot points at runs out, the slot rebinds to another item of the
same `ItemClass` — category, function *and* effect, all three. Spend the last
Cobblestone and the key holds Moss Stone; eat the last Large Fruit and it holds
some other healing consumable. Never across a class: with the whole stone
category gone, a key that wanted stone goes empty rather than take the Leaves
sitting unassigned next to it. An empty key wiggles when pressed, so a dead key
is visible rather than an input that seems to have been swallowed.

`ItemClass` is new, in `soils-protocol` beside `ItemKind` for the same reason
recorded there. Blocks carry theirs in `blocks.yaml`; `soils-sim/items.yaml`
covers the tool/weapon/consumable ids, following the `entities.yaml` pattern.
It ships with three empty lists — the registry exists so the substitution rule
is written against real data rather than a special case, and so authoring the
first fruit is a YAML edit. The block categories were chosen so the rule is
demonstrable with the default starter kit and no new content.

`reconcile` is two passes, not one: every slot must let go of what it lost
before any slot refills, or the first key in the array takes a replacement the
second had a better claim to. Candidates come from `PlayerInventory::kinds()`,
which is in inventory-slot order, so the choice is deterministic and testable.

A slot that has never held anything takes whatever is going — otherwise a new
player faces eight blanks over a pack the server just filled. A slot that *has*
held something only ever takes a like replacement. That is the whole of the
`want` field.

### The screen groups by category

Slot positions were the only reason the flat grid existed, and they are not
something the player can see or act on, so the screen shows one cell per item
*kind* (128 Cobblestone reads as one entry of 128, not two of 64) under a row
per category, empty categories omitted. Rearranging slots therefore leaves the
UI: `MoveItem` stays in the protocol and the server, where
`soils-server/tests/inventory.rs` still covers it, but the client no longer
sends it.

### What did not port

`clip-path` chamfers and the scanline overlay have no `bevy_ui` equivalent;
square borders in the mockup's amber palette are the first cut, and a 9-slice
chamfer via `NodeImageMode::Sliced` is possible later at the cost of a new PNG.
Emoji category icons are impossible for the reason already recorded here — the
bundled font is a FiraMono subset — so each category borrows the atlas tile of a
representative block, derived from the registry rather than hard-coded.

The dimmed icon is a compromise worth naming: `blocks.png` tiles are fully
opaque squares, so a true silhouette would turn every assigned block into the
same dark square. The mockup's silhouettes read because its icons are emoji with
outlines. `theme::SILHOUETTE_TINT` dims far enough to be obviously different
while staying recognisable, and the badge is what actually says which key holds
it.
