//! Shared harness for scripted network-interaction tests: an embedded server
//! on a scratch data dir plus a minimal client speaking the real websocket
//! protocol. Scenario tests (`scenarios.rs`) and the embedded-path test
//! (`embedded.rs`) both build on this; it grows with each TODO phase.
#![allow(dead_code)] // each test binary uses a subset of the helpers

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ChunkVolume, ClientMsg, ServerMsg, decode, decode_chunk, encode};
use soils_server::{ServerConfig, ServerHandle};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

/// An embedded server on an ephemeral loopback port with its own scratch data
/// dir. Dropped: shut down and the dir removed (best-effort — the background
/// persister may still be flushing).
pub struct TestServer {
    pub handle: ServerHandle,
    pub data_dir: PathBuf,
}

impl TestServer {
    /// `tag` keeps parallel tests in the same binary from sharing a data dir.
    pub fn start(tag: &str) -> Self {
        let data_dir =
            std::env::temp_dir().join(format!("soils-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let handle = soils_server::spawn(ServerConfig {
            bind: "127.0.0.1:0".into(),
            data_dir: data_dir.clone(),
            enable_discovery: false,
            name: format!("test-{tag}"),
            ..ServerConfig::default()
        })
        .expect("spawn embedded server");
        Self { handle, data_dir }
    }

    pub fn addr(&self) -> SocketAddr {
        self.handle.addr()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.shutdown();
        let _ = std::fs::remove_dir_all(&self.data_dir);
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
}

impl Client {
    /// Connect without logging in (for pre-auth behavior tests).
    pub async fn connect(addr: SocketAddr) -> Self {
        let (ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}")).await.expect("connect");
        Self { ws, id: 0, spawn: [0.0; 3] }
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

    /// Request a single chunk and wait for it (`Bundle` or `Chunk`), decoded.
    pub async fn req_chunk(&mut self, pos: [i32; 3]) -> ChunkVolume {
        self.send(&ClientMsg::ReqChunks { positions: vec![pos] }).await;
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

    /// Request `positions` in one message and drain bundles until every one has
    /// arrived. Returns raw payloads keyed by position (byte-comparable).
    pub async fn collect_chunks(
        &mut self,
        positions: &[[i32; 3]],
    ) -> std::collections::HashMap<[i32; 3], Vec<u8>> {
        self.send(&ClientMsg::ReqChunks { positions: positions.to_vec() }).await;
        let mut got = std::collections::HashMap::new();
        while got.len() < positions.len() {
            match self.next_msg().await {
                ServerMsg::Bundle { chunks } => {
                    for c in chunks {
                        got.insert(c.pos, c.payload);
                    }
                }
                ServerMsg::Chunk { pos, payload } => {
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
