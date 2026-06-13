//! End-to-end smoke test against a running server: logs in, requests the chunks
//! around spawn, and verifies the server generates real terrain.
//!
//! Run the server first (`cargo run -p soils-server`), then:
//!   cargo run -p soils-server --example smoke

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{CHUNK_BIT, ClientMsg, ServerMsg, decode, encode};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let (ws, _) = tokio_tungstenite::connect_async("ws://127.0.0.1:9001")
        .await
        .expect("connect to server (is it running?)");
    let (mut tx, mut rx) = ws.split();

    // Log in and read the Init reply.
    tx.send(Message::Binary(encode(&ClientMsg::Login { name: "smoke".into() }).into()))
        .await
        .unwrap();

    let mut spawn = [0.0f32; 3];
    while let Some(Ok(Message::Binary(b))) = rx.next().await {
        if let Some(ServerMsg::Init { id, spawn: s, seed, daytime }) = decode(b.as_ref()) {
            println!("Init: id={id} spawn={s:?} seed={seed} daytime={daytime}");
            spawn = s;
            break;
        }
    }

    // Request the 3x3x3 cube of chunks around spawn.
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
    tx.send(Message::Binary(encode(&ClientMsg::ReqChunks { positions }).into())).await.unwrap();

    let mut received = 0;
    let mut non_empty = 0;
    let mut total_solid_bytes = 0usize;
    while received < want {
        match rx.next().await {
            Some(Ok(Message::Binary(b))) => {
                if let Some(ServerMsg::Chunk { pos, empty, voxels }) = decode(b.as_ref()) {
                    received += 1;
                    if !empty {
                        non_empty += 1;
                        let solid = voxels.iter().filter(|&&v| v != 0).count();
                        total_solid_bytes += solid;
                        println!("chunk {pos:?}: {solid} solid voxels");
                    }
                }
            }
            _ => break,
        }
    }

    println!("\nreceived {received}/{want} chunks, {non_empty} non-empty, {total_solid_bytes} solid voxels total");
    assert_eq!(received, want, "did not receive all requested chunks");
    assert!(non_empty > 0, "expected terrain near spawn, got only empty chunks");
    println!("SMOKE TEST PASSED");
}
