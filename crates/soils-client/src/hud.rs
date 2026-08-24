//! In-game HUD: a toggleable debug overlay (FPS, position, chunk counts, time,
//! selected block), mirroring the JS debug panel. F3 toggles it.

use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::prelude::*;
use soils_protocol::CHUNK_BIT;

use crate::chunk::WorldTime;
use crate::edit::Hotbar;
use crate::player::{Player, Streaming};

/// Marker for the debug overlay text node.
#[derive(Component)]
pub struct DebugHud;

/// Marker for the chat overlay text node.
#[derive(Component)]
pub struct ChatHud;

/// Chat lines shown at once. Enough to follow a conversation without covering
/// the view; the full backlog lives in the `Social` resource.
const CHAT_LINES: usize = 8;

/// Spawn the (initially visible) debug overlay in the top-left corner.
pub fn setup_hud(mut commands: Commands) {
    commands.spawn((
        DebugHud,
        Text::new(""),
        TextFont { font_size: 13.0.into(), ..default() },
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.92)),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(8.0),
            left: Val::Px(8.0),
            ..default()
        },
    ));

    // Bottom-left, above the console line, and always visible — chat is not
    // debug output and should not vanish with F3.
    commands.spawn((
        ChatHud,
        Text::new(""),
        TextFont { font_size: 13.0.into(), ..default() },
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.85)),
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(40.0),
            left: Val::Px(8.0),
            ..default()
        },
    ));
}

/// Render the newest chat lines. Empty (and so invisible) when there is no
/// database configured or nothing has been said.
pub fn update_chat(
    social: Res<crate::social::Social>,
    mut hud: Query<&mut Text, With<ChatHud>>,
) {
    if !social.is_changed() {
        return;
    }
    let Ok(mut text) = hud.single_mut() else { return };
    let start = social.chat.len().saturating_sub(CHAT_LINES);
    let body = social.chat[start..]
        .iter()
        .map(|l| format!("<{}> {}", l.sender, l.text))
        .collect::<Vec<_>>()
        .join("\n");
    if text.0 != body {
        text.0 = body;
    }
}

/// F3 toggles the debug overlay.
pub fn toggle_hud(keys: Res<ButtonInput<KeyCode>>, mut hud: Query<&mut Visibility, With<DebugHud>>) {
    if keys.just_pressed(KeyCode::F3) {
        if let Ok(mut vis) = hud.single_mut() {
            *vis = match *vis {
                Visibility::Hidden => Visibility::Inherited,
                _ => Visibility::Hidden,
            };
        }
    }
}

/// Refresh the debug overlay text each frame.
pub fn update_hud(
    diagnostics: Res<DiagnosticsStore>,
    slots: Res<crate::pool::ChunkSlots>,
    world_time: Res<WorldTime>,
    streaming: Res<Streaming>,
    hotbar: Res<Hotbar>,
    player: Query<&Transform, With<Player>>,
    mut text: Query<&mut Text, With<DebugHud>>,
) {
    let Ok(mut text) = text.single_mut() else { return };
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    let pos = player.single().map(|t| t.translation).unwrap_or_default();
    let vox = pos.floor().as_ivec3();
    let chunk = IVec3::new(vox.x >> CHUNK_BIT, vox.y >> CHUNK_BIT, vox.z >> CHUNK_BIT);
    let loaded = slots.len();
    let pending = streaming.pending;
    let progress = if pending == 0 { 100.0 } else { 100.0 * loaded as f32 / (loaded + pending) as f32 };
    text.0 = format!(
        "new-soils (Rust/Bevy)\n\
         fps {fps:.0}\n\
         pos {:.1} {:.1} {:.1}\n\
         vox {} {} {}\n\
         chunk {} {} {}\n\
         chunks loaded {}  radius {}\n\
         generating {pending}  ({progress:.0}%)\n\
         daytime {:.2}\n\
         block [{}] {}",
        pos.x, pos.y, pos.z,
        vox.x, vox.y, vox.z,
        chunk.x, chunk.y, chunk.z,
        loaded, streaming.load_radius,
        world_time.daytime,
        hotbar.selected + 1, hotbar.block_name(),
    );
}
