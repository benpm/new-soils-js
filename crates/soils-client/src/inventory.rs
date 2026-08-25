//! The inventory mirror and its UI: the full-screen slot grid, the item ring,
//! and the backpack affordance.
//!
//! The server owns the inventory; everything here is a *view* of the last
//! `InventoryUpdate` it pushed. Nothing in this module changes what the player
//! holds — slot moves and drops are requests, and the screen only redraws once
//! the server has answered. See `docs/plan-ui.md`.

use bevy::prelude::*;
use soils_protocol::{ClientMsg, ItemKind, ItemStack};
use soils_worldgen::BlockRegistry;

use crate::chunk::Blocks;
use crate::net::NetClient;
use crate::ui::UiMode;

/// `blocks.png` is a grid of 16x16 tiles, 8 across (see `atlas.wgsl`).
const ATLAS_TILE: u32 = 16;
const ATLAS_COLS: u32 = 8;
const ATLAS_ROWS: u32 = 8;

/// Slots per row in the inventory screen.
const GRID_COLS: usize = 9;
const SLOT_PX: f32 = 48.0;

/// The client's copy of the server's inventory, plus which stack is selected
/// for placement.
#[derive(Resource, Default)]
pub struct PlayerInventory {
    pub slots: Vec<Option<ItemStack>>,
    /// Index into [`Self::placeable`], not into `slots` — the placeable list
    /// is what the number keys and the HUD walk.
    pub selected: usize,
}

impl PlayerInventory {
    /// Slots holding a block, in slot order: what right-click can place.
    pub fn placeable(&self) -> Vec<(usize, ItemStack)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|s| (i, s)))
            .filter(|(_, s)| s.kind.block().is_some())
            .collect()
    }

    /// The block id right-click should place, if any is selected and held.
    pub fn selected_block(&self) -> Option<u8> {
        let placeable = self.placeable();
        placeable.get(self.selected.min(placeable.len().saturating_sub(1)))?.1.kind.block()
    }

    /// Tools, weapons and consumables — what the ring shows.
    pub fn ring_items(&self) -> Vec<ItemStack> {
        self.slots.iter().flatten().copied().filter(|s| s.kind.in_ring()).collect()
    }
}

/// Atlas handles for drawing item icons.
#[derive(Resource)]
pub struct ItemIcons {
    pub atlas: Handle<Image>,
    pub layout: Handle<TextureAtlasLayout>,
}

impl ItemIcons {
    /// Atlas tile for an item. Blocks use their top face, which is the one that
    /// reads as the block at icon size; everything else falls back to tile 0
    /// until tools and weapons have art of their own.
    pub fn tile(&self, kind: ItemKind, registry: &BlockRegistry) -> usize {
        match kind.block().and_then(|id| registry.get(id)) {
            Some(def) => def.faces[1] as usize,
            None => 0,
        }
    }

    pub fn node(&self, kind: ItemKind, registry: &BlockRegistry) -> ImageNode {
        ImageNode::from_atlas_image(
            self.atlas.clone(),
            TextureAtlas { index: self.tile(kind, registry), layout: self.layout.clone() },
        )
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

/// Root of the full-screen inventory panel.
#[derive(Component)]
pub struct InventoryScreen;
/// The slot grid, rebuilt whenever the inventory changes.
#[derive(Component)]
pub struct InventoryGrid;
/// The item ring along the bottom of the screen.
#[derive(Component)]
pub struct ItemRing;
/// The "press E" backpack affordance.
#[derive(Component)]
pub struct BackpackHint;
/// The one-slot indicator of what right-click will place.
///
/// Not a hotbar: the design calls for none, and the ring deliberately shows
/// only tools/weapons/consumables. But with the old hotbar gone, and the block
/// name living on the F3 debug overlay, a player otherwise has no way to see
/// what they are about to build with.
#[derive(Component)]
pub struct HeldItem;
/// One clickable slot, carrying its index into the server's slot array.
#[derive(Component, Clone, Copy)]
pub struct SlotButton(pub usize);

pub fn setup_inventory_ui(mut commands: Commands) {
    commands
        .spawn((
            InventoryScreen,
            Visibility::Hidden,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    padding: UiRect::all(Val::Px(14.0)),
                    row_gap: Val::Px(10.0),
                    border_radius: BorderRadius::all(Val::Px(6.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.08, 0.09, 0.11, 0.94)),
            ))
            .with_children(|panel| {
                panel.spawn((
                    Text::new("Inventory"),
                    TextFont { font_size: 20.0.into(), ..default() },
                    TextColor(Color::srgb(0.9, 0.9, 0.9)),
                ));
                panel.spawn((
                    InventoryGrid,
                    Node {
                        display: Display::Grid,
                        grid_template_columns: vec![RepeatedGridTrack::px(
                            GRID_COLS as u16,
                            SLOT_PX,
                        )],
                        row_gap: Val::Px(4.0),
                        column_gap: Val::Px(4.0),
                        ..default()
                    },
                ));
                panel.spawn((
                    Text::new("click a slot to pick it up, click another to place it"),
                    TextFont { font_size: 12.0.into(), ..default() },
                    TextColor(Color::srgba(0.75, 0.75, 0.78, 0.9)),
                ));
                panel.spawn((
                    // ASCII only: Bevy's default font is a FiraMono subset with
                    // no U+00B7, so a middot here renders as a tofu box. Caught
                    // in the demo recording, in the UI the recording was of.
                    Text::new("1-9 choose what to build with   -   Q throws one while playing"),
                    TextFont { font_size: 12.0.into(), ..default() },
                    TextColor(Color::srgba(0.65, 0.65, 0.68, 0.85)),
                ));
            });
        });

    // What right-click will place, bottom-centre above the ring.
    commands.spawn((
        HeldItem,
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(14.0),
            left: Val::Percent(50.0),
            margin: UiRect::left(Val::Px(-26.0)),
            width: Val::Px(52.0),
            height: Val::Px(52.0),
            align_items: AlignItems::FlexEnd,
            justify_content: JustifyContent::FlexEnd,
            border: UiRect::all(Val::Px(2.0)),
            border_radius: BorderRadius::all(Val::Px(5.0)),
            ..default()
        },
        BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.35)),
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.4)),
    ));

    // Always-visible ring along the bottom.
    commands.spawn((
        ItemRing,
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(64.0),
            left: Val::Percent(50.0),
            margin: UiRect::left(Val::Px(-140.0)),
            column_gap: Val::Px(6.0),
            ..default()
        },
    ));

    // Backpack affordance, bottom-left, as the design note asks: a pack with a
    // circled "E" on it.
    //
    // Drawn from nodes rather than set as an emoji — Bevy's default font is a
    // FiraMono subset with no emoji coverage, so a "🎒" renders as nothing at
    // all. The circled E is the part that actually teaches the binding.
    commands
        .spawn((
            BackpackHint,
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(10.0),
                left: Val::Px(10.0),
                width: Val::Px(38.0),
                height: Val::Px(40.0),
                ..default()
            },
        ))
        .with_children(|hint| {
            // Straps.
            hint.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(9.0),
                    top: Val::Px(0.0),
                    width: Val::Px(20.0),
                    height: Val::Px(12.0),
                    border_radius: BorderRadius::all(Val::Px(5.0)),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.42, 0.30, 0.18)),
            ));
            // Body of the pack.
            hint.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(7.0),
                    width: Val::Px(38.0),
                    height: Val::Px(33.0),
                    border_radius: BorderRadius::all(Val::Px(7.0)),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.55, 0.38, 0.22)),
            ));
            // Front pocket, so it reads as a bag and not a box.
            hint.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(7.0),
                    top: Val::Px(22.0),
                    width: Val::Px(24.0),
                    height: Val::Px(12.0),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.40, 0.27, 0.15)),
            ));
            // The circled key hint, overlapping the corner.
            hint.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(-7.0),
                    top: Val::Px(-2.0),
                    width: Val::Px(19.0),
                    height: Val::Px(19.0),
                    border: UiRect::all(Val::Px(2.0)),
                    border_radius: BorderRadius::all(Val::Percent(50.0)),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    ..default()
                },
                BorderColor::all(Color::srgb(0.96, 0.96, 0.96)),
                BackgroundColor(Color::srgba(0.05, 0.05, 0.06, 0.92)),
            ))
            .with_children(|c| {
                c.spawn((
                    Text::new("E"),
                    TextFont { font_size: 11.0.into(), ..default() },
                    TextColor(Color::srgb(0.98, 0.98, 0.98)),
                ));
            });
        });
}

/// Show the screen only in [`UiMode::Inventory`]; hide the ring and the hint
/// while it is up, since the screen supersedes both.
pub fn update_inventory_visibility(
    mode: Res<State<UiMode>>,
    mut screen: Query<&mut Visibility, With<InventoryScreen>>,
    mut others: Query<
        &mut Visibility,
        (
            Or<(With<ItemRing>, With<BackpackHint>, With<HeldItem>)>,
            Without<InventoryScreen>,
        ),
    >,
) {
    let open = *mode.get() == UiMode::Inventory;
    if let Ok(mut vis) = screen.single_mut() {
        let want = if open { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
    }
    let want = if open { Visibility::Hidden } else { Visibility::Inherited };
    for mut vis in &mut others {
        if *vis != want {
            *vis = want;
        }
    }
}

/// Rebuild the slot grid and the ring from the mirror.
///
/// Rebuilt wholesale rather than diffed: an inventory is a few dozen slots and
/// changes at human speed, so the simplest correct thing is also fast enough, and a
/// diff would be one more place for the view to drift from the authority.
pub fn rebuild_inventory_ui(
    mut commands: Commands,
    inventory: Res<PlayerInventory>,
    icons: Option<Res<ItemIcons>>,
    registry: Res<Blocks>,
    grid: Query<Entity, With<InventoryGrid>>,
    ring: Query<Entity, With<ItemRing>>,
    held: Query<Entity, With<HeldItem>>,
) {
    if !inventory.is_changed() {
        return;
    }
    let Some(icons) = icons else { return };

    if let Ok(grid) = grid.single() {
        commands.entity(grid).despawn_related::<Children>();
        commands.entity(grid).with_children(|parent| {
            let selected_slot = inventory
                .placeable()
                .get(inventory.selected)
                .map(|(i, _)| *i);
            for (i, slot) in inventory.slots.iter().enumerate() {
                let chosen = selected_slot == Some(i);
                let mut cell = parent.spawn((
                    Button,
                    SlotButton(i),
                    Node {
                        width: Val::Px(SLOT_PX),
                        height: Val::Px(SLOT_PX),
                        align_items: AlignItems::FlexEnd,
                        justify_content: JustifyContent::FlexEnd,
                        border: UiRect::all(Val::Px(if chosen { 2.0 } else { 1.0 })),
                        ..default()
                    },
                    BorderColor::all(if chosen {
                        Color::srgb(0.95, 0.8, 0.35)
                    } else {
                        Color::srgba(1.0, 1.0, 1.0, 0.18)
                    }),
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.06)),
                ));
                if let Some(stack) = slot {
                    let stack = *stack;
                    cell.with_children(|c| {
                        c.spawn((
                            icons.node(stack.kind, &registry.0),
                            Node {
                                position_type: PositionType::Absolute,
                                width: Val::Px(SLOT_PX - 10.0),
                                height: Val::Px(SLOT_PX - 10.0),
                                left: Val::Px(5.0),
                                top: Val::Px(5.0),
                                ..default()
                            },
                        ));
                        if stack.count > 1 {
                            c.spawn((
                                Text::new(stack.count.to_string()),
                                TextFont { font_size: 12.0.into(), ..default() },
                                TextColor(Color::WHITE),
                                Node {
                                    position_type: PositionType::Absolute,
                                    right: Val::Px(3.0),
                                    bottom: Val::Px(1.0),
                                    ..default()
                                },
                            ));
                        }
                    });
                }
            }
        });
    }

    if let Ok(held) = held.single() {
        commands.entity(held).despawn_related::<Children>();
        if let Some(stack) = inventory
            .placeable()
            .get(inventory.selected.min(inventory.placeable().len().saturating_sub(1)))
            .map(|(_, s)| *s)
        {
            commands.entity(held).with_children(|c| {
                c.spawn((
                    icons.node(stack.kind, &registry.0),
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(4.0),
                        top: Val::Px(4.0),
                        width: Val::Px(40.0),
                        height: Val::Px(40.0),
                        ..default()
                    },
                ));
                c.spawn((
                    Text::new(stack.count.to_string()),
                    TextFont { font_size: 12.0.into(), ..default() },
                    TextColor(Color::WHITE),
                    Node {
                        position_type: PositionType::Absolute,
                        right: Val::Px(4.0),
                        bottom: Val::Px(2.0),
                        ..default()
                    },
                ));
            });
        }
    }

    if let Ok(ring) = ring.single() {
        commands.entity(ring).despawn_related::<Children>();
        commands.entity(ring).with_children(|parent| {
            for stack in inventory.ring_items() {
                parent
                    .spawn((
                        Node {
                            width: Val::Px(40.0),
                            height: Val::Px(40.0),
                            border: UiRect::all(Val::Px(1.0)),
                            ..default()
                        },
                        BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.25)),
                        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.35)),
                    ))
                    .with_children(|c| {
                        c.spawn((
                            icons.node(stack.kind, &registry.0),
                            Node {
                                width: Val::Px(32.0),
                                height: Val::Px(32.0),
                                margin: UiRect::all(Val::Px(3.0)),
                                ..default()
                            },
                        ));
                    });
            }
        });
    }
}

/// Number keys pick which held block right-click places. This replaces the old
/// fixed nine-block `Hotbar`: the choices are now what the player actually has.
pub fn select_placeable(
    keys: Res<ButtonInput<KeyCode>>,
    mut inventory: ResMut<PlayerInventory>,
) {
    const DIGITS: [KeyCode; 9] = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    let count = inventory.placeable().len();
    for (i, key) in DIGITS.iter().enumerate() {
        if keys.just_pressed(*key) && i < count {
            inventory.selected = i;
        }
    }
}

/// Clicking a slot asks the server to move the selected stack onto it. The
/// mirror is not touched — the next `InventoryUpdate` is what redraws.
pub fn inventory_slot_clicks(
    mode: Res<State<UiMode>>,
    net: Res<NetClient>,
    inventory: Res<PlayerInventory>,
    mut held: Local<Option<usize>>,
    buttons: Query<(&Interaction, &SlotButton), Changed<Interaction>>,
) {
    if *mode.get() != UiMode::Inventory {
        return;
    }
    for (interaction, slot) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match *held {
            None => {
                if inventory.slots.get(slot.0).is_some_and(|s| s.is_some()) {
                    *held = Some(slot.0);
                }
            }
            Some(from) => {
                if from != slot.0 {
                    net.send(ClientMsg::MoveItem { from: from as u16, to: slot.0 as u16 });
                }
                *held = None;
            }
        }
    }
}

/// Q throws one of the selected stack on the ground.
pub fn drop_selected(
    keys: Res<ButtonInput<KeyCode>>,
    net: Res<NetClient>,
    inventory: Res<PlayerInventory>,
) {
    if !keys.just_pressed(KeyCode::KeyQ) {
        return;
    }
    let placeable = inventory.placeable();
    let Some((slot, _)) = placeable.get(inventory.selected) else { return };
    net.send(ClientMsg::DropItem { slot: *slot as u16, count: 1 });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(slots: Vec<Option<ItemStack>>) -> PlayerInventory {
        PlayerInventory { slots, selected: 0 }
    }

    #[test]
    fn placeable_lists_only_blocks_in_slot_order() {
        let i = inv(vec![
            Some(ItemStack::one(ItemKind::Tool(0))),
            Some(ItemStack::new(ItemKind::Block(3), 5).unwrap()),
            None,
            Some(ItemStack::new(ItemKind::Block(1), 2).unwrap()),
        ]);
        let p = i.placeable();
        assert_eq!(p.len(), 2, "tools are not placeable");
        assert_eq!(p[0].0, 1, "slot order preserved");
        assert_eq!(p[1].0, 3);
    }

    #[test]
    fn the_ring_shows_tools_and_never_blocks() {
        let i = inv(vec![
            Some(ItemStack::new(ItemKind::Block(1), 9).unwrap()),
            Some(ItemStack::one(ItemKind::Tool(0))),
            Some(ItemStack::one(ItemKind::Consumable(2))),
        ]);
        assert_eq!(i.ring_items().len(), 2);
        assert!(i.ring_items().iter().all(|s| s.kind.in_ring()));
    }

    #[test]
    fn selected_block_follows_the_selection() {
        let mut i = inv(vec![
            Some(ItemStack::new(ItemKind::Block(7), 1).unwrap()),
            Some(ItemStack::new(ItemKind::Block(9), 1).unwrap()),
        ]);
        assert_eq!(i.selected_block(), Some(7));
        i.selected = 1;
        assert_eq!(i.selected_block(), Some(9));
    }

    /// A selection can outlive the stack it pointed at — spend the last block
    /// and the list shrinks under the index. It must clamp, not panic and not
    /// place a block the player no longer has.
    #[test]
    fn a_stale_selection_clamps_instead_of_panicking() {
        let mut i = inv(vec![Some(ItemStack::new(ItemKind::Block(7), 1).unwrap())]);
        i.selected = 5;
        assert_eq!(i.selected_block(), Some(7));

        let empty = inv(vec![None, None]);
        assert_eq!(empty.selected_block(), None, "nothing held, nothing placeable");
    }

    /// Every block must resolve to a tile inside the atlas grid. Cheap, and it
    /// catches a widened `blocks.png` or a re-ordered `blocks.yaml` silently
    /// pointing every icon at the wrong cell.
    #[test]
    fn every_block_resolves_to_a_tile_inside_the_atlas() {
        let registry = soils_worldgen::default_registry();
        assert!(registry.len() > 1, "registry must actually be loaded");
        for id in 0..registry.len() as u8 {
            let def = registry.get(id).expect("id came from len()");
            let tile = def.faces[1];
            assert!(
                (tile as u32) < ATLAS_COLS * ATLAS_ROWS,
                "block {} ({}) points at tile {tile}, outside the {ATLAS_COLS}x{ATLAS_ROWS} grid",
                id,
                def.name,
            );
        }
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

    /// Build the real UI tree headlessly. Bevy UI mistakes — a bad grid track,
    /// a component that is really a `Node` field — surface at *runtime*, so a
    /// compiling `setup_inventory_ui` proves very little on its own.
    fn ui_app(inventory: PlayerInventory) -> App {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin);
        app.init_state::<UiMode>();
        app.insert_resource(inventory);
        app.insert_resource(Blocks(soils_worldgen::default_registry()));
        app.insert_resource(ItemIcons {
            atlas: Handle::default(),
            layout: Handle::default(),
        });
        app.add_systems(Startup, setup_inventory_ui);
        app.add_systems(Update, rebuild_inventory_ui);
        app.update();
        app.update();
        app
    }

    fn count<C: Component>(app: &mut App) -> usize {
        // Bound first: `world_mut()` and `world()` in one expression would
        // overlap a mutable and an immutable borrow of the same `App`.
        let mut q = app.world_mut().query_filtered::<Entity, With<C>>();
        q.iter(app.world()).count()
    }

    #[test]
    fn the_inventory_ui_builds_a_slot_per_slot() {
        let mut inv = PlayerInventory {
            slots: vec![None; soils_sim::Inventory::DEFAULT_SLOTS],
            selected: 0,
        };
        inv.slots[0] = ItemStack::new(ItemKind::Block(4), 12);
        inv.slots[3] = Some(ItemStack::one(ItemKind::Tool(0)));
        let mut app = ui_app(inv);

        assert_eq!(count::<InventoryScreen>(&mut app), 1);
        assert_eq!(count::<BackpackHint>(&mut app), 1);
        assert_eq!(count::<HeldItem>(&mut app), 1);
        assert_eq!(
            count::<SlotButton>(&mut app),
            soils_sim::Inventory::DEFAULT_SLOTS,
            "one clickable slot per inventory slot, filled or not"
        );
    }

    #[test]
    fn the_screen_is_hidden_until_the_inventory_mode_is_entered() {
        let mut app = ui_app(PlayerInventory::default());
        app.add_systems(Update, update_inventory_visibility);
        app.update();
        let mut q = app.world_mut().query_filtered::<&Visibility, With<InventoryScreen>>();
        assert_eq!(*q.iter(app.world()).next().unwrap(), Visibility::Hidden);

        app.world_mut().resource_mut::<NextState<UiMode>>().set(UiMode::Inventory);
        app.update();
        app.update();
        let mut q = app.world_mut().query_filtered::<&Visibility, With<InventoryScreen>>();
        assert_eq!(
            *q.iter(app.world()).next().unwrap(),
            Visibility::Inherited,
            "opening the inventory must show it"
        );
    }

    #[test]
    fn an_empty_inventory_has_an_empty_ring() {
        assert!(inv(vec![None; 4]).ring_items().is_empty());
        assert!(inv(Vec::new()).placeable().is_empty());
        assert_eq!(inv(Vec::new()).selected_block(), None);
    }
}