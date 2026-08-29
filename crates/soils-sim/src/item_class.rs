//! Names, icons and item classes for the kinds that are not blocks.
//!
//! Mirrors the `entities.yaml` pattern in [`crate::entities`]: `items.yaml` is
//! embedded at compile time so the client and server binaries cannot disagree
//! about which id means what.
//!
//! Blocks deliberately do *not* live here — they already carry a name, an atlas
//! tile and an [`ItemClass`] in `blocks.yaml`. [`ItemRegistry::view`] is the one
//! lookup that spans both sources, so callers never branch on [`ItemKind`]
//! themselves.

use serde::Deserialize;
use soils_protocol::{Category, Effect, Function, ItemClass, ItemKind};
use soils_worldgen::BlockRegistry;

/// One tool, weapon or consumable.
#[derive(Debug, Clone, Deserialize)]
pub struct ItemDef {
    pub name: String,
    /// Index into `blocks.png`. Tools and weapons have no art yet, so the
    /// placeholder is tile 0 rather than a missing-texture branch at every icon.
    #[serde(default)]
    pub tile: u8,
    /// Overrides for the list's default class. The category is fixed by which
    /// list the entry sits in — a weapon in the tools list would be a lie the
    /// YAML has no way to express.
    #[serde(default)]
    pub function: Option<Function>,
    #[serde(default)]
    pub effect: Option<Effect>,
    #[serde(default = "one")]
    pub weight: f32,
}

fn one() -> f32 {
    1.0
}

/// A uniform read of any item, whichever registry it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ItemView<'a> {
    pub name: &'a str,
    pub class: ItemClass,
    /// Atlas tile in `blocks.png`.
    pub tile: u8,
    pub weight: f32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemRegistry {
    #[serde(default)]
    tools: Vec<ItemDef>,
    #[serde(default)]
    weapons: Vec<ItemDef>,
    #[serde(default)]
    consumables: Vec<ItemDef>,
}

impl ItemRegistry {
    pub fn parse(yaml: &str) -> Self {
        let reg: ItemRegistry = serde_yaml::from_str(yaml).expect("items.yaml parses");
        for (list, what) in
            [(&reg.tools, "tools"), (&reg.weapons, "weapons"), (&reg.consumables, "consumables")]
        {
            assert!(list.len() <= u16::MAX as usize + 1, "{what} ids are u16");
        }
        reg
    }

    /// The defs, the requested id, and the default class for a kind's list.
    fn list_of(&self, kind: ItemKind) -> Option<(&[ItemDef], u16, ItemClass)> {
        let class = |category, function| ItemClass { category, function, effect: Effect::None };
        match kind {
            ItemKind::Block(_) => None,
            ItemKind::Tool(id) => Some((&self.tools, id, class(Category::Tool, Function::Mine))),
            ItemKind::Weapon(id) => {
                Some((&self.weapons, id, class(Category::Weapon, Function::Strike)))
            }
            ItemKind::Consumable(id) => {
                Some((&self.consumables, id, class(Category::Consumable, Function::Eat)))
            }
        }
    }

    /// Name, class, icon and weight for any item.
    ///
    /// `None` means the id names nothing — a stack of an item this build has
    /// never heard of. Callers must read that as "do not show it", never as a
    /// reason to panic: the id arrives from the server.
    pub fn view<'a>(&'a self, kind: ItemKind, blocks: &'a BlockRegistry) -> Option<ItemView<'a>> {
        if let Some(id) = kind.block() {
            let def = blocks.get(id)?;
            // The top face is the one that reads as the block at icon size.
            return Some(ItemView {
                name: &def.name,
                class: def.class,
                tile: def.faces[1],
                weight: def.weight,
            });
        }
        let (list, id, default) = self.list_of(kind)?;
        let def = list.get(id as usize)?;
        Some(ItemView {
            name: &def.name,
            class: ItemClass {
                function: def.function.unwrap_or(default.function),
                effect: def.effect.unwrap_or(default.effect),
                ..default
            },
            tile: def.tile,
            weight: def.weight,
        })
    }

    /// Just the class — what the hotbar's substitution rule compares.
    pub fn class_of(&self, kind: ItemKind, blocks: &BlockRegistry) -> Option<ItemClass> {
        self.view(kind, blocks).map(|v| v.class)
    }
}

/// The embedded registry source.
pub const ITEMS_YAML: &str = include_str!("../items.yaml");

/// Parse the embedded registry (both binaries call this once at startup).
pub fn default_item_registry() -> ItemRegistry {
    ItemRegistry::parse(ITEMS_YAML)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regs() -> (ItemRegistry, BlockRegistry) {
        (default_item_registry(), soils_worldgen::default_registry())
    }

    #[test]
    fn the_embedded_yaml_parses() {
        let _ = default_item_registry();
    }

    /// Every block must be viewable as an item, or it can be held and never
    /// drawn. The atlas bound is asserted here too: a widened `blocks.png` or a
    /// reordered `blocks.yaml` would otherwise point icons at the wrong cell.
    #[test]
    fn every_block_resolves_to_a_view() {
        let (items, blocks) = regs();
        assert!(blocks.len() > 1, "registry must actually be loaded");
        for id in 0..blocks.len() as u8 {
            let view = items
                .view(ItemKind::Block(id), &blocks)
                .unwrap_or_else(|| panic!("block {id} has no item view"));
            assert!(!view.name.is_empty());
            assert!(view.tile < 64, "{} points outside the 8x8 atlas", view.name);
            assert!(view.weight > 0.0, "{} weighs nothing", view.name);
        }
    }

    /// The groupings the hotbar's substitution rule leans on. If Cobblestone
    /// and Moss Stone ever stop sharing a class, the starter kit stops
    /// demonstrating a refill and the feature quietly loses its only content.
    #[test]
    fn the_starter_blocks_group_the_way_the_hotbar_expects() {
        let (items, blocks) = regs();
        let class = |name: &str| {
            let id = blocks.id_of(name).unwrap_or_else(|| panic!("no block named {name}"));
            items.class_of(ItemKind::Block(id), &blocks).expect("every block has a class")
        };
        assert_eq!(class("Cobblestone"), class("Moss Stone"));
        assert_eq!(class("Cobblestone"), class("Stone Bricks"));
        assert_eq!(class("Dirt"), class("Grass"));
        assert_eq!(class("Log"), class("Leaves"));
        assert_eq!(class("Wooden Crate"), class("Clay Pot"));
        assert_ne!(class("Cobblestone"), class("Dirt"), "stone must not stand in for earth");
        assert_ne!(class("Log"), class("Wooden Crate"));
    }

    /// An id nothing defines must read as absent rather than panic — it can
    /// arrive from a server built against a newer `items.yaml`.
    #[test]
    fn an_unknown_id_is_none_rather_than_a_panic() {
        let (items, blocks) = regs();
        assert!(items.view(ItemKind::Tool(9999), &blocks).is_none());
        assert!(items.view(ItemKind::Consumable(0), &blocks).is_none(), "none authored yet");
        assert!(items.view(ItemKind::Block(250), &blocks).is_none());
    }

    /// A YAML-authored consumable must pick up its list's default class while
    /// still honouring an explicit `effect`. Exercised against a literal rather
    /// than the embedded file, which is empty until content exists.
    #[test]
    fn an_authored_consumable_takes_its_lists_defaults() {
        let blocks = soils_worldgen::default_registry();
        let items = ItemRegistry::parse(
            "consumables:\n  - name: Large Fruit\n    tile: 3\n    effect: healing\n\
             \n  - name: Dry Ration\n    tile: 4\n",
        );
        let fruit = items.view(ItemKind::Consumable(0), &blocks).expect("id 0 is authored");
        assert_eq!(fruit.name, "Large Fruit");
        assert_eq!(fruit.class.category, Category::Consumable);
        assert_eq!(fruit.class.function, Function::Eat, "the list sets the function");
        assert_eq!(fruit.class.effect, Effect::Healing);
        assert_eq!(fruit.weight, 1.0, "an omitted weight defaults rather than zeroing");

        let ration = items.view(ItemKind::Consumable(1), &blocks).expect("id 1 is authored");
        assert_ne!(
            ration.class, fruit.class,
            "a ration that heals nothing must not stand in for a healing fruit"
        );
    }
}
