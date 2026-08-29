//! Raycast block editing: left click breaks, right click places. Mirrors the
//! JS `player.update` raycast + `Voxels.edit` flow (optimistic local apply plus
//! an `Edit` sent to the server). The raycast and edit-legality rules live in
//! `soils-sim`, shared with (future) server-side validation.

use bevy::prelude::*;
use soils_protocol::{CHUNK_BIT, CHUNK_CLIP, ClientMsg};
use soils_sim::{raycast_voxel, validate_edit};

use crate::chunk::{Blocks, ChunkMap, VoxelChunk, voxel_at};
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

/// Draw a wireframe box around the voxel the player is aiming at (JS selection
/// box). Runs while `UiMode::Playing` holds the cursor.
pub fn selection_highlight(
    map: Res<ChunkMap>,
    chunks: Query<&VoxelChunk>,
    camera: Query<&Transform, With<Player>>,
    mut gizmos: Gizmos,
) {
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
    keys: Res<ButtonInput<KeyCode>>,
    net: Res<NetClient>,
    registry: Res<Blocks>,
    hotbar: Res<crate::inventory::Hotbar>,
    map: Res<ChunkMap>,
    mut directory: ResMut<crate::demand::ChunkDirectory>,
    mut chunks: Query<&mut VoxelChunk>,
    mut slots: ResMut<crate::pool::ChunkSlots>,
    mut pool_ops: ResMut<crate::pool::PoolOpQueue>,
    mut dirty_mesh: ResMut<crate::pool::DirtyMesh>,
    mut light_queue: ResMut<LightQueue>,
    mut pending: ResMut<PendingEdits>,
    camera: Query<&Transform, With<Player>>,
) {
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
    } else if registry
        .0
        .get({
            let ro = chunks.as_readonly();
            voxel_at(&map, &ro, hit.voxel)
        })
        .is_some_and(|def| def.container.is_some())
        && !keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight])
    {
        // Right-clicking a container opens it instead of placing against it.
        // Shift is the override, so a chest is still something you can build
        // on rather than a permanent hole in the placement grid.
        net.send(ClientMsg::OpenContainer {
            pos: [hit.voxel.x, hit.voxel.y, hit.voxel.z],
        });
        return;
    } else {
        // Placement spends an item, so what is placeable is whatever the live
        // hotbar key points at — and a key only points at something the server
        // says we hold. With an empty key there is nothing to place; the server
        // would refuse it anyway.
        let Some(id) = hotbar.selected_block() else { return };
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
    directory.edited.insert(IVec3::new(
        target.x >> CHUNK_BIT,
        target.y >> CHUNK_BIT,
        target.z >> CHUNK_BIT,
    ));
    apply_edit(&map, &mut chunks, &mut slots, &mut pool_ops, &mut dirty_mesh, target, value);
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
    mut slots: ResMut<crate::pool::ChunkSlots>,
    mut pool_ops: ResMut<crate::pool::PoolOpQueue>,
    mut dirty_mesh: ResMut<crate::pool::DirtyMesh>,
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
            apply_edit(&map, &mut chunks, &mut slots, &mut pool_ops, &mut dirty_mesh, pos, prev);
            light_queue.edits.push(pos);
        }
    }
}

/// Apply an edit to a local chunk: update the CPU voxels, write the changed
/// u32 word through to the pooled voxel buffer, and queue a remesh of the
/// chunk (plus AO-visible neighbors when the voxel touches a border).
pub fn apply_edit(
    map: &ChunkMap,
    chunks: &mut Query<&mut VoxelChunk>,
    slots: &mut crate::pool::ChunkSlots,
    pool_ops: &mut crate::pool::PoolOpQueue,
    dirty_mesh: &mut crate::pool::DirtyMesh,
    v: IVec3,
    value: u8,
) {
    let cpos = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
    let Some(&e) = map.map.get(&cpos) else { return };
    let Ok(mut chunk) = chunks.get_mut(e) else { return };
    let l = IVec3::new(v.x & CHUNK_CLIP, v.y & CHUNK_CLIP, v.z & CHUNK_CLIP);
    chunk.volume.set(l.x, l.y, l.z, value);

    // An air chunk gaining its first solid voxel needs a mesh slot now.
    let mesh = match slots.get(cpos) {
        Some(s) if s.mesh != crate::pool::NO_MESH => s.mesh,
        Some(_) => match slots.ensure_mesh(cpos) {
            Some(m) => {
                let s = slots.get(cpos).expect("just ensured");
                pool_ops.push(crate::pool::PoolOp::UploadVoxels {
                    mesh: m,
                    volume: chunk.volume.clone(),
                });
                pool_ops.push(crate::pool::PoolOp::WriteMeshInfo { mesh: m, cpos, slot: s.slot });
                pool_ops.push(crate::pool::PoolOp::WriteDesc { slot: s.slot, cpos, mesh: m });
                dirty_mesh.0.push(m);
                return;
            }
            None => return,
        },
        None => return,
    };
    let idx = ((l.y + l.z * 32) * 32 + l.x) as u32;
    let word_idx = idx >> 2;
    let base = (word_idx * 4) as usize;
    let bytes = chunk.volume.as_bytes();
    let word = u32::from_le_bytes([bytes[base], bytes[base + 1], bytes[base + 2], bytes[base + 3]]);
    pool_ops.push(crate::pool::PoolOp::WriteVoxelWord { mesh, word_idx, word });
    dirty_mesh.0.push(mesh);
    // The mesher's AO probes read 1 voxel out-of-chunk (air today), but the
    // CPU volumes are per-chunk; border edits still change *this* chunk's own
    // faces only, so only it remeshes. (Neighbor-aware meshing is a later
    // phase.)
}
