//! Pause / settings menu, shown whenever the cursor is released (Esc). Mirrors
//! the JS pause menu: adjust load radius and toggle ambient occlusion and fog.

use bevy::prelude::*;

use crate::gi::GiSettings;
use crate::player::{LookSettings, Streaming};
use crate::singleplayer::Singleplayer;

/// Render settings toggled from the pause menu. New chunks read this; toggling
/// rewrites every existing chunk material.
#[derive(Resource)]
pub struct RenderToggles {
    pub ao: bool,
    pub fog: bool,
    /// Baked L0 light-grid shading (console `/light on|off`; off = flat brightness).
    pub light: bool,
}

impl Default for RenderToggles {
    fn default() -> Self {
        Self { ao: true, fog: true, light: true }
    }
}

const RADIUS_MIN: i32 = 2;
const RADIUS_MAX: i32 = 8;

#[derive(Component, Clone, Copy)]
pub enum MenuButton {
    RadiusDown,
    RadiusUp,
    SensDown,
    SensUp,
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
pub(crate) struct SensLabel;
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
                    TextFont { font_size: 26.0.into(), ..default() },
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
                            TextFont { font_size: 18.0.into(), ..default() },
                            TextColor(Color::WHITE),
                            RadiusLabel,
                        ));
                        button(row, "+", MenuButton::RadiusUp);
                    });

                // Mouse sensitivity row: [-]  Mouse sensitivity: N.N  [+]
                panel
                    .spawn(Node {
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        column_gap: Val::Px(10.0),
                        ..default()
                    })
                    .with_children(|row| {
                        button(row, "-", MenuButton::SensDown);
                        row.spawn((
                            Text::new("Mouse sensitivity: 1.0"),
                            TextFont { font_size: 18.0.into(), ..default() },
                            TextColor(Color::WHITE),
                            SensLabel,
                        ));
                        button(row, "+", MenuButton::SensUp);
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
                            TextFont { font_size: 18.0.into(), ..default() },
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
                TextFont { font_size: 18.0.into(), ..default() },
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
                TextFont { font_size: 18.0.into(), ..default() },
                TextColor(Color::WHITE),
                marker,
            ));
        });
}

/// Show the menu in [`UiMode::Menu`], hide it otherwise.
///
/// This used to key off the cursor being released, which made "pointer free"
/// and "paused" the same thing and left no room for any other UI.
pub fn pause_menu_visibility(
    mode: Res<State<crate::ui::UiMode>>,
    mut menu: Query<&mut Visibility, With<PauseMenu>>,
) {
    let want = if *mode.get() == crate::ui::UiMode::Menu {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    for mut vis in &mut menu {
        if *vis != want {
            *vis = want;
        }
    }
}

/// Handle pause-menu button presses.
pub fn pause_menu_buttons(
    buttons: Query<(&Interaction, &MenuButton), (Changed<Interaction>, With<Button>)>,
    mut streaming: ResMut<Streaming>,
    mut look: ResMut<LookSettings>,
    mut toggles: ResMut<RenderToggles>,
    mut sp: ResMut<Singleplayer>,
    mut gi: ResMut<GiSettings>,
    mut next_mode: ResMut<NextState<crate::ui::UiMode>>,
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
            MenuButton::SensDown => look.nudge(-1),
            MenuButton::SensUp => look.nudge(1),
            // Toggles flow into the terrain uniform every frame
            // (world_draw::update_terrain_params).
            MenuButton::ToggleAo => toggles.ao = !toggles.ao,
            MenuButton::ToggleFog => toggles.fog = !toggles.fog,
            MenuButton::ToggleGi => {
                gi.enabled = !gi.enabled;
            }
            MenuButton::ToggleDiscovery => {
                sp.toggle_discovery();
            }
            MenuButton::Resume => {
                // The mode owns the cursor; `ui::apply_cursor_mode` re-grabs.
                next_mode.set(crate::ui::UiMode::Playing);
            }
        }
    }
}

/// Keep the dynamic labels in sync with the current settings.
pub fn update_pause_labels(
    streaming: Res<Streaming>,
    look: Res<LookSettings>,
    toggles: Res<RenderToggles>,
    sp: Res<Singleplayer>,
    gi: Res<GiSettings>,
    mut radius: Query<
        &mut Text,
        (
            With<RadiusLabel>,
            Without<SensLabel>,
            Without<AoLabel>,
            Without<FogLabel>,
            Without<GiLabel>,
            Without<DiscoveryLabel>,
        ),
    >,
    mut sens: Query<
        &mut Text,
        (With<SensLabel>, Without<AoLabel>, Without<FogLabel>, Without<GiLabel>, Without<DiscoveryLabel>),
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
    if let Ok(mut t) = sens.single_mut() {
        t.0 = format!("Mouse sensitivity: {:.1}", look.sensitivity);
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
            ..ServerConfig::default()
        })
        .expect("embedded server");

        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin);
        app.init_state::<crate::ui::UiMode>();
        app.insert_resource(Streaming::default());
        app.insert_resource(LookSettings::default());
        app.insert_resource(RenderToggles::default());
        app.insert_resource(GiSettings::default());
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
