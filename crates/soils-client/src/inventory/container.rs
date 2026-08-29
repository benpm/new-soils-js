//! The open-container panel: a chest's contents, shown above the pack, and the
//! two clicks that move items between them.
//!
//! Like the hotbar, this holds no authority — but for the opposite reason. The
//! hotbar stores nothing, so there is nothing to be authoritative *about*; a
//! container stores a great deal, and all of it belongs to the server. So
//! [`OpenContainer`] is a mirror of the last `ContainerUpdate`, the panel is
//! open exactly while the server says it is, and every interaction sends an
//! intent rather than applying anything locally. There is no optimistic path:
//! two players can be in one chest, and a client that guessed at the outcome
//! would be wrong about half the time and quietly duplicate items the rest.

use bevy::prelude::*;
use soils_protocol::{ClientMsg, ItemStack, SlotRef};

use super::{ItemIcons, Items, PlayerInventory, screen::ItemButton};
use crate::chunk::Blocks;
use crate::net::NetClient;
use crate::theme;
use crate::ui::UiMode;

/// The container the server says this client has open, and what is in it.
///
/// `pos` is the authority on whether the panel is up. Nothing here is set
/// optimistically — asking to open a chest and having it open are different
/// events, a tick apart.
#[derive(Resource, Default)]
pub struct OpenContainer {
    pub pos: Option<IVec3>,
    pub slots: Vec<Option<ItemStack>>,
}

impl OpenContainer {
    pub fn is_open(&self) -> bool {
        self.pos.is_some()
    }
}

/// The panel's outer node, shown and hidden with [`OpenContainer::is_open`].
#[derive(Component)]
pub struct ContainerPanel;

/// Where the cells are rebuilt into.
#[derive(Component)]
pub struct ContainerGrid;

/// One container slot, by index into [`OpenContainer::slots`].
#[derive(Component, Clone, Copy)]
pub struct ContainerCell(pub u16);

/// Panel body, spawned into the inventory window between the header and the
/// pack. Built once and left in place — the cells inside it are what get
/// rebuilt.
pub fn spawn_panel(window: &mut ChildSpawnerCommands) {
    window
        .spawn((
            ContainerPanel,
            Visibility::Hidden,
            Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(8.0),
                padding: UiRect::axes(Val::Px(20.0), Val::Px(14.0)),
                border: UiRect::bottom(Val::Px(1.0)),
                ..default()
            },
            BackgroundColor(theme::BG_DEEP),
            BorderColor::all(theme::BORDER_MID),
        ))
        .with_children(|panel| {
            panel.spawn((
                Text::new("CONTAINER"),
                TextFont { font_size: theme::FONT_TINY.into(), ..default() },
                TextColor(theme::TEXT_DIM),
            ));
            panel.spawn((
                ContainerGrid,
                Node {
                    flex_direction: FlexDirection::Row,
                    flex_wrap: FlexWrap::Wrap,
                    column_gap: Val::Px(theme::SLOT_GAP_PX),
                    row_gap: Val::Px(theme::SLOT_GAP_PX),
                    ..default()
                },
            ));
        });
}

pub fn update_container_visibility(
    open: Res<OpenContainer>,
    mut panel: Query<&mut Visibility, With<ContainerPanel>>,
) {
    let want = if open.is_open() { Visibility::Inherited } else { Visibility::Hidden };
    for mut vis in &mut panel {
        if *vis != want {
            *vis = want;
        }
    }
}

/// Rebuild the cells whenever the server's copy changes.
///
/// Wholesale, for the same reason the pack is: a chest is a few dozen slots
/// changing at human speed, and a diff would be one more place for the view to
/// drift from the authority.
pub fn rebuild_container_ui(
    mut commands: Commands,
    open: Res<OpenContainer>,
    items: Res<Items>,
    blocks: Res<Blocks>,
    icons: Option<Res<ItemIcons>>,
    grid: Query<Entity, With<ContainerGrid>>,
) {
    if !open.is_changed() {
        return;
    }
    let Some(icons) = icons else { return };
    let Ok(grid) = grid.single() else { return };

    commands.entity(grid).despawn_related::<Children>();
    if !open.is_open() {
        return;
    }
    commands.entity(grid).with_children(|grid| {
        for (i, slot) in open.slots.iter().enumerate() {
            let tile = slot
                .and_then(|s| items.0.view(s.kind, &blocks.0))
                .map(|v| v.tile);
            cell(grid, &icons, i as u16, *slot, tile);
        }
    });
}

fn cell(
    parent: &mut ChildSpawnerCommands,
    icons: &ItemIcons,
    index: u16,
    stack: Option<ItemStack>,
    tile: Option<u8>,
) {
    let mut cell = parent.spawn((
        Button,
        ContainerCell(index),
        Node {
            width: Val::Px(theme::SLOT_PX),
            height: Val::Px(theme::SLOT_PX),
            border: UiRect::all(Val::Px(1.0)),
            ..default()
        },
        BackgroundColor(theme::BG_SLOT),
        BorderColor::all(theme::BORDER_DIM),
    ));
    cell.observe(withdraw_on_click);
    cell.with_children(|cell| {
        let Some(tile) = tile else { return };
        cell.spawn((
            icons.node(tile),
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
        if let Some(stack) = stack
            && stack.count > 1
        {
            cell.spawn((
                Text::new(stack.count.to_string()),
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

/// How many items a click means: one, or the whole stack with shift held.
///
/// Shift rather than a second button because the buttons are spoken for —
/// right-click already means "one of these" on both sides of the panel, and a
/// middle click is not a thing every mouse has.
fn amount(keys: &ButtonInput<KeyCode>, whole: u16) -> u16 {
    if keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]) { whole } else { 1 }
}

/// Take from the container: right-click for one, shift-click for the stack.
fn withdraw_on_click(
    click: On<Pointer<Click>>,
    net: Res<NetClient>,
    keys: Res<ButtonInput<KeyCode>>,
    open: Res<OpenContainer>,
    cells: Query<&ContainerCell>,
) {
    let Ok(cell) = cells.get(click.event_target()) else { return };
    let Some(stack) = open.slots.get(cell.0 as usize).copied().flatten() else { return };
    // A plain left click on a container cell is not a selection — there is
    // nothing to select — so only the two transfer gestures do anything.
    let shift = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
    if click.button != PointerButton::Secondary && !shift {
        return;
    }
    net.send(ClientMsg::TransferItem {
        from: SlotRef::Container(cell.0),
        count: amount(&keys, stack.count),
    });
}

/// Put into the container: right-click a pack cell for one, shift-click for
/// every slot holding that kind.
///
/// The pack is listed by *kind*, not by slot, so "all of it" may span several
/// server slots — hence one message per slot rather than one big count. The
/// server would clamp a count larger than the slot holds, so sending the total
/// would silently deposit only the first stack.
pub fn deposit_on_click(
    click: On<Pointer<Click>>,
    net: Res<NetClient>,
    keys: Res<ButtonInput<KeyCode>>,
    open: Res<OpenContainer>,
    inventory: Res<PlayerInventory>,
    cells: Query<&ItemButton>,
) {
    if !open.is_open() {
        return;
    }
    let Ok(cell) = cells.get(click.event_target()) else { return };
    let shift = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
    if click.button != PointerButton::Secondary && !shift {
        return;
    }
    for (i, slot) in inventory.slots.iter().enumerate() {
        let Some(stack) = slot.filter(|s| s.kind == cell.0) else { continue };
        net.send(ClientMsg::TransferItem {
            from: SlotRef::Pack(i as u16),
            count: amount(&keys, stack.count),
        });
        if !shift {
            return; // one item, from the first slot holding it
        }
    }
}

/// Leaving the inventory screen closes the container.
///
/// The server closes it too — on distance, on the block being broken, on
/// disconnect — so this is politeness rather than bookkeeping. It matters
/// because the open container pins a page in the server's block-data cache, and
/// a player who walks away without saying so holds it until the reach check
/// notices.
pub fn close_on_exit(
    mode: Res<State<UiMode>>,
    net: Res<NetClient>,
    open: Res<OpenContainer>,
    mut was_open: Local<bool>,
) {
    let showing = *mode.get() == UiMode::Inventory;
    if *was_open && !showing && open.is_open() {
        net.send(ClientMsg::CloseContainer);
    }
    *was_open = showing;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::test_support::{count, ui_app};
    use soils_protocol::ItemKind;

    fn stack(id: u8, n: u16) -> ItemStack {
        ItemStack::new(ItemKind::Block(id), n).unwrap()
    }

    /// Open a container on the app exactly as `ServerMsg::ContainerUpdate`
    /// does, and let the panel settle.
    fn open(app: &mut App, slots: Vec<Option<ItemStack>>) {
        let mut open = app.world_mut().resource_mut::<OpenContainer>();
        open.pos = Some(IVec3::ZERO);
        open.slots = slots;
        app.update();
    }

    fn panel_visible(app: &mut App) -> bool {
        app.world_mut()
            .query_filtered::<&Visibility, With<ContainerPanel>>()
            .iter(app.world())
            .any(|v| *v != Visibility::Hidden)
    }

    /// The panel is open exactly while the server says so. A test rather than a
    /// comment because "the client decides" is the tempting shortcut, and it is
    /// what makes two players in one chest go wrong.
    #[test]
    fn the_panel_follows_the_server_and_not_the_client() {
        let mut open = OpenContainer::default();
        assert!(!open.is_open());
        open.pos = Some(IVec3::new(1, 2, 3));
        open.slots = vec![Some(stack(4, 5)), None];
        assert!(open.is_open());
        open.pos = None;
        assert!(!open.is_open(), "a ContainerClosed leaves nothing showing");
    }

    #[test]
    fn the_panel_is_hidden_until_the_server_opens_one() {
        let mut app = ui_app(&[("Cobblestone", 64)]);
        assert!(!panel_visible(&mut app), "nothing is open on a fresh client");
        assert_eq!(count::<ContainerCell>(&mut app), 0);

        open(&mut app, vec![None; 27]);
        assert!(panel_visible(&mut app));
    }

    /// Empty slots draw too: a chest is a grid with holes in it, and skipping
    /// the holes would make a half-full chest look like a small one.
    #[test]
    fn every_slot_gets_a_cell_including_the_empty_ones() {
        let mut app = ui_app(&[("Cobblestone", 64)]);
        let mut slots = vec![None; 27];
        slots[0] = Some(stack(4, 12));
        slots[9] = Some(stack(5, 1));
        open(&mut app, slots);

        assert_eq!(count::<ContainerCell>(&mut app), 27);
    }

    #[test]
    fn a_close_takes_the_cells_down_with_the_panel() {
        let mut app = ui_app(&[("Cobblestone", 64)]);
        open(&mut app, vec![Some(stack(4, 3)); 9]);
        assert_eq!(count::<ContainerCell>(&mut app), 9);

        app.world_mut().resource_mut::<OpenContainer>().pos = None;
        app.update();
        assert!(!panel_visible(&mut app));
        assert_eq!(count::<ContainerCell>(&mut app), 0, "a closed panel leaves no cells behind");
    }

    #[test]
    fn shift_takes_the_stack_and_a_bare_click_takes_one() {
        let mut keys = ButtonInput::<KeyCode>::default();
        assert_eq!(amount(&keys, 40), 1);
        keys.press(KeyCode::ShiftLeft);
        assert_eq!(amount(&keys, 40), 40);
    }
}
