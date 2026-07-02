//! Headless authoritative server for the new-soils Rust port.
//!
//! Listens for WebSocket clients, streams generated chunks on request, applies
//! and broadcasts block edits, ticks the day/night clock, and supports multiple
//! named worlds (clients can `Warp` between them). This is the Rust counterpart
//! to `server.js`, trimmed to what the slice needs (no MySQL, no schemapack).

mod auth;
mod region;
mod world;

use auth::Accounts;

use std::collections::HashMap;
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
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

use world::World;

/// Default bind address: all interfaces, so the server is reachable over LAN
/// (and discoverable). Override with `SOILS_BIND` (e.g. `127.0.0.1:9001`).
const DEFAULT_BIND: &str = "0.0.0.0:9001";
/// TCP port the game listens on; advertised in discovery replies so clients
/// know where to dial.
const GAME_PORT: u16 = 9001;
/// Real seconds for a full day cycle (JS used ~20 minutes; shortened so the
/// effect is visible while testing the slice).
const DAY_SECONDS: f32 = 120.0;
/// How often to broadcast actor positions.
const ACTOR_TICK: Duration = Duration::from_millis(100);
/// Chunks per `Bundle` response. Small because solid chunks are ~32 KB each.
const BUNDLE_SIZE: usize = 16;
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

/// Fetch a world by name, creating (opening) it on first request.
fn get_world(worlds: &Worlds, name: &str) -> SharedWorld {
    worlds
        .lock()
        .unwrap()
        .entry(name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(World::new(name, world_seed(name)))))
        .clone()
}

#[tokio::main]
async fn main() {
    let worlds: Worlds = Arc::new(Mutex::new(HashMap::new()));
    // Pre-create the default world so it's ready before the first client.
    get_world(&worlds, DEFAULT_WORLD);
    let accounts = Arc::new(Accounts::load());
    let (bcast, _) = broadcast::channel::<(u16, String, ServerMsg)>(1024);
    let players: Players = Arc::new(Mutex::new(HashMap::new()));
    let clock: Clock = Arc::new(Mutex::new(0.0));
    let next_id = Arc::new(AtomicU16::new(1));

    // Day/night clock: advance and broadcast time of day every second (global).
    {
        let bcast = bcast.clone();
        let clock = clock.clone();
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
        let players = players.clone();
        let bcast = bcast.clone();
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

    // LAN discovery responder: replies to UDP probes so clients can find us.
    {
        let players = players.clone();
        let name = std::env::var("SOILS_NAME").unwrap_or_else(|_| "new-soils".into());
        tokio::spawn(discovery_responder(GAME_PORT, players, name));
    }

    let bind = std::env::var("SOILS_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());
    let listener = TcpListener::bind(&bind).await.expect("bind");
    println!("new-soils server listening on ws://{bind}");

    while let Ok((stream, peer)) = listener.accept().await {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let worlds = worlds.clone();
        let bcast = bcast.clone();
        let players = players.clone();
        let clock = clock.clone();
        let accounts = accounts.clone();
        tokio::spawn(async move {
            let cleanup_bcast = bcast.clone();
            let world_name = {
                if let Err(e) =
                    handle_connection(stream, id, worlds, bcast, players.clone(), clock, accounts).await
                {
                    eprintln!("connection {peer} ({id}) ended: {e}");
                }
                players.lock().unwrap().remove(&id).map(|e| e.world)
            };
            // Tell same-world clients the actor is gone.
            if let Some(world) = world_name {
                let _ = cleanup_bcast.send((id, world, ServerMsg::ActorRemove { id }));
            }
        });
    }
}

/// Answer LAN discovery probes. Binds a UDP socket on [`DISCOVERY_PORT`] and,
/// for each datagram matching [`PROBE_MAGIC`], replies (unicast, to the sender)
/// with `PROBE_MAGIC` + bincode([`ServerInfo`]). If the port is unavailable
/// (e.g. a second server on the same host), discovery is simply disabled — the
/// game listener still runs.
async fn discovery_responder(game_port: u16, players: Players, name: String) {
    let sock = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("discovery disabled (could not bind UDP {DISCOVERY_PORT}): {e}");
            return;
        }
    };
    println!("discovery responder listening on udp/{DISCOVERY_PORT}");
    let mut buf = [0u8; 64];
    loop {
        let (n, src) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => continue,
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
}

async fn handle_connection(
    stream: TcpStream,
    id: u16,
    worlds: Worlds,
    bcast: Broadcast,
    players: Players,
    clock: Clock,
    accounts: Arc<Accounts>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    // The client's current world, shared with the broadcast forwarder so it can
    // filter messages to the right world.
    let current_world = Arc::new(Mutex::new(DEFAULT_WORLD.to_string()));
    let mut world = get_world(&worlds, DEFAULT_WORLD);
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
    let mut bcast_rx = bcast.subscribe();
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
                if let Err(reason) = accounts.authenticate(&name, &password, signup) {
                    println!("login denied: {name} (id {id}): {reason}");
                    let _ = out_tx.send(ServerMsg::LoginError { message: reason });
                    continue;
                }
                println!("login: {name} (id {id})");
                authenticated = true;
                let spawn = world.lock().unwrap().spawn;
                let seed = world.lock().unwrap().seed;
                let daytime = *clock.lock().unwrap();
                players.lock().unwrap().insert(
                    id,
                    PlayerEntry {
                        world: current_world.lock().unwrap().clone(),
                        state: ActorState { id, pos: spawn, velocity: [0.0; 3] },
                    },
                );
                let _ = out_tx.send(ServerMsg::Init { id, spawn, seed, daytime });
            }
            ClientMsg::ReqChunks { positions } => {
                let mut batch: Vec<ChunkData> = Vec::with_capacity(BUNDLE_SIZE);
                for p in positions {
                    let cpos = IVec3::new(p[0], p[1], p[2]);
                    let (empty, voxels) = {
                        let mut w = world.lock().unwrap();
                        let chunk = w.get_or_generate(cpos);
                        if chunk.is_empty() {
                            (true, Vec::new())
                        } else {
                            (false, chunk.as_bytes().to_vec())
                        }
                    };
                    batch.push(ChunkData { pos: p, empty, voxels });
                    if batch.len() >= BUNDLE_SIZE {
                        let _ = out_tx.send(ServerMsg::Bundle { chunks: std::mem::take(&mut batch) });
                    }
                }
                if !batch.is_empty() {
                    let _ = out_tx.send(ServerMsg::Bundle { chunks: batch });
                }
            }
            ClientMsg::Edit { pos, value } => {
                let applied = world.lock().unwrap().edit(pos[0], pos[1], pos[2], value);
                if applied {
                    let w = current_world.lock().unwrap().clone();
                    let _ = bcast.send((id, w, ServerMsg::Edit { pos, value }));
                }
            }
            ClientMsg::Move { pos, velocity } => {
                // Server authority: reject implausible jumps (teleport/speed
                // hacks) and snap the client back to its last accepted position.
                let mut g = players.lock().unwrap();
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
                let _ = bcast.send((id, old, ServerMsg::ActorRemove { id }));

                world = get_world(&worlds, &name);
                *current_world.lock().unwrap() = name.clone();
                let spawn = world.lock().unwrap().spawn;
                if let Some(entry) = players.lock().unwrap().get_mut(&id) {
                    entry.world = name;
                    entry.state.pos = spawn;
                }
                let daytime = *clock.lock().unwrap();
                let _ = out_tx.send(ServerMsg::Warp { spawn, daytime });
            }
        }
    }

    forwarder.abort();
    writer.abort();
    Ok(())
}
