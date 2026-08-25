# Changelog
****
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
