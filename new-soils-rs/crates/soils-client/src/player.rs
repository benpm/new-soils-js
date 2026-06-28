//! First-person player: mouse-look, fly/walk movement with AABB voxel
//! collision, pointer-lock, and distance-based chunk streaming requests.

use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use soils_protocol::{CHUNK_BIT, CHUNK_SIZE, ClientMsg};

use crate::chunk::{ChunkMap, VoxelChunk, is_solid};
use crate::net::NetClient;

const MOVE_SPEED: f32 = 8.0;
const SPRINT_MULT: f32 = 4.0;
const MOUSE_SENS: f32 = 0.0022;
const GRAVITY: f32 = 28.0;
const JUMP_SPEED: f32 = 9.0;

// Player AABB relative to the eye position (eye near the top of the body).
const EYE_TO_FEET: f32 = 1.6;
const EYE_TO_HEAD: f32 = 0.2;
const HALF_WIDTH: f32 = 0.3;

#[derive(Component)]
pub struct Player {
    pub yaw: f32,
    pub pitch: f32,
    pub velocity: Vec3,
    pub flying: bool,
    pub grounded: bool,
}

impl Default for Player {
    fn default() -> Self {
        // Start looking slightly downward so terrain is in view on spawn.
        Self { yaw: 0.0, pitch: -0.5, velocity: Vec3::ZERO, flying: true, grounded: false }
    }
}

/// Tracks which chunk the player was last in, to drive streaming.
#[derive(Resource)]
pub struct Streaming {
    pub last_chunk: Option<IVec3>,
    pub load_radius: i32,
}

impl Default for Streaming {
    fn default() -> Self {
        Self { last_chunk: None, load_radius: 4 }
    }
}

/// Toggle pointer-lock with Escape; re-grab on click.
pub fn cursor_toggle(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut cursor: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    let Ok(mut cursor) = cursor.single_mut() else { return };
    if keys.just_pressed(KeyCode::Escape) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    } else if buttons.just_pressed(MouseButton::Left)
        || buttons.just_pressed(MouseButton::Right)
    {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
}

/// Mouse-look: accumulate yaw/pitch and orient the camera.
pub fn mouse_look(
    motion: Res<AccumulatedMouseMotion>,
    cursor: Query<&CursorOptions, With<PrimaryWindow>>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    // Only look while the cursor is grabbed.
    if let Ok(cursor) = cursor.single() {
        if cursor.grab_mode == CursorGrabMode::None {
            return;
        }
    }
    let delta = motion.delta;
    if delta == Vec2::ZERO {
        return;
    }
    for (mut player, mut transform) in &mut query {
        player.yaw -= delta.x * MOUSE_SENS;
        player.pitch = (player.pitch - delta.y * MOUSE_SENS)
            .clamp(-std::f32::consts::FRAC_PI_2 + 0.01, std::f32::consts::FRAC_PI_2 - 0.01);
        transform.rotation =
            Quat::from_axis_angle(Vec3::Y, player.yaw) * Quat::from_axis_angle(Vec3::X, player.pitch);
    }
}

/// Movement + physics. Fly mode is free 6-DOF; walk mode applies gravity and
/// AABB voxel collision.
pub fn movement(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    map: Res<ChunkMap>,
    chunks: Query<&VoxelChunk>,
    mut query: Query<(&mut Player, &mut Transform)>,
) {
    let dt = time.delta_secs();
    for (mut player, mut transform) in &mut query {
        if keys.just_pressed(KeyCode::KeyF) {
            player.flying = !player.flying;
            player.velocity = Vec3::ZERO;
        }

        // Horizontal basis from yaw only.
        let yaw_rot = Quat::from_axis_angle(Vec3::Y, player.yaw);
        let forward = yaw_rot * Vec3::NEG_Z;
        let right = yaw_rot * Vec3::X;

        let mut wish = Vec3::ZERO;
        if keys.pressed(KeyCode::KeyW) {
            wish += forward;
        }
        if keys.pressed(KeyCode::KeyS) {
            wish -= forward;
        }
        if keys.pressed(KeyCode::KeyD) {
            wish += right;
        }
        if keys.pressed(KeyCode::KeyA) {
            wish -= right;
        }
        wish = wish.normalize_or_zero();

        let mut speed = MOVE_SPEED;
        if keys.pressed(KeyCode::ShiftLeft) {
            speed *= SPRINT_MULT;
        }

        if player.flying {
            let mut dir = wish * speed;
            if keys.pressed(KeyCode::Space) {
                dir.y += speed;
            }
            if keys.pressed(KeyCode::ControlLeft) {
                dir.y -= speed;
            }
            transform.translation += dir * dt;
            continue;
        }

        // Walking: horizontal from input, vertical integrates gravity.
        player.velocity.x = wish.x * speed;
        player.velocity.z = wish.z * speed;
        player.velocity.y -= GRAVITY * dt;
        if player.grounded && keys.just_pressed(KeyCode::Space) {
            player.velocity.y = JUMP_SPEED;
        }

        let delta = player.velocity * dt;
        let mut pos = transform.translation;

        // Resolve one axis at a time; stop on contact.
        pos.x += delta.x;
        if collides(&map, &chunks, pos) {
            pos.x -= delta.x;
            player.velocity.x = 0.0;
        }
        pos.z += delta.z;
        if collides(&map, &chunks, pos) {
            pos.z -= delta.z;
            player.velocity.z = 0.0;
        }
        pos.y += delta.y;
        player.grounded = false;
        if collides(&map, &chunks, pos) {
            pos.y -= delta.y;
            if player.velocity.y < 0.0 {
                player.grounded = true;
            }
            player.velocity.y = 0.0;
        }

        transform.translation = pos;
    }
}

/// True if the player AABB at `eye` overlaps any solid voxel.
fn collides(map: &ChunkMap, chunks: &Query<&VoxelChunk>, eye: Vec3) -> bool {
    let min = Vec3::new(eye.x - HALF_WIDTH, eye.y - EYE_TO_FEET, eye.z - HALF_WIDTH);
    let max = Vec3::new(eye.x + HALF_WIDTH, eye.y + EYE_TO_HEAD, eye.z + HALF_WIDTH);
    let (x0, y0, z0) = (min.x.floor() as i32, min.y.floor() as i32, min.z.floor() as i32);
    let (x1, y1, z1) = (max.x.floor() as i32, max.y.floor() as i32, max.z.floor() as i32);
    for x in x0..=x1 {
        for y in y0..=y1 {
            for z in z0..=z1 {
                if is_solid(map, chunks, IVec3::new(x, y, z)) {
                    return true;
                }
            }
        }
    }
    false
}

/// Request chunks around the player whenever they cross a chunk boundary.
pub fn request_chunks(
    net: Res<NetClient>,
    map: Res<ChunkMap>,
    mut streaming: ResMut<Streaming>,
    query: Query<&Transform, With<Player>>,
) {
    let Ok(transform) = query.single() else { return };
    let p = transform.translation;
    let pc = IVec3::new(
        (p.x.floor() as i32) >> CHUNK_BIT,
        (p.y.floor() as i32) >> CHUNK_BIT,
        (p.z.floor() as i32) >> CHUNK_BIT,
    );

    if streaming.last_chunk == Some(pc) {
        return;
    }
    streaming.last_chunk = Some(pc);

    let r = streaming.load_radius;
    let mut positions = Vec::new();
    for dx in -r..=r {
        for dy in -r..=r {
            for dz in -r..=r {
                let cpos = pc + IVec3::new(dx, dy, dz);
                if !map.map.contains_key(&cpos) {
                    positions.push([cpos.x, cpos.y, cpos.z]);
                }
            }
        }
    }
    if !positions.is_empty() {
        // Nearest-first so the area around the player fills in first.
        positions.sort_by_key(|c| {
            let d = IVec3::new(c[0], c[1], c[2]) - pc;
            d.x * d.x + d.y * d.y + d.z * d.z
        });
        net.send(ClientMsg::ReqChunks { positions });
    }

    let _ = CHUNK_SIZE;
}
