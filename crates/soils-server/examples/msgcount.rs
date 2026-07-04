//! Diagnostic: connect to a running server, log in, send Move at the client's
//! real cadence, and count message types per second.
//!
//!   cargo run -p soils-server --example msgcount

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ClientMsg, ServerMsg, decode, encode};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let (ws, _) = tokio_tungstenite::connect_async("ws://127.0.0.1:9001").await.expect("connect");
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::Binary(encode(&ClientMsg::Login {
        name: "msgcount".into(),
        password: String::new(),
        signup: true,
    })))
    .await
    .unwrap();

    let mover = tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(50));
        loop {
            iv.tick().await;
            let m = ClientMsg::Move { pos: [282.0, 285.0, 268.0], velocity: [0.0; 3] };
            if tx.send(Message::Binary(encode(&m))).await.is_err() {
                break;
            }
        }
    });

    let mut counts: std::collections::HashMap<&'static str, u32> = Default::default();
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs_f32() < 3.0 {
        let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(std::time::Duration::from_millis(500), rx.next()).await
        else {
            continue;
        };
        let name = match decode::<ServerMsg>(b.as_ref()) {
            Some(ServerMsg::Init { .. }) => "Init",
            Some(ServerMsg::Time { .. }) => "Time",
            Some(ServerMsg::ActorUpdate { .. }) => "ActorUpdate",
            Some(ServerMsg::ActorRemove { .. }) => "ActorRemove",
            Some(ServerMsg::Position { .. }) => "Position",
            Some(ServerMsg::Bundle { .. }) => "Bundle",
            Some(ServerMsg::Chunk { .. }) => "Chunk",
            Some(ServerMsg::Edit { .. }) => "Edit",
            _ => "other",
        };
        *counts.entry(name).or_default() += 1;
    }
    mover.abort();
    println!("over 3 s: {counts:?}");
}
