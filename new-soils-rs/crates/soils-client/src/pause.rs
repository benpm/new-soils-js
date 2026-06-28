//! Pause / settings menu, shown whenever the cursor is released (Esc). Mirrors
//! the JS pause menu: adjust load radius and toggle ambient occlusion and fog.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::material::{ChunkMeshMaterial, FOG_DENSITY};
use crate::player::Streaming;

/// Render settings toggled from the pause menu. New chunks read this; toggling
/// rewrites every existing chunk material.
#[derive(Resource)]
pub struct RenderToggles {
    pub ao: bool,
    pub fog: bool,
}

impl Default for RenderToggles {
    fn default() -> Self {
        Self { ao: true, fog: true }
    }
}

const RADIUS_MIN: i32 = 2;
const RADIUS_MAX: i32 = 8;

#[derive(Component, Clone, Copy)]
pub(crate) enum MenuButton {
    RadiusDown,
    RadiusUp,
    ToggleAo,
    ToggleFog,
    Resume,
}

/// Marker for the root pause-menu node (toggled visible with the cursor).
#[derive(Component)]
pub(crate) struct PauseMenu;

/// Markers for the dynamic value labels.
#[derive(Component)]
pub(crate) struct RadiusLabel;
#[derive(Component)]
pub(crate) struct AoLabel;
#[derive(Component)]
pub(crate) struct FogLabel;

const PANEL_BG: Color = Color::srgba(0.05, 0.06, 0.08, 0.86);
const BTN_BG: Color = Color::srgba(0.20, 0.22, 0.26, 0.95);

/// Spawn the (hidden) pause menu.
pub fn setup_pause_menu(mut commands: Commands) {
    commands
        .spawn((
            PauseMenu,
            Visibility::Hidden,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: Val::Px(10.0),
                    padding: UiRect::all(Val::Px(24.0)),
                    ..default()
                },
                BackgroundColor(PANEL_BG),
            ))
            .with_children(|panel| {
                panel.spawn((
                    Text::new("Paused"),
                    TextFont { font_size: 26.0, ..default() },
                    TextColor(Color::WHITE),
                ));

                // Load radius row: [-]  Load radius: N  [+]
                panel
                    .spawn(Node {
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        column_gap: Val::Px(10.0),
                        ..default()
                    })
                    .with_children(|row| {
                        button(row, "-", MenuButton::RadiusDown);
                        row.spawn((
                            Text::new("Load radius: 4"),
                            TextFont { font_size: 18.0, ..default() },
                            TextColor(Color::WHITE),
                            RadiusLabel,
                        ));
                        button(row, "+", MenuButton::RadiusUp);
                    });

                labelled_button(panel, "Ambient occlusion: ON", MenuButton::ToggleAo, AoLabel);
                labelled_button(panel, "Fog: ON", MenuButton::ToggleFog, FogLabel);
                button(panel, "Resume", MenuButton::Resume);
            });
        });
}

/// Spawn a button with a static label.
fn button(parent: &mut ChildSpawnerCommands, label: &str, kind: MenuButton) {
    parent
        .spawn((
            Button,
            kind,
            Node {
                padding: UiRect::axes(Val::Px(14.0), Val::Px(8.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(BTN_BG),
        ))
        .with_children(|b| {
            b.spawn((
                Text::new(label),
                TextFont { font_size: 18.0, ..default() },
                TextColor(Color::WHITE),
            ));
        });
}

/// Spawn a button whose label text carries a marker so it can be updated.
fn labelled_button(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    kind: MenuButton,
    marker: impl Component,
) {
    parent
        .spawn((
            Button,
            kind,
            Node {
                padding: UiRect::axes(Val::Px(14.0), Val::Px(8.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(BTN_BG),
        ))
        .with_children(|b| {
            b.spawn((
                Text::new(label),
                TextFont { font_size: 18.0, ..default() },
                TextColor(Color::WHITE),
                marker,
            ));
        });
}

/// Show the menu while the cursor is released, hide it while grabbed.
pub fn pause_menu_visibility(
    cursor: Query<&CursorOptions, With<PrimaryWindow>>,
    mut menu: Query<&mut Visibility, With<PauseMenu>>,
) {
    let Ok(cursor) = cursor.single() else { return };
    let Ok(mut vis) = menu.single_mut() else { return };
    let want = if cursor.grab_mode == CursorGrabMode::None {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    if *vis != want {
        *vis = want;
    }
}

/// Handle pause-menu button presses.
pub fn pause_menu_buttons(
    buttons: Query<(&Interaction, &MenuButton), (Changed<Interaction>, With<Button>)>,
    mut streaming: ResMut<Streaming>,
    mut toggles: ResMut<RenderToggles>,
    mut materials: ResMut<Assets<ChunkMeshMaterial>>,
    mut cursor: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    for (interaction, kind) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match kind {
            MenuButton::RadiusDown => {
                streaming.load_radius = (streaming.load_radius - 1).max(RADIUS_MIN);
                streaming.last_chunk = None; // force a re-stream pass
            }
            MenuButton::RadiusUp => {
                streaming.load_radius = (streaming.load_radius + 1).min(RADIUS_MAX);
                streaming.last_chunk = None;
            }
            MenuButton::ToggleAo => {
                toggles.ao = !toggles.ao;
                let v = if toggles.ao { 1.0 } else { 0.0 };
                for (_, m) in materials.iter_mut() {
                    m.params.ambient_occlusion = v;
                }
            }
            MenuButton::ToggleFog => {
                toggles.fog = !toggles.fog;
                let d = if toggles.fog { FOG_DENSITY } else { 0.0 };
                for (_, m) in materials.iter_mut() {
                    m.params.fog_density = d;
                }
            }
            MenuButton::Resume => {
                if let Ok(mut cursor) = cursor.single_mut() {
                    cursor.grab_mode = CursorGrabMode::Locked;
                    cursor.visible = false;
                }
            }
        }
    }
}

/// Keep the dynamic labels in sync with the current settings.
pub fn update_pause_labels(
    streaming: Res<Streaming>,
    toggles: Res<RenderToggles>,
    mut radius: Query<&mut Text, (With<RadiusLabel>, Without<AoLabel>, Without<FogLabel>)>,
    mut ao: Query<&mut Text, (With<AoLabel>, Without<FogLabel>)>,
    mut fog: Query<&mut Text, With<FogLabel>>,
) {
    if let Ok(mut t) = radius.single_mut() {
        t.0 = format!("Load radius: {}", streaming.load_radius);
    }
    if let Ok(mut t) = ao.single_mut() {
        t.0 = format!("Ambient occlusion: {}", if toggles.ao { "ON" } else { "OFF" });
    }
    if let Ok(mut t) = fog.single_mut() {
        t.0 = format!("Fog: {}", if toggles.fog { "ON" } else { "OFF" });
    }
}
