//! Shared harness for scripted network-interaction tests: an embedded server
//! on a scratch data dir plus a minimal client speaking the real websocket
//! protocol. Scenario tests (`scenarios.rs`) and the embedded-path test
//! (`embedded.rs`) both build on this; it grows with each TODO phase.
#![allow(dead_code)] // each test binary uses a subset of the helpers

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use soils_protocol::netsim::{Lane, NetSim};
use soils_protocol::{ChunkInfo, 
    ChunkVolume, ClientMsg, EntityState, InputFrame, ServerMsg, SnapshotTracker, decode,
    decode_chunk, encode,
};
use soils_server::{ServerConfig, ServerHandle};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

/// An embedded server on an ephemeral loopback port. Dropping it shuts the
/// server down *synchronously* (edits flushed to disk) and removes the data
/// dir if this instance owns it.
pub struct TestServer {
    pub handle: ServerHandle,
    pub data_dir: PathBuf,
    /// Whether `Drop` deletes `data_dir` (false for [`start_at`]
    /// (Self::start_at), whose caller manages the dir across restarts).
    owns_dir: bool,
    /// Serializes server-backed tests within one binary: each embedded server
    /// runs a full worldgen burst + light floods on the process-global rayon
    /// pool, and ten at once starve each other into effective deadlock.
    _gate: std::sync::MutexGuard<'static, ()>,
}

static SERVER_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Configure the process-global rayon pool once, before any server uses it.
///
/// Worldgen and light jobs run there. Debug builds give async/generator frames
/// no layout optimisation, and the default worker stack is not enough once a
/// test binary has already run a hundred-client test in the same process — the
/// symptom is `thread '<unknown>' has overflowed its stack`, unnamed because
/// rayon does not name its workers by default. Naming them keeps any future
/// overflow attributable instead of anonymous.
fn init_rayon() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("soils-rayon-{i}"))
            .stack_size(16 * 1024 * 1024)
            .build_global();
    });
}

impl TestServer {
    /// Fresh scratch data dir. `tag` keeps parallel tests in the same binary
    /// from sharing one.
    pub fn start(tag: &str) -> Self {
        Self::start_with(tag, |_| {})
    }

    /// Fresh scratch data dir with config tweaks (e.g. test critters).
    pub fn start_with(tag: &str, tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        let data_dir =
            std::env::temp_dir().join(format!("soils-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let mut server = Self::start_at_with(data_dir, tag, tweak);
        server.owns_dir = true;
        server
    }

    /// Open (or reuse) an explicit data dir — for restart/persistence
    /// scenarios. The caller owns the dir's lifetime.
    pub fn start_at(data_dir: PathBuf, tag: &str) -> Self {
        Self::start_at_with(data_dir, tag, |_| {})
    }

    /// Reuse a data dir *and* tweak config — for restart scenarios where the
    /// second run differs, e.g. gaining a database.
    pub fn start_at_with(
        data_dir: PathBuf,
        tag: &str,
        tweak: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        init_rayon();
        let gate = SERVER_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let mut config = ServerConfig {
            bind: "127.0.0.1:0".into(),
            data_dir: data_dir.clone(),
            enable_discovery: false,
            name: format!("test-{tag}"),
            ..ServerConfig::default()
        };
        tweak(&mut config);
        let handle = soils_server::spawn(config).expect("spawn embedded server");
        Self { handle, data_dir, owns_dir: false, _gate: gate }
    }

    pub fn addr(&self) -> SocketAddr {
        self.handle.addr()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Synchronous: on return the dirty flush + writer drain are complete,
        // so restart scenarios can reopen the dir immediately.
        self.handle.shutdown_and_wait();
        if self.owns_dir {
            let _ = std::fs::remove_dir_all(&self.data_dir);
        }
    }
}

/// A scripted client. Dropping it closes the connection (the server then
/// broadcasts `ActorRemove` to same-world clients).
///
/// Messages travel through [`spawn_link`], so an optional [`NetSim`] can add
/// latency, gaussian jitter, and loss without any test needing to know.
/// A block every new player is stocked with (`STARTER_BLOCKS` in the server).
///
/// Placement spends an item, so a test that places a block the player does not
/// hold gets `EditRejected` — and a wait for `EditAccepted` then spins forever,
/// because other traffic keeps the receive deadline alive.
pub const HELD_BLOCK: u8 = 4; // Cobblestone

pub struct Client {
    out: UnboundedSender<ClientMsg>,
    inbox: UnboundedReceiver<ServerMsg>,
    /// Non-snapshot messages set aside by [`Client::drain_self_pos`], so
    /// skipping ahead to the freshest position never loses a manifest or an
    /// edit ack that a later `recv_until` is waiting for.
    held: std::collections::VecDeque<ServerMsg>,
    /// Every chunk the server has pushed, and every entity it has announced.
    ///
    /// `recv_until` *discards* messages that do not match its predicate, so
    /// waiting for an entity spawn silently throws away any chunk manifest
    /// that arrives first — and the server sends each chunk once, so the later
    /// `await_chunk` would wait forever. A real client keeps state from every
    /// message it sees; so does this one.
    seen_chunks: std::collections::HashMap<[i32; 3], ChunkInfo>,
    seen_spawns: std::collections::HashMap<u32, u16>,
    /// Item carried by each dropped-item entity, from its `EntitySpawn`.
    seen_items: std::collections::HashMap<u32, soils_sim::ItemStack>,
    /// Mirror of the server's authoritative inventory, from `InventoryUpdate`.
    inventory: Vec<Option<soils_sim::ItemStack>>,
    /// Mirror of the open container, from `ContainerUpdate`. `None` once the
    /// server says it is closed — a real client never decides that itself.
    container: Option<([i32; 3], Vec<Option<soils_sim::ItemStack>>)>,
    /// Last position seen for each entity.
    ///
    /// Snapshots are deltas: an entity that has not moved is simply absent, so
    /// there is no message to wait for. Without this, asking a standing
    /// player where it is would block until it moved again — which, for a
    /// player deliberately holding still, is never.
    known: std::collections::HashMap<u32, [f32; 3]>,
    /// Player id from `Init` (0 until logged in).
    pub id: u16,
    /// NetId of our own player entity, from `Init`.
    pub self_entity: u32,
    /// Spawn position from `Init`.
    pub spawn: [f32; 3],
    /// Movement input sequence (one per simulated tick).
    input_seq: u32,
    /// Edit sequence for `ClientMsg::Edit`.
    pub edit_seq: u32,
    /// Snapshot decode state (baselines + latest tick, acked on `fly`).
    pub tracker: SnapshotTracker,
    pub worldgen: Option<soils_protocol::GenParams>,
    genctx: Option<(soils_worldgen::TerrainGen, soils_worldgen::BlockRegistry)>,
}

impl Client {
    /// Connect without logging in (for pre-auth behavior tests).
    pub async fn connect(addr: SocketAddr) -> Self {
        Self::connect_with(addr, None).await
    }

    /// Connect over a simulated link. `None` is a direct connection.
    pub async fn connect_with(addr: SocketAddr, sim: Option<NetSim>) -> Self {
        let (ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}")).await.expect("connect");
        let (out, inbox) = spawn_link(ws, sim);
        Self {
            out,
            inbox,
            held: std::collections::VecDeque::new(),
            seen_chunks: std::collections::HashMap::new(),
            seen_spawns: std::collections::HashMap::new(),
            seen_items: std::collections::HashMap::new(),
            inventory: Vec::new(),
            container: None,
            known: std::collections::HashMap::new(),
            id: 0,
            self_entity: 0,
            spawn: [0.0; 3],
            input_seq: 0,
            edit_seq: 0,
            tracker: SnapshotTracker::default(),
            worldgen: None,
            genctx: None,
        }
    }

    /// Connect and log in as a guest, returning once `Init` arrives.
    pub async fn join(addr: SocketAddr, name: &str) -> Self {
        let mut c = Self::connect(addr).await;
        c.login(name).await;
        c
    }

    /// [`join`](Self::join) over a simulated link.
    pub async fn join_with(addr: SocketAddr, name: &str, sim: Option<NetSim>) -> Self {
        let mut c = Self::connect_with(addr, sim).await;
        c.login(name).await;
        c
    }

    /// Guest-signup login; waits for `Init` and records id + spawn.
    pub async fn login(&mut self, name: &str) {
        self.send(&ClientMsg::Login {
            name: name.into(),
            password: String::new(),
            signup: true,
            protocol: soils_protocol::PROTOCOL_VERSION,
        })
        .await;
        let (id, self_entity, spawn, worldgen) = self
            .recv_until(|msg| match msg {
                ServerMsg::Init { id, self_entity, spawn, worldgen, .. } => {
                    Some((id, self_entity, spawn, worldgen))
                }
                ServerMsg::LoginError { message } => panic!("login failed: {message}"),
                _ => None,
            })
            .await;
        self.id = id;
        self.self_entity = self_entity;
        self.spawn = spawn;
        self.worldgen = Some(worldgen);
        self.known.insert(self_entity, spawn);
    }

    pub async fn send(&mut self, msg: &ClientMsg) {
        self.out.send(msg.clone()).expect("link closed");
    }

    /// Next `ServerMsg`, with a 10 s deadline. Every message is recorded on
    /// the way past, so nothing a later wait depends on can be thrown away.
    pub async fn next_msg(&mut self) -> ServerMsg {
        if let Some(msg) = self.held.pop_front() {
            return msg;
        }
        let msg = tokio::time::timeout(Duration::from_secs(10), self.inbox.recv())
            .await
            .expect("timed out waiting for server message")
            .expect("connection closed");
        self.record(&msg);
        msg
    }

    /// Note the durable facts carried by a message: which chunks exist, which
    /// entities exist. Idempotent; safe to call for a message twice.
    fn record(&mut self, msg: &ServerMsg) {
        match msg {
            ServerMsg::Manifest { chunks } => {
                for info in chunks {
                    self.seen_chunks.insert(info.pos(), info.clone());
                }
            }
            ServerMsg::EntitySpawn { id, kind, pos, item } => {
                self.seen_spawns.insert(*id, *kind);
                if let Some(item) = item {
                    self.seen_items.insert(*id, *item);
                }
                // Seed the position table from the spawn message. A body that
                // settles never appears in a delta snapshot again, so without
                // this its position would only ever be knowable if it happened
                // to move after we started listening.
                self.known.entry(*id).or_insert(*pos);
            }
            ServerMsg::EntityDespawn { id } => {
                self.seen_spawns.remove(id);
                self.seen_items.remove(id);
                self.known.remove(id);
            }
            ServerMsg::InventoryUpdate { slots } => self.inventory = slots.clone(),
            ServerMsg::ContainerUpdate { pos, slots } => {
                self.container = Some((*pos, slots.clone()))
            }
            ServerMsg::ContainerClosed { pos } => {
                if self.container.as_ref().is_some_and(|(p, _)| p == pos) {
                    self.container = None;
                }
            }
            _ => {}
        }
    }

    /// NetIds of every dropped item the server has announced, with contents.
    pub fn items_seen(&self) -> Vec<(u32, soils_sim::ItemStack)> {
        let mut v: Vec<_> = self.seen_items.iter().map(|(id, s)| (*id, *s)).collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    /// The last inventory the server pushed.
    pub fn inventory(&self) -> &[Option<soils_sim::ItemStack>] {
        &self.inventory
    }

    /// Contents of the container the server says we have open.
    pub fn container(&self) -> Option<&[Option<soils_sim::ItemStack>]> {
        self.container.as_ref().map(|(_, s)| s.as_slice())
    }

    /// How many of `kind` the open container holds.
    pub fn container_count(&self, kind: soils_sim::ItemKind) -> u32 {
        self.container()
            .unwrap_or_default()
            .iter()
            .flatten()
            .filter(|s| s.kind == kind)
            .map(|s| s.count as u32)
            .sum()
    }

    /// Index of the first container slot holding `kind`.
    pub fn container_slot_of(&self, kind: soils_sim::ItemKind) -> Option<u16> {
        self.container()?
            .iter()
            .position(|s| s.is_some_and(|s| s.kind == kind))
            .map(|i| i as u16)
    }

    /// Index of the first pack slot holding `kind`.
    pub fn pack_slot_of(&self, kind: soils_sim::ItemKind) -> Option<u16> {
        self.inventory
            .iter()
            .position(|s| s.is_some_and(|s| s.kind == kind))
            .map(|i| i as u16)
    }

    /// How many of `kind` the server says we hold.
    pub fn count_of(&self, kind: soils_sim::ItemKind) -> u32 {
        self.inventory.iter().flatten().filter(|s| s.kind == kind).map(|s| s.count as u32).sum()
    }

    /// Pump messages until `cond` holds over the mirrored inventory, or the
    /// deadline passes. Returns whether it held.
    pub async fn await_inventory(
        &mut self,
        mut cond: impl FnMut(&Self) -> bool,
        timeout: Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while !cond(self) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            let Ok(msg) = tokio::time::timeout(Duration::from_millis(500), self.inbox.recv()).await
            else {
                continue;
            };
            let Some(msg) = msg else { return false };
            self.record(&msg);
        }
        true
    }

    /// Last known position of an entity, without waiting for it to move.
    pub fn known_pos(&self, net: u32) -> Option<[f32; 3]> {
        self.known.get(&net).copied()
    }

    /// NetIds of every physics prop the server has announced.
    pub fn props_seen(&self) -> Vec<u32> {
        self.seen_spawns
            .iter()
            .filter(|(_, kind)| **kind == soils_sim::KIND_PHYSICS_CUBE)
            .map(|(id, _)| *id)
            .collect()
    }

    /// NetIds of other players the server has announced.
    pub fn peer_players(&self) -> Vec<u32> {
        self.seen_spawns
            .iter()
            .filter(|(id, kind)| **kind == soils_sim::KIND_PLAYER && **id != self.self_entity)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Wait until the server has announced another player, and return its
    /// NetId. Checks what has already been seen before waiting.
    pub async fn await_peer_player(&mut self) -> u32 {
        loop {
            if let Some(id) = self.peer_players().into_iter().min() {
                return id;
            }
            let msg = self.next_msg().await;
            self.record(&msg);
        }
    }

    /// Freshest server-reported position of our own entity, taken from
    /// everything already queued without blocking. `None` means no snapshot
    /// mentioned us — which, in a delta stream, means we did not move.
    ///
    /// Tests that pause (building a structure, waiting on a barrier) build up
    /// a backlog; reading one message off the front of it reports where the
    /// player *was*, not where they are.
    pub fn drain_self_pos(&mut self) -> Option<[f32; 3]> {
        self.drain_entity_pos(self.self_entity)
    }

    /// As [`drain_self_pos`](Self::drain_self_pos), for any entity.
    ///
    /// Pulls only from the socket. `held` is append-only here: draining it too
    /// would re-queue each non-snapshot message it just popped, and spin.
    pub fn drain_entity_pos(&mut self, net: u32) -> Option<[f32; 3]> {
        let mut latest = None;
        while let Ok(msg) = self.inbox.try_recv() {
            match msg {
                ServerMsg::Snapshot { tick, baseline_tick, ref payload, .. } => {
                    if let Some(states) = self.tracker.apply(tick, baseline_tick, payload) {
                        for st in &states {
                            self.known.insert(st.id, st.pos);
                            if st.id == net {
                                latest = Some(st.pos);
                            }
                        }
                    }
                }
                other => {
                    self.record(&other);
                    self.held.push_back(other);
                }
            }
        }
        latest
    }

    /// Our current position: the freshest queued snapshot, else the last one
    /// seen, else the next to arrive.
    pub async fn current_self_pos(&mut self) -> [f32; 3] {
        self.current_entity_pos(self.self_entity).await
    }

    /// Drain until the snapshot stream has advanced `n` server ticks.
    ///
    /// This is what makes "no update for us" mean "we did not move". Waiting a
    /// fixed wall-clock interval instead would be wrong on a slow link: with a
    /// 240 ms round trip the reply to an input has not arrived after 150 ms,
    /// and silence would be misread as stillness — reporting a falling player
    /// as landed.
    pub async fn await_server_ticks(&mut self, n: u32) {
        let target = self.tracker.latest_tick + n;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while self.tracker.latest_tick < target {
            // Drains snapshots, which advances the tracker and refreshes the
            // last-known position table.
            self.drain_self_pos();
            assert!(
                std::time::Instant::now() < deadline,
                "snapshot stream stalled below tick {target} (at {})",
                self.tracker.latest_tick
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Poll our own server-reported position until `pred` holds.
    ///
    /// Polling, rather than waiting on the next snapshot mentioning us, is
    /// what makes this safe in both directions: a player who has stopped
    /// never appears in a delta snapshot again, and a player under load may
    /// not appear for many ticks. Sampling the freshest known position on a
    /// timer covers both.
    pub async fn await_self_where(
        &mut self,
        mut pred: impl FnMut([f32; 3]) -> bool,
        what: &str,
    ) -> [f32; 3] {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let p = self.current_self_pos().await;
            if pred(p) {
                return p;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}; last position {p:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Any entity's current position, same freshness rule. Never blocks on an
    /// entity that has simply stopped moving.
    pub async fn current_entity_pos(&mut self, net: u32) -> [f32; 3] {
        if let Some(p) = self.drain_entity_pos(net) {
            return p;
        }
        if let Some(p) = self.known.get(&net).copied() {
            return p;
        }
        self.await_entity(net, |_| true).await.pos
    }

    /// Drain messages until `f` yields a value; interleaved broadcasts
    /// (`Time`, `ActorUpdate`, ...) are skipped by returning `None`.
    pub async fn recv_until<T>(&mut self, mut f: impl FnMut(ServerMsg) -> Option<T>) -> T {
        loop {
            if let Some(v) = f(self.next_msg().await) {
                return v;
            }
        }
    }

    /// Fly for `ticks` fixed ticks with forward held, facing `yaw` (0 = -Z,
    /// -π/2 = +X). Players spawn in fly mode, so this moves at 8 u/s (32 u/s
    /// with `sprint`).
    pub async fn fly(&mut self, ticks: u32, yaw: f32, sprint: bool) {
        let input = soils_sim::PlayerInput {
            move_axes: glam::Vec2::new(0.0, 1.0),
            yaw,
            sprint,
            ..Default::default()
        };
        self.drive(ticks, |_| input).await;
    }

    /// Send `ticks` input frames, each built by `make`. Paced in bursts matched
    /// to the server's input token refill (64/s) so no frame is dropped.
    pub async fn drive(
        &mut self,
        ticks: u32,
        mut make: impl FnMut(u32) -> soils_sim::PlayerInput,
    ) {
        let mut sent = 0;
        while sent < ticks {
            let batch = (ticks - sent).min(16);
            let frames: Vec<InputFrame> = (0..batch)
                .map(|i| {
                    self.input_seq += 1;
                    let input = make(sent + i);
                    let (buttons, flags, yaw_q) = soils_sim::pack_input(&input);
                    InputFrame { seq: self.input_seq, buttons, flags, yaw: yaw_q }
                })
                .collect();
            self.send(&ClientMsg::Inputs { ack_tick: self.tracker.latest_tick, frames }).await;
            sent += batch;
            if sent < ticks {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    /// Hold `input` for `ticks` ticks.
    pub async fn hold(&mut self, ticks: u32, input: soils_sim::PlayerInput) {
        self.drive(ticks, |_| input).await;
    }

    /// Walk with forward held, facing `yaw`. Only meaningful out of fly mode
    /// (see [`land`](Self::land)) — fly mode is noclip, so a flying player
    /// passes through everything a walking one collides with.
    pub async fn walk(&mut self, ticks: u32, yaw: f32) {
        self.hold(ticks, soils_sim::PlayerInput {
            move_axes: glam::Vec2::new(0.0, 1.0),
            yaw,
            ..Default::default()
        })
        .await;
    }

    /// Send idle input for `dur` of wall-clock time.
    ///
    /// [`hold`](Self::hold) with a small tick count returns *immediately* — it
    /// paces only between batches — so holding a position for a real interval
    /// needs this. A client that simply stops sending is frozen, not standing.
    pub async fn idle_for(&mut self, dur: Duration) {
        let end = tokio::time::Instant::now() + dur;
        while tokio::time::Instant::now() < end {
            self.hold(16, soils_sim::PlayerInput::default()).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Toggle fly mode with a single edge-flagged frame.
    pub async fn toggle_fly(&mut self) {
        self.drive(1, |_| soils_sim::PlayerInput { toggle_fly: true, ..Default::default() })
            .await;
    }

    /// Leave fly mode and fall until the server reports us at rest. Returns the
    /// resting eye position.
    ///
    /// "At rest" is judged from the server's own echo rather than a fixed tick
    /// count, because how far there is to fall depends on the terrain (and, in
    /// the stacking tests, on whoever is already standing below).
    pub async fn land(&mut self) -> [f32; 3] {
        self.toggle_fly().await;
        self.settle().await
    }

    /// Idle until the server reports us at rest vertically, *without* touching
    /// fly mode. Use after a jump; [`land`](Self::land) is this plus the
    /// toggle.
    pub async fn settle(&mut self) -> [f32; 3] {
        let mut last = self.current_self_pos().await;
        let mut still = 0;
        for _ in 0..120 {
            // Idle frames: gravity only integrates on ticks the server is
            // given input for.
            self.hold(8, soils_sim::PlayerInput::default()).await;
            // Wait for the server to actually report back before judging
            // stillness — see `await_server_ticks`.
            self.await_server_ticks(4).await;
            let now = self.current_self_pos().await;
            still = if (now[1] - last[1]).abs() < 1e-3 { still + 1 } else { 0 };
            last = now;
            if still >= 3 {
                return now;
            }
        }
        panic!("never came to rest while landing (last y {})", last[1]);
    }

    /// Send one edit with the next sequence number; returns that `seq`.
    pub async fn edit(&mut self, pos: [i32; 3], value: u8) -> u32 {
        self.edit_seq += 1;
        let seq = self.edit_seq;
        self.send(&ClientMsg::Edit { seq, pos, value }).await;
        seq
    }

    /// Wait for the server's verdict on `seq`. Returns whether it was applied.
    ///
    /// Always prefer this to matching `EditAccepted` alone: a rejection sends
    /// no accept, and the surrounding traffic keeps `next_msg` from ever timing
    /// out, so the bare version hangs instead of failing.
    pub async fn edit_verdict(&mut self, seq: u32) -> bool {
        self.recv_until(|msg| match msg {
            ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(true),
            ServerMsg::EditRejected { seq: s } if s == seq => Some(false),
            _ => None,
        })
        .await
    }

    /// Place `value` and assert the server applied it.
    pub async fn place(&mut self, pos: [i32; 3], value: u8) {
        let seq = self.edit(pos, value).await;
        assert!(self.edit_verdict(seq).await, "edit {seq} at {pos:?} (value {value}) was rejected");
    }

    /// Apply the next snapshot and return the entities it updated.
    pub async fn next_snapshot(&mut self) -> Vec<EntityState> {
        loop {
            if let ServerMsg::Snapshot { tick, baseline_tick, payload, .. } =
                self.next_msg().await
                && let Some(updated) = self.tracker.apply(tick, baseline_tick, &payload)
            {
                for st in &updated {
                    self.known.insert(st.id, st.pos);
                }
                return updated;
            }
        }
    }

    /// The next server-echoed position of this client's own player entity.
    pub async fn await_self_pos(&mut self) -> [f32; 3] {
        let net = self.self_entity;
        loop {
            if let Some(s) = self.next_snapshot().await.into_iter().find(|s| s.id == net) {
                return s.pos;
            }
        }
    }

    /// Wait until an entity's snapshot state satisfies `pred`; returns it.
    pub async fn await_entity(
        &mut self,
        net: u32,
        mut pred: impl FnMut(&EntityState) -> bool,
    ) -> EntityState {
        loop {
            if let Some(s) =
                self.next_snapshot().await.into_iter().find(|s| s.id == net && pred(s))
            {
                return s;
            }
        }
    }

    /// The local generator mirror (built lazily from `Init`'s GenParams) —
    /// the same materialization path the real client runs.
    pub fn generator(&mut self) -> &(soils_worldgen::TerrainGen, soils_worldgen::BlockRegistry) {
        if self.genctx.is_none() {
            let p = self.worldgen.expect("login() captures GenParams");
            let wt = match p.world_type {
                1 => soils_worldgen::WorldType::Flat,
                _ => soils_worldgen::WorldType::Normal,
            };
            let terrain = soils_worldgen::TerrainGen::new(p.seed as u32, wt);
            assert_eq!(
                soils_worldgen::graph_hash(terrain.graph()),
                p.graph_hash,
                "server generator identity differs from this build"
            );
            self.genctx = Some((terrain, soils_worldgen::default_registry()));
        }
        self.genctx.as_ref().unwrap()
    }

    /// A manifest entry as a voxel volume: `Edited` decodes its payload,
    /// `Pristine` generates locally (bit-exact by worldgen v2's contract).
    pub fn materialize(&mut self, info: &ChunkInfo) -> ChunkVolume {
        match info {
            ChunkInfo::Edited { payload, .. } => {
                decode_chunk(payload).expect("chunk payload decodes")
            }
            ChunkInfo::Pristine { pos } => {
                let (terrain, registry) = self.generator();
                terrain.generate(glam::IVec3::from_array(*pos), registry)
            }
        }
    }

    /// Wait for the server to push a specific chunk (the server owns the
    /// subscription — chunks stream in after login/moves without a request).
    /// Returns it materialized.
    pub async fn await_chunk(&mut self, pos: [i32; 3]) -> ChunkVolume {
        loop {
            if let Some(info) = self.seen_chunks.get(&pos).cloned() {
                return self.materialize(&info);
            }
            let msg = self.next_msg().await;
            self.record(&msg);
        }
    }

    /// Drain pushed chunks until every position in `positions` has arrived.
    /// Payloads are canonical `chunk_codec` bytes (pristine entries are
    /// generated locally and re-encoded), so two clients' maps byte-compare.
    pub async fn collect_chunks(&mut self, positions: &[[i32; 3]]) -> CollectedChunks {
        let want: std::collections::HashSet<[i32; 3]> = positions.iter().copied().collect();
        let mut out = CollectedChunks::default();
        while out.payloads.len() < want.len() {
            if let ServerMsg::Manifest { chunks } = self.next_msg().await {
                for info in chunks {
                    let pos = info.pos();
                    if !want.contains(&pos) {
                        continue;
                    }
                    match &info {
                        ChunkInfo::Edited { payload, .. } => {
                            out.edited += 1;
                            out.wire_bytes += payload.len() + 13;
                        }
                        ChunkInfo::Pristine { .. } => out.wire_bytes += 13,
                    }
                    let vol = self.materialize(&info);
                    out.payloads.insert(pos, soils_protocol::encode_chunk(&vol));
                }
            }
        }
        out
    }
    /// Drain pushed chunks until the manifest stream goes quiet for `quiet`,
    /// keeping only positions in `want`.
    ///
    /// The counterpart to [`collect_chunks`](Self::collect_chunks) for anything
    /// the server may legitimately never send: occlusion culling withholds
    /// chunks sealed behind solid neighbours, so waiting for a fixed set hangs
    /// forever. Quiet is measured on *manifests*, not on the socket — snapshots
    /// go out every tick, so the connection is never idle.
    pub async fn collect_available(
        &mut self,
        want: &[[i32; 3]],
        quiet: Duration,
    ) -> CollectedChunks {
        let want: std::collections::HashSet<[i32; 3]> = want.iter().copied().collect();
        let mut out = CollectedChunks::default();
        let mut last = tokio::time::Instant::now();
        while last.elapsed() < quiet {
            if let Ok(ServerMsg::Manifest { chunks }) =
                tokio::time::timeout(Duration::from_millis(150), self.next_msg()).await
            {
                last = tokio::time::Instant::now();
                for info in chunks {
                    let pos = info.pos();
                    if !want.contains(&pos) {
                        continue;
                    }
                    match &info {
                        ChunkInfo::Edited { payload, .. } => {
                            out.edited += 1;
                            out.wire_bytes += payload.len() + 13;
                        }
                        ChunkInfo::Pristine { .. } => out.wire_bytes += 13,
                    }
                    let vol = self.materialize(&info);
                    out.payloads.insert(pos, soils_protocol::encode_chunk(&vol));
                }
            }
        }
        out
    }
}

/// Result of [`Client::collect_chunks`]: canonical payloads plus what the
/// stream actually cost (manifest wire bytes, edited-entry count).
#[derive(Default)]
pub struct CollectedChunks {
    pub payloads: std::collections::HashMap<[i32; 3], Vec<u8>>,
    pub wire_bytes: usize,
    pub edited: usize,
}

/// Pump a websocket through an optional simulated link.
///
/// Each direction gets its own [`NetSim`]. Delay is applied by stamping an
/// absolute deadline the moment a message is read and sleeping until it in a
/// separate task, rather than sleeping inline: sleeping in the read loop would
/// stall the *next* read too, so a 100 ms link fed at 10 ms intervals would
/// accumulate delay without bound instead of holding steady at 100 ms.
/// Deadlines from `NetSim::delay` are non-decreasing, so FIFO + `sleep_until`
/// preserves order exactly.
fn spawn_link(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    sim: Option<NetSim>,
) -> (UnboundedSender<ClientMsg>, UnboundedReceiver<ServerMsg>) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (out_tx, mut out_rx) = unbounded_channel::<ClientMsg>();
    let (in_tx, in_rx) = unbounded_channel::<ServerMsg>();
    let (mut up, mut down) = sim.map(|s| s.split_direction()).unzip();

    // Uplink: test -> (delay/loss) -> server.
    let (uq_tx, mut uq_rx) = unbounded_channel::<(tokio::time::Instant, ClientMsg)>();
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let at = match &mut up {
                None => tokio::time::Instant::now(),
                Some(sim) => {
                    // Inputs re-send the last 3 frames precisely so the link
                    // may drop them; everything else must arrive.
                    let lane = match msg {
                        ClientMsg::Inputs { .. } => Lane::Unreliable,
                        _ => Lane::Reliable,
                    };
                    if sim.should_drop(lane) {
                        continue;
                    }
                    tokio::time::Instant::now() + sim.delay(std::time::Instant::now())
                }
            };
            if uq_tx.send((at, msg)).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some((at, msg)) = uq_rx.recv().await {
            tokio::time::sleep_until(at).await;
            if ws_tx.send(Message::Binary(encode(&msg))).await.is_err() {
                break;
            }
        }
    });

    // Downlink: server -> (delay/loss) -> test.
    let (dq_tx, mut dq_rx) = unbounded_channel::<(tokio::time::Instant, ServerMsg)>();
    tokio::spawn(async move {
        while let Some(Ok(frame)) = ws_rx.next().await {
            let Message::Binary(b) = frame else { continue };
            let Some(msg) = decode::<ServerMsg>(b.as_ref()) else { continue };
            let at = match &mut down {
                None => tokio::time::Instant::now(),
                Some(sim) => {
                    // Snapshots are delta-coded against acked baselines, so a
                    // lost one costs precision, not correctness.
                    let lane = match msg {
                        ServerMsg::Snapshot { .. } => Lane::Unreliable,
                        _ => Lane::Reliable,
                    };
                    if sim.should_drop(lane) {
                        continue;
                    }
                    tokio::time::Instant::now() + sim.delay(std::time::Instant::now())
                }
            };
            if dq_tx.send((at, msg)).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some((at, msg)) = dq_rx.recv().await {
            tokio::time::sleep_until(at).await;
            if in_tx.send(msg).is_err() {
                break;
            }
        }
    });

    (out_tx, in_rx)
}

/// Run `body` on its own OS thread with its own current-thread runtime, after
/// joining as `name`. Real threads (not just tasks) are the point: the server
/// must hold up when its connections are driven by genuinely parallel clients,
/// not merely interleaved ones on a single executor.
pub fn spawn_peer<F, Fut, T>(
    addr: SocketAddr,
    name: impl Into<String>,
    sim: Option<NetSim>,
    body: F,
) -> std::thread::JoinHandle<T>
where
    F: FnOnce(Client) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let name = name.into();
    std::thread::Builder::new()
        .name(format!("peer-{name}"))
        // Debug builds do not shrink async state machines, and a `Client`
        // future nested through login/stream/settle overflows the 1 MB Windows
        // default once a hundred of them run at once.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("peer runtime");
            rt.block_on(async move {
                let client = Client::join_with(addr, &name, sim).await;
                body(client).await
            })
        })
        .expect("spawn peer thread")
}

/// Poll `cond` until it holds or `timeout` elapses. Used for asynchronous
/// effects (e.g. the background chunk-persistence writer).
pub fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}
