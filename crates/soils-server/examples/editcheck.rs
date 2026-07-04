//! Persistence check: `write` edits a known voxel; `verify` (after a server
//! restart) confirms the edit survived to disk.
//!
//!   cargo run -p soils-server --example editcheck -- write
//!   # restart the server
//!   cargo run -p soils-server --example editcheck -- verify

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ChunkVolume, ClientMsg, ServerMsg, decode, encode};
use tokio_tungstenite::tungstenite::Message;

// A voxel well below the surface (reliably solid), and the block we stamp there.
const VOXEL: [i32; 3] = [282, 200, 268];
const CHUNK: [i32; 3] = [8, 6, 8]; // VOXEL >> 5 per axis
const LOCAL: (i32, i32, i32) = (26, 8, 12); // VOXEL & 31 per axis
const STAMP: u8 = 5;

#[tokio::main]
async fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let (ws, _) = tokio_tungstenite::connect_async("ws://127.0.0.1:9001").await.expect("connect");
    let (mut tx, mut rx) = ws.split();

    tx.send(bin(&ClientMsg::Login { name: "editcheck".into(), password: String::new(), signup: true })).await.unwrap();
    // Drain until Init.
    while let Some(Ok(Message::Binary(b))) = rx.next().await {
        if matches!(decode::<ServerMsg>(b.as_ref()), Some(ServerMsg::Init { .. })) {
            break;
        }
    }

    // The join burst pushes the subscription around the spawn — the target
    // chunk is inside it, so just wait for it to stream in.
    let chunk = recv_chunk(&mut rx).await;

    match mode.as_str() {
        "write" => {
            assert_eq!(chunk.get(LOCAL.0, LOCAL.1, LOCAL.2) != STAMP, true, "stamp already present?");
            tx.send(bin(&ClientMsg::Edit { pos: VOXEL, value: STAMP })).await.unwrap();
            // Give the server a moment to apply + persist before we disconnect.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            println!("WROTE stamp {STAMP} at {VOXEL:?}");
        }
        "verify" => {
            let got = chunk.get(LOCAL.0, LOCAL.1, LOCAL.2);
            println!("verify: voxel {VOXEL:?} = {got} (expected {STAMP})");
            assert_eq!(got, STAMP, "edit did not persist across restart");
            println!("PERSISTENCE TEST PASSED");
        }
        _ => eprintln!("usage: editcheck -- [write|verify]"),
    }
}

fn bin(msg: &ClientMsg) -> Message {
    Message::Binary(encode(msg).into())
}

async fn recv_chunk<S>(rx: &mut S) -> ChunkVolume
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    // Drain the pushed stream until the target chunk appears.
    while let Some(Ok(Message::Binary(b))) = rx.next().await {
        let payload = match decode::<ServerMsg>(b.as_ref()) {
            Some(ServerMsg::Chunk { pos, payload }) if pos == CHUNK => payload,
            Some(ServerMsg::Bundle { chunks }) => {
                match chunks.into_iter().find(|c| c.pos == CHUNK) {
                    Some(c) => c.payload,
                    None => continue,
                }
            }
            _ => continue,
        };
        return soils_protocol::decode_chunk(&payload).expect("chunk payload decodes");
    }
    panic!("server closed before sending the chunk");
}
