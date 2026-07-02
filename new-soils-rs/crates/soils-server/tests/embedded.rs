//! End-to-end check of the embedded (single-player) server path: `spawn` on an
//! ephemeral loopback port, then run the same connect → login → chunk-request
//! handshake the client performs.

use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ClientMsg, ServerMsg, decode, encode};
use soils_server::ServerConfig;
use tokio_tungstenite::tungstenite::Message;

#[test]
fn spawn_login_and_stream_chunks() {
    let data_dir = std::env::temp_dir().join(format!("soils-embedded-test-{}", std::process::id()));
    let handle = soils_server::spawn(ServerConfig {
        bind: "127.0.0.1:0".into(),
        data_dir: data_dir.clone(),
        enable_discovery: false,
        name: "test".into(),
    })
    .expect("spawn embedded server");
    assert_ne!(handle.port(), 0, "ephemeral port must be resolved");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let url = format!("ws://{}", handle.addr());
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

        // Pre-auth messages must be ignored, then Login (signup) must Init.
        let req = ClientMsg::ReqChunks { positions: vec![[17, 17, 16]] };
        ws.send(Message::Binary(encode(&req).into())).await.unwrap();
        let login =
            ClientMsg::Login { name: "Player".into(), password: String::new(), signup: true };
        ws.send(Message::Binary(encode(&login).into())).await.unwrap();

        loop {
            match next_msg(&mut ws).await {
                ServerMsg::Init { .. } => break,
                ServerMsg::LoginError { message } => panic!("login failed: {message}"),
                _ => continue, // Time/ActorUpdate broadcasts interleave.
            }
        }

        // Chunk streaming works and persists under the configured data dir.
        ws.send(Message::Binary(encode(&req).into())).await.unwrap();
        loop {
            match next_msg(&mut ws).await {
                ServerMsg::Bundle { chunks } => {
                    assert_eq!(chunks[0].pos, [17, 17, 16]);
                    break;
                }
                _ => continue, // Time/ActorUpdate broadcasts interleave.
            }
        }
    });

    assert!(data_dir.join("accounts.bin").is_file());
    assert!(data_dir.join("worlds").join("default").join("regions").is_dir());
    handle.shutdown();
    let _ = std::fs::remove_dir_all(&data_dir);
}

async fn next_msg(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> ServerMsg {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("timed out waiting for server message")
            .expect("connection closed")
            .expect("websocket error");
        if let Message::Binary(b) = frame {
            if let Some(msg) = decode::<ServerMsg>(b.as_ref()) {
                return msg;
            }
        }
    }
}
