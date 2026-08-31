//! The full-screen inventory: everything carried, grouped into categories.
//!
//! Organised by what an item *is* rather than by which slot the server happens
//! to have packed it into. Slot positions were the only reason the old grid
//! existed, and they are not something the player can see or act on, so this
//! shows one cell per item kind — 128 Cobblestone is one entry reading 128, not
//! two reading 64 — under a row per category, with empty categories left out.
//!
//! Nothing here moves an item. An item a hotbar key points at is still listed,
//! drawn dim and wearing that key's number; assigning is
//! [`super::hotbar`]'s business and costs no message.
//!
//! Layout follows the mockup in `scratch/`. Two of its touches do not survive
//! the port and are not attempted: chamfered corners (`clip-path` has no
//! `bevy_ui` equivalent) and emoji category icons (the bundled font is a
//! FiraMono subset with no emoji coverage — see the note on `BackpackHint`).
//! Categories use an atlas tile from a representative block instead, which
//! costs no new art.

use bevy::prelude::*;
use soils_protocol::{Category, ItemKind};

use super::hotbar::{DragItem, Hotbar};
use super::{ItemIcons, Items, PlayerInventory};
use crate::chunk::Blocks;
use crate::theme;
use crate::ui::UiMode;

/// Which item the detail panel is describing, and what the number keys bind.
#[derive(Resource, Default)]
pub struct SelectedItem(pub Option<ItemKind>);

/// Root of the full-screen panel.
#[derive(Component)]
pub struct InventoryScreen;
/// The scrolling column of category rows, rebuilt when anything changes.
#[derive(Component)]
pub struct CategoryList;
/// One category's row, carrying the category it lists.
///
/// The payload is read only by the tests — which is the point of carrying it:
/// "a row exists for every non-empty category and no others" is not something
/// an entity count can check.
#[derive(Component, Clone, Copy)]
pub struct CategoryRow(#[allow(dead_code)] pub Category);
/// The `N items - M kg` readout in the header.
#[derive(Component)]
pub struct InventoryTotals;
/// The `N/M categories` readout in the footer.
#[derive(Component)]
pub struct CategoryCount;
/// The detail panel body.
#[derive(Component)]
pub struct DetailPanel;
/// One item cell, carrying the kind it shows.
#[derive(Component, Clone, Copy)]
pub struct ItemButton(pub ItemKind);
/// The "press E" backpack affordance.
#[derive(Component)]
pub struct BackpackHint;

/// The key-hint line, which changes when a container is open.
#[derive(Component)]
pub struct FooterHint;

const HINT_PLAIN: &str = "[1-8] assign to a key   [Q] drop one   [E] close";
const HINT_CONTAINER: &str =
    "[RMB] move one   [SHIFT+click] move all   [1-8] assign   [E] close";

/// Swap the hint line for the container bindings while a chest is open, so the
/// two gestures that only exist then are discoverable at the moment they work.
pub fn update_footer_hint(
    open: Res<super::container::OpenContainer>,
    mut hint: Query<&mut Text, With<FooterHint>>,
) {
    if !open.is_changed() {
        return;
    }
    let want = if open.is_open() { HINT_CONTAINER } else { HINT_PLAIN };
    for mut text in &mut hint {
        if text.0 != want {
            text.0 = want.into();
        }
    }
}

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
                // Clear of the hotbar, which stays up as the drop target.
                padding: UiRect::bottom(Val::Px(90.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.62)),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    width: Val::Px(theme::WINDOW_PX),
                    max_height: Val::Percent(76.0),
                    border: UiRect::all(Val::Px(1.0)),
                    ..default()
                },
                BackgroundColor(theme::BG_PANEL),
                BorderColor::all(theme::BORDER_MID),
            ))
            .with_children(|window| {
                header(window);
                divider(window);
                // Above the pack, so a transfer reads top-to-bottom: what is in
                // the box, then what is on you.
                super::container::spawn_panel(window);
                body(window);
                divider(window);
                footer(window);
            });
        });

    backpack_hint(&mut commands);
}

fn divider(parent: &mut ChildSpawnerCommands) {
    parent.spawn((
        Node { height: Val::Px(1.0), width: Val::Percent(100.0), ..default() },
        BackgroundColor(theme::BORDER_MID),
    ));
}

fn header(parent: &mut ChildSpawnerCommands) {
    parent
        .spawn(Node {
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            padding: UiRect::axes(Val::Px(20.0), Val::Px(12.0)),
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Text::new("INVENTORY"),
                TextFont { font_size: theme::FONT_TITLE.into(), ..default() },
                TextColor(theme::AMBER),
            ));
            row.spawn((
                InventoryTotals,
                Text::new(""),
                TextFont { font_size: theme::FONT_SMALL.into(), ..default() },
                TextColor(theme::TEXT_MUTED),
            ));
        });
}

fn body(parent: &mut ChildSpawnerCommands) {
    parent
        .spawn(Node { flex_grow: 1.0, min_height: Val::Px(0.0), ..default() })
        .with_children(|body| {
            body.spawn((
                CategoryList,
                bevy::ui_widgets::ScrollArea,
                Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(18.0),
                    padding: UiRect::axes(Val::Px(20.0), Val::Px(16.0)),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
            ));
            body.spawn((
                Node {
                    width: Val::Px(theme::DETAIL_PX),
                    flex_shrink: 0.0,
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(10.0),
                    padding: UiRect::axes(Val::Px(14.0), Val::Px(16.0)),
                    border: UiRect::left(Val::Px(1.0)),
                    ..default()
                },
                BorderColor::all(theme::BORDER_DIM),
            ))
            .with_children(|panel| {
                panel.spawn((
                    Text::new("DETAIL"),
                    TextFont { font_size: theme::FONT_TINY.into(), ..default() },
                    TextColor(theme::TEXT_DIM),
                ));
                panel.spawn((
                    DetailPanel,
                    Node { flex_direction: FlexDirection::Column, ..default() },
                ));
            });
        });
}

fn footer(parent: &mut ChildSpawnerCommands) {
    parent
        .spawn(Node {
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            padding: UiRect::axes(Val::Px(20.0), Val::Px(8.0)),
            ..default()
        })
        .with_children(|row| {
            // ASCII only: the bundled font is a FiraMono subset with no U+00B7,
            // so a middot renders as a tofu box. Caught once in a recording, in
            // the very UI the recording was of.
            row.spawn((
                FooterHint,
                Text::new(HINT_PLAIN),
                TextFont { font_size: theme::FONT_TINY.into(), ..default() },
                TextColor(theme::TEXT_DIM),
            ));
            row.spawn((
                CategoryCount,
                Text::new(""),
                TextFont { font_size: theme::FONT_TINY.into(), ..default() },
                TextColor(theme::TEXT_FAINT),
            ));
        });
}

/// The backpack affordance, bottom-left, as the design note asks: a pack with a
/// circled "E" on it.
///
/// Drawn from nodes rather than set as an emoji — the bundled font has no emoji
/// coverage, so a "backpack" glyph renders as nothing at all. The circled E is
/// the part that actually teaches the binding.
fn backpack_hint(commands: &mut Commands) {
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
                    ..default()
                },
                BackgroundColor(Color::srgb(0.30, 0.22, 0.13)),
            ));
            // Body of the pack.
            hint.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(7.0),
                    width: Val::Px(38.0),
                    height: Val::Px(33.0),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.40, 0.29, 0.17)),
            ));
            // Front pocket, so it reads as a bag and not a box.
            hint.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(7.0),
                    top: Val::Px(22.0),
                    width: Val::Px(24.0),
                    height: Val::Px(12.0),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.28, 0.20, 0.11)),
            ));
            // The circled key hint, overlapping the corner.
            hint.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(-7.0),
                    top: Val::Px(-2.0),
                    width: Val::Px(19.0),
                    height: Val::Px(19.0),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Percent(50.0)),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    ..default()
                },
                BorderColor::all(theme::AMBER),
                BackgroundColor(theme::BG_DEEP),
            ))
            .with_children(|c| {
                c.spawn((
                    Text::new("E"),
                    TextFont { font_size: theme::FONT_SMALL.into(), ..default() },
                    TextColor(theme::AMBER),
                ));
            });
        });
}

/// Show the screen only in [`UiMode::Inventory`]; the backpack hint is the
/// thing that advertises it, so it goes away while it is up.
pub fn update_inventory_visibility(
    mode: Res<State<UiMode>>,
    mut screen: Query<&mut Visibility, With<InventoryScreen>>,
    mut hint: Query<&mut Visibility, (With<BackpackHint>, Without<InventoryScreen>)>,
) {
    let open = *mode.get() == UiMode::Inventory;
    let set = |vis: &mut Visibility, want: Visibility| {
        if *vis != want {
            *vis = want;
        }
    };
    for mut vis in &mut screen {
        set(&mut vis, if open { Visibility::Inherited } else { Visibility::Hidden });
    }
    for mut vis in &mut hint {
        set(&mut vis, if open { Visibility::Hidden } else { Visibility::Inherited });
    }
}

/// An item's row, resolved once so the rebuild does not walk the registry twice.
struct Entry {
    kind: ItemKind,
    count: u32,
    tile: u8,
    /// The hotbar key pointing at this item, if one does.
    on_key: Option<usize>,
}

/// Rebuild the category rows.
///
/// Wholesale rather than diffed: an inventory is a few dozen kinds and changes
/// at human speed, so the simplest correct thing is fast enough, and a diff
/// would be one more place for the view to drift from the authority. The
/// hotbar's slots are the exception and persist — see [`super::hotbar`].
pub fn rebuild_inventory_ui(
    mut commands: Commands,
    inventory: Res<PlayerInventory>,
    hotbar: Res<Hotbar>,
    items: Res<Items>,
    blocks: Res<Blocks>,
    icons: Option<Res<ItemIcons>>,
    list: Query<Entity, With<CategoryList>>,
    mut totals: Query<&mut Text, (With<InventoryTotals>, Without<CategoryCount>)>,
    mut counts: Query<&mut Text, (With<CategoryCount>, Without<InventoryTotals>)>,
) {
    if !inventory.is_changed() && !hotbar.is_changed() {
        return;
    }
    let Some(icons) = icons else { return };
    let Ok(list) = list.single() else { return };

    // Group by category, keeping inventory order within each row.
    let mut rows: Vec<(Category, Vec<Entry>)> =
        Category::ALL.iter().map(|c| (*c, Vec::new())).collect();
    let (mut total_items, mut total_kg) = (0u32, 0.0f32);
    for (kind, count) in inventory.by_kind() {
        // An item this build cannot name is one the server knows about and we
        // do not. Skipping it is the only honest option; drawing a nameless
        // placeholder would imply we know what it is.
        let Some(view) = items.0.view(kind, &blocks.0) else { continue };
        total_items += count;
        total_kg += view.weight * count as f32;
        let row = rows
            .iter_mut()
            .find(|(c, _)| *c == view.class.category)
            .expect("Category::ALL covers every category");
        row.1.push(Entry { kind, count, tile: view.tile, on_key: hotbar.slot_of(kind) });
    }

    if let Ok(mut text) = totals.single_mut() {
        text.0 = format!("{total_items} items - {total_kg:.1} kg");
    }
    if let Ok(mut text) = counts.single_mut() {
        let filled = rows.iter().filter(|(_, e)| !e.is_empty()).count();
        text.0 = format!("{filled}/{} categories", Category::ALL.len());
    }

    commands.entity(list).despawn_related::<Children>();
    commands.entity(list).with_children(|parent| {
        for (category, entries) in rows.iter().filter(|(_, e)| !e.is_empty()) {
            category_row(parent, &icons, *category, entries, category_tile(&blocks.0, *category));
        }
    });
}

/// A tile to stand for a category: the first block that belongs to it. Derived
/// rather than hard-coded so adding a block to an empty category gives that row
/// an icon for free, and reordering `blocks.yaml` cannot leave a stale index.
fn category_tile(blocks: &soils_worldgen::BlockRegistry, category: Category) -> Option<u8> {
    (1..blocks.len() as u8)
        .filter_map(|id| blocks.get(id))
        .find(|def| def.class.category == category)
        .map(|def| def.faces[1])
}

fn category_row(
    parent: &mut ChildSpawnerCommands,
    icons: &ItemIcons,
    category: Category,
    entries: &[Entry],
    tile: Option<u8>,
) {
    parent
        .spawn((
            CategoryRow(category),
            Node { column_gap: Val::Px(12.0), align_items: AlignItems::FlexStart, ..default() },
        ))
        .with_children(|row| {
            row.spawn((
                Node {
                    width: Val::Px(theme::CATEGORY_ICON_PX),
                    height: Val::Px(theme::CATEGORY_ICON_PX),
                    flex_shrink: 0.0,
                    margin: UiRect::top(Val::Px(8.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    ..default()
                },
                BackgroundColor(theme::BG_SLOT),
                BorderColor::all(theme::BORDER_DIM),
            ))
            .with_children(|icon| {
                if let Some(tile) = tile {
                    icon.spawn((
                        icons.node(tile),
                        Node { width: Val::Px(24.0), height: Val::Px(24.0), ..default() },
                        Pickable::IGNORE,
                    ));
                }
            });

            row.spawn(Node { flex_direction: FlexDirection::Column, row_gap: Val::Px(6.0), flex_grow: 1.0, ..default() })
                .with_children(|column| {
                    column.spawn((
                        Text::new(category.label().to_uppercase()),
                        TextFont { font_size: theme::FONT_TINY.into(), ..default() },
                        TextColor(theme::TEXT_DIM),
                    ));
                    column
                        .spawn(Node {
                            flex_wrap: FlexWrap::Wrap,
                            column_gap: Val::Px(theme::SLOT_GAP_PX),
                            row_gap: Val::Px(theme::SLOT_GAP_PX),
                            ..default()
                        })
                        .with_children(|grid| {
                            for entry in entries {
                                item_cell(grid, icons, entry);
                            }
                        });
                });
        });
}

fn item_cell(parent: &mut ChildSpawnerCommands, icons: &ItemIcons, entry: &Entry) {
    let bound = entry.on_key.is_some();
    parent
        .spawn((
            Button,
            ItemButton(entry.kind),
            Node {
                width: Val::Px(theme::SLOT_PX),
                height: Val::Px(theme::SLOT_PX),
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BackgroundColor(theme::BG_SLOT),
            BorderColor::all(if bound { theme::AMBER_DIM } else { theme::BORDER_DIM }),
        ))
        .observe(start_drag)
        .observe(super::container::deposit_on_click)
        .with_children(|cell| {
            let icon = if bound { icons.silhouette(entry.tile) } else { icons.node(entry.tile) };
            cell.spawn((
                icon,
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px((theme::SLOT_PX - theme::ICON_PX) / 2.0),
                    top: Val::Px((theme::SLOT_PX - theme::ICON_PX) / 2.0),
                    width: Val::Px(theme::ICON_PX),
                    height: Val::Px(theme::ICON_PX),
                    ..default()
                },
                Pickable::IGNORE,
            ));
            if let Some(key) = entry.on_key {
                // The badge, not the dimming, is what says *which* key holds
                // it — and with opaque atlas tiles it is the clearer signal of
                // the two.
                cell.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(3.0),
                        top: Val::Px(3.0),
                        width: Val::Px(14.0),
                        height: Val::Px(14.0),
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        ..default()
                    },
                    BackgroundColor(theme::BADGE_BG),
                    BorderColor::all(theme::AMBER_DIM),
                    Pickable::IGNORE,
                ))
                .with_children(|badge| {
                    badge.spawn((
                        Text::new((key + 1).to_string()),
                        TextFont { font_size: 9.0.into(), ..default() },
                        TextColor(theme::AMBER),
                    ));
                });
            }
            if entry.count > 1 {
                cell.spawn((
                    Text::new(entry.count.to_string()),
                    TextFont { font_size: theme::FONT_BODY.into(), ..default() },
                    TextColor(theme::TEXT_PRIMARY),
                    Node {
                        position_type: PositionType::Absolute,
                        right: Val::Px(4.0),
                        bottom: Val::Px(2.0),
                        ..default()
                    },
                    Pickable::IGNORE,
                ));
            }
        });
}

/// Picking a cell up names it as the drag payload; the hotbar slot's own
/// observer is what consumes it.
fn start_drag(drag: On<Pointer<DragStart>>, mut item: ResMut<DragItem>, cells: Query<&ItemButton>) {
    if let Ok(cell) = cells.get(drag.event_target()) {
        item.0 = Some(cell.0);
    }
}

/// Clicking a cell selects it: the detail panel describes it and `1`-`8` assign
/// it to a key.
pub fn select_item(
    mut selected: ResMut<SelectedItem>,
    cells: Query<(&Interaction, &ItemButton), Changed<Interaction>>,
) {
    for (interaction, cell) in &cells {
        if *interaction == Interaction::Pressed {
            selected.0 = if selected.0 == Some(cell.0) { None } else { Some(cell.0) };
        }
    }
}

/// Hover and selection feedback.
///
/// Runs every frame rather than on `Changed<Interaction>`: the rows are rebuilt
/// wholesale whenever the pack changes, and a fresh entity has no interaction
/// change to react to, so a change-filtered version loses the highlight under
/// the pointer every time an item is picked up.
pub fn highlight_item_cells(
    selected: Res<SelectedItem>,
    hotbar: Res<Hotbar>,
    mut cells: Query<(&Interaction, &ItemButton, &mut BackgroundColor, &mut BorderColor)>,
) {
    for (interaction, cell, mut background, mut border) in &mut cells {
        let chosen = selected.0 == Some(cell.0);
        let hovered = *interaction != Interaction::None;
        let bound = hotbar.slot_of(cell.0).is_some();
        *background = BackgroundColor(match (chosen, hovered) {
            (true, _) => theme::BG_SLOT_SELECTED,
            (_, true) => theme::BG_SLOT_HOVER,
            _ => theme::BG_SLOT,
        });
        *border = BorderColor::all(match (chosen, hovered, bound) {
            (true, ..) => theme::AMBER,
            (_, true, _) => theme::BORDER_BRIGHT,
            (_, _, true) => theme::AMBER_DIM,
            _ => theme::BORDER_DIM,
        });
    }
}

/// A selection outlives the item it named — spend the last of a stack and it is
/// describing something the player no longer has.
pub fn forget_missing_selection(
    inventory: Res<PlayerInventory>,
    mut selected: ResMut<SelectedItem>,
) {
    if !inventory.is_changed() {
        return;
    }
    if selected.0.is_some_and(|kind| !inventory.holds(kind)) {
        selected.0 = None;
    }
}

pub fn rebuild_detail_panel(
    mut commands: Commands,
    selected: Res<SelectedItem>,
    inventory: Res<PlayerInventory>,
    hotbar: Res<Hotbar>,
    items: Res<Items>,
    blocks: Res<Blocks>,
    icons: Option<Res<ItemIcons>>,
    panel: Query<Entity, With<DetailPanel>>,
) {
    if !selected.is_changed() && !inventory.is_changed() && !hotbar.is_changed() {
        return;
    }
    let Some(icons) = icons else { return };
    let Ok(panel) = panel.single() else { return };
    commands.entity(panel).despawn_related::<Children>();

    let view = selected.0.and_then(|kind| items.0.view(kind, &blocks.0).map(|v| (kind, v)));
    let Some((kind, view)) = view else {
        commands.entity(panel).with_children(|c| {
            c.spawn((
                Text::new("Select an item to inspect"),
                TextFont { font_size: theme::FONT_SMALL.into(), ..default() },
                TextColor(theme::TEXT_FAINT),
            ));
        });
        return;
    };

    let count = inventory.total_of(kind);
    let key = hotbar.slot_of(kind);
    commands.entity(panel).with_children(|c| {
        c.spawn((
            icons.node(view.tile),
            Node {
                width: Val::Px(44.0),
                height: Val::Px(44.0),
                margin: UiRect::bottom(Val::Px(8.0)),
                ..default()
            },
        ));
        c.spawn((
            Text::new(view.class.category.label().to_uppercase()),
            TextFont { font_size: 9.0.into(), ..default() },
            TextColor(theme::AMBER),
        ));
        c.spawn((
            Text::new(view.name.to_string()),
            TextFont { font_size: theme::FONT_BODY.into(), ..default() },
            TextColor(theme::TEXT_PRIMARY),
            Node { margin: UiRect::bottom(Val::Px(10.0)), ..default() },
        ));
        for (label, value) in [
            ("QTY", format!("x{count}")),
            ("WT", format!("{:.1} kg", view.weight * count as f32)),
            ("KEY", key.map_or_else(|| "-".to_string(), |k| (k + 1).to_string())),
        ] {
            c.spawn(Node {
                justify_content: JustifyContent::SpaceBetween,
                margin: UiRect::bottom(Val::Px(4.0)),
                ..default()
            })
            .with_children(|line| {
                line.spawn((
                    Text::new(label),
                    TextFont { font_size: theme::FONT_TINY.into(), ..default() },
                    TextColor(theme::TEXT_DIM),
                ));
                line.spawn((
                    Text::new(value),
                    TextFont { font_size: theme::FONT_TINY.into(), ..default() },
                    TextColor(theme::TEXT_MUTED),
                ));
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::test_support::{block, count, stock, ui_app};
    use crate::inventory::PlayerInventory;

    /// The pack the tests read: five kinds over four categories, one of them
    /// split across two stacks.
    const PACK: [(&str, u16); 6] = [
        ("Cobblestone", 64),
        ("Cobblestone", 64),
        ("Slate", 9),
        ("Dirt", 30),
        ("Log", 4),
        ("Iron Ore", 2),
    ];

    fn rows(app: &mut App) -> Vec<Category> {
        let mut q = app.world_mut().query::<&CategoryRow>();
        q.iter(app.world()).map(|r| r.0).collect()
    }

    fn listed(app: &mut App) -> Vec<ItemKind> {
        let mut q = app.world_mut().query::<&ItemButton>();
        q.iter(app.world()).map(|c| c.0).collect()
    }

    #[test]
    fn a_row_appears_for_every_non_empty_category_and_no_others() {
        let mut app = ui_app(&PACK);
        let mut shown = rows(&mut app);
        shown.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(shown, vec![Category::Earth, Category::Ore, Category::Organic, Category::Stone]);
        assert!(
            !shown.contains(&Category::Consumable),
            "a category holding nothing must not take up a row"
        );
    }

    /// The categorized analogue of the item-void bug: an item that belongs to
    /// no row is gone from the player's view while still in the pack, and a
    /// test that only counted cells would not notice.
    #[test]
    fn every_kind_held_is_listed_exactly_once() {
        let mut app = ui_app(&PACK);
        let shown = listed(&mut app);
        let held = app.world().resource::<PlayerInventory>().kinds();

        assert_eq!(shown.len(), held.len(), "one cell per kind, no more and no fewer");
        for kind in &held {
            assert_eq!(
                shown.iter().filter(|k| *k == kind).count(),
                1,
                "{kind:?} must appear exactly once — twice would be slot packing leaking through"
            );
        }
    }

    /// The requirement, on the screen side: an item a key points at is still
    /// *there*. It is drawn differently, not taken away.
    #[test]
    fn an_assigned_item_is_still_listed() {
        let mut app = ui_app(&PACK);
        let cobble = block(&app, "Cobblestone");
        {
            let mut hotbar = app.world_mut().resource_mut::<Hotbar>();
            hotbar.bind(2, cobble, None);
        }
        app.update();

        assert!(listed(&mut app).contains(&cobble), "assigning must not remove it from the pack");
        assert_eq!(app.world().resource::<Hotbar>().slot_of(cobble), Some(2), "and it is badged");
        assert_eq!(
            app.world().resource::<PlayerInventory>().total_of(cobble),
            128,
            "nor take any of it"
        );
    }

    #[test]
    fn the_screen_is_hidden_until_the_inventory_mode_is_entered() {
        let mut app = ui_app(&PACK);
        assert_eq!(count::<InventoryScreen>(&mut app), 1);
        assert_eq!(count::<BackpackHint>(&mut app), 1);

        let visibility = |app: &mut App| {
            let mut q = app.world_mut().query_filtered::<&Visibility, With<InventoryScreen>>();
            *q.iter(app.world()).next().expect("the screen exists")
        };
        assert_eq!(visibility(&mut app), Visibility::Hidden);

        app.world_mut().resource_mut::<NextState<UiMode>>().set(UiMode::Inventory);
        app.update();
        app.update();
        assert_eq!(visibility(&mut app), Visibility::Inherited, "opening it must show it");
    }

    /// A selection outlives the item it named unless something forgets it, and
    /// the detail panel would then describe a thing the player does not have.
    #[test]
    fn a_selection_is_dropped_when_its_item_runs_out() {
        let mut app = ui_app(&PACK);
        app.add_systems(Update, forget_missing_selection);
        let log = block(&app, "Log");
        app.world_mut().resource_mut::<SelectedItem>().0 = Some(log);
        app.update();
        assert_eq!(app.world().resource::<SelectedItem>().0, Some(log));

        stock(&mut app, &[("Dirt", 30)]);
        app.update();
        assert_eq!(app.world().resource::<SelectedItem>().0, None);
    }

    /// An empty pack must still build: no rows, no cells, and nothing panicking
    /// on the way through.
    #[test]
    fn an_empty_pack_draws_an_empty_screen() {
        let mut app = ui_app(&[]);
        assert!(rows(&mut app).is_empty());
        assert!(listed(&mut app).is_empty());
        assert_eq!(count::<InventoryScreen>(&mut app), 1);
    }
}
