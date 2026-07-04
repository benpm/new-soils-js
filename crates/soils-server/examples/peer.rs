//! A minimal headless peer: logs in and parks at the spawn point (the server
//! owns positions since the input-authority rework, so an idle peer just
//! stays put and is advertised to other clients). Used to verify actor
//! rendering without running a second full Bevy client.
//!
//!   cargo run -p soils-server --example peer

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ClientMsg, encode};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let (ws, _) = tokio_tungstenite::connect_async("ws://127.0.0.1:9001").await.expect("connect");
    let (mut tx, mut rx) = ws.split();

    tx.send(Message::Binary(encode(&ClientMsg::Login {
        name: "peer".into(),
        password: String::new(),
        signup: true,
    })))
    .await
    .unwrap();
    println!("peer logged in, parked at spawn (ctrl-C to leave)");

    // Drain forever; the connection staying open keeps the actor alive.
    while rx.next().await.is_some() {}
}
