//! Demand-driven chunk residency (stream pipeline phase 6).
//!
//! Manifests no longer materialize chunks; they fill a [`ChunkDirectory`]
//! (what the server says exists: pristine or edited-with-payload). The GPU's
//! demand queue ([`crate::cull::DemandedChunks`], nearest-first, re-issued
//! every frame until fulfilled) drives actual materialization: edited entries
//! decode their payload, pristine entries generate locally (worldgen v2 is
//! bit-exact with the server). Both end as pooled GPU residency; a CPU
//! `VoxelChunk` entity exists only for the *mirror set* — the chunks CPU
//! systems actually read:
//!
//!   - a Chebyshev [`MIRROR_RADIUS`] box around the player (prediction,
//!     raycasts, physics ground),
//!   - a 1-box around every predicted physics prop,
//!   - every ever-edited chunk (edits need read-modify-write on real bytes).
//!
//! Everything else is GPU-only; `voxel_at` keeps its unloaded-=-air contract.
//! Edits racing materialization buffer in an overlay and re-apply when the
//! volume lands. Demands with no directory entry (readback races the
//! manifest) age out to a [`ClientMsg::ChunkFetch`] repair.

use std::collections::VecDeque;

use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use soils_protocol::{CHUNK_BIT, ChunkInfo, ChunkVolume};

use crate::chunk::{ChunkMap, VoxelChunk};
use crate::cull::DemandedChunks;
use crate::gpu_gen::{GEN_BUDGET as GPU_GEN_BUDGET, GenReady, GpuGenQueue};
use crate::light::LightQueue;
use crate::player::{Player, Streaming};
use crate::pool::{ChunkSlots, DirtyMesh, PoolOpQueue};
use crate::server_msg::{ClientGen, WorldEpoch};

/// Chebyshev radius of CPU-resident chunks around the player. Covers the
/// edit/raycast reach (~8 voxels) and the physics ground box (radius 1) with
/// a full chunk to spare.
pub const MIRROR_RADIUS: i32 = 2;
/// Chebyshev radius of CPU-resident chunks around each predicted prop.
const PROP_RADIUS: i32 = 1;
/// Seconds a demanded-but-unknown position may age before a `ChunkFetch`
/// repair (absorbs the readback-vs-manifest race on joins).
const FETCH_TTL: f32 = 2.0;
/// Hard cap on chunks mapped into the pools per frame (see the old
/// apply_chunks: a join burst mapping everything at once hangs weak GPUs).
const MAP_MAX: usize = 32;
/// Time box within the cap; wall-time so slow frames self-regulate.
const MAP_MS: f32 = 3.0;
/// Pristine positions handed to one background gen batch per frame.
const GEN_BATCH: usize = 64;

/// What the server says about a subscribed, not-yet-resident chunk.
pub enum DirEntry {
    /// Reproducible locally from the world identity.
    Pristine,
    /// Diverged from gen; the server's codec payload.
    Edited(Vec<u8>),
}

/// The manifest's view of the subscription, minus what's already resident,
/// plus the residency policy state (edited set, edit overlay).
#[derive(Resource, Default)]
pub struct ChunkDirectory {
    pub entries: HashMap<IVec3, DirEntry>,
    /// Ever-edited chunks: stay CPU-resident while subscribed.
    pub edited: HashSet<IVec3>,
    /// Edits that arrived before their chunk had CPU bytes to apply them to;
    /// drained when the volume materializes.
    pub overlay: HashMap<IVec3, Vec<(IVec3, u8)>>,
}

/// Ordered directory changes from the server. One message type because the
/// *relative order* of manifest and unload is the protocol (a chunk leaving
/// and re-entering the subscription is Unload then Manifest).
#[derive(Message)]
pub enum DirMsg {
    Manifest { infos: Vec<ChunkInfo>, epoch: u32 },
    Unload { pos: [i32; 3], epoch: u32 },
}

/// Materialization state: in-flight local gen, mirror-priority work, and the
/// fetch-repair aging table.
#[derive(Resource, Default)]
pub struct DemandProcessor {
    /// Dispatched to a gen worker, not yet landed.
    generating: HashSet<IVec3>,
    /// Positions the CPU mirror needs *now* (served before GPU demands).
    priority: VecDeque<IVec3>,
    priority_set: HashSet<IVec3>,
    /// Demanded positions with no directory entry, aging toward a fetch.
    fetch_wait: HashMap<IVec3, f32>,
}

impl DemandProcessor {
    pub fn clear(&mut self) {
        self.generating.clear();
        self.priority.clear();
        self.priority_set.clear();
        self.fetch_wait.clear();
    }

    fn push_priority(&mut self, pos: IVec3) {
        if self.priority_set.insert(pos) {
            self.priority.push_back(pos);
        }
    }
}

/// The chunks that must be CPU-resident this frame (player box + prop boxes;
/// the edited set is tracked separately in the directory). Rebuilt by
/// [`maintain_cpu_mirror`].
#[derive(Resource, Default)]
pub struct MirrorSet(pub HashSet<IVec3>);

fn chunk_of_pos(p: Vec3) -> IVec3 {
    let v = p.floor().as_ivec3();
    IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT)
}

/// Apply ordered manifest/unload messages to the directory. Edited entries
/// for already-resident chunks (fetch repairs, server re-sends) re-map
/// immediately — rare, and the data supersedes whatever we had.
#[allow(clippy::too_many_arguments)]
pub fn apply_directory(
    mut reader: MessageReader<DirMsg>,
    epoch: Res<WorldEpoch>,
    mut commands: Commands,
    mut dir: ResMut<ChunkDirectory>,
    mut proc: ResMut<DemandProcessor>,
    mut cgen: ResMut<ClientGen>,
    mut map: ResMut<ChunkMap>,
    mut chunks: Query<&mut VoxelChunk>,
    mut slots: ResMut<ChunkSlots>,
    mut pool_ops: ResMut<PoolOpQueue>,
    mut dirty_mesh: ResMut<DirtyMesh>,
    mut light_queue: ResMut<LightQueue>,
) {
    for msg in reader.read() {
        match msg {
            DirMsg::Manifest { infos, epoch: e } if *e == epoch.0 => {
                for info in infos {
                    match info {
                        ChunkInfo::Pristine { pos } => {
                            let cpos = IVec3::from_array(*pos);
                            if slots.get(cpos).is_some() {
                                continue; // already resident
                            }
                            if cgen.hash_ok {
                                dir.entries.insert(cpos, DirEntry::Pristine);
                            } else {
                                // Can't reproduce: ask for the payload (it
                                // returns as an Edited manifest entry).
                                cgen.fetch.push(*pos);
                            }
                            proc.fetch_wait.remove(&cpos);
                        }
                        ChunkInfo::Edited { pos, payload } => {
                            let cpos = IVec3::from_array(*pos);
                            dir.edited.insert(cpos);
                            proc.fetch_wait.remove(&cpos);
                            if slots.get(cpos).is_some() {
                                // Fresh authoritative bytes for a resident
                                // chunk: the payload already includes any
                                // edits we had overlaid.
                                dir.overlay.remove(&cpos);
                                let Some(volume) = soils_protocol::decode_chunk(payload) else {
                                    warn!("dropping undecodable chunk payload at {cpos}");
                                    continue;
                                };
                                materialize(
                                    &mut commands, &mut dir, &mut map, &mut chunks, &mut slots,
                                    &mut pool_ops, &mut dirty_mesh, &mut light_queue, cpos, volume,
                                    true,
                                );
                            } else {
                                dir.entries.insert(cpos, DirEntry::Edited(payload.clone()));
                            }
                        }
                    }
                }
            }
            DirMsg::Unload { pos, epoch: e } if *e == epoch.0 => {
                let cpos = IVec3::from_array(*pos);
                dir.entries.remove(&cpos);
                dir.edited.remove(&cpos);
                dir.overlay.remove(&cpos);
                proc.generating.remove(&cpos); // result drops on landing
                proc.fetch_wait.remove(&cpos);
                if let Some(entity) = map.map.remove(&cpos) {
                    commands.entity(entity).despawn();
                }
                slots.unmap_chunk(&mut pool_ops, &mut dirty_mesh, cpos);
                light_queue.unload(cpos);
            }
            _ => {} // stale epoch
        }
    }
}

/// Map a volume into the pooled caches and (when policy wants it) the CPU
/// mirror; drains any overlaid edits. Returns false on pool exhaustion (the
/// demand re-issues next frame).
#[allow(clippy::too_many_arguments)]
fn materialize(
    commands: &mut Commands,
    dir: &mut ChunkDirectory,
    map: &mut ChunkMap,
    chunks: &mut Query<&mut VoxelChunk>,
    slots: &mut ChunkSlots,
    pool_ops: &mut PoolOpQueue,
    dirty_mesh: &mut DirtyMesh,
    light_queue: &mut LightQueue,
    cpos: IVec3,
    mut volume: ChunkVolume,
    cpu_resident: bool,
) -> bool {
    // Overlaid edits bake into the volume *before* upload — one map, no
    // word-write churn.
    if let Some(edits) = dir.overlay.remove(&cpos) {
        for (v, value) in edits {
            volume.set(
                v.x & soils_protocol::CHUNK_CLIP,
                v.y & soils_protocol::CHUNK_CLIP,
                v.z & soils_protocol::CHUNK_CLIP,
                value,
            );
        }
    }
    if slots.map_chunk(pool_ops, dirty_mesh, cpos, &volume).is_none() {
        warn!("chunk pools exhausted; deferring {cpos}");
        return false;
    }
    light_queue.chunks.push(cpos);
    if cpu_resident {
        if let Some(&entity) = map.map.get(&cpos) {
            if let Ok(mut vc) = chunks.get_mut(entity) {
                vc.volume = volume;
            }
        } else {
            let e = commands.spawn(VoxelChunk { pos: cpos, volume }).id();
            map.map.insert(cpos, e);
        }
    }
    dir.entries.remove(&cpos);
    true
}

/// Keep the CPU mirror aligned with the player/prop boxes: request volumes
/// for mirror chunks that lack them, and drop entities that fell out of the
/// mirror (their GPU residency is untouched — that's the memory win).
pub fn maintain_cpu_mirror(
    mut commands: Commands,
    mut mirror: ResMut<MirrorSet>,
    dir: Res<ChunkDirectory>,
    mut proc: ResMut<DemandProcessor>,
    mut map: ResMut<ChunkMap>,
    slots: Res<ChunkSlots>,
    player: Query<&Transform, With<Player>>,
    props: Query<&Transform, With<crate::physics::PredictedProp>>,
) {
    if crate::gi_demo::demo_enabled() {
        return; // the demo owns its local chunk scene
    }
    let Ok(ptf) = player.single() else { return };
    let mut set = HashSet::default();
    let pc = chunk_of_pos(ptf.translation);
    for dx in -MIRROR_RADIUS..=MIRROR_RADIUS {
        for dy in -MIRROR_RADIUS..=MIRROR_RADIUS {
            for dz in -MIRROR_RADIUS..=MIRROR_RADIUS {
                set.insert(pc + IVec3::new(dx, dy, dz));
            }
        }
    }
    for tf in &props {
        let c = chunk_of_pos(tf.translation);
        for dx in -PROP_RADIUS..=PROP_RADIUS {
            for dy in -PROP_RADIUS..=PROP_RADIUS {
                for dz in -PROP_RADIUS..=PROP_RADIUS {
                    set.insert(c + IVec3::new(dx, dy, dz));
                }
            }
        }
    }

    // Missing volumes: known chunks materialize with priority; GPU-only
    // residents are pristine by policy (edited chunks always keep entities),
    // so a local re-gen reproduces their exact bytes.
    for &cpos in set.iter().chain(dir.edited.iter()) {
        if map.map.contains_key(&cpos) || proc.generating.contains(&cpos) {
            continue;
        }
        if dir.entries.contains_key(&cpos) || slots.get(cpos).is_some() {
            proc.push_priority(cpos);
        }
        // Neither in the directory nor resident: outside the subscription (or
        // its manifest hasn't landed); nothing to do yet.
    }

    // Entities that no longer need to be CPU-resident.
    let stale: Vec<IVec3> = map
        .map
        .keys()
        .filter(|c| !set.contains(*c) && !dir.edited.contains(*c))
        .copied()
        .collect();
    for cpos in stale {
        if let Some(entity) = map.map.remove(&cpos) {
            commands.entity(entity).despawn();
        }
    }
    mirror.0 = set;
}

/// The world-mutation half of the demand pump's parameters (bundled: Bevy
/// systems cap out at 16 params).
#[derive(bevy::ecs::system::SystemParam)]
pub struct MaterializeParams<'w, 's> {
    commands: Commands<'w, 's>,
    dir: ResMut<'w, ChunkDirectory>,
    map: ResMut<'w, ChunkMap>,
    chunks: Query<'w, 's, &'static mut VoxelChunk>,
    slots: ResMut<'w, ChunkSlots>,
    pool_ops: ResMut<'w, PoolOpQueue>,
    dirty_mesh: ResMut<'w, DirtyMesh>,
    light_queue: ResMut<'w, LightQueue>,
}

/// The demand pump: land finished gen batches, materialize mirror-priority
/// and GPU-demanded chunks under a time box, dispatch new gen batches, and
/// age unknown demands toward a fetch repair.
#[allow(clippy::too_many_arguments)]
pub fn process_demands(
    time: Res<Time>,
    epoch: Res<WorldEpoch>,
    m: MaterializeParams,
    mut proc: ResMut<DemandProcessor>,
    mut cgen: ResMut<ClientGen>,
    demanded: Res<DemandedChunks>,
    mirror: Res<MirrorSet>,
    mut streaming: ResMut<Streaming>,
    gen_ready: Option<Res<GenReady>>,
    mut gpu_gen: ResMut<GpuGenQueue>,
) {
    let MaterializeParams {
        mut commands,
        mut dir,
        mut map,
        mut chunks,
        mut slots,
        mut pool_ops,
        mut dirty_mesh,
        mut light_queue,
    } = m;
    // Land finished gen batches (stale epochs drop on the floor).
    while let Ok((e, batch)) = cgen.rx.try_recv() {
        if e == epoch.0 {
            cgen.ready.extend(batch);
        }
    }

    let t0 = std::time::Instant::now();
    let mut mapped = 0;
    let budget_left =
        |mapped: usize| mapped < MAP_MAX && (mapped < 2 || t0.elapsed().as_secs_f32() * 1000.0 < MAP_MS);

    // Landed gen results map first — they're either mirror-priority or were
    // demanded when dispatched.
    let landed: Vec<IVec3> = cgen.ready.keys().copied().collect();
    for cpos in landed {
        if !budget_left(mapped) {
            break;
        }
        // Unloaded while generating: drop the bytes.
        if !proc.generating.contains(&cpos) && !dir.entries.contains_key(&cpos) {
            cgen.ready.remove(&cpos);
            continue;
        }
        let Some(volume) = cgen.ready.remove(&cpos) else { continue };
        let cpu = mirror.0.contains(&cpos) || dir.edited.contains(&cpos);
        proc.generating.remove(&cpos);
        if materialize(
            &mut commands, &mut dir, &mut map, &mut chunks, &mut slots, &mut pool_ops,
            &mut dirty_mesh, &mut light_queue, cpos, volume, cpu,
        ) {
            proc.priority_set.remove(&cpos);
            mapped += 1;
        }
        // Pool exhaustion drops the bytes: the entry stays in the directory
        // and the demand re-issues, so the chunk re-generates later.
    }

    // Work intake: mirror priority first, then the GPU's nearest-first
    // demands. Pristine positions batch into one gen dispatch; edited
    // payloads decode inline under the map budget.
    let mut gen_batch: Vec<IVec3> = Vec::new();
    let take: Vec<IVec3> = proc
        .priority
        .drain(..)
        .chain(demanded.positions.iter().copied())
        .collect();
    let mut seen: HashSet<IVec3> = HashSet::default();
    for cpos in take {
        if !seen.insert(cpos) || proc.generating.contains(&cpos) {
            continue;
        }
        match dir.entries.get(&cpos) {
            Some(DirEntry::Pristine) => {
                let want_cpu = mirror.0.contains(&cpos)
                    || dir.edited.contains(&cpos)
                    || dir.overlay.contains_key(&cpos);
                if !want_cpu {
                    // Born on the GPU: allocate slots now, dispatch the gen
                    // kernel this frame (occupancy readback demotes all-air
                    // chunks later). Waits for the pipelines to compile —
                    // demands re-issue until then.
                    if gen_ready.is_some() && gpu_gen.0.len() < GPU_GEN_BUDGET {
                        if let Some(s) =
                            slots.map_chunk_gen(&mut pool_ops, &mut dirty_mesh, cpos)
                        {
                            gpu_gen.0.push((cpos, s.mesh));
                            light_queue.chunks.push(cpos);
                            dir.entries.remove(&cpos);
                            proc.priority_set.remove(&cpos);
                        }
                    }
                } else if gen_batch.len() < GEN_BATCH {
                    // Mirror/edited chunks need CPU bytes: background gen.
                    gen_batch.push(cpos);
                    proc.generating.insert(cpos);
                }
            }
            Some(DirEntry::Edited(payload)) => {
                if !budget_left(mapped) {
                    continue;
                }
                let Some(volume) = soils_protocol::decode_chunk(payload) else {
                    warn!("dropping undecodable chunk payload at {cpos}");
                    dir.entries.remove(&cpos);
                    continue;
                };
                let cpu = true; // edited chunks stay CPU-resident
                if materialize(
                    &mut commands, &mut dir, &mut map, &mut chunks, &mut slots, &mut pool_ops,
                    &mut dirty_mesh, &mut light_queue, cpos, volume, cpu,
                ) {
                    proc.priority_set.remove(&cpos);
                    mapped += 1;
                }
            }
            None => {
                if slots.get(cpos).is_some() {
                    // Resident GPU-only chunk needed by the mirror: pristine
                    // by policy — re-gen it for CPU bytes. Skip when the
                    // entity already exists (the demand list is a frame-start
                    // snapshot; re-genning a just-materialized chunk would
                    // clobber its overlaid edits with pristine bytes).
                    if (mirror.0.contains(&cpos) || dir.edited.contains(&cpos))
                        && !map.map.contains_key(&cpos)
                    {
                        if gen_batch.len() < GEN_BATCH {
                            gen_batch.push(cpos);
                            proc.generating.insert(cpos);
                        }
                    } else {
                        proc.priority_set.remove(&cpos);
                    }
                    continue;
                }
                // Unknown: age toward a fetch repair, but only for positions
                // the server plausibly owes us (inside our view box — stale
                // post-warp readbacks fall outside and just expire).
                let t = proc.fetch_wait.entry(cpos).or_insert(0.0);
                *t += time.delta_secs();
                if *t > FETCH_TTL {
                    proc.fetch_wait.remove(&cpos);
                    if let Some(last) = streaming.last_chunk
                        && (cpos - last).abs().max_element() <= streaming.load_radius
                    {
                        cgen.fetch.push(cpos.to_array());
                    }
                }
            }
        }
    }
    // Re-queue mirror-priority positions that couldn't be served this frame
    // (still generating or waiting on the directory).
    let requeue: Vec<IVec3> =
        proc.priority_set.iter().copied().filter(|c| !proc.generating.contains(c)).collect();
    for c in requeue {
        proc.priority.push_back(c);
    }

    if !gen_batch.is_empty() {
        cgen.dispatch(epoch.0, gen_batch);
    }

    // HUD estimate: subscribed-but-unmapped plus in-flight gen.
    streaming.pending = dir.entries.len() + proc.generating.len();
}

/// Buffer a remote edit whose chunk has no CPU bytes yet: overlay it and (if
/// the chunk is GPU-resident) force it into the mirror path so the combined
/// volume re-uploads. Called by `server_msg::apply_edits` when the normal
/// entity path misses.
pub fn overlay_edit(
    dir: &mut ChunkDirectory,
    proc: &mut DemandProcessor,
    v: IVec3,
    value: u8,
) {
    let cpos = IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT);
    dir.edited.insert(cpos);
    dir.overlay.entry(cpos).or_default().push((v, value));
    proc.push_priority(cpos);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_msg::register;
    use soils_protocol::{GenParams, encode_chunk};
    use soils_worldgen::{TerrainGen, WorldType};

    const SEED: u32 = 12345;

    /// Build a headless app running the demand pipeline exactly as main.rs
    /// wires it, with a player parked at `spawn` chunk.
    fn test_app(spawn: IVec3) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        register(&mut app);
        app.insert_resource(ChunkMap::default())
            .insert_resource(Streaming {
                last_chunk: None,
                load_radius: 3,
                sent_radius: Some(3),
                pending: 0,
            })
            .init_resource::<ChunkSlots>()
            .init_resource::<PoolOpQueue>()
            .init_resource::<DirtyMesh>()
            .init_resource::<LightQueue>()
            .init_resource::<DemandedChunks>()
            .init_resource::<GpuGenQueue>()
            // Pretend the GPU gen pipelines are up; a fake sink stands in for
            // the render-world dispatch (jobs are resident the moment their
            // slots map, which is what the assertions check).
            .insert_resource(GenReady)
            .add_systems(
                Update,
                (
                    fake_demand_scan,
                    apply_directory,
                    maintain_cpu_mirror,
                    process_demands,
                    |mut q: ResMut<GpuGenQueue>| q.0.clear(),
                )
                    .chain(),
            );
        app.world_mut().spawn((
            Player::at((spawn * 32 + IVec3::splat(16)).as_vec3()),
            Transform::from_translation((spawn * 32 + IVec3::splat(16)).as_vec3()),
        ));
        app.world_mut().resource_mut::<ClientGen>().configure(gen_params());
        assert!(app.world().resource::<ClientGen>().hash_ok);
        app
    }

    fn gen_params() -> GenParams {
        let t = TerrainGen::new(SEED, WorldType::Normal);
        GenParams {
            seed: SEED as i64,
            world_type: 0,
            graph_hash: soils_worldgen::graph_hash(t.graph()),
        }
    }

    /// CPU replica of the GPU demand scan: every unmapped window position.
    fn fake_demand_scan(
        mut demanded: ResMut<DemandedChunks>,
        slots: Res<ChunkSlots>,
        streaming: Res<Streaming>,
        player: Query<&Transform, With<Player>>,
    ) {
        let Ok(tf) = player.single() else { return };
        let pc = super::chunk_of_pos(tf.translation);
        let r = streaming.load_radius;
        demanded.positions.clear();
        for dx in -r..=r {
            for dy in -r..=r {
                for dz in -r..=r {
                    let c = pc + IVec3::new(dx, dy, dz);
                    if slots.get(c).is_none() {
                        demanded.positions.push(c);
                    }
                }
            }
        }
        demanded.total = demanded.positions.len() as u32;
    }

    fn manifest_for_box(app: &mut App, center: IVec3, r: i32, edited: &[(IVec3, IVec3, u8)]) {
        let terrain = TerrainGen::new(SEED, WorldType::Normal);
        let registry = soils_worldgen::default_registry();
        let mut infos = Vec::new();
        for dx in -r..=r {
            for dy in -r..=r {
                for dz in -r..=r {
                    let c = center + IVec3::new(dx, dy, dz);
                    let ed: Vec<_> = edited.iter().filter(|(ec, ..)| *ec == c).collect();
                    if ed.is_empty() {
                        infos.push(ChunkInfo::Pristine { pos: c.to_array() });
                    } else {
                        let mut vol = terrain.generate(c, &registry);
                        for (_, v, val) in ed {
                            vol.set(v.x & 31, v.y & 31, v.z & 31, *val);
                        }
                        infos.push(ChunkInfo::Edited { pos: c.to_array(), payload: encode_chunk(&vol) });
                    }
                }
            }
        }
        let epoch = app.world().resource::<WorldEpoch>().0;
        app.world_mut().write_message(DirMsg::Manifest { infos, epoch });
    }

    /// Update until the directory drains and gen lands (async worker threads).
    fn settle(app: &mut App) {
        for _ in 0..600 {
            app.update();
            let done = {
                let w = app.world();
                w.resource::<ChunkDirectory>().entries.is_empty()
                    && w.resource::<DemandProcessor>().generating.is_empty()
                    && w.resource::<DemandProcessor>().priority.is_empty()
            };
            if done {
                // A couple more frames so mirror upgrades finish too.
                app.update();
                app.update();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("demand pipeline did not settle");
    }

    fn cpu_volume(app: &mut App, cpos: IVec3) -> Option<ChunkVolume> {
        let e = *app.world().resource::<ChunkMap>().map.get(&cpos)?;
        Some(app.world().get::<VoxelChunk>(e)?.volume.clone())
    }

    #[test]
    fn mirror_invariant_and_residency() {
        let spawn = IVec3::new(8, 8, 8);
        let mut app = test_app(spawn);
        let edit_v = spawn * 32 + IVec3::new(1, 2, 3);
        manifest_for_box(&mut app, spawn, 3, &[(spawn, edit_v, 9)]);
        settle(&mut app);

        let terrain = TerrainGen::new(SEED, WorldType::Normal);
        let registry = soils_worldgen::default_registry();

        // Everything in the window is GPU-resident.
        for dx in -3..=3 {
            for dy in -3..=3 {
                for dz in -3..=3 {
                    let c = spawn + IVec3::new(dx, dy, dz);
                    assert!(
                        app.world().resource::<ChunkSlots>().get(c).is_some(),
                        "chunk {c} not GPU-resident"
                    );
                }
            }
        }
        // The R2 mirror has entities with bit-exact volumes; outside it (and
        // not edited) there are none.
        for dx in -3..=3 {
            for dy in -3..=3 {
                for dz in -3..=3 {
                    let c = spawn + IVec3::new(dx, dy, dz);
                    let d = IVec3::new(dx, dy, dz).abs().max_element();
                    let vol = cpu_volume(&mut app, c);
                    if d <= MIRROR_RADIUS {
                        let vol = vol.unwrap_or_else(|| panic!("mirror chunk {c} has no entity"));
                        let mut want = terrain.generate(c, &registry);
                        if c == spawn {
                            want.set(edit_v.x & 31, edit_v.y & 31, edit_v.z & 31, 9);
                        }
                        assert_eq!(vol.as_bytes(), want.as_bytes(), "mirror bytes at {c}");
                    } else {
                        assert!(vol.is_none(), "non-mirror chunk {c} kept an entity");
                    }
                }
            }
        }

        // Walk the player: the mirror follows, old entities drop, the edited
        // chunk keeps its entity even out of mirror range.
        let new_pc = spawn + IVec3::new(3, 0, 0);
        {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&mut Transform, With<Player>>();
            let mut tf = q.single_mut(world).unwrap();
            tf.translation = (new_pc * 32 + IVec3::splat(16)).as_vec3();
        }
        // The server re-manifests the window around the new position.
        manifest_for_box(&mut app, new_pc, 3, &[]);
        settle(&mut app);
        for dx in -MIRROR_RADIUS..=MIRROR_RADIUS {
            let c = new_pc + IVec3::new(dx, 0, 0);
            assert!(cpu_volume(&mut app, c).is_some(), "moved mirror missing {c}");
        }
        let far = spawn - IVec3::new(2, 2, 2); // out of new mirror, unedited
        assert!(cpu_volume(&mut app, far).is_none(), "stale mirror entity survived at {far}");
        assert!(cpu_volume(&mut app, spawn).is_some(), "edited chunk lost its entity");

        // Unload drops everything for that chunk.
        let epoch = app.world().resource::<WorldEpoch>().0;
        app.world_mut().write_message(DirMsg::Unload { pos: spawn.to_array(), epoch });
        app.update();
        assert!(cpu_volume(&mut app, spawn).is_none(), "unloaded chunk kept its entity");
        assert!(
            app.world().resource::<ChunkSlots>().get(spawn).is_none(),
            "unloaded chunk kept slots"
        );
    }

    #[test]
    fn overlay_edit_race() {
        let spawn = IVec3::new(4, 9, 4);
        let mut app = test_app(spawn);
        // Edit a chunk OUTSIDE the mirror before anything materializes: it
        // must overlay, then land baked into the materialized volume, and the
        // chunk must become CPU-resident (edited policy).
        let target_c = spawn + IVec3::new(3, 0, 3);
        let v = target_c * 32 + IVec3::new(5, 6, 7);
        manifest_for_box(&mut app, spawn, 3, &[]);
        app.world_mut()
            .resource_scope(|world, mut dir: Mut<ChunkDirectory>| {
                let mut proc = world.resource_mut::<DemandProcessor>();
                overlay_edit(&mut dir, &mut proc, v, 7);
            });
        settle(&mut app);

        let vol = cpu_volume(&mut app, target_c).expect("edited chunk should be CPU-resident");
        assert_eq!(vol.get(v.x & 31, v.y & 31, v.z & 31), 7, "overlay edit not baked");
        let terrain = TerrainGen::new(SEED, WorldType::Normal);
        let registry = soils_worldgen::default_registry();
        let mut want = terrain.generate(target_c, &registry);
        want.set(v.x & 31, v.y & 31, v.z & 31, 7);
        assert_eq!(vol.as_bytes(), want.as_bytes(), "base bytes + overlay mismatch");
        assert!(
            app.world().resource::<ChunkDirectory>().overlay.is_empty(),
            "overlay not drained"
        );
    }
}
