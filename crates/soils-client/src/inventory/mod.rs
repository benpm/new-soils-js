//! The inventory mirror and the two views onto it: the [`hotbar`] along the
//! bottom of the screen and the full [`screen`] that `E` opens.
//!
//! The server owns the inventory; everything here is a *view* of the last
//! `InventoryUpdate` it pushed. Nothing in this module changes what the player
//! holds — dropping is a request, and the views only redraw once the server has
//! answered. See `docs/plan-ui.md`.
//!
//! The hotbar is the one part that is purely local, and deliberately so: it
//! stores **references** to item kinds, never items. Putting a block on the
//! hotbar moves nothing and sends nothing.

pub mod container;
pub mod hotbar;
pub mod screen;

use bevy::prelude::*;
use soils_protocol::{ItemKind, ItemStack};

#[allow(unused_imports)]
pub use hotbar::{HOTBAR_SLOTS, Hotbar, HotbarSlot};
#[allow(unused_imports)]
pub use screen::{InventoryScreen, SelectedItem};

/// `blocks.png` is a grid of 16x16 tiles, 8 across (see `atlas.wgsl`).
const ATLAS_TILE: u32 = 16;
const ATLAS_COLS: u32 = 8;
const ATLAS_ROWS: u32 = 8;

/// The client's copy of the server's inventory.
///
/// Slot *positions* are not shown anywhere any more — the screen groups by
/// category and the hotbar points at kinds — so nothing here needs to track a
/// selected slot. That removes the old index-into-`placeable()` selection,
/// which went stale every time a stack was spent.
#[derive(Resource, Default)]
pub struct PlayerInventory {
    pub slots: Vec<Option<ItemStack>>,
}

impl PlayerInventory {
    /// How many of `kind` are held, summed across every slot.
    ///
    /// `u32`, not `u16`: the starter kit alone is 128 of a kind, and a stack
    /// caps at 64, so a kind routinely spans slots and the total can exceed
    /// what one stack can express.
    pub fn total_of(&self, kind: ItemKind) -> u32 {
        self.slots
            .iter()
            .flatten()
            .filter(|s| s.kind == kind)
            .map(|s| s.count as u32)
            .sum()
    }

    pub fn holds(&self, kind: ItemKind) -> bool {
        self.slots.iter().flatten().any(|s| s.kind == kind)
    }

    /// The first slot holding `kind` — what a drop request needs to name.
    pub fn first_slot_holding(&self, kind: ItemKind) -> Option<usize> {
        self.slots.iter().position(|s| s.is_some_and(|s| s.kind == kind))
    }

    /// Distinct kinds held, in first-appearance slot order.
    ///
    /// The order is what makes the hotbar's auto-fill deterministic, so it is
    /// part of the contract rather than an accident of iteration.
    pub fn kinds(&self) -> Vec<ItemKind> {
        let mut seen = Vec::new();
        for stack in self.slots.iter().flatten() {
            if !seen.contains(&stack.kind) {
                seen.push(stack.kind);
            }
        }
        seen
    }

    /// Distinct kinds with their totals, in first-appearance slot order.
    ///
    /// The screen shows one cell per *kind*, not per stack: 128 Cobblestone is
    /// one entry reading 128, not two reading 64. Two cells for one item would
    /// be an artefact of slot packing that the player cannot see and cannot act
    /// on, and it would make the hotbar badge ambiguous.
    pub fn by_kind(&self) -> Vec<(ItemKind, u32)> {
        self.kinds().into_iter().map(|k| (k, self.total_of(k))).collect()
    }
}

/// Item names, classes, icons and weights: `blocks.yaml` for blocks,
/// `items.yaml` for everything else.
#[derive(Resource)]
pub struct Items(pub soils_sim::ItemRegistry);

impl Default for Items {
    fn default() -> Self {
        Self(soils_sim::default_item_registry())
    }
}

/// Atlas handles for drawing item icons.
#[derive(Resource)]
pub struct ItemIcons {
    pub atlas: Handle<Image>,
    pub layout: Handle<TextureAtlasLayout>,
}

impl ItemIcons {
    /// An icon for an atlas tile. Which tile an item uses is
    /// [`soils_sim::ItemRegistry`]'s business, not this type's.
    pub fn node(&self, tile: u8) -> ImageNode {
        ImageNode::from_atlas_image(
            self.atlas.clone(),
            TextureAtlas { index: tile as usize, layout: self.layout.clone() },
        )
    }

    /// The same icon, dimmed to say "a hotbar slot points at this".
    pub fn silhouette(&self, tile: u8) -> ImageNode {
        ImageNode { color: crate::theme::SILHOUETTE_TINT, ..self.node(tile) }
    }
}

pub fn setup_item_icons(
    mut commands: Commands,
    assets: Res<AssetServer>,
    mut layouts: ResMut<Assets<TextureAtlasLayout>>,
) {
    let layout = layouts.add(TextureAtlasLayout::from_grid(
        UVec2::splat(ATLAS_TILE),
        ATLAS_COLS,
        ATLAS_ROWS,
        None,
        None,
    ));
    commands.insert_resource(ItemIcons { atlas: assets.load("blocks.png"), layout });
}

/// Meshes and materials for items lying in the world, cached per atlas tile.
///
/// A dropped item is drawn as a small cube wearing its block's own texture, so
/// what fell out of a broken block is recognisable on the ground. The cube's
/// UVs are remapped into the tile — a stock `Cuboid` samples the whole atlas
/// on every face and reads as noise.
#[derive(Resource, Default)]
pub struct DroppedItemVisuals {
    by_tile: std::collections::HashMap<u8, (Handle<Mesh>, Handle<StandardMaterial>)>,
}

/// Edge length of a dropped item, matching `DroppedItem` in `entities.yaml`.
const DROP_SIZE: f32 = 2.0 * 0.15;

impl DroppedItemVisuals {
    /// Mesh + material for `tile`, building them on first use.
    pub fn get(
        &mut self,
        tile: u8,
        atlas: &Handle<Image>,
        meshes: &mut Assets<Mesh>,
        materials: &mut Assets<StandardMaterial>,
    ) -> (Handle<Mesh>, Handle<StandardMaterial>) {
        self.by_tile
            .entry(tile)
            .or_insert_with(|| {
                let mut mesh = Mesh::from(Cuboid::new(DROP_SIZE, DROP_SIZE, DROP_SIZE));
                remap_uvs_to_tile(&mut mesh, tile);
                let material = materials.add(StandardMaterial {
                    base_color_texture: Some(atlas.clone()),
                    perceptual_roughness: 0.9,
                    ..default()
                });
                (meshes.add(mesh), material)
            })
            .clone()
    }
}

/// Squeeze a mesh's 0..1 UVs into one atlas cell.
fn remap_uvs_to_tile(mesh: &mut Mesh, tile: u8) {
    let (col, row) = ((tile % ATLAS_COLS as u8) as f32, (tile / ATLAS_COLS as u8) as f32);
    let (w, h) = (1.0 / ATLAS_COLS as f32, 1.0 / ATLAS_ROWS as f32);
    let Some(bevy::render::mesh::VertexAttributeValues::Float32x2(uvs)) =
        mesh.attribute_mut(Mesh::ATTRIBUTE_UV_0)
    else {
        return;
    };
    for uv in uvs.iter_mut() {
        // Inset by half a texel: sampling exactly on a cell edge bleeds the
        // neighbouring tile in under linear filtering.
        let inset = 0.5 / (ATLAS_COLS as f32 * ATLAS_TILE as f32);
        uv[0] = (col + uv[0].clamp(0.0, 1.0)) * w;
        uv[1] = (row + uv[1].clamp(0.0, 1.0)) * h;
        uv[0] = uv[0].clamp(col * w + inset, (col + 1.0) * w - inset);
        uv[1] = uv[1].clamp(row * h + inset, (row + 1.0) * h - inset);
    }
}

/// Shared scaffolding for the headless UI tests in [`hotbar`] and [`screen`].
///
/// Bevy UI mistakes — a bad grid track, a component that is really a `Node`
/// field, a despawn that takes the wrong subtree — surface at *runtime*. A
/// module that merely compiles proves very little, so both views are built for
/// real here, with no window and no renderer.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::chunk::Blocks;
    use crate::ui::UiMode;
    use soils_protocol::ItemStack;

    /// An app with both views built and their systems wired, holding `entries`
    /// (block name, count).
    pub fn ui_app(entries: &[(&str, u16)]) -> App {
        let blocks = soils_worldgen::default_registry();
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin);
        app.init_state::<UiMode>();
        app.init_resource::<ButtonInput<KeyCode>>();
        app.insert_resource(Time::<()>::default());
        app.init_resource::<PlayerInventory>();
        app.insert_resource(Blocks(blocks));
        app.init_resource::<Items>();
        app.init_resource::<Hotbar>();
        app.init_resource::<SelectedItem>();
        app.init_resource::<hotbar::DragItem>();
        app.init_resource::<container::OpenContainer>();
        app.insert_resource(ItemIcons { atlas: Handle::default(), layout: Handle::default() });
        app.add_systems(
            Startup,
            (screen::setup_inventory_ui, hotbar::setup_hotbar),
        );
        app.add_systems(
            Update,
            (
                screen::update_inventory_visibility,
                hotbar::reconcile_hotbar,
                hotbar::rebuild_hotbar,
                screen::rebuild_inventory_ui,
                screen::rebuild_detail_panel,
                hotbar::select_hotbar_slot,
                hotbar::animate_wiggle,
                container::update_container_visibility,
                container::rebuild_container_ui,
                screen::update_footer_hint,
            )
                .chain(),
        );
        app.update();
        stock(&mut app, entries);
        app
    }

    /// Replace the pack, exactly as `ServerMsg::InventoryUpdate` does, and let
    /// the views settle.
    ///
    /// The tests go through this rather than seeding `PlayerInventory` before
    /// the first frame: a resource inserted during app building has not
    /// *changed* by the time the rebuilds first run, so a pre-seeded pack would
    /// never be drawn. The real client never gets its inventory that way
    /// either — it always arrives as a later message.
    pub fn stock(app: &mut App, entries: &[(&str, u16)]) {
        let slots = {
            let blocks = &app.world().resource::<Blocks>().0;
            entries
                .iter()
                .map(|(name, count)| {
                    let id =
                        blocks.id_of(name).unwrap_or_else(|| panic!("no block named {name}"));
                    ItemStack::new(ItemKind::Block(id), *count)
                })
                .collect()
        };
        app.world_mut().resource_mut::<PlayerInventory>().slots = slots;
        app.update();
    }

    /// One key tap. The `clear` matters — `bevy_input` normally retires
    /// `just_pressed` each frame and is absent here, so without it the key reads
    /// as freshly pressed on every later frame.
    pub fn press(app: &mut App, key: KeyCode) {
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(key);
        app.update();
        let mut input = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        input.release(key);
        input.clear();
    }

    pub fn count<C: Component>(app: &mut App) -> usize {
        // Bound first: `world_mut()` and `world()` in one expression would
        // overlap a mutable and an immutable borrow of the same `App`.
        let mut q = app.world_mut().query_filtered::<Entity, With<C>>();
        q.iter(app.world()).count()
    }

    pub fn block(app: &App, name: &str) -> ItemKind {
        let blocks = &app.world().resource::<Blocks>().0;
        ItemKind::Block(blocks.id_of(name).unwrap_or_else(|| panic!("no block named {name}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn inv(slots: Vec<Option<ItemStack>>) -> PlayerInventory {
        PlayerInventory { slots }
    }

    /// One kind spread over several slots is one item to the player. This is
    /// the ordinary case, not an edge one: the starter kit is 128 of each kind
    /// and a stack caps at 64.
    #[test]
    fn a_kind_spanning_slots_reads_as_one_entry_with_the_full_count() {
        let i = inv(vec![
            ItemStack::new(ItemKind::Block(3), 64),
            Some(ItemStack::one(ItemKind::Tool(0))),
            ItemStack::new(ItemKind::Block(3), 64),
            None,
        ]);
        assert_eq!(i.total_of(ItemKind::Block(3)), 128);
        assert_eq!(i.by_kind(), vec![(ItemKind::Block(3), 128), (ItemKind::Tool(0), 1)]);
        assert_eq!(i.first_slot_holding(ItemKind::Block(3)), Some(0));
    }

    #[test]
    fn kind_order_is_first_appearance_so_auto_fill_is_deterministic() {
        let i = inv(vec![
            None,
            ItemStack::new(ItemKind::Block(9), 1),
            ItemStack::new(ItemKind::Block(2), 1),
            ItemStack::new(ItemKind::Block(9), 1),
        ]);
        assert_eq!(i.kinds(), vec![ItemKind::Block(9), ItemKind::Block(2)]);
    }

    #[test]
    fn an_empty_inventory_holds_nothing() {
        let i = inv(vec![None; 4]);
        assert!(i.kinds().is_empty());
        assert!(i.by_kind().is_empty());
        assert!(!i.holds(ItemKind::Block(1)));
        assert_eq!(i.total_of(ItemKind::Block(1)), 0);
        assert_eq!(i.first_slot_holding(ItemKind::Block(1)), None);
        assert!(inv(Vec::new()).kinds().is_empty());
    }

    /// The UV remap must land strictly inside the tile, or a dropped item
    /// samples its neighbour along the seam under linear filtering.
    #[test]
    fn remapped_uvs_stay_inside_their_tile() {
        for tile in [0u8, 7, 8, 63] {
            let mut mesh = Mesh::from(Cuboid::new(1.0, 1.0, 1.0));
            remap_uvs_to_tile(&mut mesh, tile);
            let Some(bevy::render::mesh::VertexAttributeValues::Float32x2(uvs)) =
                mesh.attribute(Mesh::ATTRIBUTE_UV_0)
            else {
                panic!("cuboid must carry UVs");
            };
            let (col, row) = ((tile % ATLAS_COLS as u8) as f32, (tile / ATLAS_COLS as u8) as f32);
            let (w, h) = (1.0 / ATLAS_COLS as f32, 1.0 / ATLAS_ROWS as f32);
            for uv in uvs {
                assert!(
                    uv[0] > col * w && uv[0] < (col + 1.0) * w,
                    "u {} escaped tile {tile}",
                    uv[0]
                );
                assert!(
                    uv[1] > row * h && uv[1] < (row + 1.0) * h,
                    "v {} escaped tile {tile}",
                    uv[1]
                );
            }
        }
    }
}
