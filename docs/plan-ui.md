# Plan: inventory, the item ring, and a real UI mode

> **Status: not started.** Companion to `plan-game-systems.md` (authority,
> protocol) and `architecture.md` (current state). Task checkoffs live in
> `TODO.md` under `## UI`.

Goal: replace the placeholder nine-slot block hotbar with an actual inventory —
an item model, a radial selector for tools/weapons/consumables, and a full
inventory screen — without inheriting Minecraft's UI.

The design intent is recorded in `TODO.md`:

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

**Escape.** `TODO.md` lists Escape among the inventory keys, but Escape is
currently the cursor release and so the pause menu. Both cannot be true.

Recommendation: **Escape closes whatever is open, and opens the pause menu only
when nothing is.** That is the near-universal convention, it keeps a guaranteed
way out of any UI, and E/I/Tab already give the inventory three bindings. The
alternative — Escape opens the inventory and the pause menu moves elsewhere —
is defensible but costs the "get me out of here" key.

**Alt: hold or toggle.** Hold is more predictable; toggle is kinder for long
interactions. Suggest hold, with a toggle if it proves annoying.

## 7. Later: authority and persistence

Out of scope for the UI work, and listed here so the phases above do not
accidentally foreclose it. `TODO.md` already records the intended split:
authoritative in `soils-server` during a session, persisted to SpacetimeDB on
logout — the same shape as `player_profile`.

That phase adds inventory messages, bumps `PROTOCOL_VERSION` to 4, makes
breaking a block yield an item and placing one consume it, and is where the
ring stops being cosmetic. Until then the client inventory is local and
unvalidated, which is fine for building the UI and **must not ship as
multiplayer** — a client-authoritative inventory is a client-authoritative
item spawner.

## 8. Milestones

| # | Phase | Ships | Gate |
|---|---|---|---|
| 0 | UI mode | State machine, single cursor owner, Alt | Transition + no-re-grab tests |
| 1 | Item model | `soils-sim` items, stacks, `Inventory` | Stacking/conservation tests |
| 2 | Icons | Atlas layout + `icon(kind)` | Every block resolves in range |
| 3 | Ring | Radial selector; `Hotbar` retired | Angle mapping, wrap seam |
| 4 | Screen | E/I/Tab, slot grid, backpack affordance | Open/close, move conserves |
| 5 | Authority | Protocol v4, server-owned, STDB on logout | Out of scope here |

Phases 0-2 are invisible to a player and are most of the risk. Phase 0 in
particular is a refactor of code that currently works, so it ships and is
verified on its own before anything is built on it.
