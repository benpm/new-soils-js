//! Client light bookkeeping: the [`LightQueue`] event bus (chunks queue when
//! they stream in, voxels when edited) and the day/night `sky_term`. The
//! flood itself runs on the GPU over the pooled light cache — see
//! `gpu_light.rs` / `assets/shaders/light_flood.wgsl` (semantics defined by
//! `soils_sim::light`, which stays the CPU oracle and the server's
//! implementation).

use bevy::prelude::*;

use crate::chunk::WorldTime;
use crate::pool::ChunkSlots;
use soils_protocol::chunk_of;
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
/// The player is a light source: a blocklight emitter riding the voxel the
/// camera is standing in.
///
/// It is seeded into the L0 grid by the flood's reseed pass, so it is occluded
/// by geometry exactly like a placed torch — it wraps around corners and stops
/// at walls, which a shader-side glow could not do. The cost is a reflood of
/// the emitter's chunk neighbourhood every time the player crosses a voxel
/// boundary, which is why [`track_player_light`] only queues on a *change* of
/// voxel and coalesces the vacated and occupied cells into one batch.
///
/// Client-side only. Nothing about light crosses the wire (the server holds no
/// light state for a player), so this never diverges anything that is shared.
#[derive(Resource)]
pub struct PlayerLight {
    /// Emission level, 0-15 as in [`soils_sim::light`]. 0 turns it off.
    pub level: u8,
    /// The voxel it is currently seeded at, once the player has one.
    pub voxel: Option<IVec3>,
}

/// Bright enough to walk a cave by (reach = the level, in voxels) and dim
/// enough that a lit surface still reads as lit next to it.
pub const DEFAULT_PLAYER_LIGHT: u8 = 12;

impl Default for PlayerLight {
    fn default() -> Self {
        Self { level: DEFAULT_PLAYER_LIGHT, voxel: None }
    }
}

/// The player's lantern as configured for this run: `SOILS_PLAYER_LIGHT`
/// (0-15, 0 = off) overriding [`DEFAULT_PLAYER_LIGHT`].
///
/// Exists for the lighting demo. A level-12 emitter riding the camera lights a
/// 12-voxel sphere around the player, so a dark room is not dark and the first
/// lamp placed in it adds nothing you can see. The `playerlight` console
/// command is the interactive equivalent — and a bot cannot type.
pub fn configured_player_light() -> PlayerLight {
    let mut light = PlayerLight::default();
    if let Some(level) = std::env::var("SOILS_PLAYER_LIGHT").ok().and_then(|v| v.parse().ok()) {
        light.set_level(level);
    }
    light
}

impl PlayerLight {
    /// Clamped to the 4-bit block channel; anything above [`MAX_LIGHT`] would
    /// wrap into the sky nibble.
    pub fn set_level(&mut self, level: i32) {
        self.level = level.clamp(0, soils_sim::light::MAX_LIGHT as i32) as u8;
    }

    /// Move the emitter to `pos`, returning the cells whose light that
    /// invalidates: the one it left and the one it arrived at.
    ///
    /// `mapped` answers whether a chunk is resident. The planner silently drops
    /// edits for chunks that are not, so committing the move before then would
    /// lose the light for good — at startup the player is standing in their
    /// chunk several seconds before it finishes streaming. Instead the move is
    /// held until the destination can actually be flooded, and retried on the
    /// frames in between.
    pub fn advance(&mut self, pos: Option<Vec3>, mapped: impl Fn(IVec3) -> bool) -> Vec<IVec3> {
        let wanted = pos.filter(|_| self.level > 0).map(|p| p.floor().as_ivec3());
        if wanted == self.voxel {
            return Vec::new();
        }
        if let Some(v) = wanted
            && !mapped(chunk_of(v))
        {
            return Vec::new();
        }
        let dirty = self.voxel.into_iter().chain(wanted).collect();
        self.voxel = wanted;
        dirty
    }
}

/// Move the player's emitter, queueing the light work its move implies.
///
/// Both the vacated and the newly occupied voxel go on the queue: the first to
/// clear the light that is no longer there, the second to seed it where it now
/// is. `plan_light_jobs` unions the two neighbourhoods, and for a single step
/// they overlap almost completely, so a walking player costs about one edit's
/// worth of flood per voxel crossed rather than two.
pub fn track_player_light(
    mut light: ResMut<PlayerLight>,
    mut queue: ResMut<LightQueue>,
    slots: Res<ChunkSlots>,
    player: Query<&crate::player::Player>,
) {
    let pos = player.single().ok().map(|p| p.sim.pos);
    queue.edits.extend(light.advance(pos, |c| slots.get(c).is_some()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything resident, the steady state once the world has streamed.
    fn resident(_: IVec3) -> bool {
        true
    }

    /// Drive the emitter through a series of positions, collecting what each
    /// step dirtied.
    fn walk(light: &mut PlayerLight, steps: &[Vec3]) -> Vec<Vec<IVec3>> {
        steps.iter().map(|&p| light.advance(Some(p), resident)).collect()
    }

    #[test]
    fn the_emitter_seeds_where_the_player_stands() {
        let mut light = PlayerLight::default();
        let dirty = light.advance(Some(Vec3::new(4.7, 20.2, -3.1)), resident);
        assert_eq!(dirty, vec![IVec3::new(4, 20, -4)], "floor, not truncate, below zero");
        assert_eq!(light.voxel, Some(IVec3::new(4, 20, -4)));
    }

    /// Moving inside one voxel must not queue anything: at walking speed that
    /// would reflood the neighbourhood every frame instead of every step.
    #[test]
    fn sub_voxel_movement_queues_nothing() {
        let mut light = PlayerLight::default();
        let steps = walk(&mut light, &[Vec3::new(4.1, 20.1, 4.1), Vec3::new(4.9, 20.9, 4.9)]);
        assert_eq!(steps[1], Vec::<IVec3>::new());
    }

    /// Crossing a boundary must clear the vacated cell as well as seed the new
    /// one, or the light smears a trail behind the player.
    #[test]
    fn crossing_a_boundary_queues_both_cells() {
        let mut light = PlayerLight::default();
        let steps = walk(&mut light, &[Vec3::new(4.5, 20.5, 4.5), Vec3::new(5.5, 20.5, 4.5)]);
        assert_eq!(steps[1], vec![IVec3::new(4, 20, 4), IVec3::new(5, 20, 4)]);
    }

    /// The bug this cost a debugging session to find: at startup the player is
    /// standing in a chunk that has not streamed yet. The planner drops edits
    /// for unmapped chunks, so committing the move then would lose the light
    /// permanently — the emitter is registered and the world stays black.
    #[test]
    fn the_move_waits_for_the_chunk_to_be_resident() {
        let mut light = PlayerLight::default();
        let pos = Vec3::new(282.0, 285.0, 268.0);

        assert!(light.advance(Some(pos), |_| false).is_empty(), "nothing to flood yet");
        assert_eq!(light.voxel, None, "the move must not be committed");

        let dirty = light.advance(Some(pos), resident);
        assert_eq!(dirty, vec![IVec3::new(282, 285, 268)], "seeds once the chunk lands");
    }

    /// Level 0 is off: the emitter is withdrawn, and its last cell reflooded
    /// so the light actually goes away.
    #[test]
    fn switching_it_off_withdraws_the_emitter() {
        let mut light = PlayerLight::default();
        let pos = Vec3::new(4.5, 20.5, 4.5);
        light.advance(Some(pos), resident);
        light.set_level(0);
        assert_eq!(light.advance(Some(pos), resident), vec![IVec3::new(4, 20, 4)]);
        assert_eq!(light.voxel, None);
    }

    /// The block channel is 4 bits; a level above 15 would carry into the sky
    /// nibble and light the world as if the sun were inside the player.
    #[test]
    fn the_level_is_clamped_to_the_block_channel() {
        let mut l = PlayerLight::default();
        l.set_level(99);
        assert_eq!(l.level, soils_sim::light::MAX_LIGHT);
        l.set_level(-4);
        assert_eq!(l.level, 0);
    }
}
