//! A self-contained scene for eyeballing the radiance-cascades GI, enabled with
//! `SOILS_GI_DEMO=1` (best paired with `SOILS_SELFTEST=1` for the auto
//! screenshot, `SOILS_GI=1`, and `SOILS_DAYTIME=0.5` for a dark night so the
//! emissive blocks' bounce dominates). It bypasses the server and login,
//! hand-builds one chunk — a stone floor and back wall with a cyan Diamond-ore
//! and a red Ruby-ore light source sitting on the floor — and frames the camera
//! on them. With GI on you should see coloured light pooling on the floor and
//! wall around each ore; with GI off the scene is uniformly dim.

use bevy::prelude::*;
use bevy::render::storage::ShaderStorageBuffer;
use soils_protocol::ChunkVolume;

use crate::chunk::{Blocks, ChunkMap};
use crate::gi::GiAssets;
use crate::gpu_mesh::{spawn_gpu_chunk, AtlasAssets, GpuChunk};
use crate::login::LoginState;
use crate::material::{AtlasParams, ChunkMeshMaterial};
use crate::player::Player;

/// The demo's single chunk entity, so we can keep it dirty until it meshes.
#[derive(Resource)]
pub struct GiDemoChunk(pub Entity);

/// True when the GI demo scene is requested.
pub fn demo_enabled() -> bool {
    std::env::var("SOILS_GI_DEMO").is_ok()
}

/// Keep the demo chunk flagged dirty for the first few seconds so the GPU
/// mesher re-runs until its voxel buffer is resident and the mesh is built
/// (a single freshly-spawned chunk can otherwise lose the 4-frame dirty window
/// before its buffer uploads, and never mesh).
pub fn gi_demo_keep_dirty(
    time: Res<Time>,
    demo: Option<Res<GiDemoChunk>>,
    mut chunks: Query<&mut GpuChunk>,
) {
    if !demo_enabled() || time.elapsed_secs() > 4.0 {
        return;
    }
    if let Some(demo) = demo {
        if let Ok(mut gc) = chunks.get_mut(demo.0) {
            gc.pending = 2;
        }
    }
}

/// Build the demo scene once, on the first frame it can (all render assets and
/// the player exist by then). No-op unless `SOILS_GI_DEMO` is set.
#[allow(clippy::too_many_arguments)]
pub fn setup_gi_demo(
    mut commands: Commands,
    atlas: Option<Res<AtlasAssets>>,
    gi: Option<ResMut<GiAssets>>,
    blocks: Res<Blocks>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    mut materials: ResMut<Assets<ChunkMeshMaterial>>,
    mut map: ResMut<ChunkMap>,
    mut login: ResMut<LoginState>,
    mut player: Query<&mut Transform, With<Player>>,
    mut done: Local<bool>,
) {
    if *done || !demo_enabled() {
        return;
    }
    let (Some(atlas), Some(mut gi)) = (atlas, gi) else { return };
    let Ok(mut cam) = player.single_mut() else { return };
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
    // Two ore lights as 3x3x3 clusters floating mid-room, spread apart. Bigger
    // and off the floor so probes in the surrounding air can actually trace to
    // them (a lone floor voxel is missed by the coarse probe rays), and their
    // coloured bounce (cyan Diamond, red Ruby) pools on floor and walls.
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                vol.set(8 + dx, 20 + dy, 14 + dz, diamond); // ~world (264,276,270)
                vol.set(23 + dx, 20 + dy, 14 + dz, ruby); // ~world (279,276,270)
            }
        }
    }

    let (gi_origin, gi_enabled) = gi.apply_params();
    let params = AtlasParams {
        ambient_occlusion: 1.0,
        // Low flat ambient so the room reads as dark without GI; the ores' GI
        // bounce then stands out. Fog off for a crisp close-up.
        brightness: 300.0,
        fog_density: 0.0,
        gi_origin,
        gi_enabled,
        ..default()
    };
    let e = spawn_gpu_chunk(
        &mut commands,
        &mut buffers,
        &mut materials,
        &atlas,
        cpos,
        vol,
        params,
        gi.cascade0(),
    );
    map.map.insert(cpos, e);
    commands.insert_resource(GiDemoChunk(e));

    // Force the GI volume to refill (now that the room chunk exists) and re-push
    // origin/enable into all materials next frame — otherwise this chunk, spawned
    // after GI settled, keeps stale params and never lights up.
    gi.mark_scene_dirty();

    // Frame the camera at the front of the room, looking down at the floor
    // between the two ore lights (the downward view renders reliably here).
    cam.translation = Vec3::new(272.0, 278.0, 261.0);
    cam.look_at(Vec3::new(272.0, 270.0, 274.0), Vec3::Y);

    info!("SOILS_GI_DEMO: built demo scene (chunk {cpos:?}); GI enabled={gi_enabled}");
}
