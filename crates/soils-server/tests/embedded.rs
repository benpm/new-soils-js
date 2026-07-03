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
        ..ServerConfig::default()
    })
    .expect("spawn embedded server");
    assert_ne!(handle.port(), 0, "ephemeral port must be resolved");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let url = format!("ws://{}", handle.addr());
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

        // A below-surface chunk, so generation produces real (non-empty) voxels.
        let req = ClientMsg::ReqChunks { positions: vec![[8, 7, 8]] };
        // Pre-auth messages must be ignored, then Login (signup) must Init.
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

        // Chunk streaming works: the requested chunk comes back with voxels.
        ws.send(Message::Binary(encode(&req).into())).await.unwrap();
        loop {
            match next_msg(&mut ws).await {
                ServerMsg::Bundle { chunks } => {
                    assert_eq!(chunks[0].pos, [8, 7, 8]);
                    assert!(!chunks[0].empty, "below-surface chunk should have voxels");
                    break;
                }
                _ => continue, // Time/ActorUpdate broadcasts interleave.
            }
        }

        // Edit a voxel in the now-loaded chunk; the edit is enqueued for
        // background persistence. Round-trip another request to keep the
        // connection lively while the writer flushes.
        let edit = ClientMsg::Edit { pos: [8 * 32, 7 * 32, 8 * 32], value: 0 };
        ws.send(Message::Binary(encode(&edit).into())).await.unwrap();
        ws.send(Message::Binary(encode(&req).into())).await.unwrap();
        loop {
            match next_msg(&mut ws).await {
                ServerMsg::Bundle { .. } => break,
                _ => continue,
            }
        }
    });

    assert!(data_dir.join("accounts.bin").is_file());
    // Generated + edited chunks persist ASYNCHRONOUSLY on the background writer,
    // so poll for the region file to appear rather than asserting immediately.
    let regions = data_dir.join("worlds").join("default").join("regions");
    assert!(
        wait_until(|| regions.is_dir(), std::time::Duration::from_secs(3)),
        "generated/edited chunks should persist to a region file"
    );
    handle.shutdown();
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// Poll `cond` until it holds or `timeout` elapses. Used because chunk
/// persistence is asynchronous (a background writer thread).
fn wait_until(mut cond: impl FnMut() -> bool, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    cond()
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
