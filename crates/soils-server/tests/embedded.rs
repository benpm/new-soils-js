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
    c.send(&ClientMsg::ReqChunks { positions: vec![[8, 7, 8]] }).await;
    c.login("Player").await;

    // Chunk streaming works: a below-surface chunk comes back with voxels.
    let vol = c.req_chunk([8, 7, 8]).await;
    assert!(!vol.is_empty(), "below-surface chunk should have voxels");

    // Edit a voxel in the now-loaded chunk; the edit is enqueued for
    // background persistence. Round-trip another request to keep the
    // connection lively while the writer flushes.
    c.send(&ClientMsg::Edit { pos: [8 * 32, 7 * 32, 8 * 32], value: 0 }).await;
    c.req_chunk([8, 7, 8]).await;

    assert!(server.data_dir.join("accounts.bin").is_file());
    // Generated + edited chunks persist ASYNCHRONOUSLY on the background
    // writer, so poll for the region file rather than asserting immediately.
    let regions = server.data_dir.join("worlds").join("default").join("regions");
    assert!(
        wait_until(|| regions.is_dir(), Duration::from_secs(3)),
        "generated/edited chunks should persist to a region file"
    );
}
