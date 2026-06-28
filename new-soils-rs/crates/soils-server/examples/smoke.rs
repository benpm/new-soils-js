//! End-to-end smoke test against a running server: signs up / logs in, requests
//! the chunks around spawn, verifies real terrain, then warps to another world
//! and checks it streams a different (still solid) world.
//!
//! Run the server first (`cargo run -p soils-server`), then:
//!   cargo run -p soils-server --example smoke

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use soils_protocol::{CHUNK_BIT, ClientMsg, ServerMsg, decode, encode};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Tx = SplitSink<Ws, Message>;
type Rx = SplitStream<Ws>;

async fn send(tx: &mut Tx, msg: ClientMsg) {
    tx.send(Message::Binary(encode(&msg).into())).await.unwrap();
}

/// Request the 3x3x3 cube of chunks around a spawn and count solid voxels,
/// accepting both `Bundle` and single `Chunk` responses.
async fn stream_around(tx: &mut Tx, rx: &mut Rx, spawn: [f32; 3]) -> (usize, usize) {
    let sc = [
        (spawn[0] as i32) >> CHUNK_BIT,
        (spawn[1] as i32) >> CHUNK_BIT,
        (spawn[2] as i32) >> CHUNK_BIT,
    ];
    let mut positions = Vec::new();
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                positions.push([sc[0] + dx, sc[1] + dy, sc[2] + dz]);
            }
        }
    }
    let want = positions.len();
    send(tx, ClientMsg::ReqChunks { positions }).await;

    let mut received = 0;
    let mut solid = 0usize;
    while received < want {
        let Some(Ok(Message::Binary(b))) = rx.next().await else { break };
        match decode(b.as_ref()) {
            Some(ServerMsg::Bundle { chunks }) => {
                for c in chunks {
                    received += 1;
                    solid += c.voxels.iter().filter(|&&v| v != 0).count();
                }
            }
            Some(ServerMsg::Chunk { empty, voxels, .. }) => {
                received += 1;
                if !empty {
                    solid += voxels.iter().filter(|&&v| v != 0).count();
                }
            }
            _ => {} // ignore Time / ActorUpdate while streaming
        }
    }
    (received, solid)
}

#[tokio::main]
async fn main() {
    let (ws, _) = tokio_tungstenite::connect_async("ws://127.0.0.1:9001")
        .await
        .expect("connect to server (is it running?)");
    let (mut tx, mut rx) = ws.split();

    // Sign up (idempotent for matching credentials, so this also "logs in").
    send(&mut tx, ClientMsg::Login { name: "smoke".into(), password: "pw".into(), signup: true }).await;
    let mut spawn = [0.0f32; 3];
    while let Some(Ok(Message::Binary(b))) = rx.next().await {
        match decode(b.as_ref()) {
            Some(ServerMsg::Init { id, spawn: s, seed, daytime }) => {
                println!("Init: id={id} spawn={s:?} seed={seed} daytime={daytime}");
                spawn = s;
                break;
            }
            Some(ServerMsg::LoginError { message }) => panic!("login failed: {message}"),
            _ => {}
        }
    }

    // Default world.
    let (received, solid) = stream_around(&mut tx, &mut rx, spawn).await;
    println!("default world: received {received} chunks, {solid} solid voxels");
    assert_eq!(received, 27, "did not receive all default-world chunks");
    assert!(solid > 0, "expected terrain near spawn in the default world");

    // Warp to a fresh world and stream it.
    send(&mut tx, ClientMsg::Warp { world: "smoke-nether".into() }).await;
    let mut warp_spawn = spawn;
    while let Some(Ok(Message::Binary(b))) = rx.next().await {
        if let Some(ServerMsg::Warp { spawn: s, daytime }) = decode(b.as_ref()) {
            println!("Warp: spawn={s:?} daytime={daytime}");
            warp_spawn = s;
            break;
        }
    }
    let (wreceived, wsolid) = stream_around(&mut tx, &mut rx, warp_spawn).await;
    println!("warped world: received {wreceived} chunks, {wsolid} solid voxels");
    assert_eq!(wreceived, 27, "did not receive all warped-world chunks");
    assert!(wsolid > 0, "expected terrain in the warped world");
    assert_ne!(solid, wsolid, "warped world has identical terrain to default (seed not applied?)");

    println!("SMOKE TEST PASSED (auth + bundle streaming + warp)");
}
