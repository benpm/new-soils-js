//! Shared harness for scripted network-interaction tests: an embedded server
//! on a scratch data dir plus a minimal client speaking the real websocket
//! protocol. Scenario tests (`scenarios.rs`) and the embedded-path test
//! (`embedded.rs`) both build on this; it grows with each TODO phase.
#![allow(dead_code)] // each test binary uses a subset of the helpers

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ChunkVolume, ClientMsg, InputFrame, ServerMsg, decode, decode_chunk, encode};
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
}

impl TestServer {
    /// Fresh scratch data dir. `tag` keeps parallel tests in the same binary
    /// from sharing one.
    pub fn start(tag: &str) -> Self {
        let data_dir =
            std::env::temp_dir().join(format!("soils-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let mut server = Self::start_at(data_dir, tag);
        server.owns_dir = true;
        server
    }

    /// Open (or reuse) an explicit data dir — for restart/persistence
    /// scenarios. The caller owns the dir's lifetime.
    pub fn start_at(data_dir: PathBuf, tag: &str) -> Self {
        let handle = soils_server::spawn(ServerConfig {
            bind: "127.0.0.1:0".into(),
            data_dir: data_dir.clone(),
            enable_discovery: false,
            name: format!("test-{tag}"),
            ..ServerConfig::default()
        })
        .expect("spawn embedded server");
        Self { handle, data_dir, owns_dir: false }
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
    /// Spawn position from `Init`.
    pub spawn: [f32; 3],
    /// Movement input sequence (one per simulated tick).
    input_seq: u32,
    /// Edit sequence for `ClientMsg::Edit`.
    pub edit_seq: u32,
}

impl Client {
    /// Connect without logging in (for pre-auth behavior tests).
    pub async fn connect(addr: SocketAddr) -> Self {
        let (ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}")).await.expect("connect");
        Self { ws, id: 0, spawn: [0.0; 3], input_seq: 0, edit_seq: 0 }
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
        let (id, spawn) = self
            .recv_until(|msg| match msg {
                ServerMsg::Init { id, spawn, .. } => Some((id, spawn)),
                ServerMsg::LoginError { message } => panic!("login failed: {message}"),
                _ => None,
            })
            .await;
        self.id = id;
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

    /// Fly for `ticks` fixed ticks with forward held, facing `yaw` (0 = -Z,
    /// -π/2 = +X). Paced in bursts matched to the server's input token refill
    /// (64/s) so no frame is dropped; players spawn in fly mode, so this moves
    /// at 8 u/s (32 u/s with `sprint`).
    pub async fn fly(&mut self, ticks: u32, yaw: f32, sprint: bool) {
        let mut sent = 0;
        while sent < ticks {
            let batch = (ticks - sent).min(16);
            let frames: Vec<InputFrame> = (0..batch)
                .map(|_| {
                    self.input_seq += 1;
                    let input = soils_sim::PlayerInput {
                        move_axes: glam::Vec2::new(0.0, 1.0),
                        yaw,
                        sprint,
                        ..Default::default()
                    };
                    let (buttons, flags, yaw_q) = soils_sim::pack_input(&input);
                    InputFrame { seq: self.input_seq, buttons, flags, yaw: yaw_q }
                })
                .collect();
            self.send(&ClientMsg::Inputs { frames }).await;
            sent += batch;
            if sent < ticks {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    /// Send one edit with the next sequence number; returns that `seq`.
    pub async fn edit(&mut self, pos: [i32; 3], value: u8) -> u32 {
        self.edit_seq += 1;
        let seq = self.edit_seq;
        self.send(&ClientMsg::Edit { seq, pos, value }).await;
        seq
    }

    /// The next server-echoed position of this client's own actor.
    pub async fn await_self_pos(&mut self) -> [f32; 3] {
        let id = self.id;
        self.recv_until(|msg| match msg {
            ServerMsg::ActorUpdate { actors } => {
                actors.into_iter().find(|s| s.id == id).map(|s| s.pos)
            }
            _ => None,
        })
        .await
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
