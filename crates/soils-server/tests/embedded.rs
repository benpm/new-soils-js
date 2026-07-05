//! End-to-end check of the embedded (single-player) server path: `spawn` on an
//! ephemeral loopback port, then run the same connect → login → chunk-request
//! handshake the client performs.

mod common;

use common::{Client, TestServer, wait_until};
use soils_protocol::ClientMsg;
use std::time::Duration;

#[tokio::test]
async fn spawn_login_and_stream_chunks() {
    let server = TestServer::start("embedded");
    assert_ne!(server.handle.port(), 0, "ephemeral port must be resolved");

    // Pre-auth messages must be ignored, then Login (signup) must Init.
    let mut c = Client::connect(server.addr()).await;
    c.send(&ClientMsg::Edit { seq: 1, pos: [282, 280, 268], value: 0 }).await;
    c.login("Player").await;

    // Chunk streaming works: the join burst pushes the subscription around
    // the spawn without any request; a below-surface chunk has voxels.
    let vol = c.await_chunk([8, 7, 8]).await;
    assert!(!vol.is_empty(), "below-surface chunk should have voxels");

    // An in-reach edit is validated and applied (persisted via the dirty
    // flush; the join burst's generated chunks persist right away).
    let seq = c.edit([282, 280, 268], 3).await;
    c.recv_until(|msg| match msg {
        soils_protocol::ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(()),
        _ => None,
    })
    .await;

    assert!(server.data_dir.join("accounts.bin").is_file());
    // Generated + edited chunks persist ASYNCHRONOUSLY on the background
    // writer, so poll for the region file rather than asserting immediately.
    let regions = server.data_dir.join("worlds").join("default").join("regions");
    assert!(
        wait_until(|| regions.is_dir(), Duration::from_secs(3)),
        "generated/edited chunks should persist to a region file"
    );
}
