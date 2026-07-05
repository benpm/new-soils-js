//! Raycast block editing: left click breaks, right click places. Mirrors the
//! JS `player.update` raycast + `Voxels.edit` flow (optimistic local apply plus
//! an `Edit` sent to the server). The raycast and edit-legality rules live in
//! `soils-sim`, shared with (future) server-side validation.

use bevy::prelude::*;
use bevy::render::storage::ShaderStorageBuffer;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ClientMsg};
use soils_sim::{raycast_voxel, validate_edit};

use crate::chunk::{Blocks, ChunkMap, VoxelChunk, voxel_at};
use crate::gpu_mesh::{self, GpuChunk};
use crate::light::LightQueue;
use crate::net::NetClient;
use crate::player::Player;

/// Optimistically applied edits awaiting the server's verdict. On
/// `EditRejected` the voxel rolls back to its recorded previous value (unless
/// a later pending edit targets the same voxel — that one's ack settles it).
#[derive(Resource, Default)]
pub struct PendingEdits {
    next_seq: u32,
    list: Vec<(u32, IVec3, u8)>,
}

impl PendingEdits {
    /// Drop everything (warp: the world the edits targeted is gone).
    pub fn clear(&mut self) {
        self.list.clear();
    }
}

/// The nine right-click placement blocks, selectable with the 1-9 keys. Mirrors
/// the JS hotbar (`player.placeBlock`), which defaults to "Stone Bricks".
#[derive(Resource)]
pub struct Hotbar {
    pub slots: [&'static str; 9],
    pub selected: usize,
}

impl Default for Hotbar {
    fn default() -> Self {
        Self {
            slots: [
                "Cobblestone", "Moss Stone", "Stone Bricks", "Dirt", "Grass",
                "Wooden Crate", "Clay Pot", "Log", "Leaves",
            ],
            selected: 2, // Stone Bricks
        }
    }
}

impl Hotbar {
    /// Name of the currently selected block.
    pub fn block_name(&self) -> &'static str {
        self.slots[self.selected]
    }
}

/// Select the right-click block with the 1-9 number keys (JS hotbar).
pub fn hotbar_select(keys: Res<ButtonInput<KeyCode>>, mut hotbar: ResMut<Hotbar>) {
    const DIGITS: [KeyCode; 9] = [
        KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4,
        KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    for (i, key) in DIGITS.iter().enumerate() {
        if keys.just_pressed(*key) {
            hotbar.selected = i;
        }
    }
}

/// Draw a wireframe box around the voxel the player is aiming at (JS selection
/// box). Runs every frame while the cursor is grabbed.
pub fn selection_highlight(
    cursor: Query<&CursorOptions, With<PrimaryWindow>>,
    map: Res<ChunkMap>,
    chunks: Query<&VoxelChunk>,
    camera: Query<&Transform, With<Player>>,
    mut gizmos: Gizmos,
) {
    if let Ok(cursor) = cursor.single() {
        if cursor.grab_mode == CursorGrabMode::None {
            return;
        }
    }
    let Ok(transform) = camera.single() else { return };
    let dir = (transform.rotation * Vec3::NEG_Z).normalize();
    let sampler = |v: IVec3| voxel_at(&map, &chunks, v);
    if let Some(hit) = raycast_voxel(transform.translation, dir, &sampler) {
        let center = hit.voxel.as_vec3() + Vec3::splat(0.5);
        // Slightly oversized to sit just outside the block faces (no z-fighting).
        gizmos.cube(
            Transform::from_translation(center).with_scale(Vec3::splat(1.002)),
            Color::srgb(0.02, 0.02, 0.02),
        );
    }
}

/// Spawn a simple screen-centred crosshair (two thin bars forming a `+`).
pub fn setup_crosshair(mut commands: Commands) {
    let color = Color::srgba(1.0, 1.0, 1.0, 0.65);
    for (w, h) in [(Val::Px(2.0), Val::Px(12.0)), (Val::Px(12.0), Val::Px(2.0))] {
        commands
            .spawn(Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|p| {
                p.spawn((Node { width: w, height: h, ..default() }, BackgroundColor(color)));
            });
    }
}

#[allow(clippy::too_many_arguments)]
pub fn edit_blocks(
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Query<&CursorOptions, With<PrimaryWindow>>,
    net: Res<NetClient>,
    registry: Res<Blocks>,
    hotbar: Res<Hotbar>,
    map: Res<ChunkMap>,
    mut chunks: Query<&mut VoxelChunk>,
    mut gpu_chunks: Query<&mut GpuChunk>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut light_queue: ResMut<LightQueue>,
    mut pending: ResMut<PendingEdits>,
    camera: Query<&Transform, With<Player>>,
) {
    // Ignore clicks while the cursor isn't grabbed (UI/escape state).
    if let Ok(cursor) = cursor.single() {
        if cursor.grab_mode == CursorGrabMode::None {
            return;
        }
    }
    let break_block = buttons.just_pressed(MouseButton::Left);
    let place_block = buttons.just_pressed(MouseButton::Right);
    if !break_block && !place_block {
        return;
    }
    let Ok(transform) = camera.single() else { return };

    let origin = transform.translation;
    let dir = (transform.rotation * Vec3::NEG_Z).normalize();

    let hit = {
        let ro = chunks.as_readonly();
        let sampler = |v: IVec3| voxel_at(&map, &ro, v);
        let Some(hit) = raycast_voxel(origin, dir, &sampler) else { return };
        hit
    };

    let (target, value) = if break_block {
        (hit.voxel, 0u8)
    } else {
        let id = registry.0.id_of(hotbar.block_name()).unwrap_or(1);
        (hit.prev, id)
    };

    // Shared legality rule (reach + known id); the server runs the same check
    // authoritatively and answers EditAccepted/EditRejected by seq.
    if !validate_edit(origin, target, value, &registry.0) {
        return;
    }

    let prev = {
        let ro = chunks.as_readonly();
        voxel_at(&map, &ro, target)
    };
    apply_edit(&map, &mut chunks, &mut gpu_chunks, &mut buffers, target, value);
    light_queue.edits.push(target);
    pending.next_seq += 1;
    let seq = pending.next_seq;
    pending.list.push((seq, target, prev));
    net.send(ClientMsg::Edit { seq, pos: [target.x, target.y, target.z], value });
}

/// Settle the server's edit verdicts: accepted seqs just leave the pending
/// list; rejected ones roll the optimistic application back.
pub fn apply_edit_acks(
    mut reader: MessageReader<crate::server_msg::EditAck>,
    mut pending: ResMut<PendingEdits>,
    map: Res<ChunkMap>,
    mut chunks: Query<&mut VoxelChunk>,
    mut gpu_chunks: Query<&mut GpuChunk>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut light_queue: ResMut<LightQueue>,
) {
    for msg in reader.read() {
        let Some(i) = pending.list.iter().position(|(s, ..)| *s == msg.seq) else { continue };
        let (_, pos, prev) = pending.list.remove(i);
        if msg.accepted {
            continue;
        }
        // Roll back unless a later pending edit owns this voxel now.
        if !pending.list.iter().any(|(_, p, _)| *p == pos) {
            apply_edit(&map, &mut chunks, &mut gpu_chunks, &mut buffers, pos, prev);
            light_queue.edits.push(pos);
        }
    }
}

/// Apply an edit to a local chunk: update the CPU voxels, re-upload the GPU
/// voxel buffer, and mark the chunk dirty so the compute mesher regenerates it.
pub fn apply_edit(
    map: &ChunkMap,
    chunks: &mut Query<&mut VoxelChunk>,
    gpu_chunks: &mut Query<&mut GpuChunk>,
    buffers: &mut Assets<ShaderStorageBuffer>,
    v: IVec3,
    value: u8,
) {
    let cpos = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
    let Some(&e) = map.map.get(&cpos) else { return };
    let Ok(mut chunk) = chunks.get_mut(e) else { return };
    chunk.volume.set(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP, value);
    let vol = chunk.volume.clone();
    if let Ok(mut gc) = gpu_chunks.get_mut(e) {
        gpu_mesh::refresh_gpu_chunk(buffers, &mut gc, &vol);
    }
}
