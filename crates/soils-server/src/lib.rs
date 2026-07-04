//! Headless authoritative server for the new-soils Rust port, usable both as
//! the dedicated `soils-server` binary ([`run`]) and embedded in the client for
//! single-player ([`spawn`], which runs the server on its own threads bound to
//! a loopback ephemeral port).
//!
//! Since TODO phase 5 (game-systems M2) the server is a headless Bevy ECS app
//! (`app.rs`) ticking at a fixed rate; this module owns only the network edge:
//! the tokio accept loop, per-connection pump tasks (decode → inbox, outbox →
//! socket), and the LAN discovery responder. The wire protocol is unchanged.

mod app;
mod auth;
mod persist;
mod region;
mod world;

use persist::{PersistHandle, Persister};

use auth::Accounts;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU16, Ordering},
};

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{
    ClientMsg, DISCOVERY_PORT, PROBE_MAGIC, ServerInfo, ServerMsg, decode, encode,
};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

/// Real seconds for a full day cycle (JS used ~20 minutes; shortened so the
/// effect is visible while testing the slice).
pub(crate) const DAY_SECONDS: f32 = 120.0;
/// Chunks per `Bundle` response. Small because solid chunks are ~32 KB each.
pub(crate) const BUNDLE_SIZE: usize = 16;
/// Chunks generated per wave. A fresh world's first request is up to 9³=729
/// chunks; splitting it into nearest-first waves (generated in parallel on the
/// rayon pool, adopted as they complete) lets the near ring stream to the
/// client while the outer rings are still generating.
pub(crate) const WAVE_SIZE: usize = 48;
/// The world every client starts in.
pub(crate) const DEFAULT_WORLD: &str = "default";
/// Max accepted movement between two `Move` updates (world units). Generous —
/// well above sprint-fly + lag spikes (~32 u/s, sent every 50 ms) — so it only
/// catches gross teleport/speed hacks, not legitimate play.
pub(crate) const MAX_STEP: f32 = 64.0;

/// A freshly handshaken connection, handed from the tokio accept loop to the
/// ECS app. The app owns the inbox/outbox ends; the connection task is a pure
/// pump with no game state.
pub(crate) struct NewConn {
    pub id: u16,
    pub inbox: mpsc::UnboundedReceiver<ClientMsg>,
    pub outbox: mpsc::UnboundedSender<ServerMsg>,
}

/// How to run a server: where to bind, where to persist, whether to be
/// discoverable on the LAN.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Address to bind, as `host:port`. A `String` (not `SocketAddr`) so names
    /// like `localhost:9001` resolve; use port `0` for an ephemeral port
    /// (embedded single-player).
    pub bind: String,
    /// Root for all persistence: `<data_dir>/accounts.bin` and
    /// `<data_dir>/worlds/<name>/regions`.
    pub data_dir: PathBuf,
    /// Whether the LAN discovery responder starts enabled. Toggle at runtime
    /// with [`ServerHandle::set_discovery`]. Off for embedded servers by
    /// default, which should stay invisible unless the player opts in.
    pub enable_discovery: bool,
    /// UDP port for the discovery responder. Normally [`DISCOVERY_PORT`];
    /// use `0` in tests for an ephemeral port (read back via
    /// [`ServerHandle::discovery_port`]).
    pub discovery_port: u16,
    /// Server name shown in discovery replies.
    pub name: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // All interfaces, so the server is reachable over LAN.
            bind: "0.0.0.0:9001".into(),
            data_dir: PathBuf::from("data"),
            enable_discovery: true,
            discovery_port: DISCOVERY_PORT,
            name: "new-soils".into(),
        }
    }
}

/// Handle to an embedded server running on its own detached threads.
/// Dropping it does NOT stop the server; call [`shutdown`](Self::shutdown) or
/// let process exit tear it down.
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    discovery: watch::Sender<bool>,
    discovery_port: watch::Receiver<Option<u16>>,
    /// The embedded server thread, joinable for a synchronous shutdown.
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ServerHandle {
    /// The actual bound address (resolves the ephemeral port).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Enable or disable the LAN discovery responder at runtime. Enabling
    /// binds the UDP socket (asynchronously — watch [`discovery_port`]
    /// (Self::discovery_port) for the result); disabling releases it.
    pub fn set_discovery(&self, on: bool) {
        let _ = self.discovery.send(on);
    }

    /// The *desired* discovery state (what was last requested).
    pub fn discovery_enabled(&self) -> bool {
        *self.discovery.borrow()
    }

    /// The UDP port the discovery responder is actually bound to, or `None`
    /// while discovery is off, still starting, or failed to bind.
    pub fn discovery_port(&self) -> Option<u16> {
        *self.discovery_port.borrow()
    }

    /// Ask the server to stop: the accept loop breaks and the ECS app exits
    /// (flushing queued chunk writes on the way down).
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// [`shutdown`](Self::shutdown), then block until the server thread has
    /// fully exited — including the dirty-chunk flush and the persistence
    /// writer drain — so on return every edit is on disk.
    pub fn shutdown_and_wait(&self) {
        self.shutdown();
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

/// Deterministic per-world seed; the default world keeps seed 0 so its terrain
/// (and any persisted data) is unchanged.
pub(crate) fn world_seed(name: &str) -> u32 {
    if name == DEFAULT_WORLD {
        return 0;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    h.finish() as u32
}

/// Bind and serve forever on the current runtime. Used by the dedicated binary.
pub async fn run(config: ServerConfig) -> std::io::Result<()> {
    let listener = TcpListener::bind(&config.bind).await?;
    println!("new-soils server listening on ws://{}", config.bind);
    let persister = Persister::new();
    // Never-firing shutdown/discovery senders: they stay alive in this frame
    // for the whole await, so `changed()` pends forever and the initial
    // discovery state holds until process exit.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (discovery_tx, discovery_rx) = watch::channel(config.enable_discovery);
    let (discovery_port_tx, _discovery_port_rx) = watch::channel(None);
    let result =
        serve(listener, config, persister.handle(), shutdown_rx, discovery_rx, discovery_port_tx)
            .await;
    // The ECS app has exited (joined inside `serve`); flush queued chunk writes.
    persister.shutdown();
    drop(shutdown_tx);
    drop(discovery_tx);
    result
}

/// Start a server on a dedicated background thread with its own tokio runtime
/// (plus the ECS app thread `serve` spawns). Blocks only until the TCP bind
/// has completed, then returns the handle with the real bound address. Used by
/// the client for single-player.
pub fn spawn(config: ServerConfig) -> std::io::Result<ServerHandle> {
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<SocketAddr>>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (discovery_tx, discovery_rx) = watch::channel(config.enable_discovery);
    let (discovery_port_tx, discovery_port_rx) = watch::channel(None);
    let thread = std::thread::Builder::new()
        .name("soils-embedded-server".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async move {
                let listener = match TcpListener::bind(&config.bind).await {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let addr = match listener.local_addr() {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let _ = tx.send(Ok(addr));
                let persister = Persister::new();
                let _ = serve(
                    listener,
                    config,
                    persister.handle(),
                    shutdown_rx,
                    discovery_rx,
                    discovery_port_tx,
                )
                .await;
                // The ECS app has exited; flush queued chunk writes before the
                // runtime thread exits.
                persister.shutdown();
            });
        })?;
    let addr = rx
        .recv()
        .map_err(|_| std::io::Error::other("embedded server thread died before binding"))??;
    Ok(ServerHandle {
        addr,
        shutdown: shutdown_tx,
        discovery: discovery_tx,
        discovery_port: discovery_port_rx,
        thread: std::sync::Mutex::new(Some(thread)),
    })
}

/// Run the network edge (ECS app thread, discovery responder, accept loop)
/// until `shutdown` fires (or forever, for the dedicated binary). Joins the
/// ECS app thread before returning so the caller can safely flush persistence.
async fn serve(
    listener: TcpListener,
    config: ServerConfig,
    persist: PersistHandle,
    mut shutdown: watch::Receiver<bool>,
    discovery: watch::Receiver<bool>,
    discovery_port_tx: watch::Sender<Option<u16>>,
) -> std::io::Result<()> {
    let accounts = Arc::new(Accounts::load(&config.data_dir));
    let player_count = Arc::new(AtomicU16::new(0));
    let (conns_tx, conns_rx) = mpsc::unbounded_channel::<NewConn>();

    // The ECS app owns all game state on its own thread; it exits when the
    // same shutdown watch fires.
    let app_thread = {
        let shutdown = shutdown.clone();
        let data_dir = config.data_dir.clone();
        let accounts = accounts.clone();
        let player_count = player_count.clone();
        std::thread::Builder::new().name("soils-ecs".into()).spawn(move || {
            app::run_app(conns_rx, shutdown, data_dir, persist, accounts, player_count);
        })?
    };

    // LAN discovery supervisor: runs the UDP probe responder while the
    // `discovery` watch says on, releases the socket while off. Advertises the
    // actually-bound game port (matters when binding port 0).
    {
        let game_port = listener.local_addr()?.port();
        tokio::spawn(discovery_supervisor(
            config.discovery_port,
            game_port,
            player_count.clone(),
            config.name.clone(),
            discovery,
            discovery_port_tx,
        ));
    }

    let next_id = AtomicU16::new(1);
    loop {
        let (stream, peer) = tokio::select! {
            _ = shutdown.changed() => break,
            res = listener.accept() => match res {
                Ok(conn) => conn,
                Err(_) => break,
            },
        };
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let conns_tx = conns_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = pump_connection(stream, id, conns_tx).await {
                eprintln!("connection {peer} ({id}) ended: {e}");
            }
        });
    }

    // Let the app drain: closing the conns channel is not required (the app
    // exits on the shutdown watch), but joining guarantees every queued
    // persistence job is enqueued before the caller flushes.
    drop(conns_tx);
    let _ = app_thread.join();
    Ok(())
}

/// The per-connection pump: WS handshake, then decode incoming frames into the
/// app's inbox and flush the app's outbox back to the socket. Holds no game
/// state; dropping the inbox sender on exit is the disconnect signal.
async fn pump_connection(
    stream: TcpStream,
    id: u16,
    conns: mpsc::UnboundedSender<NewConn>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<ClientMsg>();
    if conns.send(NewConn { id, inbox: in_rx, outbox: out_tx }).is_err() {
        return Ok(()); // server is shutting down
    }

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(encode(&msg))).await.is_err() {
                break;
            }
        }
    });

    let mut result = Ok(());
    while let Some(frame) = ws_rx.next().await {
        match frame {
            Ok(Message::Binary(b)) => {
                if let Some(msg) = decode::<ClientMsg>(b.as_ref())
                    && in_tx.send(msg).is_err()
                {
                    break; // app gone (shutdown)
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                result = Err(e.into());
                break;
            }
        }
    }

    writer.abort();
    // `in_tx` drops here; the app notices the closed inbox and despawns the
    // player next tick.
    result
}

/// Answer LAN discovery probes while `enabled` says on. When on, binds a UDP
/// socket on `udp_port` (normally [`DISCOVERY_PORT`]) and, for each datagram
/// matching [`PROBE_MAGIC`], replies (unicast, to the sender) with
/// `PROBE_MAGIC` + bincode([`ServerInfo`]). When toggled off, the socket is
/// dropped so the host stops answering (and being visible) immediately. The
/// actually-bound port is published on `port_tx` (`None` while off or if the
/// bind failed — e.g. a second server on the same host; the game listener
/// still runs, and a later re-toggle retries the bind).
async fn discovery_supervisor(
    udp_port: u16,
    game_port: u16,
    player_count: Arc<AtomicU16>,
    name: String,
    mut enabled: watch::Receiver<bool>,
    port_tx: watch::Sender<Option<u16>>,
) {
    loop {
        while !*enabled.borrow() {
            if enabled.changed().await.is_err() {
                return; // server handle gone; nothing can re-enable us
            }
        }
        let sock = match UdpSocket::bind(("0.0.0.0", udp_port)).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("discovery disabled (could not bind UDP {udp_port}): {e}");
                let _ = port_tx.send(None);
                match enabled.changed().await {
                    Ok(()) => continue, // retry on the next toggle
                    Err(_) => return,
                }
            }
        };
        let bound = match sock.local_addr() {
            Ok(a) => a.port(),
            Err(_) => udp_port,
        };
        println!("discovery responder listening on udp/{bound}");
        let _ = port_tx.send(Some(bound));
        let mut buf = [0u8; 64];
        loop {
            let (n, src) = tokio::select! {
                changed = enabled.changed() => match changed {
                    Ok(()) if *enabled.borrow() => continue,
                    Ok(()) => break, // toggled off: drop the socket
                    Err(_) => return,
                },
                res = sock.recv_from(&mut buf) => match res {
                    Ok(v) => v,
                    Err(_) => continue,
                },
            };
            if &buf[..n] != PROBE_MAGIC {
                continue;
            }
            let info = ServerInfo {
                name: name.clone(),
                game_port,
                players: player_count.load(Ordering::Relaxed),
            };
            let mut pkt = PROBE_MAGIC.to_vec();
            pkt.extend(encode(&info));
            let _ = sock.send_to(&pkt, src).await;
        }
        println!("discovery responder stopped");
        let _ = port_tx.send(None);
    }
}
