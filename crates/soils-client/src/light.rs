//! Client light bookkeeping: the [`LightQueue`] event bus (chunks queue when
//! they stream in, voxels when edited) and the day/night `sky_term`. The
//! flood itself runs on the GPU over the pooled light cache — see
//! `gpu_light.rs` / `assets/shaders/light_flood.wgsl` (semantics defined by
//! `soils_sim::light`, which stays the CPU oracle and the server's
//! implementation).

use bevy::prelude::*;

use crate::chunk::WorldTime;
use crate::material::TERRAIN_BRIGHTNESS;

/// Light events awaiting a GPU flood batch: chunks that (re)entered the
/// caches and voxels whose block changed. Consumed by
/// `gpu_light::plan_light_jobs` under a per-frame budget.
#[derive(Resource, Default)]
pub struct LightQueue {
    pub chunks: Vec<IVec3>,
    pub edits: Vec<IVec3>,
}

impl LightQueue {
    /// Outstanding work: (chunks to flood, voxel relights, 0). Diagnostics —
    /// non-zero at "steady state" means the flood is still draining and any
    /// fps sampled there is a backlog number. (The third slot kept the old
    /// pad-upload count; pads died with the padded volumes.)
    pub fn backlog(&self) -> (usize, usize, usize) {
        (self.chunks.len(), self.edits.len(), 0)
    }

    /// Drop all queued work (warp: the whole world went away).
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.edits.clear();
    }

    /// Drop work queued for one chunk (it left the subscription). Queued
    /// voxel-edit relights are safe to keep: they no-op on unloaded space.
    pub fn unload(&mut self, pos: IVec3) {
        self.chunks.retain(|c| *c != pos);
    }
}

/// The current day-scaled skylight illuminance, mirrored into every chunk
/// material's `sky_term` when its quantized value changes.
#[derive(Resource)]
pub struct SkyTerm(pub f32);

impl Default for SkyTerm {
    fn default() -> Self {
        Self(TERRAIN_BRIGHTNESS)
    }
}

/// Keep the world `sky_term` in step with the day/night cycle. Quantized so
/// downstream consumers see a handful of changes per day cycle, not per frame
/// (the terrain uniform is rewritten every frame regardless).
pub fn update_sky_term(
    world_time: Res<WorldTime>,
    mut sky: ResMut<SkyTerm>,
    mut last_q: Local<Option<f32>>,
) {
    let day = soils_sim::ease10(world_time.daytime * 2.0 - 1.0);
    // Floor keeps night surfaces moonlit-visible (exposure dims them further).
    let q = ((0.05 + 0.95 * day) * 64.0).round() / 64.0;
    if *last_q == Some(q) {
        return;
    }
    *last_q = Some(q);
    sky.0 = TERRAIN_BRIGHTNESS * q;
}
