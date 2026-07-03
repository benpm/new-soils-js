//! Pause / settings menu, shown whenever the cursor is released (Esc). Mirrors
//! the JS pause menu: adjust load radius and toggle ambient occlusion and fog.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::gi::GiSettings;
use crate::material::{ChunkMeshMaterial, FOG_DENSITY};
use crate::player::Streaming;
use crate::singleplayer::Singleplayer;

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
pub enum MenuButton {
    RadiusDown,
    RadiusUp,
    ToggleAo,
    ToggleFog,
    /// Toggle radiance-cascades global illumination.
    ToggleGi,
    /// Single-player only: advertise the embedded server on the LAN.
    ToggleDiscovery,
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
#[derive(Component)]
pub(crate) struct GiLabel;
#[derive(Component)]
pub(crate) struct DiscoveryLabel;

/// The LAN-discovery button node, hidden unless single-player is running.
#[derive(Component)]
pub(crate) struct DiscoveryRow;

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
                labelled_button(panel, "Global illumination: OFF", MenuButton::ToggleGi, GiLabel);

                // Single-player only (hidden otherwise): open the world to LAN
                // discovery. Off by default.
                panel
                    .spawn((
                        Button,
                        MenuButton::ToggleDiscovery,
                        DiscoveryRow,
                        Visibility::Hidden,
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
                            Text::new("LAN discovery: OFF"),
                            TextFont { font_size: 18.0, ..default() },
                            TextColor(Color::WHITE),
                            DiscoveryLabel,
                        ));
                    });

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
    mut sp: ResMut<Singleplayer>,
    mut gi: ResMut<GiSettings>,
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
            MenuButton::ToggleGi => {
                gi.enabled = !gi.enabled;
            }
            MenuButton::ToggleDiscovery => {
                sp.toggle_discovery();
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
    sp: Res<Singleplayer>,
    gi: Res<GiSettings>,
    mut radius: Query<
        &mut Text,
        (
            With<RadiusLabel>,
            Without<AoLabel>,
            Without<FogLabel>,
            Without<GiLabel>,
            Without<DiscoveryLabel>,
        ),
    >,
    mut ao: Query<
        &mut Text,
        (With<AoLabel>, Without<FogLabel>, Without<GiLabel>, Without<DiscoveryLabel>),
    >,
    mut fog: Query<&mut Text, (With<FogLabel>, Without<GiLabel>, Without<DiscoveryLabel>)>,
    mut gi_label: Query<&mut Text, (With<GiLabel>, Without<DiscoveryLabel>)>,
    mut disco: Query<&mut Text, With<DiscoveryLabel>>,
    mut disco_row: Query<&mut Visibility, With<DiscoveryRow>>,
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
    if let Ok(mut t) = gi_label.single_mut() {
        t.0 = format!("Global illumination: {}", if gi.enabled { "ON" } else { "OFF" });
    }
    // LAN discovery: only meaningful (and visible) in single-player. The label
    // reflects the *actual* responder state, so a failed UDP bind shows up.
    if let Ok(mut t) = disco.single_mut() {
        t.0 = match sp.discovery_status() {
            Some((true, Some(port))) => format!("LAN discovery: ON (udp {port})"),
            Some((true, None)) => "LAN discovery: starting…".into(),
            _ => "LAN discovery: OFF".into(),
        };
    }
    if let Ok(mut vis) = disco_row.single_mut() {
        let want = if sp.is_running() { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soils_server::ServerConfig;

    /// The pause-menu button must actually flip the embedded server's LAN
    /// discovery state — the UI wiring a server-side test can't cover. Runs
    /// `pause_menu_buttons` headlessly in a minimal ECS app against a real
    /// embedded server (temp data dir, ephemeral ports).
    #[test]
    fn discovery_button_toggles_embedded_server() {
        let data_dir =
            std::env::temp_dir().join(format!("soils-pause-test-{}", std::process::id()));
        let mut sp = Singleplayer::default();
        sp.ensure_started_with(ServerConfig {
            bind: "127.0.0.1:0".into(),
            data_dir: data_dir.clone(),
            enable_discovery: false,
            discovery_port: 0,
            name: "pause-test".into(),
        })
        .expect("embedded server");

        let mut app = App::new();
        app.insert_resource(Streaming::default());
        app.insert_resource(RenderToggles::default());
        app.insert_resource(GiSettings::default());
        app.insert_resource(Assets::<ChunkMeshMaterial>::default());
        app.insert_resource(sp);
        app.add_systems(Update, pause_menu_buttons);

        let desired = |app: &App| {
            app.world().resource::<Singleplayer>().discovery_status().map(|(on, _)| on)
        };
        assert_eq!(desired(&app), Some(false), "discovery must start off");

        let btn = app
            .world_mut()
            .spawn((Button, Interaction::Pressed, MenuButton::ToggleDiscovery))
            .id();
        app.update();
        assert_eq!(desired(&app), Some(true), "press must enable discovery");

        // Release, press again: toggles back off.
        *app.world_mut().get_mut::<Interaction>(btn).unwrap() = Interaction::None;
        app.update();
        *app.world_mut().get_mut::<Interaction>(btn).unwrap() = Interaction::Pressed;
        app.update();
        assert_eq!(desired(&app), Some(false), "second press must disable discovery");

        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
