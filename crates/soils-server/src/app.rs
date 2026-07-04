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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use bevy_app::{App, AppExit, FixedUpdate, ScheduleRunnerPlugin, Update};
use bevy_ecs::message::MessageWriter;
use bevy_ecs::prelude::{IntoScheduleConfigs, Res, ResMut, Resource};
use bevy_time::{Fixed, Time, TimePlugin};
use glam::IVec3;
use soils_protocol::{ActorState, ChunkData, ClientMsg, ChunkVolume, ServerMsg};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, error::TryRecvError};
use tokio::sync::watch;

use crate::auth::Accounts;
use crate::persist::PersistHandle;
use crate::world::World;
use crate::{BUNDLE_SIZE, DAY_SECONDS, DEFAULT_WORLD, MAX_STEP, NewConn, WAVE_SIZE, world_seed};

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

pub(crate) struct Client {
    inbox: UnboundedReceiver<ClientMsg>,
    outbox: UnboundedSender<ServerMsg>,
    authenticated: bool,
    world: String,
    state: ActorState,
    /// Queued chunk requests, served FIFO one wave at a time.
    jobs: VecDeque<ChunkJob>,
}

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
        .init_resource::<TickCount>()
        .init_resource::<Clock>()
        .add_systems(Update, check_shutdown)
        .add_systems(
            FixedUpdate,
            (accept_connections, drain_inboxes, pump_chunk_jobs, broadcast_actors, tick_clock)
                .chain(),
        )
        .run();
}

fn check_shutdown(shutdown: Res<ShutdownRx>, mut exit: MessageWriter<AppExit>) {
    if *shutdown.0.borrow() {
        exit.write(AppExit::Success);
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
                authenticated: false,
                world: DEFAULT_WORLD.to_string(),
                state: ActorState { id: conn.id, pos: [0.0; 3], velocity: [0.0; 3] },
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
fn drain_inboxes(
    mut clients: ResMut<Clients>,
    mut worlds: ResMut<Worlds>,
    clock: Res<Clock>,
    accounts: Res<AccountsRes>,
    player_count: Res<PlayerCount>,
) {
    // Phase 1: pull everything out of the inboxes (needs &mut per client).
    let mut msgs: Vec<(u16, ClientMsg)> = Vec::new();
    let mut gone: Vec<u16> = Vec::new();
    for (&id, c) in clients.0.iter_mut() {
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
                        let world = worlds.get_or_create(&c.world.clone());
                        let (spawn, seed) = (world.spawn, world.seed);
                        let c = clients.0.get_mut(&id).unwrap();
                        c.state = ActorState { id, pos: spawn, velocity: [0.0; 3] };
                        let _ = c.outbox.send(ServerMsg::Init {
                            id,
                            spawn,
                            seed,
                            daytime: clock.daytime,
                        });
                    }
                }
            }
            // Everything below requires authentication; silently drop otherwise
            // (same as the old pre-auth gate).
            _ if !clients.0[&id].authenticated => {}
            ClientMsg::ReqChunks { positions } => {
                let remaining =
                    positions.iter().map(|p| IVec3::new(p[0], p[1], p[2])).collect::<VecDeque<_>>();
                clients.0.get_mut(&id).unwrap().jobs.push_back(ChunkJob {
                    remaining,
                    inflight: VecDeque::new(),
                });
            }
            ClientMsg::Edit { pos, value } => {
                let world_name = clients.0[&id].world.clone();
                let applied =
                    worlds.get_or_create(&world_name).edit(pos[0], pos[1], pos[2], value);
                if applied {
                    send_world(&clients, &world_name, id, &ServerMsg::Edit { pos, value });
                }
            }
            ClientMsg::Move { pos, velocity } => {
                // Server authority: reject implausible jumps (teleport/speed
                // hacks) and snap the client back to its last accepted position.
                let c = clients.0.get_mut(&id).unwrap();
                let last = c.state.pos;
                let d2 = (pos[0] - last[0]).powi(2)
                    + (pos[1] - last[1]).powi(2)
                    + (pos[2] - last[2]).powi(2);
                if d2 > MAX_STEP * MAX_STEP {
                    let _ = c.outbox.send(ServerMsg::Position { pos: last });
                } else {
                    c.state.pos = pos;
                    c.state.velocity = velocity;
                }
            }
            ClientMsg::Warp { world: name } => {
                println!("warp: id {id} -> {name}");
                // Leaving the old world: tell its clients the actor is gone,
                // and drop any chunk jobs still streaming the old world.
                let c = clients.0.get_mut(&id).unwrap();
                let old = std::mem::replace(&mut c.world, name.clone());
                c.jobs.clear();
                send_world(&clients, &old, id, &ServerMsg::ActorRemove { id });

                let spawn = worlds.get_or_create(&name).spawn;
                let c = clients.0.get_mut(&id).unwrap();
                c.state.pos = spawn;
                let _ = c.outbox.send(ServerMsg::Warp { spawn, daytime: clock.daytime });
            }
        }
    }

    // Phase 3: disconnects (any final messages above were already applied).
    for id in gone {
        if let Some(c) = clients.0.remove(&id)
            && c.authenticated
        {
            player_count.0.fetch_sub(1, Ordering::Relaxed);
            send_world(&clients, &c.world, id, &ServerMsg::ActorRemove { id });
        }
    }
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
                let wave: Vec<IVec3> = job.remaining.drain(..n).collect();
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
                send_wave(world, &wave.positions, &c.outbox);
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
        let Some((empty, voxels)) = world.serve(pos) else { continue };
        batch.push(ChunkData { pos: [pos.x, pos.y, pos.z], empty, voxels });
        if batch.len() >= BUNDLE_SIZE {
            let _ = out.send(ServerMsg::Bundle { chunks: std::mem::take(&mut batch) });
        }
    }
    if !batch.is_empty() {
        let _ = out.send(ServerMsg::Bundle { chunks: batch });
    }
}

/// Broadcast actor positions a few times a second, grouped by world so players
/// only see others in the same world (the list includes the receiver; the
/// client filters itself by id).
fn broadcast_actors(mut ticks: ResMut<TickCount>, clients: Res<Clients>) {
    ticks.0 += 1;
    if !ticks.0.is_multiple_of(ACTOR_EVERY_N_TICKS) {
        return;
    }
    let mut by_world: HashMap<&str, Vec<ActorState>> = HashMap::new();
    for c in clients.0.values() {
        if c.authenticated {
            by_world.entry(c.world.as_str()).or_default().push(c.state.clone());
        }
    }
    for c in clients.0.values() {
        if !c.authenticated {
            continue;
        }
        if let Some(actors) = by_world.get(c.world.as_str()) {
            let _ = c.outbox.send(ServerMsg::ActorUpdate { actors: actors.clone() });
        }
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
