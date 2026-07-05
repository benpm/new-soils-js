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
    /// Scenario (b): pretend edit broadcasts haven't arrived, so the local
    /// world stays stale and the prediction runs into server-only terrain.
    ignore_edits: bool,
    /// Largest divergence observed at reconcile time, and the count of
    /// reconciles that had a matching history entry.
    max_divergence: f32,
    reconciles: u32,
    /// Diagnostics: snapshots seen / snapshots containing self / mismatched.
    snapshots: u32,
    self_seen: u32,
    unmatched: u32,
}

impl Predictor {
    fn new(spawn: [f32; 3]) -> Self {
        Self {
            sim: PlayerState { pos: Vec3::from_array(spawn), ..Default::default() },
            seq: 0,
            frames: Vec::new(),
            history: VecDeque::new(),
            chunks: HashMap::new(),
            ignore_edits: false,
            max_divergence: 0.0,
            reconciles: 0,
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
            ServerMsg::Bundle { chunks } => {
                for c in chunks {
                    if let Some(vol) = decode_chunk(&c.payload) {
                        self.chunks.insert(IVec3::from_array(c.pos), vol);
                    }
                }
            }
            ServerMsg::Chunk { pos, payload } => {
                if let Some(vol) = decode_chunk(payload) {
                    self.chunks.insert(IVec3::from_array(*pos), vol);
                }
            }
            ServerMsg::ChunkUnload { pos } => {
                self.chunks.remove(&IVec3::from_array(*pos));
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
    // The predictor never applies edit broadcasts — the deterministic form of
    // "the world changed server-side inside my staleness window". (Fly mode
    // is noclip by design, so this scenario must *walk* into the wall.)
    pred.ignore_edits = true;

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

    // A carves a walking tunnel north through the hillside (all within
    // reach). The server applies the carve; the stale predictor still sees
    // solid rock — so the *server* walks on while the prediction stays stuck.
    let eye = pred.sim.pos;
    let (feet_y, x0) = ((eye.y - 1.6).floor() as i32, eye.x.floor() as i32);
    let mut carved = 0u32;
    for dz in 1..=6i32 {
        for dx in -1..=1i32 {
            for dy in 0..3i32 {
                a.edit([x0 + dx, feet_y + dy, eye.z.floor() as i32 - dz], 0).await;
                carved += 1;
                if carved % 24 == 0 {
                    // Respect the server's edit rate bucket.
                    tokio::time::sleep(Duration::from_millis(800)).await;
                }
            }
        }
    }

    // Phase 2: walk north. The server strolls down the carved tunnel; the
    // local sim bumps into phantom rock; reconciliation must drag us forward.
    for _ in 0u32..150 {
        ticker.tick().await;
        let msg = pred.tick(fly_input(0.0, false), a.tracker.latest_tick);
        a.send(&msg).await;
        drain(&mut a, &mut pred, self_net).await;
    }

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
