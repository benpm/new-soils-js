//! The player inventory: slots, stacking rules, and the ring view.
//!
//! Plain data over pure functions, deliberately: the server owns the
//! authoritative copy and the client keeps a mirror for the UI, and both run
//! this exact code. [`ItemKind`] and [`ItemStack`] live in `soils-protocol`
//! because the wire carries them. See `docs/plan-ui.md`.

use serde::{Deserialize, Serialize};
pub use soils_protocol::{ItemKind, ItemStack};

use crate::{GRAVITY, VoxelSampler};
use glam::Vec3;

/// Half-extent of a dropped item, matching `DroppedItem` in `entities.yaml`.
pub const ITEM_HALF: f32 = 0.15;

/// How close a player's feet must come to pick an item up. Generous on
/// purpose: the alternative is items that visibly cannot be collected.
pub const PICKUP_RADIUS: f32 = 1.6;

/// Advance a dropped item by `dt` under gravity, resting on the first solid
/// voxel beneath it.
///
/// Deliberately not [`crate::step_player`]: that integrates a 0.3 x 1.6
/// capsule, and an item is a 0.15 cube. Reusing it would wedge items inside
/// one-block gaps and float them a player's eye-height off the ground.
pub fn fall_item(pos: &mut Vec3, vel: &mut Vec3, dt: f32, world: &impl VoxelSampler) {
    vel.y -= GRAVITY * dt;
    let next = *pos + *vel * dt;
    if vel.y < 0.0 {
        // Sweep the voxel column between here and there so a fast item cannot
        // pass through a floor in one step.
        let from = (pos.y - ITEM_HALF).floor() as i32;
        let to = (next.y - ITEM_HALF).floor() as i32;
        for y in (to..=from).rev() {
            let v = glam::IVec3::new(next.x.floor() as i32, y, next.z.floor() as i32);
            if world.is_solid(v) {
                pos.x = next.x;
                pos.z = next.z;
                pos.y = (y + 1) as f32 + ITEM_HALF;
                *vel = Vec3::ZERO;
                return;
            }
        }
    }
    *pos = next;
}

/// Fixed-size slot array. The default is [`Inventory::DEFAULT_SLOTS`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    slots: Vec<Option<ItemStack>>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self::new(Self::DEFAULT_SLOTS)
    }
}

impl Inventory {
    /// Four rows of nine, matching the inventory screen's grid.
    ///
    /// The starter kit alone occupies two rows, and there are 19 block kinds to
    /// collect — 27 slots left too little room to mine into.
    pub const DEFAULT_SLOTS: usize = 36;

    pub fn new(slots: usize) -> Self {
        Self { slots: vec![None; slots] }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| s.is_none())
    }

    pub fn slot(&self, i: usize) -> Option<&ItemStack> {
        self.slots.get(i).and_then(|s| s.as_ref())
    }

    pub fn slots(&self) -> &[Option<ItemStack>] {
        &self.slots
    }

    /// Total count of one kind across every slot.
    pub fn count_of(&self, kind: ItemKind) -> u32 {
        self.slots
            .iter()
            .flatten()
            .filter(|s| s.kind == kind)
            .map(|s| s.count as u32)
            .sum()
    }

    /// Insert a stack, filling partial stacks of the same kind before taking
    /// an empty slot.
    ///
    /// Returns what did **not** fit. Callers must handle a non-`None` return —
    /// dropping it on the floor is how items silently vanish, so this is a
    /// value rather than a `bool`.
    #[must_use = "the remainder is lost if it is not re-dropped or refused"]
    pub fn insert(&mut self, mut stack: ItemStack) -> Option<ItemStack> {
        for slot in self.slots.iter_mut().flatten() {
            if !slot.mergeable_with(&stack) {
                continue;
            }
            let moved = slot.space().min(stack.count);
            slot.count += moved;
            stack.count -= moved;
            if stack.count == 0 {
                return None;
            }
        }
        for slot in self.slots.iter_mut().filter(|s| s.is_none()) {
            let moved = stack.kind.max_stack().min(stack.count);
            *slot = Some(ItemStack { count: moved, ..stack });
            stack.count -= moved;
            if stack.count == 0 {
                return None;
            }
        }
        Some(stack)
    }

    /// Remove up to `count` of `kind`, returning how many were actually
    /// removed. Partial removal is intentional: the caller decides whether
    /// less than asked is a failure.
    pub fn remove(&mut self, kind: ItemKind, count: u16) -> u16 {
        let mut left = count;
        for slot in self.slots.iter_mut() {
            if left == 0 {
                break;
            }
            let Some(stack) = slot else { continue };
            if stack.kind != kind {
                continue;
            }
            let taken = stack.count.min(left);
            stack.count -= taken;
            left -= taken;
            if stack.count == 0 {
                *slot = None;
            }
        }
        count - left
    }

    /// Take a whole slot out.
    pub fn take(&mut self, i: usize) -> Option<ItemStack> {
        self.slots.get_mut(i).and_then(|s| s.take())
    }

    /// Put a stack into a slot, returning whatever was there. The slot index
    /// must exist; out of range gives the stack straight back.
    pub fn put(&mut self, i: usize, stack: ItemStack) -> Option<ItemStack> {
        match self.slots.get_mut(i) {
            Some(slot) => slot.replace(stack),
            None => Some(stack),
        }
    }

    /// Split `count` off the stack in slot `i`, leaving the rest behind.
    pub fn split(&mut self, i: usize, count: u16) -> Option<ItemStack> {
        let slot = self.slots.get_mut(i)?;
        let stack = slot.as_mut()?;
        let taken = count.min(stack.count);
        if taken == 0 {
            return None;
        }
        stack.count -= taken;
        let kind = stack.kind;
        let durability = stack.durability;
        if stack.count == 0 {
            *slot = None;
        }
        Some(ItemStack { kind, count: taken, durability })
    }

    /// Move slot `from` onto slot `to`: merge if the two stack together,
    /// otherwise swap. Returns whether anything changed.
    ///
    /// This is the whole of what a UI drag means, kept here rather than in the
    /// server's message handler so the rule is tested once and both ends agree.
    pub fn move_slot(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.slots.len() || to >= self.slots.len() {
            return false;
        }
        let (Some(src), dst) = (self.slots[from], self.slots[to]) else { return false };
        match dst {
            Some(mut dst) if dst.mergeable_with(&src) && dst.space() > 0 => {
                let moved = dst.space().min(src.count);
                dst.count += moved;
                self.slots[to] = Some(dst);
                match self.slots[from].as_mut() {
                    Some(s) if s.count > moved => s.count -= moved,
                    _ => self.slots[from] = None,
                }
                true
            }
            _ => {
                self.slots.swap(from, to);
                true
            }
        }
    }

    pub fn swap(&mut self, a: usize, b: usize) {
        if a != b && a < self.slots.len() && b < self.slots.len() {
            self.slots.swap(a, b);
        }
    }

    /// The tools, weapons and consumables the ring draws, in slot order.
    pub fn ring_items(&self) -> Vec<(usize, ItemStack)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|s| (i, s)))
            .filter(|(_, s)| s.kind.in_ring())
            .collect()
    }

    /// Total items held, across every slot. Used by the conservation tests and
    /// worth keeping cheap.
    pub fn total(&self) -> u32 {
        self.slots.iter().flatten().map(|s| s.count as u32).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRT: ItemKind = ItemKind::Block(1);
    const STONE: ItemKind = ItemKind::Block(3);
    const PICK: ItemKind = ItemKind::Tool(0);

    /// A pile larger than one slot holds must survive insertion by spreading,
    /// not by being clamped away at construction.
    #[test]
    fn an_over_max_pile_spreads_across_slots() {
        let mut inv = Inventory::new(4);
        assert!(inv.insert(ItemStack::new(DIRT, 150).unwrap()).is_none());
        assert_eq!(inv.slot(0).unwrap().count, 64);
        assert_eq!(inv.slot(1).unwrap().count, 64);
        assert_eq!(inv.slot(2).unwrap().count, 22);
        assert_eq!(inv.total(), 150, "nothing lost on the way in");
    }

    #[test]
    fn unstackable_items_take_one_slot_each() {
        let mut inv = Inventory::new(4);
        assert!(inv.insert(ItemStack::new(PICK, 3).unwrap()).is_none());
        for i in 0..3 {
            assert_eq!(inv.slot(i).unwrap().count, 1, "tools do not stack");
        }
        assert_eq!(inv.total(), 3);
    }

    #[test]
    fn insert_fills_partial_stacks_before_taking_an_empty_slot() {
        let mut inv = Inventory::new(4);
        assert!(inv.insert(ItemStack::new(DIRT, 60).unwrap()).is_none());
        assert!(inv.insert(ItemStack::new(DIRT, 3).unwrap()).is_none());
        assert_eq!(inv.slot(0).unwrap().count, 63);
        assert!(inv.slot(1).is_none(), "must not open a new slot while one has room");
    }

    #[test]
    fn insert_overflows_into_the_next_slot_without_exceeding_max() {
        let mut inv = Inventory::new(4);
        assert!(inv.insert(ItemStack::new(DIRT, 64).unwrap()).is_none());
        assert!(inv.insert(ItemStack::new(DIRT, 10).unwrap()).is_none());
        assert_eq!(inv.slot(0).unwrap().count, 64);
        assert_eq!(inv.slot(1).unwrap().count, 10);
        assert_eq!(inv.total(), 74);
    }

    /// The silent-drop version of this passes every test that only checks the
    /// success path, and is how items vanish in shipped games.
    #[test]
    fn insert_into_a_full_inventory_returns_the_remainder() {
        let mut inv = Inventory::new(1);
        assert!(inv.insert(ItemStack::new(DIRT, 64).unwrap()).is_none());
        let left = inv.insert(ItemStack::new(STONE, 9).unwrap()).expect("must not vanish");
        assert_eq!(left.kind, STONE);
        assert_eq!(left.count, 9);
        assert_eq!(inv.total(), 64, "nothing was absorbed");
    }

    #[test]
    fn insert_returns_only_what_did_not_fit() {
        let mut inv = Inventory::new(1);
        assert!(inv.insert(ItemStack::new(DIRT, 60).unwrap()).is_none());
        let left = inv.insert(ItemStack::new(DIRT, 10).unwrap()).expect("6 do not fit");
        assert_eq!(left.count, 6);
        assert_eq!(inv.total(), 64);
    }

    #[test]
    fn worn_tools_do_not_merge() {
        let mut inv = Inventory::new(4);
        let worn = ItemStack { kind: PICK, count: 1, durability: Some(12) };
        assert!(inv.insert(worn).is_none());
        assert!(inv.insert(ItemStack::one(PICK)).is_none());
        assert_eq!(inv.slot(0).unwrap().durability, Some(12));
        assert_eq!(inv.slot(1).unwrap().durability, None, "must take its own slot");
    }

    #[test]
    fn remove_reports_what_it_actually_took() {
        let mut inv = Inventory::new(4);
        assert!(inv.insert(ItemStack::new(DIRT, 70).unwrap()).is_none());
        assert_eq!(inv.remove(DIRT, 5), 5);
        assert_eq!(inv.total(), 65);
        assert_eq!(inv.remove(DIRT, 100), 65, "takes what exists, reports the truth");
        assert_eq!(inv.total(), 0);
        assert_eq!(inv.remove(DIRT, 1), 0);
    }

    #[test]
    fn remove_empties_slots_it_drains() {
        let mut inv = Inventory::new(2);
        let _ = inv.insert(ItemStack::new(DIRT, 64).unwrap());
        assert_eq!(inv.remove(DIRT, 64), 64);
        assert!(inv.slot(0).is_none());
        assert!(inv.is_empty());
    }

    #[test]
    fn split_conserves_the_total() {
        let mut inv = Inventory::new(2);
        let _ = inv.insert(ItemStack::new(DIRT, 40).unwrap());
        let off = inv.split(0, 15).unwrap();
        assert_eq!(off.count, 15);
        assert_eq!(inv.slot(0).unwrap().count, 25);
        assert_eq!(inv.total() + off.count as u32, 40);
    }

    #[test]
    fn split_of_everything_clears_the_slot() {
        let mut inv = Inventory::new(2);
        let _ = inv.insert(ItemStack::new(DIRT, 8).unwrap());
        assert_eq!(inv.split(0, 99).unwrap().count, 8, "clamps to what is there");
        assert!(inv.slot(0).is_none());
        assert!(inv.split(0, 1).is_none(), "empty slot splits to nothing");
    }

    #[test]
    fn take_and_put_round_trip_through_a_slot() {
        let mut inv = Inventory::new(2);
        let _ = inv.insert(ItemStack::new(DIRT, 12).unwrap());
        let held = inv.take(0).unwrap();
        assert!(inv.is_empty());
        assert!(inv.put(1, held).is_none());
        assert_eq!(inv.slot(1).unwrap().count, 12);
    }

    #[test]
    fn put_out_of_range_gives_the_stack_back() {
        let mut inv = Inventory::new(1);
        let s = ItemStack::new(DIRT, 3).unwrap();
        assert_eq!(inv.put(9, s), Some(s), "must not swallow a stack into nowhere");
    }

    #[test]
    fn the_ring_shows_tools_but_not_blocks() {
        let mut inv = Inventory::new(4);
        let _ = inv.insert(ItemStack::new(DIRT, 5).unwrap());
        let _ = inv.insert(ItemStack::one(PICK));
        let _ = inv.insert(ItemStack::one(ItemKind::Consumable(0)));
        let ring = inv.ring_items();
        assert_eq!(ring.len(), 2);
        assert!(ring.iter().all(|(_, s)| s.kind.in_ring()));
    }

    #[test]
    fn move_slot_merges_compatible_stacks() {
        let mut inv = Inventory::new(3);
        assert!(inv.put(0, ItemStack::new(DIRT, 20).unwrap()).is_none());
        assert!(inv.put(1, ItemStack::new(DIRT, 30).unwrap()).is_none());
        assert!(inv.move_slot(0, 1));
        assert!(inv.slot(0).is_none(), "source drained");
        assert_eq!(inv.slot(1).unwrap().count, 50);
        assert_eq!(inv.total(), 50, "merging conserves");
    }

    #[test]
    fn a_merge_that_overflows_leaves_the_remainder_behind() {
        let mut inv = Inventory::new(3);
        assert!(inv.put(0, ItemStack::new(DIRT, 40).unwrap()).is_none());
        assert!(inv.put(1, ItemStack::new(DIRT, 50).unwrap()).is_none());
        assert!(inv.move_slot(0, 1));
        assert_eq!(inv.slot(1).unwrap().count, 64);
        assert_eq!(inv.slot(0).unwrap().count, 26, "what did not fit stays put");
        assert_eq!(inv.total(), 90, "merging never destroys the overflow");
    }

    #[test]
    fn move_slot_swaps_incompatible_stacks() {
        let mut inv = Inventory::new(3);
        assert!(inv.put(0, ItemStack::new(DIRT, 5).unwrap()).is_none());
        assert!(inv.put(1, ItemStack::new(STONE, 7).unwrap()).is_none());
        assert!(inv.move_slot(0, 1));
        assert_eq!(inv.slot(0).unwrap().kind, STONE);
        assert_eq!(inv.slot(1).unwrap().kind, DIRT);
        assert_eq!(inv.total(), 12);
    }

    #[test]
    fn move_slot_onto_an_empty_slot_relocates() {
        let mut inv = Inventory::new(3);
        assert!(inv.put(0, ItemStack::new(DIRT, 5).unwrap()).is_none());
        assert!(inv.move_slot(0, 2));
        assert!(inv.slot(0).is_none());
        assert_eq!(inv.slot(2).unwrap().count, 5);
    }

    #[test]
    fn move_slot_out_of_range_or_from_empty_changes_nothing() {
        let mut inv = Inventory::new(2);
        assert!(inv.put(0, ItemStack::new(DIRT, 5).unwrap()).is_none());
        assert!(!inv.move_slot(0, 0), "onto itself");
        assert!(!inv.move_slot(0, 99), "past the end");
        assert!(!inv.move_slot(1, 0), "from an empty slot");
        assert_eq!(inv.total(), 5);
    }

    #[test]
    fn ring_on_an_empty_inventory_is_empty_not_a_panic() {
        assert!(Inventory::default().ring_items().is_empty());
    }

    /// A long mixed sequence must conserve items exactly. Catches the
    /// off-by-one families that individual-operation tests step over.
    #[test]
    fn a_sequence_of_operations_conserves_every_item() {
        let mut inv = Inventory::new(6);
        let mut expected: u32 = 0;
        let mut rng = soils_protocol::Rng::new(0xA17E_0001);
        for i in 0..500u32 {
            match i % 4 {
                0 | 1 => {
                    let n = 1 + rng.below(70) as u16;
                    let kind = if i % 8 == 0 { STONE } else { DIRT };
                    let put = ItemStack::new(kind, n).unwrap();
                    let back = inv.insert(put).map(|s| s.count).unwrap_or(0);
                    expected += (put.count - back) as u32;
                }
                2 => expected -= inv.remove(DIRT, 1 + rng.below(30) as u16) as u32,
                _ => {
                    let slot = rng.below(inv.len() as u64) as usize;
                    if let Some(s) = inv.split(slot, 1 + rng.below(20) as u16) {
                        // Split off and immediately re-inserted: a no-op for the
                        // total only if both halves survive.
                        let back = inv.insert(s).map(|s| s.count).unwrap_or(0);
                        expected -= back as u32;
                    }
                }
            }
            assert_eq!(inv.total(), expected, "divergence at step {i}");
        }
    }
}

#[cfg(test)]
mod fall_tests {
    use super::*;

    /// Solid everywhere at y < 0.
    fn ground(v: glam::IVec3) -> u8 {
        if v.y < 0 { 1 } else { 0 }
    }

    #[test]
    fn an_item_falls_and_comes_to_rest_on_the_ground() {
        let mut pos = Vec3::new(0.5, 8.0, 0.5);
        let mut vel = Vec3::ZERO;
        for _ in 0..200 {
            fall_item(&mut pos, &mut vel, 1.0 / 64.0, &ground);
        }
        assert!((pos.y - ITEM_HALF).abs() < 1e-3, "rests on top of y=-1, got {}", pos.y);
        assert_eq!(vel, Vec3::ZERO);
    }

    /// One big step must not teleport the item through the floor. Without the
    /// column sweep this lands far below ground and never stops.
    #[test]
    fn a_fast_item_does_not_tunnel_through_the_floor() {
        let mut pos = Vec3::new(0.5, 40.0, 0.5);
        let mut vel = Vec3::new(0.0, -300.0, 0.0);
        fall_item(&mut pos, &mut vel, 0.5, &ground);
        assert!(pos.y >= 0.0, "tunnelled to {}", pos.y);
        assert_eq!(vel, Vec3::ZERO);
    }

    #[test]
    fn an_item_in_open_air_keeps_falling() {
        let mut pos = Vec3::new(0.5, 8.0, 0.5);
        let mut vel = Vec3::ZERO;
        let empty = |_: glam::IVec3| 0u8;
        fall_item(&mut pos, &mut vel, 1.0 / 64.0, &empty);
        assert!(pos.y < 8.0 && vel.y < 0.0);
    }

    #[test]
    fn an_item_resting_on_ground_stays_put() {
        let mut pos = Vec3::new(0.5, ITEM_HALF, 0.5);
        let mut vel = Vec3::ZERO;
        for _ in 0..64 {
            fall_item(&mut pos, &mut vel, 1.0 / 64.0, &ground);
        }
        assert!((pos.y - ITEM_HALF).abs() < 1e-3, "drifted to {}", pos.y);
    }
}
