//! The headless Bevy ECS app that owns all game state (TODO phase 5 /
//! game-systems M2). Connection tasks are pure pumps — they push decoded
//! `ClientMsg`s into per-client inboxes and flush per-client outboxes — and
//! every decision happens here on a fixed tick ([`soils_sim::SERVER_TICK_HZ`],
//! 20 Hz), in plain single-threaded ECS systems. The old
//! `Arc<Mutex<HashMap>>` web (worlds, players, clock, broadcast channel) is
//! gone: this app owns that state as ECS resources; the only cross-thread
//! artifacts left are the inbox/outbox channels and a player-count atomic for
//! the LAN discovery responder.
//!
//! Chunk serving is a per-client pipeline: each request queues as a job whose
//! positions are dispatched in nearest-first waves (the client already sorts
//! them). At dispatch, cached/persisted chunks are made resident inline; the
//! missing remainder generates on the rayon pool off the tick thread and is
//! adopted the tick it completes, so a fresh world's 729-chunk burst never
//! stalls a tick.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use bevy_app::{App, AppExit, FixedUpdate, ScheduleRunnerPlugin, Update};
use bevy_ecs::message::MessageWriter;
use bevy_ecs::prelude::{
    Commands, Component, Entity, IntoScheduleConfigs, Local, Query, Res, ResMut, Resource, With,
    Without,
};
use bevy_time::{Fixed, Time, TimePlugin};
use glam::{IVec2, IVec3, Vec3};
use soils_protocol::{
    CHUNK_BIT, ChunkData, ClientMsg, ChunkVolume, QuantState, ServerMsg, encode_snapshot,
};
use soils_sim::{KIND_CRITTER, KIND_PLAYER, nav};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, error::TryRecvError};
use tokio::sync::watch;

use crate::auth::Accounts;
use crate::persist::PersistHandle;
use crate::world::World;
use crate::{BUNDLE_SIZE, DAY_SECONDS, DEFAULT_WORLD, NewConn, WAVE_SIZE, world_seed};

/// Broadcast actor positions every Nth tick (matches the old 100 ms cadence
/// at 20 Hz).
const ACTOR_EVERY_N_TICKS: u64 = 2;
/// Cap on chunk waves a single client can be served from cache/disk in one
/// tick, bounding per-tick disk I/O. Generation is not capped this way — it
/// runs off-thread and only adoption/serialization lands on the tick.
const CACHED_WAVES_PER_TICK: u32 = 8;
/// Generation waves in flight per client. One wave per tick would serialize a
/// fresh world's 729-chunk burst to ~17 ticks (~850 ms measured); pipelining
/// keeps the rayon pool fed so the burst lands in a few ticks, while delivery
/// stays in request (nearest-first) order.
const GEN_WAVES_INFLIGHT: usize = 8;
/// Default and maximum client view radii (chunks). The client's `ViewRadius`
/// only *sizes* its subscription; the server owns membership.
const DEFAULT_RADIUS: i32 = 4;
const MAX_RADIUS: i32 = 8;
/// Chunks stay subscribed until they leave radius + this margin, so hovering
/// on a chunk border doesn't thrash load/unload.
const SUB_HYSTERESIS: i32 = 1;
/// Zero-subscriber chunks evict (save-if-dirty) after this long.
const UNLOAD_TTL: Duration = Duration::from_secs(60);
/// Dirty (edited) chunks flush to the background writer on this interval;
/// they also flush on eviction and shutdown.
const FLUSH_SECS: f32 = 30.0;
/// Input-frame token bucket: refilled at the client sim rate, capped at this
/// burst. Long-term a client can't get more sim steps than real time allows
/// (flooding inputs would otherwise be a speed hack); the burst absorbs
/// tick-alignment jitter and short stalls.
/// Sized so a brief tick hitch (light floods, saves) can bank half a second
/// of legitimate inputs without dropping any; still refill-bound long term.
const INPUT_BURST: f32 = 32.0;
/// Per-client snapshot byte budget per tick (~8 KB/s at 20 Hz). Entities are
/// packed in priority order; the starved catch up via the accumulator.
const SNAPSHOT_BUDGET: usize = 410;
/// Send-history entries kept per (client, entity) for delta baselines.
const BASELINE_RING: usize = 64;
/// Per-client edit rate cap (edits per second, bucketed like inputs).
const EDIT_RATE: f32 = 32.0;

pub(crate) struct Client {
    inbox: UnboundedReceiver<ClientMsg>,
    outbox: UnboundedSender<ServerMsg>,
    /// Latest-wins snapshot lane (plan §3): replacing an unsent snapshot is
    /// correct — the tracker deltas against acked baselines, so skipped ticks
    /// are just never acked. Everything else stays on the reliable `outbox`.
    snapshot: watch::Sender<Option<ServerMsg>>,
    authenticated: bool,
    world: String,
    /// This client's player entity (spawned on login) and its NetId.
    entity: Option<Entity>,
    self_net: u32,
    /// NetIds currently replicated to this client (its interest set); diffed
    /// each replication pass into EntitySpawn/EntityDespawn.
    known: HashSet<u32>,
    /// Highest snapshot tick the client has acked (piggybacked on `Inputs`).
    ack_tick: u32,
    /// Per-entity send history for delta baselines: (tick, state) pairs, the
    /// newest entry at or below `ack_tick` is safe to delta against (the
    /// transport is ordered, so an ack covers everything sent before it).
    sent: HashMap<u32, VecDeque<(u32, QuantState)>>,
    /// Priority accumulators (grow by base/distance², reset on send).
    priority: HashMap<u32, f32>,
    /// Highest input `seq` applied (frames at or below it are duplicates from
    /// the loss-robustness bundling).
    last_seq: u32,
    /// Remaining input steps allowed (token bucket, see [`INPUT_BURST`]).
    input_tokens: f32,
    /// Remaining edits allowed (token bucket, see [`EDIT_RATE`]).
    edit_tokens: f32,
    /// View radius in chunks (client-requested via `ViewRadius`, clamped).
    radius: i32,
    /// The chunk the subscription was last computed around.
    center: Option<IVec3>,
    /// The chunks this client is subscribed to (holds a ref on each).
    subs: HashSet<IVec3>,
    /// Queued streaming jobs (subscription enters), served FIFO in waves.
    jobs: VecDeque<ChunkJob>,
}

/// Server-allocated replication id; never reused within a session.
#[derive(Component, Clone, Copy, PartialEq, Eq, Hash)]
struct NetId(u32);
/// Entity kind (index into the shared `entities.yaml` registry).
#[derive(Component, Clone, Copy)]
struct Kind(u16);
/// Simulation state (position/velocity/fly/grounded via `soils_sim`).
#[derive(Component)]
struct SimState(soils_sim::PlayerState);
/// Facing yaw in radians.
#[derive(Component)]
struct Yaw(f32);
/// Which named world the entity lives in (one ECS world hosts all of them).
#[derive(Component, Clone)]
struct InWorld(String);
/// Marks a player entity as driven by a connection's input stream.
#[derive(Component)]
#[allow(dead_code)] // client id: read when gameplay systems need "who did it"
struct PlayerControlled(u16);
/// Critter AI (plan §10 stage-2 consumer): wander until a player is in range,
/// then A*-path to the ground under them and follow the waypoints.
#[derive(Component)]
struct Wander {
    /// Next wander heading change (wander mode only).
    next_turn: u64,
    /// Remaining path waypoints (feet cells), front = next.
    path: VecDeque<IVec3>,
    /// Ground cell the current path targets (repath when the player's ground
    /// cell moves off it).
    goal: Option<IVec3>,
}

impl Wander {
    fn new() -> Self {
        Self { next_turn: 0, path: VecDeque::new(), goal: None }
    }
}

#[derive(Resource)]
struct NextNetId(u32);
/// Ambient test critters per world (from `ServerConfig::critters`).
#[derive(Resource)]
struct CritterCount(u16);
/// Worlds that already got their critters.
#[derive(Resource, Default)]
struct CrittersSpawned(HashSet<String>);

struct ChunkJob {
    remaining: VecDeque<IVec3>,
    /// Dispatched waves awaiting delivery, in request order. Waves may finish
    /// generating out of order; they are only *sent* front-first.
    inflight: VecDeque<Wave>,
}

struct Wave {
    /// The full wave in request order (cached + generated).
    positions: Vec<IVec3>,
    /// Delivers the generated missing chunks from the rayon pool; `None` for a
    /// wave that was fully resident (cache/disk) at dispatch.
    rx: Option<UnboundedReceiver<Vec<(IVec3, ChunkVolume)>>>,
}

#[derive(Resource)]
struct NetRx(UnboundedReceiver<NewConn>);
#[derive(Resource)]
struct ShutdownRx(watch::Receiver<bool>);
#[derive(Resource)]
struct AccountsRes(Arc<Accounts>);
/// Shared with the LAN discovery responder on the tokio side.
#[derive(Resource)]
struct PlayerCount(Arc<AtomicU16>);
#[derive(Resource, Default)]
struct TickCount(u64);
/// The global day/night clock (worlds share one clock, as the JS default did).
#[derive(Resource, Default)]
struct Clock {
    daytime: f32,
    /// Accumulates tick dt; every whole second advances daytime + broadcasts.
    acc: f32,
}

#[derive(Resource)]
struct Clients(HashMap<u16, Client>);

#[derive(Resource)]
struct Worlds {
    map: HashMap<String, World>,
    data_dir: PathBuf,
    persist: PersistHandle,
}

impl Worlds {
    /// Fetch a world by name, creating (opening) it on first request.
    fn get_or_create(&mut self, name: &str) -> &mut World {
        if !self.map.contains_key(name) {
            let world = World::new(&self.data_dir, name, world_seed(name), self.persist.clone());
            self.map.insert(name.to_string(), world);
        }
        self.map.get_mut(name).unwrap()
    }
}

/// Build and run the app until the shutdown watch fires. Blocks the calling
/// thread (the dedicated `soils-ecs` thread).
pub(crate) fn run_app(
    conns: UnboundedReceiver<NewConn>,
    shutdown: watch::Receiver<bool>,
    data_dir: PathBuf,
    persist: PersistHandle,
    accounts: Arc<Accounts>,
    player_count: Arc<AtomicU16>,
    critters: u16,
) {
    let mut worlds = Worlds { map: HashMap::new(), data_dir, persist };
    // Pre-create the default world so it's ready before the first client.
    worlds.get_or_create(DEFAULT_WORLD);

    App::new()
        .add_plugins((
            TimePlugin,
            // The loop sleep only bounds tick jitter; FixedUpdate fires at
            // SERVER_TICK_HZ via the Time<Fixed> accumulator.
            ScheduleRunnerPlugin::run_loop(Duration::from_millis(5)),
        ))
        .insert_resource(Time::<Fixed>::from_hz(soils_sim::SERVER_TICK_HZ))
        .insert_resource(NetRx(conns))
        .insert_resource(ShutdownRx(shutdown))
        .insert_resource(AccountsRes(accounts))
        .insert_resource(PlayerCount(player_count))
        .insert_resource(Clients(HashMap::new()))
        .insert_resource(worlds)
        .insert_resource(NextNetId(1))
        .insert_resource(CritterCount(critters))
        .init_resource::<CrittersSpawned>()
        .init_resource::<TickCount>()
        .init_resource::<Clock>()
        .add_systems(Update, check_shutdown)
        .add_systems(
            FixedUpdate,
            (
                accept_connections,
                drain_inboxes,
                wander_critters,
                pump_chunk_jobs,
                replicate_entities,
                tick_clock,
                world_lifecycle,
            )
                .chain(),
        )
        .run();
}

fn check_shutdown(
    shutdown: Res<ShutdownRx>,
    mut worlds: ResMut<Worlds>,
    mut exit: MessageWriter<AppExit>,
    mut flushed: Local<bool>,
) {
    if *shutdown.0.borrow() && !*flushed {
        *flushed = true;
        // Last chance to enqueue unsaved edits; the caller drains the writer
        // after joining this app's thread.
        for w in worlds.map.values_mut() {
            w.flush_dirty();
        }
        exit.write(AppExit::Success);
    }
}

/// Run queued light floods (budgeted), evict expired zero-ref chunks
/// (hourly-scale memory bound), and flush dirty (edited) chunks to the
/// background writer on their intervals.
fn world_lifecycle(time: Res<Time>, mut worlds: ResMut<Worlds>, mut acc: Local<(f32, f32)>) {
    for w in worlds.map.values_mut() {
        w.pump_light();
    }
    acc.0 += time.delta_secs();
    acc.1 += time.delta_secs();
    if acc.0 >= 1.0 {
        acc.0 = 0.0;
        for w in worlds.map.values_mut() {
            w.tick_lifecycle(UNLOAD_TTL);
        }
    }
    if acc.1 >= FLUSH_SECS {
        acc.1 = 0.0;
        for w in worlds.map.values_mut() {
            w.flush_dirty();
        }
    }
}

/// Adopt freshly handshaken connections from the tokio accept loop.
fn accept_connections(mut rx: ResMut<NetRx>, mut clients: ResMut<Clients>) {
    while let Ok(conn) = rx.0.try_recv() {
        clients.0.insert(
            conn.id,
            Client {
                inbox: conn.inbox,
                outbox: conn.outbox,
                snapshot: conn.snapshot,
                authenticated: false,
                world: DEFAULT_WORLD.to_string(),
                entity: None,
                self_net: 0,
                known: HashSet::new(),
                ack_tick: 0,
                sent: HashMap::new(),
                priority: HashMap::new(),
                last_seq: 0,
                input_tokens: INPUT_BURST,
                edit_tokens: EDIT_RATE,
                radius: DEFAULT_RADIUS,
                center: None,
                subs: HashSet::new(),
                jobs: VecDeque::new(),
            },
        );
    }
}

/// Send to every authenticated client in `world` except `except` (so an editor
/// never receives an echo of its own edit).
fn send_world(clients: &Clients, world: &str, except: u16, msg: &ServerMsg) {
    for (&id, c) in &clients.0 {
        if id != except && c.authenticated && c.world == world {
            let _ = c.outbox.send(msg.clone());
        }
    }
}

/// Drain every client's inbox and apply the messages. A closed inbox means the
/// connection task ended; the client is removed and its actor despawned.
#[allow(clippy::too_many_arguments)]
fn drain_inboxes(
    time: Res<Time>,
    mut commands: Commands,
    mut clients: ResMut<Clients>,
    mut worlds: ResMut<Worlds>,
    clock: Res<Clock>,
    accounts: Res<AccountsRes>,
    player_count: Res<PlayerCount>,
    mut next_net: ResMut<NextNetId>,
    critter_count: Res<CritterCount>,
    mut critters_spawned: ResMut<CrittersSpawned>,
    mut sims: Query<(&mut SimState, &mut Yaw, &mut InWorld)>,
) {
    // Phase 1: refill rate buckets and pull everything out of the inboxes.
    let dt = time.delta_secs();
    let mut msgs: Vec<(u16, ClientMsg)> = Vec::new();
    let mut gone: Vec<u16> = Vec::new();
    for (&id, c) in clients.0.iter_mut() {
        c.input_tokens = (c.input_tokens + dt * soils_sim::TICK_HZ as f32).min(INPUT_BURST);
        c.edit_tokens = (c.edit_tokens + dt * EDIT_RATE).min(EDIT_RATE);
        loop {
            match c.inbox.try_recv() {
                Ok(m) => msgs.push((id, m)),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    gone.push(id);
                    break;
                }
            }
        }
    }

    // Phase 2: apply, in per-client arrival order (all the old per-connection
    // loop guaranteed across clients was per-client FIFO too).
    for (id, msg) in msgs {
        match msg {
            ClientMsg::Login { name, password, signup } => {
                let c = clients.0.get_mut(&id).unwrap();
                match accounts.0.authenticate(&name, &password, signup) {
                    Err(reason) => {
                        println!("login denied: {name} (id {id}): {reason}");
                        let _ = c.outbox.send(ServerMsg::LoginError { message: reason });
                    }
                    Ok(()) => {
                        println!("login: {name} (id {id})");
                        if !c.authenticated {
                            player_count.0.fetch_add(1, Ordering::Relaxed);
                        }
                        c.authenticated = true;
                        let world_name = c.world.clone();
                        let world = worlds.get_or_create(&world_name);
                        let (spawn, seed) = (world.spawn, world.seed);
                        let c = clients.0.get_mut(&id).unwrap();
                        // (Re)spawn this connection's player entity.
                        if let Some(old) = c.entity.take() {
                            commands.entity(old).despawn();
                        }
                        let net = next_net.0;
                        next_net.0 += 1;
                        let sim = soils_sim::PlayerState {
                            pos: Vec3::from_array(spawn),
                            ..Default::default()
                        };
                        c.entity = Some(
                            commands
                                .spawn((
                                    NetId(net),
                                    Kind(KIND_PLAYER),
                                    SimState(sim),
                                    Yaw(0.0),
                                    InWorld(world_name.clone()),
                                    PlayerControlled(id),
                                ))
                                .id(),
                        );
                        c.self_net = net;
                        c.last_seq = 0;
                        let _ = c.outbox.send(ServerMsg::Init {
                            id,
                            self_entity: net,
                            spawn,
                            seed,
                            daytime: clock.daytime,
                        });
                        // Server-driven streaming: subscribing around the spawn
                        // point starts the join burst — no client request.
                        c.center = Some(chunk_at(spawn));
                        resubscribe(c, world);
                        // First login into a world seeds its ambient test
                        // critters. They start at spawn height (guaranteed
                        // open air) and fall to the surface once their
                        // terrain is resident — starting lower risks spawning
                        // embedded inside a hillside, immobile.
                        if critter_count.0 > 0 && critters_spawned.0.insert(world_name.clone()) {
                            for i in 0..critter_count.0 {
                                let net = next_net.0;
                                next_net.0 += 1;
                                let offset =
                                    Vec3::new(2.0 + i as f32 * 1.5, 0.0, 2.0 + i as f32);
                                commands.spawn((
                                    NetId(net),
                                    Kind(KIND_CRITTER),
                                    SimState(soils_sim::PlayerState {
                                        pos: Vec3::from_array(spawn) + offset,
                                        flying: false,
                                        ..Default::default()
                                    }),
                                    Yaw(0.0),
                                    InWorld(world_name.clone()),
                                    Wander::new(),
                                ));
                            }
                        }
                    }
                }
            }
            // Everything below requires authentication; silently drop otherwise
            // (same as the old pre-auth gate).
            _ if !clients.0[&id].authenticated => {}
            ClientMsg::ViewRadius { radius } => {
                let c = clients.0.get_mut(&id).unwrap();
                c.radius = (radius as i32).clamp(1, MAX_RADIUS);
                let world = worlds.get_or_create(&c.world.clone());
                resubscribe(c, world);
            }
            ClientMsg::Edit { seq, pos, value } => {
                // Authority (plan §6): rate cap, reach from the *server-side*
                // player position, known block id, chunk resident (within
                // reach it's inside the subscription, so a miss only happens
                // mid-join — rejected, and the client rolls back cleanly).
                let c = clients.0.get_mut(&id).unwrap();
                let world_name = c.world.clone();
                let eye = match c.entity.and_then(|e| sims.get(e).ok()) {
                    Some((sim, ..)) => sim.0.pos,
                    None => continue,
                };
                let rate_ok = c.edit_tokens >= 1.0;
                if rate_ok {
                    c.edit_tokens -= 1.0;
                }
                let target = IVec3::new(pos[0], pos[1], pos[2]);
                let world = worlds.get_or_create(&world_name);
                let applied = rate_ok
                    && soils_sim::validate_edit(eye, target, value, &world.registry)
                    && world.ensure_resident(soils_protocol::chunk_of(target))
                    && world.edit(pos[0], pos[1], pos[2], value);
                let c = &clients.0[&id];
                if applied {
                    let _ = c.outbox.send(ServerMsg::EditAccepted { seq, pos, value });
                    send_world(&clients, &world_name, id, &ServerMsg::Edit { pos, value });
                } else {
                    let _ = c.outbox.send(ServerMsg::EditRejected { seq });
                }
            }
            ClientMsg::Inputs { ack_tick, frames } => {
                // Server authority: the client sends *inputs*, the server
                // integrates them through the shared sim at the client's fixed
                // dt. Positions can't be forged, and the token bucket stops
                // input flooding from becoming a speed hack. Frames at or
                // below `last_seq` are duplicates from loss bundling.
                let c = clients.0.get_mut(&id).unwrap();
                c.ack_tick = c.ack_tick.max(ack_tick);
                let Some(entity) = c.entity else { continue };
                let Ok((mut sim, mut yaw, _)) = sims.get_mut(entity) else { continue };
                let world = worlds.get_or_create(&c.world.clone());
                let sim_dt = 1.0 / soils_sim::TICK_HZ as f32;
                for f in frames {
                    if f.seq <= c.last_seq || c.input_tokens < 1.0 {
                        continue;
                    }
                    c.input_tokens -= 1.0;
                    c.last_seq = f.seq;
                    let input = soils_sim::unpack_input(f.buttons, f.flags, f.yaw);
                    yaw.0 = input.yaw;
                    soils_sim::step_player(&mut sim.0, &input, sim_dt, &|v| world.voxel(v));
                }
                // Crossing a chunk boundary moves the subscription window.
                let pc = chunk_at(sim.0.pos.to_array());
                if c.center != Some(pc) {
                    c.center = Some(pc);
                    resubscribe(c, world);
                }
            }
            ClientMsg::Warp { world: name } => {
                println!("warp: id {id} -> {name}");
                // Leaving the old world: release its chunk refs and drop any
                // jobs still streaming it. Other clients learn via the interest
                // diff (the entity's world changes → EntityDespawn); this
                // client drops everything itself on Warp.
                let c = clients.0.get_mut(&id).unwrap();
                let old = std::mem::replace(&mut c.world, name.clone());
                c.jobs.clear();
                c.known.clear();
                let old_world = worlds.get_or_create(&old);
                for pos in c.subs.drain() {
                    old_world.dec_ref(pos);
                }

                let world = worlds.get_or_create(&name);
                let spawn = world.spawn;
                let c = clients.0.get_mut(&id).unwrap();
                if let Some(entity) = c.entity
                    && let Ok((mut sim, _, mut in_world)) = sims.get_mut(entity)
                {
                    sim.0.pos = Vec3::from_array(spawn);
                    sim.0.vel = Vec3::ZERO;
                    in_world.0 = name.clone();
                }
                let _ = c.outbox.send(ServerMsg::Warp { spawn, daytime: clock.daytime });
                c.center = Some(chunk_at(spawn));
                resubscribe(c, world);
            }
        }
    }

    // Phase 3: disconnects (any final messages above were already applied).
    // Peers learn via the interest diff once the entity despawns.
    for id in gone {
        if let Some(c) = clients.0.remove(&id) {
            if let Some(entity) = c.entity {
                commands.entity(entity).despawn();
            }
            if c.authenticated {
                player_count.0.fetch_sub(1, Ordering::Relaxed);
                let world = worlds.get_or_create(&c.world);
                for pos in c.subs {
                    world.dec_ref(pos);
                }
            }
        }
    }
}

/// The chunk containing a voxel-space position.
fn chunk_at(pos: [f32; 3]) -> IVec3 {
    IVec3::new(
        (pos[0].floor() as i32) >> CHUNK_BIT,
        (pos[1].floor() as i32) >> CHUNK_BIT,
        (pos[2].floor() as i32) >> CHUNK_BIT,
    )
}

/// Recompute a client's subscription around `center`: chunks past
/// radius + [`SUB_HYSTERESIS`] unload (client told, ref released); newly
/// covered chunks queue as a nearest-first streaming job and take a ref.
fn resubscribe(c: &mut Client, world: &mut World) {
    let Some(center) = c.center else { return };
    let keep = c.radius + SUB_HYSTERESIS;
    let leaves: Vec<IVec3> = c
        .subs
        .iter()
        .copied()
        .filter(|p| {
            let d = (*p - center).abs();
            d.x.max(d.y).max(d.z) > keep
        })
        .collect();
    for pos in leaves {
        c.subs.remove(&pos);
        world.dec_ref(pos);
        let _ = c.outbox.send(ServerMsg::ChunkUnload { pos: [pos.x, pos.y, pos.z] });
    }

    let r = c.radius;
    let mut enters: Vec<IVec3> = Vec::new();
    for dx in -r..=r {
        for dy in -r..=r {
            for dz in -r..=r {
                let pos = center + IVec3::new(dx, dy, dz);
                if !c.subs.contains(&pos) {
                    enters.push(pos);
                }
            }
        }
    }
    if enters.is_empty() {
        return;
    }
    // Nearest-first, so the area around the player fills in first.
    enters.sort_by_key(|p| {
        let d = *p - center;
        d.x * d.x + d.y * d.y + d.z * d.z
    });
    for &pos in &enters {
        c.subs.insert(pos);
        world.inc_ref(pos);
    }
    c.jobs.push_back(ChunkJob { remaining: enters.into(), inflight: VecDeque::new() });
}

/// Advance every client's chunk pipeline: dispatch waves (up to
/// [`GEN_WAVES_INFLIGHT`] generating concurrently), then deliver finished
/// waves front-first so chunks always arrive in request order.
fn pump_chunk_jobs(mut clients: ResMut<Clients>, mut worlds: ResMut<Worlds>) {
    for c in clients.0.values_mut() {
        let world = worlds.get_or_create(&c.world.clone());
        let mut budget = CACHED_WAVES_PER_TICK;
        while let Some(job) = c.jobs.front_mut() {
            // Dispatch: probe waves off `remaining`; fully-resident waves are
            // queued as ready, the rest generate on the rayon pool.
            while job.inflight.len() < GEN_WAVES_INFLIGHT
                && !job.remaining.is_empty()
                && budget > 0
            {
                let n = job.remaining.len().min(WAVE_SIZE);
                // Skip positions the client unsubscribed from while queued.
                let wave: Vec<IVec3> =
                    job.remaining.drain(..n).filter(|p| c.subs.contains(p)).collect();
                if wave.is_empty() {
                    continue;
                }
                let missing: Vec<IVec3> =
                    wave.iter().copied().filter(|&p| !world.ensure_resident(p)).collect();
                let rx = if missing.is_empty() {
                    budget -= 1;
                    None
                } else {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    let (terrain, registry) = world.gen_ctx();
                    rayon::spawn(move || {
                        let t0 = Instant::now();
                        let volumes = terrain.generate_batch(&missing, &registry);
                        println!(
                            "worldgen: {} chunks in {} ms",
                            missing.len(),
                            t0.elapsed().as_millis()
                        );
                        let _ = tx.send(missing.into_iter().zip(volumes).collect());
                    });
                    Some(rx)
                };
                job.inflight.push_back(Wave { positions: wave, rx });
            }

            // Deliver: front wave only, so order holds even when a later wave
            // finishes generating first.
            let mut blocked = false;
            while let Some(wave) = job.inflight.front_mut() {
                match &mut wave.rx {
                    None => {}
                    Some(rx) => match rx.try_recv() {
                        Ok(generated) => {
                            for (pos, vol) in generated {
                                world.adopt(pos, vol);
                            }
                        }
                        Err(TryRecvError::Empty) => {
                            blocked = true; // still generating
                            break;
                        }
                        Err(TryRecvError::Disconnected) => {
                            // Generation task died (panic); skip the wave like
                            // the old spawn_blocking-error path did.
                            eprintln!("worldgen task failed; skipping a wave");
                            job.inflight.pop_front();
                            continue;
                        }
                    },
                }
                let wave = job.inflight.pop_front().unwrap();
                // Deliver only what's still subscribed: a client that moved on
                // mid-generation gets neither the chunk nor a leak (the unload
                // was already sent when the subscription shrank, and the
                // per-connection stream is FIFO).
                let deliver: Vec<IVec3> =
                    wave.positions.into_iter().filter(|p| c.subs.contains(p)).collect();
                send_wave(world, &deliver, &c.outbox);
            }
            if blocked {
                break;
            }
            if job.remaining.is_empty() && job.inflight.is_empty() {
                c.jobs.pop_front();
                continue;
            }
            if budget == 0 {
                break;
            }
        }
    }
}

/// Stream one wave's chunks in request order, bundled `BUNDLE_SIZE` at a time.
fn send_wave(world: &World, positions: &[IVec3], out: &UnboundedSender<ServerMsg>) {
    let mut batch: Vec<ChunkData> = Vec::with_capacity(BUNDLE_SIZE);
    for &pos in positions {
        // Every position is resident by now (cached at dispatch or adopted
        // above); a miss would mean a logic bug, not a recoverable state.
        let Some(payload) = world.serve(pos) else { continue };
        batch.push(ChunkData { pos: [pos.x, pos.y, pos.z], payload });
        if batch.len() >= BUNDLE_SIZE {
            let _ = out.send(ServerMsg::Bundle { chunks: std::mem::take(&mut batch) });
        }
    }
    if !batch.is_empty() {
        let _ = out.send(ServerMsg::Bundle { chunks: batch });
    }
}

/// How far critters notice players (voxels).
const SEEK_RANGE: f32 = 48.0;
/// A* expansion budget per (re)path — the per-tick knob from plan §10.2;
/// repaths are also staggered so at most one critter repaths per tick.
const PATH_BUDGET: usize = 400;
/// Minimum ticks between one critter's repaths.
const REPATH_TICKS: u64 = 10;
/// How far below a hovering player to look for the ground cell to path to.
const GROUND_SCAN: i32 = 32;

/// A critter's standing (feet) cell from its eye position.
fn feet_cell(pos: Vec3) -> IVec3 {
    (pos - Vec3::Y * (soils_sim::EYE_TO_FEET - 0.01)).floor().as_ivec3()
}

/// Step the ambient test critters (plan §10 stage 2's first consumer): if a
/// player is within [`SEEK_RANGE`], A*-path to the ground beneath them and
/// walk the waypoints (jumping 1-up steps); otherwise wander deterministically
/// (heading from NetId and tick — no wall-clock or RNG). Waypoints are
/// validated against live voxels before each step, so an edit that breaks the
/// path forces a repath — the stage-2 flavor of version invalidation (cached
/// nav *graphs* get real version keys in stage 3). Frozen while their terrain
/// isn't resident, so they can't fall through unloaded space. Also owns the
/// tick counter increment.
fn wander_critters(
    mut ticks: ResMut<TickCount>,
    mut worlds: ResMut<Worlds>,
    mut critters: Query<
        (&NetId, &InWorld, &mut SimState, &mut Yaw, &mut Wander),
        Without<PlayerControlled>,
    >,
    players: Query<(&InWorld, &SimState), With<PlayerControlled>>,
) {
    ticks.0 += 1;
    let dt = 1.0 / soils_sim::SERVER_TICK_HZ as f32;
    let player_pos: Vec<(&str, Vec3)> =
        players.iter().map(|(w, s)| (w.0.as_str(), s.0.pos)).collect();

    for (net, in_world, mut sim, mut yaw, mut wander) in &mut critters {
        let Some(world) = worlds.map.get_mut(&in_world.0) else { continue };
        if !world.has_chunk(chunk_at(sim.0.pos.to_array())) {
            continue;
        }
        let feet = feet_cell(sim.0.pos);

        // Acquire: nearest in-range player's ground cell (players fly, so
        // drop from their feet to the first walkable cell).
        let target = player_pos
            .iter()
            .filter(|(w, _)| *w == in_world.0)
            .map(|(_, p)| (*p, p.distance(sim.0.pos)))
            .filter(|(_, d)| *d < SEEK_RANGE)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .and_then(|(p, _)| {
                nav::resolve_walkable(&|v: IVec3| world.voxel(v), feet_cell(p), GROUND_SCAN)
            });

        let mut input = soils_sim::PlayerInput {
            move_axes: glam::Vec2::new(0.0, 1.0),
            yaw: yaw.0,
            ..Default::default()
        };
        if let Some(goal) = target {
            // (Re)path when the goal cell moved or the path ran out, at most
            // once per REPATH_TICKS, staggered across critters by NetId.
            let stale = wander.goal != Some(goal) || (wander.path.is_empty() && feet != goal);
            if stale && (ticks.0 + net.0 as u64) % REPATH_TICKS == 0 {
                wander.goal = Some(goal);
                wander.path.clear();
                // The body may hover, mid-fall, or stand on a block edge —
                // snap the start to the nearest standable cell.
                let flat = {
                    let sampler = |v: IVec3| world.voxel(v);
                    nav::resolve_walkable(&sampler, feet, 2)
                        .map(|s| (s, nav::find_path(&sampler, s, goal, PATH_BUDGET)))
                };
                match flat {
                    Some((_, nav::PathResult::Path(p))) => wander.path = p.into(),
                    // Too far for the flat budget: refresh the touched
                    // chunks' cached graphs and go hierarchical (§10.3).
                    Some((s, nav::PathResult::Budget)) => {
                        let (c0, c1) = (
                            chunk_of(s.min(goal)) - IVec3::ONE,
                            chunk_of(s.max(goal)) + IVec3::ONE,
                        );
                        for cy in c0.y..=c1.y {
                            for cz in c0.z..=c1.z {
                                for cx in c0.x..=c1.x {
                                    world.ensure_nav(IVec3::new(cx, cy, cz));
                                }
                            }
                        }
                        let sampler = |v: IVec3| world.voxel(v);
                        let lookup = |c: IVec3| world.nav(c);
                        if let nav::PathResult::Path(p) =
                            nav::hpa_path(&sampler, &lookup, s, goal, PATH_BUDGET)
                        {
                            wander.path = p.into();
                        }
                    }
                    _ => {}
                }
            }
            // Reached (or fell past) waypoints are popped; a waypoint no
            // longer walkable (edit underneath it) drops the whole path.
            while wander.path.front() == Some(&feet) {
                wander.path.pop_front();
            }
            if let Some(&next) = wander.path.front() {
                if !nav::walkable(&|v: IVec3| world.voxel(v), next) {
                    wander.path.clear();
                } else {
                    let center = next.as_vec3() + Vec3::new(0.5, 0.0, 0.5);
                    let d = center - Vec3::new(sim.0.pos.x, center.y, sim.0.pos.z);
                    yaw.0 = f32::atan2(-d.x, -d.z);
                    input.yaw = yaw.0;
                    input.jump = next.y > feet.y && sim.0.grounded;
                }
            } else {
                // At (or under) the goal: face the player and hold position.
                input.move_axes = glam::Vec2::ZERO;
            }
        } else {
            // No player near: the old deterministic wander.
            wander.goal = None;
            wander.path.clear();
            if ticks.0 >= wander.next_turn {
                let h = (net.0 as u64)
                    .wrapping_mul(0x9E3779B97F4A7C15)
                    .wrapping_add(ticks.0.wrapping_mul(0xD1B54A32D192ED03));
                yaw.0 = (h % 6283) as f32 / 1000.0;
                wander.next_turn = ticks.0 + 30 + (h >> 32) % 40;
            }
            input.yaw = yaw.0;
        }
        soils_sim::step_player(&mut sim.0, &input, dt, &|v: IVec3| world.voxel(v));
    }
}

/// Chunk coordinate containing a voxel.
fn chunk_of(v: IVec3) -> IVec3 {
    IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT)
}

/// Replicate entities (plan §4/§7): per client, interest = entities in the
/// same world within the subscription radius by chunk column (buckets rebuilt
/// per pass — trivial at current counts). The interest set diffs into
/// EntitySpawn/EntityDespawn a few times a second; *state* goes out every
/// tick as a delta snapshot: entities packed in priority order (base/dist²,
/// accumulator reset on send) under a per-tick byte budget, encoded against
/// the newest baseline the client has acked.
fn replicate_entities(
    ticks: Res<TickCount>,
    mut clients: ResMut<Clients>,
    entities: Query<(&NetId, &Kind, &InWorld, &SimState, &Yaw)>,
) {
    struct Snap {
        net: u32,
        kind: u16,
        pos: [f32; 3],
        quant: QuantState,
    }
    let mut by_col: HashMap<(&str, IVec2), Vec<Snap>> = HashMap::new();
    for (net, kind, in_world, sim, yaw) in &entities {
        let col = IVec2::new(
            (sim.0.pos.x.floor() as i32) >> CHUNK_BIT,
            (sim.0.pos.z.floor() as i32) >> CHUNK_BIT,
        );
        let yaw_q = soils_sim::pack_yaw(yaw.0);
        by_col.entry((in_world.0.as_str(), col)).or_default().push(Snap {
            net: net.0,
            kind: kind.0,
            pos: sim.0.pos.to_array(),
            quant: QuantState::quantize(sim.0.pos.to_array(), sim.0.vel.to_array(), yaw_q),
        });
    }
    let tick = ticks.0 as u32;
    let diff_pass = ticks.0.is_multiple_of(ACTOR_EVERY_N_TICKS);

    for c in clients.0.values_mut() {
        if !c.authenticated {
            continue;
        }
        let Some(center) = c.center else { continue };
        let ccol = IVec2::new(center.x, center.z);
        let r = c.radius;
        let mut interest: Vec<&Snap> = Vec::new();
        for dx in -r..=r {
            for dz in -r..=r {
                if let Some(bucket) = by_col.get(&(c.world.as_str(), ccol + IVec2::new(dx, dz))) {
                    interest.extend(bucket.iter());
                }
            }
        }

        if diff_pass {
            let current: HashSet<u32> = interest.iter().map(|s| s.net).collect();
            for snap in &interest {
                if !c.known.contains(&snap.net) {
                    let _ = c.outbox.send(ServerMsg::EntitySpawn {
                        id: snap.net,
                        kind: snap.kind,
                        pos: snap.pos,
                    });
                }
            }
            for &gone in c.known.difference(&current) {
                let _ = c.outbox.send(ServerMsg::EntityDespawn { id: gone });
                c.sent.remove(&gone);
                c.priority.remove(&gone);
            }
            c.known = current;
        }

        // Accumulate priorities for everything in interest (players over
        // critters, near over far; own entity is distance ~0 → always first).
        let center_pos = (center * 32 + IVec3::splat(16)).as_vec3();
        for snap in &interest {
            if !c.known.contains(&snap.net) {
                continue; // spawns not announced yet (between diff passes)
            }
            let base = if snap.kind == KIND_PLAYER { 2.0 } else { 1.0 };
            let d2 = (Vec3::from_array(snap.pos) - center_pos).length_squared().max(1.0);
            *c.priority.entry(snap.net).or_insert(0.0) += base * 1024.0 / d2;
        }

        // Fill the packet in priority order until the byte budget. Costs are
        // conservative estimates (FULL ≈ 20 B, delta ≈ 12 B); the encoder's
        // real output is what ships.
        let mut candidates: Vec<&Snap> =
            interest.iter().copied().filter(|s| c.known.contains(&s.net)).collect();
        candidates.sort_by(|a, b| {
            let pa = c.priority.get(&a.net).copied().unwrap_or(0.0);
            let pb = c.priority.get(&b.net).copied().unwrap_or(0.0);
            pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal).then(a.net.cmp(&b.net))
        });
        let ack = c.ack_tick;
        let mut picked: Vec<(u32, QuantState)> = Vec::new();
        let mut cost = 0usize;
        for snap in candidates {
            let has_baseline = c
                .sent
                .get(&snap.net)
                .is_some_and(|ring| ring.iter().any(|(t, _)| *t <= ack));
            cost += if has_baseline { 12 } else { 20 };
            if cost > SNAPSHOT_BUDGET && !picked.is_empty() {
                break;
            }
            picked.push((snap.net, snap.quant));
        }
        if picked.is_empty() {
            continue;
        }
        picked.sort_by_key(|(id, _)| *id);

        let sent = &mut c.sent;
        let payload = encode_snapshot(&picked, |id| {
            sent.get(&id)?.iter().rev().find(|(t, _)| *t <= ack).map(|(_, s)| *s)
        });
        for (id, state) in &picked {
            let ring = sent.entry(*id).or_default();
            ring.push_back((tick, *state));
            if ring.len() > BASELINE_RING {
                ring.pop_front();
            }
            // Baselines older than the newest acked one can never be used.
            while ring.len() > 1 && ring[1].0 <= ack {
                ring.pop_front();
            }
            c.priority.insert(*id, 0.0);
        }
        // Latest-wins: if the socket (or a future datagram lane) hasn't sent
        // the previous snapshot yet, it is replaced, never queued.
        let _ = c.snapshot.send_replace(Some(ServerMsg::Snapshot {
            tick,
            baseline_tick: ack,
            last_input_seq: c.last_seq,
            payload,
        }));
    }
}

/// Advance and broadcast the day/night clock once per second (global, all
/// worlds — and, like the old broadcast forwarder, all connections).
fn tick_clock(time: Res<Time>, mut clock: ResMut<Clock>, clients: Res<Clients>) {
    clock.acc += time.delta_secs();
    if clock.acc < 1.0 {
        return;
    }
    clock.acc -= 1.0;
    clock.daytime = (clock.daytime + 1.0 / DAY_SECONDS) % 1.0;
    for c in clients.0.values() {
        let _ = c.outbox.send(ServerMsg::Time { daytime: clock.daytime });
    }
}
