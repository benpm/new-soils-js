//! Shared harness for scripted network-interaction tests: an embedded server
//! on a scratch data dir plus a minimal client speaking the real websocket
//! protocol. Scenario tests (`scenarios.rs`) and the embedded-path test
//! (`embedded.rs`) both build on this; it grows with each TODO phase.
#![allow(dead_code)] // each test binary uses a subset of the helpers

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{
    ChunkVolume, ClientMsg, EntityState, InputFrame, ServerMsg, SnapshotTracker, decode,
    decode_chunk, encode,
};
use soils_sim::PlayerInput;
use soils_server::{ServerConfig, ServerHandle};
use tokio::net::TcpStream;
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

    fn start_at_with(
        data_dir: PathBuf,
        tag: &str,
        tweak: impl FnOnce(&mut ServerConfig),
    ) -> Self {
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
pub struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
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
}

impl Client {
    /// Connect without logging in (for pre-auth behavior tests).
    pub async fn connect(addr: SocketAddr) -> Self {
        let (ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}")).await.expect("connect");
        Self {
            ws,
            id: 0,
            self_entity: 0,
            spawn: [0.0; 3],
            input_seq: 0,
            edit_seq: 0,
            tracker: SnapshotTracker::default(),
        }
    }

    /// Connect and log in as a guest, returning once `Init` arrives.
    pub async fn join(addr: SocketAddr, name: &str) -> Self {
        let mut c = Self::connect(addr).await;
        c.login(name).await;
        c
    }

    /// Guest-signup login; waits for `Init` and records id + spawn.
    pub async fn login(&mut self, name: &str) {
        self.send(&ClientMsg::Login { name: name.into(), password: String::new(), signup: true })
            .await;
        let (id, self_entity, spawn) = self
            .recv_until(|msg| match msg {
                ServerMsg::Init { id, self_entity, spawn, .. } => {
                    Some((id, self_entity, spawn))
                }
                ServerMsg::LoginError { message } => panic!("login failed: {message}"),
                _ => None,
            })
            .await;
        self.id = id;
        self.self_entity = self_entity;
        self.spawn = spawn;
    }

    pub async fn send(&mut self, msg: &ClientMsg) {
        self.ws.send(Message::Binary(encode(msg))).await.expect("send");
    }

    /// Next decodable `ServerMsg`, with a 10 s deadline.
    pub async fn next_msg(&mut self) -> ServerMsg {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("timed out waiting for server message")
                .expect("connection closed")
                .expect("websocket error");
            if let Message::Binary(b) = frame
                && let Some(msg) = decode::<ServerMsg>(b.as_ref())
            {
                return msg;
            }
        }
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

    /// Drive `ticks` fixed ticks of arbitrary input, paced in bursts matched to
    /// the server's input token refill (64/s) so no frame is dropped. `make(i)`
    /// builds the input for 0-based tick `i`, so callers can inject one-shot
    /// edge events (toggle_fly, jump) on chosen ticks.
    pub async fn drive(&mut self, ticks: u32, mut make: impl FnMut(u32) -> PlayerInput) {
        let mut sent = 0;
        while sent < ticks {
            let batch = (ticks - sent).min(16);
            let frames: Vec<InputFrame> = (0..batch)
                .map(|i| {
                    self.input_seq += 1;
                    let (buttons, flags, yaw) = soils_sim::pack_input(&make(sent + i));
                    InputFrame { seq: self.input_seq, buttons, flags, yaw }
                })
                .collect();
            self.send(&ClientMsg::Inputs { ack_tick: self.tracker.latest_tick, frames }).await;
            sent += batch;
            if sent < ticks {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    /// Fly for `ticks` fixed ticks with forward held, facing `yaw` (0 = -Z,
    /// -π/2 = +X). Players spawn in fly mode, so this moves at 8 u/s (32 u/s
    /// with `sprint`).
    pub async fn fly(&mut self, ticks: u32, yaw: f32, sprint: bool) {
        self.drive(ticks, |_| PlayerInput {
            move_axes: glam::Vec2::new(0.0, 1.0),
            yaw,
            sprint,
            ..Default::default()
        })
        .await;
    }

    /// Send one edit with the next sequence number; returns that `seq`.
    pub async fn edit(&mut self, pos: [i32; 3], value: u8) -> u32 {
        self.edit_seq += 1;
        let seq = self.edit_seq;
        self.send(&ClientMsg::Edit { seq, pos, value }).await;
        seq
    }

    /// Apply the next snapshot and return the entities it updated.
    pub async fn next_snapshot(&mut self) -> Vec<EntityState> {
        loop {
            if let ServerMsg::Snapshot { tick, baseline_tick, payload, .. } =
                self.next_msg().await
                && let Some(updated) = self.tracker.apply(tick, baseline_tick, &payload)
            {
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

    /// Wait for the server to push a specific chunk (the server owns the
    /// subscription — chunks stream in after login/moves without a request).
    /// Returns it decoded.
    pub async fn await_chunk(&mut self, pos: [i32; 3]) -> ChunkVolume {
        let payload = self
            .recv_until(|msg| match msg {
                ServerMsg::Bundle { chunks } => {
                    chunks.into_iter().find(|c| c.pos == pos).map(|c| c.payload)
                }
                ServerMsg::Chunk { pos: p, payload } if p == pos => Some(payload),
                _ => None,
            })
            .await;
        decode_chunk(&payload).expect("chunk payload decodes")
    }

    /// Drain pushed chunks until every position in `positions` has arrived.
    /// Returns raw payloads keyed by position (byte-comparable).
    pub async fn collect_chunks(
        &mut self,
        positions: &[[i32; 3]],
    ) -> std::collections::HashMap<[i32; 3], Vec<u8>> {
        let want: std::collections::HashSet<[i32; 3]> = positions.iter().copied().collect();
        let mut got = std::collections::HashMap::new();
        while got.len() < want.len() {
            match self.next_msg().await {
                ServerMsg::Bundle { chunks } => {
                    for c in chunks {
                        if want.contains(&c.pos) {
                            got.insert(c.pos, c.payload);
                        }
                    }
                }
                ServerMsg::Chunk { pos, payload } if want.contains(&pos) => {
                    got.insert(pos, payload);
                }
                _ => {}
            }
        }
        got
    }
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
