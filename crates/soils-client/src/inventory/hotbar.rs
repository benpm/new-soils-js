//! Eight references into the inventory, along the bottom of the screen.
//!
//! The hotbar is **not** storage. A slot holds an [`ItemKind`], never an
//! [`soils_protocol::ItemStack`]: putting Cobblestone on key 1 moves nothing,
//! sends nothing, and leaves the stack exactly where the server put it. The
//! inventory screen still lists that item — dimmed, wearing a badge that names
//! the key holding it.
//!
//! Binding by *kind* rather than by slot index is what makes the rest of this
//! work. Slot indices are not stable: the server merges, splits and relocates
//! stacks, and one kind routinely spans several slots (the starter kit is 128
//! of each, against a 64 stack cap). A kind is the thing the player actually
//! means.
//!
//! ## A slot heals itself
//!
//! When the item a slot points at runs out, the slot rebinds to another item of
//! the same [`ItemClass`] — same category, function and effect. Eat the last
//! Large Fruit and the key holds some other healing consumable; spend the last
//! Cobblestone and it holds another building stone. With nothing suitable in
//! the pack the slot goes empty and *says so*: pressing its key wiggles it
//! rather than silently doing nothing.
//!
//! Crucially the substitution never crosses a class. A spent stack of stone is
//! not replaced by leaves merely because leaves are unassigned.

use bevy::prelude::*;
use soils_protocol::{ClientMsg, ItemClass, ItemKind};
use soils_sim::ItemRegistry;
use soils_worldgen::BlockRegistry;

use super::{ItemIcons, Items, PlayerInventory};
use crate::chunk::Blocks;
use crate::net::NetClient;
use crate::theme;
use crate::ui::UiMode;

/// Slots on the bar, matching the design mockup.
pub const HOTBAR_SLOTS: usize = 8;

/// The key for each slot.
pub const HOTBAR_KEYS: [KeyCode; HOTBAR_SLOTS] = [
    KeyCode::Digit1,
    KeyCode::Digit2,
    KeyCode::Digit3,
    KeyCode::Digit4,
    KeyCode::Digit5,
    KeyCode::Digit6,
    KeyCode::Digit7,
    KeyCode::Digit8,
];

/// How long a dead slot's protest lasts.
const WIGGLE_SECS: f32 = 0.35;
/// How far it swings, in pixels.
const WIGGLE_PX: f32 = 5.0;
/// Angular rate, radians per second — about three and a half cycles over
/// [`WIGGLE_SECS`], which reads as a shake rather than a slide.
const WIGGLE_RATE: f32 = 62.0;

/// One position on the bar.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HotbarSlot {
    /// The kind this slot points at, for as long as the inventory holds one.
    pub bound: Option<ItemKind>,
    /// What this slot is *for*. Set the moment the slot first holds something,
    /// and kept when [`Self::bound`] empties, so the slot waits for a like item
    /// instead of grabbing whatever happens to be unassigned.
    ///
    /// `None` means the slot has never held anything — only then will it take
    /// an arbitrary item, which is what stops a new player facing eight blanks
    /// over a pack the server just filled.
    pub want: Option<ItemClass>,
}

#[derive(Resource)]
pub struct Hotbar {
    pub slots: [HotbarSlot; HOTBAR_SLOTS],
    /// Which key is live. Always a valid index — the bar is a fixed array, so
    /// unlike the old `placeable()` index this can never go stale.
    pub selected: usize,
}

impl Default for Hotbar {
    fn default() -> Self {
        Self { slots: [HotbarSlot::default(); HOTBAR_SLOTS], selected: 0 }
    }
}

impl Hotbar {
    /// Which key points at `kind`, if any. Drives the inventory's dimmed icon
    /// and its badge.
    pub fn slot_of(&self, kind: ItemKind) -> Option<usize> {
        self.slots.iter().position(|s| s.bound == Some(kind))
    }

    /// What the live key points at.
    pub fn selected_kind(&self) -> Option<ItemKind> {
        self.slots[self.selected].bound
    }

    /// The block right-click would place. Replaces
    /// `PlayerInventory::selected_block`.
    pub fn selected_block(&self) -> Option<u8> {
        self.selected_kind()?.block()
    }

    /// Point `i` at `kind`.
    ///
    /// Any other slot pointing at the same kind gives it up: one item on two
    /// keys would leave one of them dead the moment the item ran out, and the
    /// player would have no way to tell which.
    pub fn bind(&mut self, i: usize, kind: ItemKind, class: Option<ItemClass>) {
        if i >= HOTBAR_SLOTS {
            return;
        }
        for slot in &mut self.slots {
            if slot.bound == Some(kind) {
                slot.bound = None;
            }
        }
        self.slots[i] = HotbarSlot { bound: Some(kind), want: class };
    }

    /// Empty `i`, remembering what it held so it refills with a like item
    /// rather than the first thing to hand.
    pub fn clear(&mut self, i: usize, class: Option<ItemClass>) {
        let Some(slot) = self.slots.get_mut(i) else { return };
        slot.want = slot.want.or(class);
        slot.bound = None;
    }

    /// Bring the bar back in line with what the player is actually carrying.
    ///
    /// Pure, and the whole substitution rule: everything below this line is
    /// just drawing it. Two passes, because a slot can only be refilled once
    /// every slot has let go of what it lost — otherwise the first slot in the
    /// array would take a replacement the second one had a better claim to.
    pub fn reconcile(&mut self, inv: &PlayerInventory, items: &ItemRegistry, blocks: &BlockRegistry) {
        // Pass 1: let go of what is gone, remembering what the slot was for.
        for slot in &mut self.slots {
            let Some(kind) = slot.bound else { continue };
            if inv.holds(kind) {
                continue;
            }
            slot.want = items.class_of(kind, blocks).or(slot.want);
            slot.bound = None;
        }

        // Pass 2: fill the empties in key order, never handing one kind to two
        // keys. `kinds()` is in inventory-slot order, so the choice is
        // deterministic and testable rather than whatever iteration produced.
        let held = inv.kinds();
        for i in 0..HOTBAR_SLOTS {
            if self.slots[i].bound.is_some() {
                continue;
            }
            let want = self.slots[i].want;
            let pick = held.iter().copied().find(|kind| {
                if self.slots.iter().any(|s| s.bound == Some(*kind)) {
                    return false;
                }
                match want {
                    Some(w) => items.class_of(*kind, blocks) == Some(w),
                    None => true,
                }
            });
            self.slots[i].bound = pick;
        }
    }
}

/// The item currently being dragged out of the inventory screen.
///
/// Never cleared on drag end: a stale kind here is harmless because
/// `Pointer<DragDrop>` only fires during a real drag, and clearing it would
/// race the drop — `DragEnd` and `DragDrop` both land on pointer release, on
/// different entities, in no guaranteed order.
#[derive(Resource, Default)]
pub struct DragItem(pub Option<ItemKind>);

/// Root of the bar.
#[derive(Component)]
pub struct HotbarBar;

/// One slot on the bar, carrying its key index. Spawned once and kept: only
/// its children are rebuilt, so an in-flight [`Wiggle`] survives the inventory
/// changing underneath it.
#[derive(Component, Clone, Copy)]
pub struct HotbarSlotNode(pub usize);

/// A dead slot protesting a keypress.
#[derive(Component, Default)]
pub struct Wiggle {
    t: f32,
}

pub fn setup_hotbar(mut commands: Commands) {
    commands
        .spawn((
            HotbarBar,
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(0.0),
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                padding: UiRect::new(Val::Px(0.0), Val::Px(0.0), Val::Px(10.0), Val::Px(0.0)),
                ..default()
            },
        ))
        .with_children(|bar| {
            bar.spawn((
                Node {
                    column_gap: Val::Px(theme::HOTBAR_GAP_PX),
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(8.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    ..default()
                },
                BackgroundColor(theme::BG_HOTBAR),
                BorderColor::all(theme::BORDER_MID),
            ))
            .with_children(|strip| {
                for i in 0..HOTBAR_SLOTS {
                    strip
                        .spawn((
                            HotbarSlotNode(i),
                            Node {
                                width: Val::Px(theme::SLOT_PX),
                                height: Val::Px(theme::SLOT_PX),
                                border: UiRect::all(Val::Px(1.0)),
                                ..default()
                            },
                            UiTransform::default(),
                            BackgroundColor(theme::BG_SLOT),
                            BorderColor::all(theme::BORDER_DIM),
                        ))
                        .observe(bind_on_drop)
                        .observe(clear_on_right_click);
                }
            });
        });
}

/// Dropping an inventory item on a slot points that key at it. No `MoveItem`:
/// the stack does not go anywhere.
fn bind_on_drop(
    drop: On<Pointer<DragDrop>>,
    mut hotbar: ResMut<Hotbar>,
    drag: Res<DragItem>,
    items: Res<Items>,
    blocks: Res<Blocks>,
    slots: Query<&HotbarSlotNode>,
) {
    let Some(kind) = drag.0 else { return };
    let Ok(slot) = slots.get(drop.event_target()) else { return };
    hotbar.bind(slot.0, kind, items.0.class_of(kind, &blocks.0));
}

/// Right-clicking a slot gives up the key.
///
/// Gated on the screen being open, and not incidentally: with the pointer
/// locked during play its position is frozen wherever it last was, so a
/// right-click to place a block would clear a slot every time it happened to be
/// frozen over the bar.
fn clear_on_right_click(
    click: On<Pointer<Click>>,
    mode: Res<State<UiMode>>,
    mut hotbar: ResMut<Hotbar>,
    items: Res<Items>,
    blocks: Res<Blocks>,
    slots: Query<&HotbarSlotNode>,
) {
    if click.button != PointerButton::Secondary || *mode.get() != UiMode::Inventory {
        return;
    }
    let Ok(slot) = slots.get(click.event_target()) else { return };
    let class = hotbar.slots[slot.0].bound.and_then(|k| items.0.class_of(k, &blocks.0));
    hotbar.clear(slot.0, class);
}

/// Re-run the substitution rule whenever what the player carries changes.
pub fn reconcile_hotbar(
    inventory: Res<PlayerInventory>,
    items: Res<Items>,
    blocks: Res<Blocks>,
    mut hotbar: ResMut<Hotbar>,
) {
    if !inventory.is_changed() {
        return;
    }
    // `reconcile` is idempotent, so bypass change detection and only mark the
    // resource when it actually moved — otherwise every inventory tick would
    // force a full hotbar redraw.
    let mut next = Hotbar { slots: hotbar.slots, selected: hotbar.selected };
    next.reconcile(&inventory, &items.0, &blocks.0);
    if next.slots != hotbar.slots {
        hotbar.slots = next.slots;
    }
}

/// Redraw the slots' contents. The slot entities themselves persist.
pub fn rebuild_hotbar(
    mut commands: Commands,
    hotbar: Res<Hotbar>,
    inventory: Res<PlayerInventory>,
    items: Res<Items>,
    blocks: Res<Blocks>,
    icons: Option<Res<ItemIcons>>,
    mut slots: Query<(Entity, &HotbarSlotNode, &mut BorderColor, &mut BackgroundColor)>,
) {
    if !hotbar.is_changed() && !inventory.is_changed() {
        return;
    }
    let Some(icons) = icons else { return };

    for (entity, slot, mut border, mut background) in &mut slots {
        let live = slot.0 == hotbar.selected;
        *border = BorderColor::all(if live { theme::BORDER_BRIGHT } else { theme::BORDER_DIM });
        *background = BackgroundColor(if live { theme::BG_SLOT_SELECTED } else { theme::BG_SLOT });

        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|c| {
            // The key number, always shown — an empty slot still has a key.
            c.spawn((
                Text::new((slot.0 + 1).to_string()),
                TextFont { font_size: 9.0.into(), ..default() },
                TextColor(if live { theme::AMBER } else { theme::TEXT_FAINT }),
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(3.0),
                    right: Val::Px(5.0),
                    ..default()
                },
                Pickable::IGNORE,
            ));
            let Some(kind) = hotbar.slots[slot.0].bound else { return };
            let Some(view) = items.0.view(kind, &blocks.0) else { return };
            c.spawn((
                icons.node(view.tile),
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
            let count = inventory.total_of(kind);
            if count > 1 {
                c.spawn((
                    Text::new(count.to_string()),
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
}

/// The number keys choose the live slot while playing. On an empty slot there
/// is nothing to choose, so the slot says so instead.
pub fn select_hotbar_slot(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    mut hotbar: ResMut<Hotbar>,
    slots: Query<(Entity, &HotbarSlotNode)>,
) {
    for (i, key) in HOTBAR_KEYS.iter().enumerate() {
        if !keys.just_pressed(*key) {
            continue;
        }
        if hotbar.slots[i].bound.is_some() {
            hotbar.selected = i;
        } else if let Some((entity, _)) = slots.iter().find(|(_, s)| s.0 == i) {
            // Reinserted rather than left alone, so a second press restarts the
            // shake instead of looking like the key was swallowed.
            commands.entity(entity).insert(Wiggle::default());
        }
    }
}

/// With an item picked in the screen, the number keys point a slot at it.
pub fn bind_selected_to_hotbar(
    keys: Res<ButtonInput<KeyCode>>,
    selected: Res<super::SelectedItem>,
    items: Res<Items>,
    blocks: Res<Blocks>,
    mut hotbar: ResMut<Hotbar>,
) {
    let Some(kind) = selected.0 else { return };
    for (i, key) in HOTBAR_KEYS.iter().enumerate() {
        if keys.just_pressed(*key) {
            hotbar.bind(i, kind, items.0.class_of(kind, &blocks.0));
        }
    }
}

/// Advance the protest, and put the slot back where it belongs when it ends.
pub fn animate_wiggle(
    mut commands: Commands,
    time: Res<Time>,
    mut wiggling: Query<(Entity, &mut Wiggle, &mut UiTransform)>,
) {
    for (entity, mut wiggle, mut transform) in &mut wiggling {
        wiggle.t += time.delta_secs();
        if wiggle.t >= WIGGLE_SECS {
            transform.translation = Val2::ZERO;
            commands.entity(entity).remove::<Wiggle>();
            continue;
        }
        // Damped so it settles rather than stopping mid-swing.
        let decay = 1.0 - wiggle.t / WIGGLE_SECS;
        transform.translation = Val2::px(WIGGLE_PX * (wiggle.t * WIGGLE_RATE).sin() * decay, 0.0);
    }
}

/// Q throws one of the live slot's item on the ground.
pub fn drop_selected(
    keys: Res<ButtonInput<KeyCode>>,
    net: Res<NetClient>,
    inventory: Res<PlayerInventory>,
    hotbar: Res<Hotbar>,
) {
    if !keys.just_pressed(KeyCode::KeyQ) {
        return;
    }
    let Some(kind) = hotbar.selected_kind() else { return };
    let Some(slot) = inventory.first_slot_holding(kind) else { return };
    net.send(ClientMsg::DropItem { slot: slot as u16, count: 1 });
}

/// The bar stays up while the inventory screen is open — it is the drop target
/// for assigning items, so hiding it there would remove the point of the drag.
/// Only the pause menu takes it away.
pub fn update_hotbar_visibility(
    mode: Res<State<UiMode>>,
    mut bar: Query<&mut Visibility, With<HotbarBar>>,
) {
    let want =
        if *mode.get() == UiMode::Menu { Visibility::Hidden } else { Visibility::Inherited };
    for mut vis in &mut bar {
        if *vis != want {
            *vis = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soils_protocol::ItemStack;

    fn regs() -> (ItemRegistry, BlockRegistry) {
        (soils_sim::default_item_registry(), soils_worldgen::default_registry())
    }

    fn id(blocks: &BlockRegistry, name: &str) -> u8 {
        blocks.id_of(name).unwrap_or_else(|| panic!("no block named {name}"))
    }

    /// An inventory holding `count` of each named block, one stack per entry.
    fn pack(blocks: &BlockRegistry, entries: &[(&str, u16)]) -> PlayerInventory {
        PlayerInventory {
            slots: entries
                .iter()
                .map(|(n, c)| ItemStack::new(ItemKind::Block(id(blocks, n)), *c))
                .collect(),
        }
    }

    fn named(hotbar: &Hotbar, blocks: &BlockRegistry, i: usize) -> Option<String> {
        let kind = hotbar.slots[i].bound?;
        Some(blocks.get(kind.block()?)?.name.clone())
    }

    fn settled(inv: &PlayerInventory) -> (Hotbar, ItemRegistry, BlockRegistry) {
        let (items, blocks) = regs();
        let mut hotbar = Hotbar::default();
        hotbar.reconcile(inv, &items, &blocks);
        (hotbar, items, blocks)
    }

    /// The requirement, stated as a test: pointing a key at an item must not
    /// touch the inventory. A test that only checked the hotbar would pass just
    /// as happily while the item was quietly moved out of the pack.
    #[test]
    fn binding_an_item_does_not_move_it() {
        let (items, blocks) = regs();
        let inv = pack(&blocks, &[("Cobblestone", 64), ("Dirt", 12)]);
        let before: Vec<_> = inv.slots.clone();

        let mut hotbar = Hotbar::default();
        let cobble = ItemKind::Block(id(&blocks, "Cobblestone"));
        hotbar.bind(3, cobble, items.class_of(cobble, &blocks));

        assert_eq!(inv.slots, before, "the pack must be untouched");
        assert_eq!(hotbar.slots[3].bound, Some(cobble));
        assert_eq!(hotbar.slot_of(cobble), Some(3), "the inventory badge reads this");
    }

    /// A kind split across slots is still one binding, and the badge is
    /// unambiguous. This is the ordinary case: the starter kit is 128 apiece.
    #[test]
    fn one_kind_over_two_stacks_is_one_binding() {
        let (items, blocks) = regs();
        let cobble = ItemKind::Block(id(&blocks, "Cobblestone"));
        let inv = PlayerInventory {
            slots: vec![ItemStack::new(cobble, 64), ItemStack::new(cobble, 64)],
        };
        let mut hotbar = Hotbar::default();
        hotbar.bind(0, cobble, items.class_of(cobble, &blocks));
        hotbar.reconcile(&inv, &items, &blocks);
        assert_eq!(hotbar.slot_of(cobble), Some(0));
        assert_eq!(inv.total_of(cobble), 128, "both stacks count toward the one entry");
    }

    /// The headline behaviour, on the real starter kit: spend the last of a
    /// bound item and the key holds a like one.
    #[test]
    fn a_spent_stack_is_replaced_by_a_like_item() {
        let (items, blocks) = regs();
        let mut inv = pack(&blocks, &[("Cobblestone", 1), ("Moss Stone", 64)]);
        let cobble = ItemKind::Block(id(&blocks, "Cobblestone"));

        let mut hotbar = Hotbar::default();
        hotbar.bind(0, cobble, items.class_of(cobble, &blocks));

        inv.slots[0] = None; // the last one is placed
        hotbar.reconcile(&inv, &items, &blocks);

        assert_eq!(named(&hotbar, &blocks, 0).as_deref(), Some("Moss Stone"));
    }

    /// And never across a class. With the whole stone category gone, a slot
    /// that wanted stone must go empty rather than take the Leaves sitting
    /// unassigned right next to it.
    #[test]
    fn a_replacement_is_never_taken_from_another_class() {
        let (items, blocks) = regs();
        let mut inv = pack(&blocks, &[("Cobblestone", 1), ("Leaves", 40)]);
        let cobble = ItemKind::Block(id(&blocks, "Cobblestone"));

        let mut hotbar = Hotbar::default();
        hotbar.bind(0, cobble, items.class_of(cobble, &blocks));

        inv.slots[0] = None;
        hotbar.reconcile(&inv, &items, &blocks);

        assert_eq!(hotbar.slots[0].bound, None, "leaves are not a building stone");
        assert!(hotbar.slots[0].want.is_some(), "the slot must remember what it was for");
        // ...and it must not have been swept up by the never-held rule either.
        assert!(
            hotbar.slots.iter().skip(1).any(|s| s.bound == Some(ItemKind::Block(id(&blocks, "Leaves")))),
            "the leaves belong on some other, never-used key"
        );
    }

    /// A slot that was emptied for want of a match refills the moment a like
    /// item turns up — the alternative is a key that is dead for the rest of
    /// the session.
    #[test]
    fn an_empty_slot_refills_when_a_like_item_is_picked_up() {
        let (items, blocks) = regs();
        let mut inv = pack(&blocks, &[("Cobblestone", 1)]);
        let cobble = ItemKind::Block(id(&blocks, "Cobblestone"));
        let mut hotbar = Hotbar::default();
        hotbar.bind(0, cobble, items.class_of(cobble, &blocks));

        inv.slots[0] = None;
        hotbar.reconcile(&inv, &items, &blocks);
        assert_eq!(hotbar.slots[0].bound, None);

        inv.slots[0] = ItemStack::new(ItemKind::Block(id(&blocks, "Slate")), 3);
        hotbar.reconcile(&inv, &items, &blocks);
        assert_eq!(named(&hotbar, &blocks, 0).as_deref(), Some("Slate"));
    }

    /// A slot the player deliberately emptied must not be refilled with
    /// something unrelated on the next pickup.
    #[test]
    fn clearing_a_slot_leaves_it_waiting_for_a_like_item() {
        let (items, blocks) = regs();
        let inv = pack(&blocks, &[("Cobblestone", 5), ("Leaves", 5)]);
        let cobble = ItemKind::Block(id(&blocks, "Cobblestone"));

        let mut hotbar = Hotbar::default();
        hotbar.bind(0, cobble, items.class_of(cobble, &blocks));
        hotbar.clear(0, items.class_of(cobble, &blocks));
        hotbar.reconcile(&inv, &items, &blocks);

        assert_eq!(
            named(&hotbar, &blocks, 0).as_deref(),
            Some("Cobblestone"),
            "the stone is still in the pack, so the slot takes it back"
        );
    }

    /// No kind on two keys: the second binding takes it off the first, or one
    /// of them would die unexplained the moment the item ran out.
    #[test]
    fn a_kind_is_never_bound_to_two_keys() {
        let (items, blocks) = regs();
        let inv = pack(&blocks, &[("Cobblestone", 5), ("Dirt", 5), ("Log", 5)]);
        let cobble = ItemKind::Block(id(&blocks, "Cobblestone"));
        let mut hotbar = Hotbar::default();
        hotbar.reconcile(&inv, &items, &blocks);

        hotbar.bind(0, cobble, items.class_of(cobble, &blocks));
        hotbar.bind(4, cobble, items.class_of(cobble, &blocks));
        assert_eq!(hotbar.slots.iter().filter(|s| s.bound == Some(cobble)).count(), 1);
        assert_eq!(hotbar.slots[4].bound, Some(cobble));

        hotbar.reconcile(&inv, &items, &blocks);
        let bound: Vec<_> = hotbar.slots.iter().filter_map(|s| s.bound).collect();
        let mut unique = bound.clone();
        unique.dedup();
        assert_eq!(bound.len(), unique.len(), "reconcile must not duplicate either");
    }

    /// A fresh player gets a usable bar rather than eight blanks over a full
    /// pack, and the order is the pack's own so it is predictable.
    #[test]
    fn a_never_used_bar_fills_itself_from_the_pack() {
        let (_, blocks) = regs();
        let inv = pack(
            &blocks,
            &[
                ("Cobblestone", 128),
                ("Moss Stone", 128),
                ("Stone Bricks", 128),
                ("Dirt", 128),
                ("Grass", 128),
                ("Wooden Crate", 128),
                ("Clay Pot", 128),
                ("Log", 128),
                ("Leaves", 128),
            ],
        );
        let (hotbar, ..) = settled(&inv);
        assert_eq!(named(&hotbar, &blocks, 0).as_deref(), Some("Cobblestone"));
        assert_eq!(named(&hotbar, &blocks, 7).as_deref(), Some("Log"));
        assert!(
            hotbar.slots.iter().all(|s| s.bound.is_some()),
            "nine kinds and eight keys: every key gets one"
        );
        assert!(
            hotbar.slots.iter().all(|s| s.want.is_none()),
            "an auto-fill is a convenience, not a choice — it must not pin the slot"
        );
    }

    /// Deterministic under a tie: two equally good candidates, the earlier
    /// inventory slot wins. Without this the substitution is untestable and
    /// jumps around between runs.
    #[test]
    fn the_replacement_choice_is_deterministic() {
        let (items, blocks) = regs();
        for _ in 0..8 {
            let mut inv = pack(&blocks, &[("Cobblestone", 1), ("Slate", 9), ("Moss Stone", 9)]);
            let cobble = ItemKind::Block(id(&blocks, "Cobblestone"));
            let mut hotbar = Hotbar::default();
            hotbar.bind(0, cobble, items.class_of(cobble, &blocks));
            inv.slots[0] = None;
            hotbar.reconcile(&inv, &items, &blocks);
            assert_eq!(named(&hotbar, &blocks, 0).as_deref(), Some("Slate"));
        }
    }

    /// Reconcile must settle: running it again on an unchanged pack changes
    /// nothing. `reconcile_hotbar` relies on this to avoid redrawing the bar on
    /// every inventory tick.
    #[test]
    fn reconcile_is_idempotent() {
        let (items, blocks) = regs();
        let inv = pack(&blocks, &[("Cobblestone", 5), ("Dirt", 5)]);
        let (mut hotbar, ..) = settled(&inv);
        let once = hotbar.slots;
        hotbar.reconcile(&inv, &items, &blocks);
        assert_eq!(hotbar.slots, once);
    }

    #[test]
    fn an_empty_pack_leaves_an_empty_bar_and_nothing_selected() {
        let inv = PlayerInventory::default();
        let (hotbar, ..) = settled(&inv);
        assert!(hotbar.slots.iter().all(|s| s.bound.is_none()));
        assert_eq!(hotbar.selected_kind(), None);
        assert_eq!(hotbar.selected_block(), None);
    }

    // ---- headless UI ----

    use crate::inventory::test_support::{block, count, press, ui_app};
    use std::time::Duration;

    fn wiggling(app: &mut App) -> Vec<usize> {
        let mut q = app.world_mut().query_filtered::<&HotbarSlotNode, With<Wiggle>>();
        q.iter(app.world()).map(|s| s.0).collect()
    }

    fn advance(app: &mut App, secs: f32) {
        app.world_mut().resource_mut::<Time<()>>().advance_by(Duration::from_secs_f32(secs));
        app.update();
    }

    #[test]
    fn the_bar_builds_one_slot_per_key() {
        let mut app = ui_app(&[("Cobblestone", 5)]);
        assert_eq!(count::<HotbarBar>(&mut app), 1);
        assert_eq!(count::<HotbarSlotNode>(&mut app), HOTBAR_SLOTS);
    }

    /// The whole point of the empty state: a key with nothing behind it must
    /// visibly refuse rather than look like a swallowed input.
    #[test]
    fn pressing_a_dead_key_wiggles_it_and_a_live_one_does_not() {
        let mut app = ui_app(&[("Cobblestone", 5)]);
        // One kind, so key 1 is filled and key 2 is not.
        press(&mut app, KeyCode::Digit2);
        app.update();
        assert_eq!(wiggling(&mut app), vec![1], "the empty key protests");
        assert_eq!(app.world().resource::<Hotbar>().selected, 0, "and does not become live");

        advance(&mut app, WIGGLE_SECS * 2.0);
        press(&mut app, KeyCode::Digit1);
        app.update();
        assert!(wiggling(&mut app).is_empty(), "a key with an item behind it just selects");
        assert_eq!(app.world().resource::<Hotbar>().selected, 0);
    }

    #[test]
    fn a_wiggle_expires_and_puts_the_slot_back() {
        let mut app = ui_app(&[]);
        press(&mut app, KeyCode::Digit1);
        app.update();
        assert_eq!(wiggling(&mut app), vec![0]);

        advance(&mut app, WIGGLE_SECS + 0.05);
        assert!(wiggling(&mut app).is_empty(), "the shake must end on its own");

        let mut q = app
            .world_mut()
            .query_filtered::<&UiTransform, With<HotbarSlotNode>>();
        for transform in q.iter(app.world()) {
            assert_eq!(transform.translation, Val2::ZERO, "and leave the slot where it belongs");
        }
    }

    /// The reason the slot entities are spawned once and kept. A wholesale
    /// rebuild — which is what the screen does — would despawn the animating
    /// slot mid-shake every time the pack changed, and picking an item up is
    /// exactly when a player is most likely to be pressing keys.
    #[test]
    fn an_inventory_change_does_not_interrupt_a_wiggle() {
        let mut app = ui_app(&[]);
        press(&mut app, KeyCode::Digit1);
        app.update();
        let before = {
            let mut q = app.world_mut().query_filtered::<Entity, With<Wiggle>>();
            q.iter(app.world()).next().expect("the key is empty, so it wiggles")
        };

        let cobble = block(&app, "Cobblestone");
        app.world_mut().resource_mut::<PlayerInventory>().slots =
            vec![soils_protocol::ItemStack::new(cobble, 9)];
        advance(&mut app, 0.01);

        let mut q = app.world_mut().query_filtered::<Entity, With<Wiggle>>();
        assert_eq!(
            q.iter(app.world()).next(),
            Some(before),
            "the same slot entity must still be shaking"
        );
    }

    /// A pack the shape of the real starter kit: nine kinds, eight keys, so one
    /// stone is left over and unassigned.
    const NINE_KINDS: [(&str, u16); 9] = [
        ("Cobblestone", 1),
        ("Dirt", 128),
        ("Grass", 128),
        ("Log", 128),
        ("Leaves", 128),
        ("Wooden Crate", 128),
        ("Clay Pot", 128),
        ("Iron Ore", 128),
        ("Slate", 128),
    ];

    /// End to end through the real systems: place the last Cobblestone and key
    /// 1 comes back holding the spare stone, untouched by anything else.
    #[test]
    fn the_bar_heals_itself_through_the_running_systems() {
        let mut app = ui_app(&NINE_KINDS);
        let cobble = block(&app, "Cobblestone");
        let slate = block(&app, "Slate");
        {
            let hotbar = app.world().resource::<Hotbar>();
            assert_eq!(hotbar.slots[0].bound, Some(cobble));
            assert_eq!(hotbar.slot_of(slate), None, "nine kinds, eight keys: slate is the spare");
        }

        app.world_mut().resource_mut::<PlayerInventory>().slots[0] = None;
        app.update();

        let hotbar = app.world().resource::<Hotbar>();
        assert_eq!(hotbar.slots[0].bound, Some(slate));
        assert_eq!(hotbar.slot_of(slate), Some(0), "and on that one key only");
    }

    /// The case the substitution deliberately declines. When the only like item
    /// is already sitting on another key, the emptied slot stays empty rather
    /// than stealing it — taking it would just move the dead key one along,
    /// and the player can already reach the stone where it is.
    #[test]
    fn a_slot_does_not_steal_a_replacement_from_another_key() {
        let (items, blocks) = regs();
        let mut inv = pack(&blocks, &[("Cobblestone", 1), ("Moss Stone", 64)]);
        let mut hotbar = Hotbar::default();
        hotbar.reconcile(&inv, &items, &blocks);
        let moss = ItemKind::Block(id(&blocks, "Moss Stone"));
        assert_eq!(hotbar.slot_of(moss), Some(1), "auto-fill already gave it key 2");

        inv.slots[0] = None;
        hotbar.reconcile(&inv, &items, &blocks);

        assert_eq!(hotbar.slots[0].bound, None, "key 1 goes quiet");
        assert_eq!(hotbar.slot_of(moss), Some(1), "key 2 keeps what it had");
    }
}

