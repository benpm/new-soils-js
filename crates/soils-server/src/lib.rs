//! Headless authoritative server for the new-soils Rust port, usable both as
//! the dedicated `soils-server` binary ([`run`]) and embedded in the client for
//! single-player ([`spawn`], which runs the server on its own thread/runtime
//! bound to a loopback ephemeral port).
//!
//! Listens for WebSocket clients, streams generated chunks on request, applies
//! and broadcasts block edits, ticks the day/night clock, and supports multiple
//! named worlds (clients can `Warp` between them). This is the Rust counterpart
//! to `server.js`, trimmed to what the slice needs (no MySQL, no schemapack).

mod auth;
mod persist;
mod region;
mod world;

use persist::{PersistHandle, Persister};

use auth::Accounts;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU16, Ordering},
};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use glam::IVec3;
use soils_protocol::{
    ActorState, ChunkData, ClientMsg, DISCOVERY_PORT, PROBE_MAGIC, ServerInfo, ServerMsg, decode,
    encode,
};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use world::World;

/// Real seconds for a full day cycle (JS used ~20 minutes; shortened so the
/// effect is visible while testing the slice).
const DAY_SECONDS: f32 = 120.0;
/// How often to broadcast actor positions.
const ACTOR_TICK: Duration = Duration::from_millis(100);
/// Chunks per `Bundle` response. Small because solid chunks are ~32 KB each.
const BUNDLE_SIZE: usize = 16;
/// Chunks generated per wave. A fresh world's first request is up to 9³=729
/// chunks; splitting it into nearest-first waves (generated in parallel on the
/// blocking pool, with an `.await` between waves) lets the near ring stream to
/// the client while the outer rings are still generating.
const WAVE_SIZE: usize = 48;
/// The world every client starts in.
const DEFAULT_WORLD: &str = "default";
/// Max accepted movement between two `Move` updates (world units). Generous —
/// well above sprint-fly + lag spikes (~32 u/s, sent every 50 ms) — so it only
/// catches gross teleport/speed hacks, not legitimate play.
const MAX_STEP: f32 = 64.0;

type SharedWorld = Arc<Mutex<World>>;
/// Named worlds, created on first use.
type Worlds = Arc<Mutex<HashMap<String, SharedWorld>>>;
/// Outgoing broadcast: `(sender_id, world, message)`. The sender is excluded so
/// an editor doesn't receive an echo of its own edit; `world == "*"` targets all
/// clients (used for the global clock), otherwise only same-world clients.
type Broadcast = broadcast::Sender<(u16, String, ServerMsg)>;
/// Each connected player's current world + latest reported state.
type Players = Arc<Mutex<HashMap<u16, PlayerEntry>>>;
/// Shared day/night clock (worlds share one clock, as the JS default did).
type Clock = Arc<Mutex<f32>>;

/// Target for messages sent to everyone regardless of world.
const ALL_WORLDS: &str = "*";

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

/// Handle to an embedded server running on its own detached thread/runtime.
/// Dropping it does NOT stop the server; call [`shutdown`](Self::shutdown) or
/// let process exit tear it down.
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    discovery: watch::Sender<bool>,
    discovery_port: watch::Receiver<Option<u16>>,
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

    /// Ask the accept loop to stop. Existing connections and background tasks
    /// die when the server runtime is dropped.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// Everything the background tasks and connection handlers share.
struct ServerState {
    worlds: Worlds,
    players: Players,
    clock: Clock,
    bcast: Broadcast,
    accounts: Arc<Accounts>,
    next_id: AtomicU16,
    data_dir: PathBuf,
    /// Enqueues chunk saves onto the background writer thread (owned by the
    /// caller of [`serve`], so it can be flushed/joined on shutdown).
    persist: PersistHandle,
}

impl ServerState {
    fn new(data_dir: PathBuf, persist: PersistHandle) -> Arc<Self> {
        let (bcast, _) = broadcast::channel::<(u16, String, ServerMsg)>(1024);
        let state = Arc::new(Self {
            worlds: Arc::new(Mutex::new(HashMap::new())),
            players: Arc::new(Mutex::new(HashMap::new())),
            clock: Arc::new(Mutex::new(0.0)),
            bcast,
            accounts: Arc::new(Accounts::load(&data_dir)),
            next_id: AtomicU16::new(1),
            data_dir,
            persist,
        });
        // Pre-create the default world so it's ready before the first client.
        state.get_world(DEFAULT_WORLD);
        state
    }

    /// Fetch a world by name, creating (opening) it on first request.
    fn get_world(&self, name: &str) -> SharedWorld {
        self.worlds
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| {
                let world = World::new(&self.data_dir, name, world_seed(name), self.persist.clone());
                Arc::new(Mutex::new(world))
            })
            .clone()
    }
}

#[derive(Clone)]
struct PlayerEntry {
    world: String,
    state: ActorState,
}

/// Deterministic per-world seed; the default world keeps seed 0 so its terrain
/// (and any persisted data) is unchanged.
fn world_seed(name: &str) -> u32 {
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
    let state = ServerState::new(config.data_dir.clone(), persister.handle());
    // Never-firing shutdown/discovery senders: they stay alive in this frame
    // for the whole await, so `changed()` pends forever and the initial
    // discovery state holds until process exit.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (discovery_tx, discovery_rx) = watch::channel(config.enable_discovery);
    let (discovery_port_tx, _discovery_port_rx) = watch::channel(None);
    let result = serve(listener, config, state, shutdown_rx, discovery_rx, discovery_port_tx).await;
    // Flush any queued chunk writes to disk before returning.
    persister.shutdown();
    drop(shutdown_tx);
    drop(discovery_tx);
    result
}

/// Start a server on a dedicated background thread with its own tokio runtime.
/// Blocks only until the TCP bind has completed, then returns the handle with
/// the real bound address. Used by the client for single-player.
pub fn spawn(config: ServerConfig) -> std::io::Result<ServerHandle> {
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<SocketAddr>>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (discovery_tx, discovery_rx) = watch::channel(config.enable_discovery);
    let (discovery_port_tx, discovery_port_rx) = watch::channel(None);
    std::thread::Builder::new()
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
                let state = ServerState::new(config.data_dir.clone(), persister.handle());
                let _ =
                    serve(listener, config, state, shutdown_rx, discovery_rx, discovery_port_tx)
                        .await;
                // Flush queued chunk writes before the runtime thread exits.
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
    })
}

/// Run the background tasks and the accept loop until `shutdown` fires (or
/// forever, for the dedicated binary).
async fn serve(
    listener: TcpListener,
    config: ServerConfig,
    state: Arc<ServerState>,
    mut shutdown: watch::Receiver<bool>,
    discovery: watch::Receiver<bool>,
    discovery_port_tx: watch::Sender<Option<u16>>,
) -> std::io::Result<()> {
    // Day/night clock: advance and broadcast time of day every second (global).
    {
        let bcast = state.bcast.clone();
        let clock = state.clock.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let daytime = {
                    let mut t = clock.lock().unwrap();
                    *t = (*t + 1.0 / DAY_SECONDS) % 1.0;
                    *t
                };
                let _ = bcast.send((0, ALL_WORLDS.to_string(), ServerMsg::Time { daytime }));
            }
        });
    }

    // Actor sync: broadcast positions a few times a second, grouped by world so
    // players only see others in the same world.
    {
        let players = state.players.clone();
        let bcast = state.bcast.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(ACTOR_TICK);
            loop {
                interval.tick().await;
                let mut by_world: HashMap<String, Vec<ActorState>> = HashMap::new();
                for entry in players.lock().unwrap().values() {
                    by_world.entry(entry.world.clone()).or_default().push(entry.state.clone());
                }
                for (world, actors) in by_world {
                    let _ = bcast.send((0, world, ServerMsg::ActorUpdate { actors }));
                }
            }
        });
    }

    // LAN discovery supervisor: runs the UDP probe responder while the
    // `discovery` watch says on, releases the socket while off. Advertises the
    // actually-bound game port (matters when binding port 0).
    {
        let players = state.players.clone();
        let game_port = listener.local_addr()?.port();
        tokio::spawn(discovery_supervisor(
            config.discovery_port,
            game_port,
            players,
            config.name.clone(),
            discovery,
            discovery_port_tx,
        ));
    }

    loop {
        let (stream, peer) = tokio::select! {
            _ = shutdown.changed() => break,
            res = listener.accept() => match res {
                Ok(conn) => conn,
                Err(_) => break,
            },
        };
        let id = state.next_id.fetch_add(1, Ordering::Relaxed);
        let state = state.clone();
        tokio::spawn(async move {
            let world_name = {
                if let Err(e) = handle_connection(stream, id, &state).await {
                    eprintln!("connection {peer} ({id}) ended: {e}");
                }
                state.players.lock().unwrap().remove(&id).map(|e| e.world)
            };
            // Tell same-world clients the actor is gone.
            if let Some(world) = world_name {
                let _ = state.bcast.send((id, world, ServerMsg::ActorRemove { id }));
            }
        });
    }
    Ok(())
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
    players: Players,
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
                players: players.lock().unwrap().len() as u16,
            };
            let mut pkt = PROBE_MAGIC.to_vec();
            pkt.extend(encode(&info));
            let _ = sock.send_to(&pkt, src).await;
        }
        println!("discovery responder stopped");
        let _ = port_tx.send(None);
    }
}

async fn handle_connection(
    stream: TcpStream,
    id: u16,
    state: &ServerState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    // The client's current world, shared with the broadcast forwarder so it can
    // filter messages to the right world.
    let current_world = Arc::new(Mutex::new(DEFAULT_WORLD.to_string()));
    let mut world = state.get_world(DEFAULT_WORLD);
    // Only authenticated connections may stream/edit/move.
    let mut authenticated = false;

    // Per-connection outgoing queue, drained by a single writer task.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(encode(&msg).into())).await.is_err() {
                break;
            }
        }
    });

    // Forward broadcasts to this client, filtered by world (and skipping self).
    let mut bcast_rx = state.bcast.subscribe();
    let fwd_tx = out_tx.clone();
    let fwd_world = current_world.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok((sender, world, msg)) = bcast_rx.recv().await {
            if sender == id {
                continue;
            }
            let here = world == ALL_WORLDS || world == *fwd_world.lock().unwrap();
            if here && fwd_tx.send(msg).is_err() {
                break;
            }
        }
    });

    while let Some(frame) = ws_rx.next().await {
        let data = match frame? {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => continue,
        };
        let Some(msg) = decode::<ClientMsg>(data.as_ref()) else { continue };

        // Reject everything until the connection has authenticated.
        if !authenticated && !matches!(msg, ClientMsg::Login { .. }) {
            continue;
        }

        match msg {
            ClientMsg::Login { name, password, signup } => {
                if let Err(reason) = state.accounts.authenticate(&name, &password, signup) {
                    println!("login denied: {name} (id {id}): {reason}");
                    let _ = out_tx.send(ServerMsg::LoginError { message: reason });
                    continue;
                }
                println!("login: {name} (id {id})");
                authenticated = true;
                let spawn = world.lock().unwrap().spawn;
                let seed = world.lock().unwrap().seed;
                let daytime = *state.clock.lock().unwrap();
                state.players.lock().unwrap().insert(
                    id,
                    PlayerEntry {
                        world: current_world.lock().unwrap().clone(),
                        state: ActorState { id, pos: spawn, velocity: [0.0; 3] },
                    },
                );
                let _ = out_tx.send(ServerMsg::Init { id, spawn, seed, daytime });
            }
            ClientMsg::ReqChunks { positions } => {
                // Serve chunks in nearest-first waves (the client already sorts
                // `positions` nearest-first). Each wave is generated off the
                // async runtime on the blocking pool — `get_or_generate_batch`
                // fans the missing chunks across all cores — and the `.await`
                // between waves lets the writer flush earlier waves while later
                // ones generate. So the ring around the player appears almost
                // immediately instead of after the whole (up to 729-chunk) burst.
                for wave in positions.chunks(WAVE_SIZE) {
                    let wave: Vec<[i32; 3]> = wave.to_vec();
                    let world = world.clone();
                    let results = tokio::task::spawn_blocking(move || {
                        let cpositions: Vec<IVec3> =
                            wave.iter().map(|p| IVec3::new(p[0], p[1], p[2])).collect();
                        world.lock().unwrap().get_or_generate_batch(&cpositions)
                    })
                    .await;
                    let results = match results {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("worldgen task failed: {e}");
                            continue;
                        }
                    };
                    let mut batch: Vec<ChunkData> = Vec::with_capacity(BUNDLE_SIZE);
                    for (cpos, empty, voxels) in results {
                        batch.push(ChunkData { pos: [cpos.x, cpos.y, cpos.z], empty, voxels });
                        if batch.len() >= BUNDLE_SIZE {
                            let _ =
                                out_tx.send(ServerMsg::Bundle { chunks: std::mem::take(&mut batch) });
                        }
                    }
                    if !batch.is_empty() {
                        let _ = out_tx.send(ServerMsg::Bundle { chunks: batch });
                    }
                }
            }
            ClientMsg::Edit { pos, value } => {
                let applied = world.lock().unwrap().edit(pos[0], pos[1], pos[2], value);
                if applied {
                    let w = current_world.lock().unwrap().clone();
                    let _ = state.bcast.send((id, w, ServerMsg::Edit { pos, value }));
                }
            }
            ClientMsg::Move { pos, velocity } => {
                // Server authority: reject implausible jumps (teleport/speed
                // hacks) and snap the client back to its last accepted position.
                let mut g = state.players.lock().unwrap();
                if let Some(entry) = g.get_mut(&id) {
                    let last = entry.state.pos;
                    let d2 = (pos[0] - last[0]).powi(2)
                        + (pos[1] - last[1]).powi(2)
                        + (pos[2] - last[2]).powi(2);
                    if d2 > MAX_STEP * MAX_STEP {
                        drop(g);
                        let _ = out_tx.send(ServerMsg::Position { pos: last });
                    } else {
                        entry.state.pos = pos;
                        entry.state.velocity = velocity;
                    }
                }
            }
            ClientMsg::Warp { world: name } => {
                println!("warp: id {id} -> {name}");
                // Leaving the old world: tell its clients the actor is gone.
                let old = current_world.lock().unwrap().clone();
                let _ = state.bcast.send((id, old, ServerMsg::ActorRemove { id }));

                world = state.get_world(&name);
                *current_world.lock().unwrap() = name.clone();
                let spawn = world.lock().unwrap().spawn;
                if let Some(entry) = state.players.lock().unwrap().get_mut(&id) {
                    entry.world = name;
                    entry.state.pos = spawn;
                }
                let daytime = *state.clock.lock().unwrap();
                let _ = out_tx.send(ServerMsg::Warp { spawn, daytime });
            }
        }
    }

    forwarder.abort();
    writer.abort();
    Ok(())
}
