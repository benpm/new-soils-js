//! Item identity and stacks — the wire-visible half of the inventory.
//!
//! The container and its rules live in `soils_sim::item`; these two types are
//! here because [`crate::ServerMsg`] carries them and `soils-sim` depends on
//! this crate rather than the other way round.

use serde::{Deserialize, Serialize};

/// What an item *is*. `Block` carries a `soils-worldgen` block id, so a
/// dropped Dirt and a placeable Dirt are the same item.
///
/// Tools, weapons and consumables have no content behind them yet — the ids
/// are reserved so the ring and the wire format do not change shape when they
/// do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemKind {
    Block(u8),
    Tool(u16),
    Weapon(u16),
    Consumable(u16),
}

impl ItemKind {
    /// Whether the item ring shows this. Blocks are excluded: they belong to
    /// the placement selector, and a ring holding every block plus every tool
    /// is unreadable.
    pub fn in_ring(&self) -> bool {
        !matches!(self, ItemKind::Block(_))
    }

    /// How many fit in one slot. Tools and weapons do not stack — a stack of
    /// pickaxes has no single durability.
    pub fn max_stack(&self) -> u16 {
        match self {
            ItemKind::Block(_) | ItemKind::Consumable(_) => 64,
            ItemKind::Tool(_) | ItemKind::Weapon(_) => 1,
        }
    }

    /// The block this item places, if it places one.
    pub fn block(&self) -> Option<u8> {
        match self {
            ItemKind::Block(id) => Some(*id),
            _ => None,
        }
    }
}

/// What an item is *for*.
///
/// Two items are interchangeable when their whole [`ItemClass`] matches — that
/// is what lets a hotbar slot heal itself when the stack it points at runs out,
/// swapping in a like item rather than going dead. The three axes are
/// deliberately separate: Cobblestone and a Healing Draught are both "one of a
/// kind you have more of", but only the axes together say a Large Fruit may
/// stand in for another fruit and never for a rock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemClass {
    pub category: Category,
    pub function: Function,
    pub effect: Effect,
}

impl ItemClass {
    /// What an unannotated block gets. Blocks are the only items with content
    /// today, and they are overwhelmingly stone, so this is the value that
    /// makes the fewest `blocks.yaml` entries need a `category:` line.
    pub const BLOCK: Self =
        Self { category: Category::Stone, function: Function::Place, effect: Effect::None };
}

/// Broad kind, and the row an item appears under in the inventory screen.
///
/// A closed enum rather than a free-form string so a typo in `blocks.yaml` is a
/// startup parse error. The alternative fails much later and much quieter: an
/// item whose category matches nothing is an item that can never substitute for
/// anything, and no test would notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Earth,
    Stone,
    Ore,
    Organic,
    Crafted,
    Tool,
    Weapon,
    Consumable,
}

impl Category {
    /// Every category, in the order the inventory screen lists them.
    pub const ALL: [Category; 8] = [
        Category::Earth,
        Category::Stone,
        Category::Ore,
        Category::Organic,
        Category::Crafted,
        Category::Tool,
        Category::Weapon,
        Category::Consumable,
    ];

    /// Title-case label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Category::Earth => "Earth",
            Category::Stone => "Stone",
            Category::Ore => "Ore",
            Category::Organic => "Organic",
            Category::Crafted => "Crafted",
            Category::Tool => "Tools",
            Category::Weapon => "Weapons",
            Category::Consumable => "Consumables",
        }
    }
}

/// What using the item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Function {
    /// Right-click puts it in the world.
    Place,
    Mine,
    Strike,
    /// Consumed on use.
    Eat,
}

/// What the item does to whoever uses it. Only meaningful alongside a
/// [`Function`] that has an effect at all; [`Effect::None`] otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    None,
    Healing,
}

/// A non-empty run of one item kind. `count == 0` is not representable through
/// the constructor; an inventory stores `None` for an empty slot instead.
///
/// `count` may exceed [`ItemKind::max_stack`] — a pile in transit, not yet
/// assigned to slots. `Inventory::insert` is what enforces the per-slot limit,
/// splitting a pile across as many slots as it needs. Constructing a clamped
/// stack instead would silently destroy the overflow at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemStack {
    pub kind: ItemKind,
    pub count: u16,
    /// Remaining uses, for kinds that wear out. `None` for everything else.
    pub durability: Option<u16>,
}

impl ItemStack {
    /// A stack of `count`. Returns `None` for a zero count so an empty stack
    /// can never enter a slot.
    pub fn new(kind: ItemKind, count: u16) -> Option<Self> {
        (count > 0).then_some(Self { kind, count, durability: None })
    }

    /// A single item.
    pub fn one(kind: ItemKind) -> Self {
        Self { kind, count: 1, durability: None }
    }

    /// Room left before this stack fills one slot.
    pub fn space(&self) -> u16 {
        self.kind.max_stack().saturating_sub(self.count)
    }

    /// Whether two stacks can merge: same kind, and neither part-worn.
    /// Merging worn tools would have to pick one durability and silently
    /// discard the other.
    pub fn mergeable_with(&self, other: &ItemStack) -> bool {
        self.kind == other.kind && self.durability.is_none() && other.durability.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_count_stack_cannot_exist() {
        assert!(ItemStack::new(ItemKind::Block(1), 0).is_none());
        assert_eq!(ItemStack::new(ItemKind::Block(1), 5).unwrap().count, 5);
    }

    #[test]
    fn only_blocks_place_and_only_non_blocks_ring() {
        assert_eq!(ItemKind::Block(7).block(), Some(7));
        assert_eq!(ItemKind::Tool(0).block(), None);
        assert!(!ItemKind::Block(7).in_ring());
        assert!(ItemKind::Tool(0).in_ring());
    }

    #[test]
    fn tools_do_not_stack_but_blocks_do() {
        assert_eq!(ItemKind::Tool(0).max_stack(), 1);
        assert_eq!(ItemKind::Weapon(0).max_stack(), 1);
        assert_eq!(ItemKind::Block(1).max_stack(), 64);
        assert_eq!(ItemKind::Consumable(0).max_stack(), 64);
    }

    #[test]
    fn worn_stacks_never_merge() {
        let fresh = ItemStack::one(ItemKind::Tool(0));
        let worn = ItemStack { durability: Some(3), ..fresh };
        assert!(fresh.mergeable_with(&fresh));
        assert!(!fresh.mergeable_with(&worn));
        assert!(!worn.mergeable_with(&worn), "two worn tools have two durabilities");
    }

    #[test]
    fn an_over_full_stack_reports_no_space_rather_than_underflowing() {
        let piled = ItemStack { kind: ItemKind::Block(1), count: 200, durability: None };
        assert_eq!(piled.space(), 0);
    }
}
