//! Voxel terrain → Avian static colliders.
//!
//! A resident chunk becomes one static `Collider::voxels` entity. Parry's voxel
//! convention matches ours exactly: grid coord `c` (with unit voxel size) spans
//! world `[c, c+1]`, the same cell `soils-sim` collides against. So a chunk's
//! collider entity sits at world `chunk_pos * CHUNK_SIZE` with chunk-local grid
//! coordinates — no half-voxel fudge.

use avian3d::prelude::*;
use bevy::prelude::*;
use soils_protocol::{CHUNK_SIZE, ChunkVolume, voxel::AIR};

/// Build a static collider for one chunk from its solid (non-air) voxels.
/// Returns `None` for an all-air chunk (nothing to collide with).
pub fn chunk_collider(volume: &ChunkVolume) -> Option<Collider> {
    let mut coords: Vec<IVec3> = Vec::new();
    for x in 0..CHUNK_SIZE {
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                if volume.get(x, y, z) != AIR {
                    coords.push(IVec3::new(x, y, z));
                }
            }
        }
    }
    if coords.is_empty() {
        return None;
    }
    Some(Collider::voxels(Vec3::ONE, &coords))
}

/// World-space translation of a chunk's collider entity: the chunk origin in
/// voxel units. Chunk-local grid coords then land on the correct world cells.
pub fn chunk_origin_world(chunk_pos: IVec3) -> Vec3 {
    (chunk_pos * CHUNK_SIZE).as_vec3()
}

/// A ready-to-spawn static-terrain bundle for a chunk, or `None` if all air.
pub fn chunk_collider_bundle(chunk_pos: IVec3, volume: &ChunkVolume) -> Option<impl Bundle> {
    let collider = chunk_collider(volume)?;
    Some((
        RigidBody::Static,
        collider,
        Transform::from_translation(chunk_origin_world(chunk_pos)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cube_body, tests_support::headless_app};
    use avian3d::prelude::Position;

    /// A chunk whose bottom layer (y=0) is solid stone, rest air.
    fn floor_chunk() -> ChunkVolume {
        let mut v = ChunkVolume::empty();
        for x in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                v.set(x, 0, z, 1);
            }
        }
        v
    }

    #[test]
    fn chunk_collider_is_none_for_air() {
        assert!(chunk_collider(&ChunkVolume::empty()).is_none());
    }

    #[test]
    fn cube_rests_on_chunk_floor_at_expected_height() {
        let mut app = headless_app();
        // Chunk at origin: solid layer occupies world cells y in [0,1].
        app.world_mut()
            .spawn(chunk_collider_bundle(IVec3::ZERO, &floor_chunk()).expect("solid chunk"));
        // Unit cube dropped from y=8, centred over the chunk.
        let cube = app
            .world_mut()
            .spawn(cube_body(Vec3::new(4.0, 8.0, 4.0), 1.0))
            .id();

        app.update();
        for _ in 0..(crate::PHYSICS_HZ as usize * 4) {
            app.update();
        }

        let pos = app.world().entity(cube).get::<Position>().unwrap().0;
        // Floor top is at world y=1; cube half-extent 0.5 → centre rests ~1.5.
        assert!(
            (pos.y - 1.5).abs() < 0.15,
            "cube should rest on chunk floor near y=1.5, got y={}",
            pos.y
        );
    }
}
