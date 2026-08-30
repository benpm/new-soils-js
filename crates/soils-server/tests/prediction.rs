//! Prediction & reconciliation validation on a degraded link (plan-game-
//! systems §9): a headless predictor — the same rewind/replay logic the real
//! client runs — talks to the server through a 75 ms-each-way proxy (~150 ms
//! RTT) while dropping 2% of its input sends (the last-3 frame bundling must
//! recover them).
//!
//! (a) Straight-line flight: predicted state at each acked input seq matches
//!     the authoritative echo within epsilon — prediction is exact when
//!     nothing interferes.
//! (b) Forced misprediction: a second, undelayed client walls off the flight
//!     path just before the predictor reaches it. The predictor's local world
//!     is 75 ms stale, so it flies through the wall locally, then must
//!     reconcile back behind it once the authoritative echo lands.

mod common;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

use common::{Client, TestServer};
use glam::{IVec3, Vec3};
use soils_protocol::{
    ChunkVolume, ClientMsg, InputFrame, ServerMsg, chunk_of, decode_chunk, local_of,
};
use soils_sim::{PlayerInput, PlayerState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One-way link delay; RTT ≈ 2×.
const LINK_DELAY: Duration = Duration::from_millis(75);
const EPSILON: f32 = 0.05;

/// TCP proxy that forwards bytes in order after [`LINK_DELAY`].
async fn delay_proxy(upstream: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("proxy bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let server = TcpStream::connect(upstream).await.expect("proxy dial");
            // Without NODELAY, Nagle on the extra hops coalesces the 20 Hz
            // snapshot stream into ~5 Hz clumps and bursts the input stream
            // into the server's rate bucket — both wreck the measurement.
            client.set_nodelay(true).ok();
            server.set_nodelay(true).ok();
            let (cr, cw) = client.into_split();
            let (sr, sw) = server.into_split();
            tokio::spawn(pump_delayed(cr, sw));
            tokio::spawn(pump_delayed(sr, cw));
        }
    });
    addr
}

async fn pump_delayed(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
) {
    // Order-preserving delay: stamp each chunk on read, sleep out the
    // remainder before writing.
    let mut buf = vec![0u8; 16 * 1024];
    let mut queue: VecDeque<(tokio::time::Instant, Vec<u8>)> = VecDeque::new();
    loop {
        // Flush everything whose deadline passed, then read more.
        while queue.front().is_some_and(|(at, _)| *at <= tokio::time::Instant::now()) {
            let (_, bytes) = queue.pop_front().unwrap();
            if to.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let wait = queue
            .front()
            .map(|(at, _)| at.saturating_duration_since(tokio::time::Instant::now()))
            .unwrap_or(Duration::from_secs(3600));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            n = from.read(&mut buf) => match n {
                Ok(0) | Err(_) => {
                    // Drain the tail, then close.
                    while let Some((at, bytes)) = queue.pop_front() {
                        tokio::time::sleep_until(at).await;
                        if to.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
                Ok(n) => {
                    queue.push_back((tokio::time::Instant::now() + LINK_DELAY, buf[..n].to_vec()));
                }
            },
        }
    }
}

/// The client-side prediction loop, headless: local chunk mirror, per-tick
/// step + history, snapshot reconciliation — the same algorithm as
/// `soils-client/src/player.rs`.
struct Predictor {
    sim: PlayerState,
    seq: u32,
    frames: Vec<InputFrame>,
    history: VecDeque<(u32, PlayerInput, PlayerState)>,
    chunks: HashMap<IVec3, ChunkVolume>,
    /// Pristine manifest positions not yet materialized. Generated lazily in
    /// [`tick`] for chunks near the player only — generating whole waves
    /// inside the drain loop stalls the 64 Hz ticker and bursts inputs into
    /// the server's token bucket, which reads as fake divergence.
    pending: std::collections::HashSet<IVec3>,
    /// Local generator mirror for pristine manifest entries (set after join
    /// from the Init GenParams).
    generator: Option<(soils_worldgen::TerrainGen, soils_worldgen::BlockRegistry)>,
    /// Scenario (b): pretend edit broadcasts haven't arrived, so the local
    /// world stays stale and the prediction runs into server-only terrain.
    ignore_edits: bool,
    /// Largest divergence observed at reconcile time, and the count of
    /// reconciles that had a matching history entry.
    max_divergence: f32,
    reconciles: u32,
    /// Where the server last said we were. The walk phase below drives until
    /// *this* has travelled far enough, rather than for a fixed tick count —
    /// see `forced_misprediction_reconciles_behind_the_wall`.
    server_pos: Option<Vec3>,
    /// Diagnostics: snapshots seen / snapshots containing self / mismatched.
    snapshots: u32,
    self_seen: u32,
    unmatched: u32,
}

impl Predictor {
    fn set_gen(&mut self, p: soils_protocol::GenParams) {
        let wt = match p.world_type {
            1 => soils_worldgen::WorldType::Flat,
            _ => soils_worldgen::WorldType::Normal,
        };
        let terrain = soils_worldgen::TerrainGen::new(p.seed as u32, wt);
        self.generator = Some((terrain, soils_worldgen::default_registry()));
    }

    fn new(spawn: [f32; 3]) -> Self {
        Self {
            sim: PlayerState { pos: Vec3::from_array(spawn), ..Default::default() },
            seq: 0,
            frames: Vec::new(),
            history: VecDeque::new(),
            chunks: HashMap::new(),
            pending: std::collections::HashSet::new(),
            generator: None,
            ignore_edits: false,
            max_divergence: 0.0,
            reconciles: 0,
            server_pos: None,
            snapshots: 0,
            self_seen: 0,
            unmatched: 0,
        }
    }

    fn voxel(&self, v: IVec3) -> u8 {
        match self.chunks.get(&chunk_of(v)) {
            Some(c) => {
                let l = local_of(v);
                c.get(l.x, l.y, l.z)
            }
            None => 0,
        }
    }

    /// One 64 Hz tick: predict locally, queue the frame bundle. Returns the
    /// `Inputs` message to send (the caller may drop it to simulate loss).
    fn tick(&mut self, input: PlayerInput, ack_tick: u32) -> ClientMsg {
        // Materialize pending pristine chunks the step could touch (player's
        // chunk ± 1); everything else stays pending (unloaded-reads-air).
        if let Some((terrain, registry)) = &self.generator {
            let pc = chunk_of(self.sim.pos.floor().as_ivec3());
            let near: Vec<IVec3> = self
                .pending
                .iter()
                .copied()
                .filter(|p| (*p - pc).abs().max_element() <= 1)
                .collect();
            for p in near {
                self.pending.remove(&p);
                self.chunks.insert(p, terrain.generate(p, registry));
            }
        }
        let chunks = &self.chunks;
        let sampler = |v: IVec3| match chunks.get(&chunk_of(v)) {
            Some(c) => {
                let l = local_of(v);
                c.get(l.x, l.y, l.z)
            }
            None => 0,
        };
        soils_sim::step_player(&mut self.sim, &input, 1.0 / soils_sim::TICK_HZ as f32, &sampler);
        self.seq += 1;
        self.history.push_back((self.seq, input, self.sim));
        if self.history.len() > 256 {
            self.history.pop_front();
        }
        let (buttons, flags, yaw) = soils_sim::pack_input(&input);
        self.frames.push(InputFrame { seq: self.seq, buttons, flags, yaw });
        if self.frames.len() > 3 {
            self.frames.remove(0);
        }
        ClientMsg::Inputs { ack_tick, frames: self.frames.clone() }
    }

    fn handle(&mut self, msg: &ServerMsg, self_net: u32, tracker_states: &[(u32, [f32; 3], [f32; 3])]) {
        match msg {
            ServerMsg::Manifest { chunks } => {
                for info in chunks {
                    let pos = IVec3::from_array(info.pos());
                    match info {
                        soils_protocol::ChunkInfo::Edited { payload, .. } => {
                            self.pending.remove(&pos);
                            if let Some(vol) = decode_chunk(payload) {
                                self.chunks.insert(pos, vol);
                            }
                        }
                        soils_protocol::ChunkInfo::Pristine { .. } => {
                            self.pending.insert(pos);
                        }
                    }
                }
            }
            ServerMsg::ChunkUnload { pos } => {
                let pos = IVec3::from_array(*pos);
                self.pending.remove(&pos);
                self.chunks.remove(&pos);
            }
            ServerMsg::Edit { pos, value } | ServerMsg::EditAccepted { pos, value, .. } => {
                if self.ignore_edits {
                    return;
                }
                let c = chunk_of(IVec3::from_array(*pos));
                if let Some(vol) = self.chunks.get_mut(&c) {
                    let l = local_of(IVec3::from_array(*pos));
                    vol.set(l.x, l.y, l.z, *value);
                }
            }
            ServerMsg::Snapshot { last_input_seq, .. } => {
                self.snapshots += 1;
                let Some(&(_, pos, vel)) =
                    tracker_states.iter().find(|(id, ..)| *id == self_net)
                else {
                    return;
                };
                self.self_seen += 1;
                self.reconcile(*last_input_seq, pos, vel);
            }
            _ => {}
        }
    }

    fn reconcile(&mut self, seq: u32, server_pos: [f32; 3], server_vel: [f32; 3]) {
        let server_pos = Vec3::from_array(server_pos);
        while self.history.front().is_some_and(|(s, ..)| *s < seq) {
            self.history.pop_front();
        }
        let recorded = match self.history.front() {
            Some((s, _, st)) if *s == seq => *st,
            _ => {
                self.unmatched += 1;
                return;
            }
        };
        self.reconciles += 1;
        let divergence = (recorded.pos - server_pos).length();
        if divergence > EPSILON && self.reconciles < 12 {
            eprintln!(
                "diverge seq {seq}: predicted z {} server z {} (cur seq {})",
                recorded.pos.z, server_pos.z, self.seq
            );
        }
        self.max_divergence = self.max_divergence.max(divergence);
        self.server_pos = Some(server_pos);
        if divergence <= EPSILON {
            return;
        }
        // Rewind + replay, exactly like the client. The anchor entry (at
        // `seq`) rebases to the authoritative state so a repeated echo of the
        // same seq doesn't re-trigger the rewind.
        let base = PlayerState {
            pos: server_pos,
            vel: Vec3::from_array(server_vel),
            flying: recorded.flying,
            grounded: recorded.grounded,
        };
        let mut sim = base;
        let chunks = &self.chunks;
        let sampler = |v: IVec3| match chunks.get(&chunk_of(v)) {
            Some(c) => {
                let l = local_of(v);
                c.get(l.x, l.y, l.z)
            }
            None => 0,
        };
        if let Some(front) = self.history.front_mut() {
            front.2 = base;
        }
        let mut replayed: Vec<(u32, PlayerInput, PlayerState)> = Vec::new();
        for (s, input, _) in self.history.iter().skip(1) {
            soils_sim::step_player(&mut sim, input, 1.0 / soils_sim::TICK_HZ as f32, &sampler);
            replayed.push((*s, *input, sim));
        }
        for (slot, new) in self.history.iter_mut().skip(1).zip(replayed) {
            *slot = new;
        }
        self.sim = sim;
    }
}

/// Drain everything currently buffered on the socket without blocking.
async fn drain(client: &mut Client, pred: &mut Predictor, self_net: u32) {
    loop {
        let msg =
            match tokio::time::timeout(Duration::from_millis(1), client.next_msg()).await {
                Ok(m) => m,
                Err(_) => return,
            };
        // Feed the shared snapshot tracker first so reconciliation sees the
        // decoded self state for this exact snapshot.
        if let ServerMsg::Snapshot { tick, baseline_tick, payload, .. } = &msg {
            let states: Vec<(u32, [f32; 3], [f32; 3])> = client
                .tracker
                .apply(*tick, *baseline_tick, payload)
                .unwrap_or_default()
                .into_iter()
                .map(|s| (s.id, s.pos, s.velocity))
                .collect();
            pred.handle(&msg, self_net, &states);
        } else {
            pred.handle(&msg, self_net, &[]);
        }
    }
}

fn fly_input(yaw: f32, sprint: bool) -> PlayerInput {
    PlayerInput {
        move_axes: glam::Vec2::new(0.0, 1.0),
        yaw,
        sprint,
        ..Default::default()
    }
}

#[tokio::test]
async fn prediction_holds_on_a_delayed_lossy_link() {
    let server = TestServer::start("predict-a");
    let direct = std::env::var("PRED_DIRECT").is_ok(); // diagnostic bypass
    let proxy = if direct { server.addr() } else { delay_proxy(server.addr()).await };
    let mut a = Client::join(proxy, "alice").await;
    let (self_net, spawn) = (a.self_entity, a.spawn);
    let mut pred = Predictor::new(spawn);
    pred.set_gen(a.worldgen.expect("init captured"));

    // Straight-line flight north for ~2.5 s at 64 Hz, dropping every 50th
    // input send (2%); the bundled last-3 frames recover the gaps.
    let mut ticker = tokio::time::interval(Duration::from_micros(15_625));
    let t0 = std::time::Instant::now();
    for i in 0u32..160 {
        ticker.tick().await;
        let msg = pred.tick(fly_input(0.0, false), a.tracker.latest_tick);
        if i % 50 != 49 {
            a.send(&msg).await;
        }
        drain(&mut a, &mut pred, self_net).await;
    }
    eprintln!("(a) loop took {:?}", t0.elapsed());
    // Let the tail of echoes arrive through the delayed link.
    tokio::time::sleep(Duration::from_millis(400)).await;
    drain(&mut a, &mut pred, self_net).await;

    eprintln!(
        "(a) snapshots {} self {} reconciles {} unmatched {} maxdiv {} pos {:?}",
        pred.snapshots, pred.self_seen, pred.reconciles, pred.unmatched, pred.max_divergence,
        pred.sim.pos
    );
    assert!(pred.reconciles > 10, "expected acked echoes to reconcile against");
    assert!(
        pred.max_divergence <= EPSILON,
        "straight-line prediction diverged {} units (> {EPSILON})",
        pred.max_divergence
    );
}

#[tokio::test]
async fn forced_misprediction_reconciles_behind_the_wall() {
    let server = TestServer::start("predict-b");
    let proxy = delay_proxy(server.addr()).await;
    let mut a = Client::join(proxy, "alice").await;
    let (self_net, spawn) = (a.self_entity, a.spawn);
    let mut pred = Predictor::new(spawn);
    pred.set_gen(a.worldgen.expect("init captured"));

    let mut ticker = tokio::time::interval(Duration::from_micros(15_625));

    // Phase 1: drop out of fly mode and fall ~29 voxels onto the terrain.
    // Gravity is deterministic and both sides see the same real chunks, so
    // the prediction must track the whole fall exactly.
    for i in 0u32..140 {
        ticker.tick().await;
        let input = PlayerInput { toggle_fly: i == 0, ..Default::default() };
        let msg = pred.tick(input, a.tracker.latest_tick);
        a.send(&msg).await;
        drain(&mut a, &mut pred, self_net).await;
    }
    assert!(pred.sim.grounded, "should have landed on terrain (at {:?})", pred.sim.pos);
    assert!(
        pred.max_divergence <= EPSILON,
        "the fall itself mispredicted ({} units)",
        pred.max_divergence
    );

    // A builds a wall just north (both sides see it), goes stale, then carves
    // a walking tunnel back through it (all within reach). Building the
    // obstacle keeps the scenario independent of what the terrain generator
    // put here. The server applies the carve; the stale predictor still sees
    // the wall — so the *server* walks on while the prediction stays stuck.
    // (Fly mode is noclip by design, so this scenario must *walk* into it.)
    let eye = pred.sim.pos;
    let (feet_y, x0) = ((eye.y - 1.6).floor() as i32, eye.x.floor() as i32);
    let mut edits = 0u32;
    // Cobblestone: the wall must be a block the player is actually stocked
    // with, or every placement is refused and the obstacle never exists.
    for value in [common::HELD_BLOCK, 0] {
        for dz in 1..=3i32 {
            for dx in -1..=1i32 {
                for dy in 0..3i32 {
                    a.edit([x0 + dx, feet_y + dy, eye.z.floor() as i32 - dz], value).await;
                    edits += 1;
                    if edits % 24 == 0 {
                        // Respect the server's edit rate bucket.
                        tokio::time::sleep(Duration::from_millis(800)).await;
                    }
                }
            }
        }
        drain(&mut a, &mut pred, self_net).await;
        // The build pass, whatever block it used. Comparing against a literal
        // id silently stops matching the moment the block changes, and the
        // predictor then never goes stale — the test still runs and proves
        // nothing.
        if value != 0 {
            // Let the wall reach the predictor's map, then stop applying
            // edits — the deterministic form of "the world changed
            // server-side inside my staleness window".
            tokio::time::sleep(Duration::from_millis(600)).await;
            drain(&mut a, &mut pred, self_net).await;
            pred.ignore_edits = true;
        }
    }

    // Phase 2: walk north. The server strolls down the carved tunnel; the
    // local sim bumps into phantom rock; reconciliation must drag us forward.
    //
    // Driven by how far the *server* has actually walked, not by a fixed tick
    // count. The ticker is wall-clock, so under full-suite contention ticks are
    // missed, the server walks less far, and the divergence this test exists to
    // observe shrinks with it — which is how a fixed 150 ticks landed on
    // exactly the 0.5 threshold and failed. The bar is not lowered; the
    // precondition for reaching it is now established rather than assumed.
    const WALK_TARGET: f32 = 2.0;
    const WALK_TICK_CAP: u32 = 900;
    let start_z = eye.z;
    let mut walked = 0u32;
    while walked < WALK_TICK_CAP {
        ticker.tick().await;
        let msg = pred.tick(fly_input(0.0, false), a.tracker.latest_tick);
        a.send(&msg).await;
        drain(&mut a, &mut pred, self_net).await;
        walked += 1;
        if pred.server_pos.is_some_and(|p| start_z - p.z >= WALK_TARGET) {
            break;
        }
    }
    let server_walk = pred.server_pos.map(|p| start_z - p.z).unwrap_or(0.0);
    assert!(
        server_walk >= WALK_TARGET,
        "the server only walked {server_walk} of {WALK_TARGET} voxels in {walked} ticks —          the scenario never set up the misprediction it is about to assert on"
    );

    // Let everything settle through the delayed link (no further inputs, so
    // the pending-replay window shrinks to nothing).
    tokio::time::sleep(Duration::from_millis(600)).await;
    drain(&mut a, &mut pred, self_net).await;

    eprintln!(
        "(b) snapshots {} self {} reconciles {} unmatched {} maxdiv {} pos {:?}",
        pred.snapshots, pred.self_seen, pred.reconciles, pred.unmatched, pred.max_divergence,
        pred.sim.pos
    );
    assert!(
        pred.max_divergence > 0.5,
        "expected a misprediction against the unseen carve, max divergence {}",
        pred.max_divergence
    );
    // Reconciliation dragged the predictor forward into the tunnel the local
    // map still thinks is rock.
    assert!(
        pred.sim.pos.z < eye.z - 2.0,
        "predictor should settle inside the carved tunnel, ended at z {} (start {})",
        pred.sim.pos.z,
        eye.z
    );
    // And the local state agrees with the server's final echo.
    let server_pos = a.await_self_pos().await;
    assert!(
        (pred.sim.pos - Vec3::from_array(server_pos)).length() < 0.5,
        "predictor ({:?}) and server ({server_pos:?}) failed to converge",
        pred.sim.pos
    );
}
