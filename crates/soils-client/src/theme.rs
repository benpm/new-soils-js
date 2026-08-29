//! Colours and metrics for the inventory UI, ported from the design mockup in
//! `scratch/src/index.css`.
//!
//! The client had no theme module: thirty-odd `Color::srgb*` literals were
//! inlined across six files, and `pause.rs` and `login.rs` had already drifted
//! apart on near-identical `PANEL_BG` constants. This is the shared set, used
//! by the hotbar and the inventory screen. The pause menu, login form, console
//! and HUD still carry their own colours and can adopt these later — reskinning
//! every screen at once would bury the inventory change.
//!
//! Values are sRGB, matching the hex in the mockup; `Color::srgb` linearises.

use bevy::prelude::*;

/// `#c89a4a` — the accent. Titles, selection, hotbar keys.
pub const AMBER: Color = Color::srgb(0.784, 0.604, 0.290);
/// Amber at half strength, for hover borders.
pub const AMBER_DIM: Color = Color::srgba(0.784, 0.604, 0.290, 0.5);

/// `#0d0c0b` — behind everything.
pub const BG_DEEP: Color = Color::srgb(0.051, 0.047, 0.043);
/// `#141210` — the window body.
pub const BG_PANEL: Color = Color::srgb(0.078, 0.071, 0.063);
/// `#1a1714` — an item slot at rest.
pub const BG_SLOT: Color = Color::srgb(0.102, 0.090, 0.078);
/// `#221f1b` — an item slot under the pointer.
pub const BG_SLOT_HOVER: Color = Color::srgb(0.133, 0.122, 0.106);
/// `#221f1a` — the selected slot.
pub const BG_SLOT_SELECTED: Color = Color::srgb(0.133, 0.122, 0.102);
/// `#0f0e0d` — the hotbar's own backing, a shade under the panel.
pub const BG_HOTBAR: Color = Color::srgb(0.059, 0.055, 0.051);

/// Warm white at three strengths, all `rgb(255, 220, 150)` with the mockup's
/// alphas. Borders only — never text.
pub const BORDER_DIM: Color = Color::srgba(1.0, 0.863, 0.588, 0.08);
pub const BORDER_MID: Color = Color::srgba(1.0, 0.863, 0.588, 0.15);
pub const BORDER_BRIGHT: Color = Color::srgba(1.0, 0.863, 0.588, 0.35);

/// `#e8d9bc` — item names, counts.
pub const TEXT_PRIMARY: Color = Color::srgb(0.910, 0.851, 0.737);
/// `#7a6e5e` — secondary readouts.
pub const TEXT_MUTED: Color = Color::srgb(0.478, 0.431, 0.369);
/// `#4a4238` — labels and key hints.
pub const TEXT_DIM: Color = Color::srgb(0.290, 0.259, 0.220);
/// `#3a342c` — the faintest legible step; empty-state text, slot key numbers.
pub const TEXT_FAINT: Color = Color::srgb(0.227, 0.204, 0.173);

/// Multiplied into the icon of an item that a hotbar slot points at, so the
/// inventory shows it as present-but-spoken-for.
///
/// Not black. `blocks.png` tiles are fully opaque squares, so a hard tint turns
/// every bound block into the same dark square — the mockup's silhouettes read
/// because its icons are emoji with real outlines. This dims the icon far
/// enough to be obviously different while leaving it recognisable; the slot
/// badge is what actually says *which* hotbar key holds it.
pub const SILHOUETTE_TINT: Color = Color::srgb(0.30, 0.28, 0.25);

/// Backing of the little "this is on hotbar key N" badge.
pub const BADGE_BG: Color = Color::srgba(0.051, 0.047, 0.043, 0.92);

/// One item slot, in the grid and on the hotbar alike.
pub const SLOT_PX: f32 = 56.0;
/// The icon inside a slot.
pub const ICON_PX: f32 = 38.0;
/// A category's icon button on the left rail of the screen.
pub const CATEGORY_ICON_PX: f32 = 40.0;
/// Gap between slots in a category grid.
pub const SLOT_GAP_PX: f32 = 4.0;
/// Gap between hotbar slots.
pub const HOTBAR_GAP_PX: f32 = 6.0;
/// Width of the detail panel on the right of the screen.
pub const DETAIL_PX: f32 = 180.0;
/// Width of the inventory window.
pub const WINDOW_PX: f32 = 680.0;

/// Font sizes, mirroring the mockup's scale.
pub const FONT_TITLE: f32 = 13.0;
pub const FONT_BODY: f32 = 12.0;
pub const FONT_SMALL: f32 = 11.0;
pub const FONT_TINY: f32 = 10.0;
