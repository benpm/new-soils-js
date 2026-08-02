//! A self-contained scene for eyeballing the radiance-cascades GI, enabled with
//! `SOILS_GI_DEMO=1` (best paired with `SOILS_SELFTEST=1` for the auto
//! screenshot, `SOILS_GI=1`, and `SOILS_DAYTIME=0.5` for a dark night so the
//! emissive blocks' bounce dominates). It bypasses the server and login,
//! hand-builds one chunk — an enclosed stone room with a cyan Diamond-ore and a
//! red Ruby-ore light cluster — and frames the camera on them. With GI on you
//! should see coloured light pooling on the floor and wall around each ore;
//! with GI off the scene is uniformly dim.

use bevy::prelude::*;
use soils_protocol::ChunkVolume;

use crate::chunk::{Blocks, ChunkMap};
use crate::gi::GiAssets;
use crate::login::LoginState;
use crate::player::{self, Player};
use crate::pool::{ChunkSlots, DirtyMesh, PoolOpQueue};
use crate::world_draw::TerrainParams;

/// The demo's single chunk position, so we can keep it remeshing briefly.
#[derive(Resource)]
pub struct GiDemoChunk(pub IVec3);

/// True when the GI demo scene is requested.
pub fn demo_enabled() -> bool {
    std::env::var("SOILS_GI_DEMO").is_ok()
}

/// Re-queue the demo chunk's mesh slot for the first few seconds so the mesher
/// re-runs after all its pool writes have landed.
pub fn gi_demo_keep_dirty(
    time: Res<Time>,
    demo: Option<Res<GiDemoChunk>>,
    slots: Res<ChunkSlots>,
    mut dirty: ResMut<DirtyMesh>,
) {
    if !demo_enabled() || time.elapsed_secs() > 4.0 {
        return;
    }
    if let Some(demo) = demo
        && let Some(s) = slots.get(demo.0)
        && s.mesh != crate::pool::NO_MESH
    {
        dirty.0.push(s.mesh);
    }
}

/// Build the demo scene once, on the first frame it can. No-op unless
/// `SOILS_GI_DEMO` is set.
#[allow(clippy::too_many_arguments)]
pub fn setup_gi_demo(
    mut commands: Commands,
    gi: Option<ResMut<GiAssets>>,
    blocks: Res<Blocks>,
    mut slots: ResMut<ChunkSlots>,
    mut pool_ops: ResMut<PoolOpQueue>,
    mut dirty_mesh: ResMut<DirtyMesh>,
    mut params: ResMut<TerrainParams>,
    mut map: ResMut<ChunkMap>,
    mut login: ResMut<LoginState>,
    mut player: Query<(&mut Player, &mut Transform)>,
    mut done: Local<bool>,
) {
    if *done || !demo_enabled() {
        return;
    }
    let Some(mut gi) = gi else { return };
    let Ok((mut p, mut cam)) = player.single_mut() else { return };
    *done = true;

    // Skip the login screen — there's no server in demo mode.
    login.done = true;

    let stone = blocks.0.id_of("Stone").unwrap_or(3);
    let diamond = blocks.0.id_of("Diamond Ore").unwrap_or(8);
    let ruby = blocks.0.id_of("Ruby Ore").unwrap_or(10);

    // One chunk at (8,8,8) => world x/y/z in [256, 288). Build a fully enclosed
    // stone room (solid, then carve an air cavity) so no skylight leaks in — the
    // only light is the two ores, making the GI bounce unmistakable.
    let cpos = IVec3::splat(8);
    let mut vol = ChunkVolume::empty();
    for lx in 0..32 {
        for ly in 0..32 {
            for lz in 0..32 {
                vol.set(lx, ly, lz, stone);
            }
        }
    }
    for lx in 2..30 {
        for ly in 15..26 {
            for lz in 2..28 {
                vol.set(lx, ly, lz, 0); // hollow interior
            }
        }
    }
    // Two ore lights as 3x3x3 clusters floating mid-room, spread apart, so the
    // coarse probe rays actually hit them.
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                vol.set(8 + dx, 20 + dy, 14 + dz, diamond); // ~world (264,276,270)
                vol.set(23 + dx, 20 + dy, 14 + dz, ruby); // ~world (279,276,270)
            }
        }
    }

    // Low flat ambient so the room reads as dark without GI; fog off for a
    // crisp close-up; L0 light bypassed (this chunk never queues for lighting,
    // and its warm blocklight would confound the GI A/B comparison). These
    // fields aren't overwritten by update_terrain_params, so setting them once
    // sticks (fog/light also flow from RenderToggles — the demo leaves the
    // toggles alone and relies on brightness + the unlit chunk).
    params.brightness = 300.0;

    slots.map_chunk(&mut pool_ops, &mut dirty_mesh, cpos, &vol).expect("demo pools");
    let e = commands
        .spawn(crate::chunk::VoxelChunk {
            pos: cpos,
            volume: vol,
        })
        .id();
    map.map.insert(cpos, e);
    commands.insert_resource(GiDemoChunk(cpos));

    // Force the GI volume to refill now that the room chunk exists.
    gi.mark_scene_dirty();

    // Frame the camera at the front of the room, looking down at the floor
    // between the two ore lights.
    player::teleport(&mut p, &mut cam, Vec3::new(272.0, 278.0, 261.0));
    cam.look_at(Vec3::new(272.0, 270.0, 274.0), Vec3::Y);

    info!("SOILS_GI_DEMO: built demo scene (chunk {cpos:?})");
}
