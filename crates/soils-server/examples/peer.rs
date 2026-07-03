//! A minimal headless peer: logs in and parks at a fixed position, sending Move
//! updates so the server advertises it as an actor to other clients. Used to
//! verify actor rendering without running a second full Bevy client.
//!
//!   cargo run -p soils-server --example peer

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ClientMsg, encode};
use tokio_tungstenite::tungstenite::Message;

const POS: [f32; 3] = [282.0, 285.0, 268.0];

#[tokio::main]
async fn main() {
    let (ws, _) = tokio_tungstenite::connect_async("ws://127.0.0.1:9001").await.expect("connect");
    let (mut tx, mut rx) = ws.split();

    // Drain and discard everything the server sends us.
    tokio::spawn(async move { while rx.next().await.is_some() {} });

    tx.send(Message::Binary(
        encode(&ClientMsg::Login { name: "peer".into(), password: String::new(), signup: true }).into(),
    ))
    .await
    .unwrap();
    println!("peer logged in, holding position {POS:?}");

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        interval.tick().await;
        let msg = ClientMsg::Move { pos: POS, velocity: [0.0; 3] };
        if tx.send(Message::Binary(encode(&msg).into())).await.is_err() {
            break;
        }
    }
}
