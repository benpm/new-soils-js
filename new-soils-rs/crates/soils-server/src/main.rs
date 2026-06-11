//! Headless authoritative server for the new-soils Rust port.
//!
//! Listens for WebSocket clients, streams generated chunks on request, applies
//! and broadcasts block edits, and ticks the day/night clock. This is the Rust
//! counterpart to `server.js`, trimmed to what the vertical slice needs (no
//! MySQL auth, no region-file persistence, no schemapack).

mod region;
mod world;

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU16, Ordering},
};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use glam::IVec3;
use soils_protocol::{ActorState, ClientMsg, ServerMsg, decode, encode};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

use world::World;

const ADDR: &str = "127.0.0.1:9001";
/// Real seconds for a full day cycle (JS used ~20 minutes; shortened so the
/// effect is visible while testing the slice).
const DAY_SECONDS: f32 = 120.0;
/// How often to broadcast actor positions.
const ACTOR_TICK: Duration = Duration::from_millis(100);

type SharedWorld = Arc<Mutex<World>>;
/// Outgoing broadcast: `(sender_id, message)`. The sender is excluded so an
/// editor doesn't receive an echo of its own optimistic edit.
type Broadcast = broadcast::Sender<(u16, ServerMsg)>;
/// Connected players' latest reported state, keyed by connection id.
type Players = Arc<Mutex<HashMap<u16, ActorState>>>;

#[tokio::main]
async fn main() {
    let world: SharedWorld = Arc::new(Mutex::new(World::new(0)));
    let (bcast, _) = broadcast::channel::<(u16, ServerMsg)>(1024);
    let players: Players = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU16::new(1));

    // Day/night clock: advance and broadcast time of day every second.
    {
        let world = world.clone();
        let bcast = bcast.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let daytime = {
                    let mut w = world.lock().unwrap();
                    w.daytime = (w.daytime + 1.0 / DAY_SECONDS) % 1.0;
                    w.daytime
                };
                let _ = bcast.send((0, ServerMsg::Time { daytime }));
            }
        });
    }

    // Actor sync: broadcast everyone's position a few times a second. Each
    // client filters out its own id when rendering.
    {
        let players = players.clone();
        let bcast = bcast.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(ACTOR_TICK);
            loop {
                interval.tick().await;
                let actors: Vec<ActorState> = players.lock().unwrap().values().cloned().collect();
                if !actors.is_empty() {
                    let _ = bcast.send((0, ServerMsg::ActorUpdate { actors }));
                }
            }
        });
    }

    let listener = TcpListener::bind(ADDR).await.expect("bind");
    println!("new-soils server listening on ws://{ADDR}");

    while let Ok((stream, peer)) = listener.accept().await {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let world = world.clone();
        let bcast = bcast.clone();
        let players = players.clone();
        tokio::spawn(async move {
            let cleanup_bcast = bcast.clone();
            if let Err(e) = handle_connection(stream, id, world, bcast, players.clone()).await {
                eprintln!("connection {peer} ({id}) ended: {e}");
            }
            // Clean up on disconnect and tell everyone the actor is gone.
            players.lock().unwrap().remove(&id);
            let _ = cleanup_bcast.send((id, ServerMsg::ActorRemove { id }));
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    id: u16,
    world: SharedWorld,
    bcast: Broadcast,
    players: Players,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Per-connection outgoing queue, drained by a single writer task so the
    // request handler and the broadcast forwarder never write concurrently.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(encode(&msg).into())).await.is_err() {
                break;
            }
        }
    });

    // Forward broadcast messages (edits, time) to this client, skipping its own.
    let mut bcast_rx = bcast.subscribe();
    let fwd_tx = out_tx.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok((sender, msg)) = bcast_rx.recv().await {
            if sender != id && fwd_tx.send(msg).is_err() {
                break;
            }
        }
    });

    // Read and handle client messages.
    while let Some(frame) = ws_rx.next().await {
        let frame = frame?;
        let data = match frame {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => continue,
        };
        let Some(msg) = decode::<ClientMsg>(data.as_ref()) else { continue };

        match msg {
            ClientMsg::Login { name } => {
                println!("login: {name} (id {id})");
                let (spawn, seed, daytime) = {
                    let w = world.lock().unwrap();
                    (w.spawn, w.seed, w.daytime)
                };
                players
                    .lock()
                    .unwrap()
                    .insert(id, ActorState { id, pos: spawn, velocity: [0.0; 3] });
                let _ = out_tx.send(ServerMsg::Init { id, spawn, seed, daytime });
            }
            ClientMsg::ReqChunks { positions } => {
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
                    let _ = out_tx.send(ServerMsg::Chunk { pos: p, empty, voxels });
                }
            }
            ClientMsg::Edit { pos, value } => {
                let applied = {
                    let mut w = world.lock().unwrap();
                    w.edit(pos[0], pos[1], pos[2], value)
                };
                if applied {
                    let _ = bcast.send((id, ServerMsg::Edit { pos, value }));
                }
            }
            ClientMsg::Move { pos, velocity } => {
                if let Some(actor) = players.lock().unwrap().get_mut(&id) {
                    actor.pos = pos;
                    actor.velocity = velocity;
                }
            }
        }
    }

    forwarder.abort();
    writer.abort();
    Ok(())
}
