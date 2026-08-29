//! Streaming-throughput scenario: a fresh world's radius-4 burst must reach
//! the client promptly. Guards the chunk pipeline's tick pacing (TODO phase 5)
//! against serializing the burst.
//!
//! The burst is no longer the whole 729-chunk cube: occlusion culling withholds
//! chunks sealed behind solid neighbours, so what must arrive promptly is the
//! visible shell. The assertions that still mean something — promptness, all
//! pristine, a manifest of a few KB — are unchanged.

mod common;

use std::time::Duration;

use common::{Client, TestServer};
use soils_protocol::{ChunkInfo, ClientMsg, ServerMsg};

/// The cube the join burst subscribes: radius 4 around the spawn chunk.
const CUBE: i32 = 729;

#[tokio::test]
async fn fresh_world_burst_streams_promptly() {
    let server = TestServer::start("burst");
    let mut c = Client::join(server.addr(), "burst").await;

    // The join burst: the server subscribes the default radius-4 cube around
    // the spawn chunk on login and pushes it — no request is sent.
    let mut wave = Vec::new();
    for x in 4..=12 {
        for y in 4..=12 {
            for z in 4..=12 {
                wave.push([x, y, z]);
            }
        }
    }
    let quiet = Duration::from_millis(600);
    let t0 = std::time::Instant::now();
    let got = c.collect_available(&wave, quiet).await;
    let elapsed = t0.elapsed().saturating_sub(quiet);
    let sent = got.payloads.len();
    println!(
        "fresh burst: {sent} of {CUBE} chunks streamed in {} ms ({} withheld)",
        elapsed.as_millis(),
        CUBE as usize - sent
    );
    // The cull withholds the buried interior, so the delivered count is a band
    // rather than a number. Both ends matter: nothing withheld would mean the
    // cull stopped working, and a nearly-empty burst would mean it ate the
    // visible world.
    assert!(
        (CUBE as usize / 3..CUBE as usize).contains(&sent),
        "{sent} of {CUBE} chunks delivered — the cull is either inert or eating the shell"
    );
    assert!(
        elapsed.as_secs_f32() < 3.0,
        "fresh burst took {elapsed:?}; the chunk pipeline is pacing waves too slowly"
    );

    // Manifest streaming gates: a fresh world is all pristine — nothing ships
    // a payload, and the whole burst is a few KB of positions.
    println!("burst manifest cost {} KB", got.wire_bytes / 1024);
    assert_eq!(got.edited, 0, "fresh world must classify every chunk pristine");
    assert!(
        got.wire_bytes < 100 * 1024,
        "join manifest grew to {} bytes — classification regression?",
        got.wire_bytes
    );
}

/// The wire oracle for client-side generation: a pristine chunk fetched as a
/// full payload must byte-equal the locally generated one — and an edit must
/// flip its class to `Edited` with the edit reflected in the payload.
#[tokio::test]
async fn pristine_payloads_byte_equal_local_generation() {
    let server = TestServer::start("wire-oracle");
    let mut c = Client::join(server.addr(), "oracle").await;

    // Spread across the join subscription: surface, deep, sky.
    let probes: Vec<[i32; 3]> = vec![[8, 8, 8], [7, 5, 9], [9, 11, 8], [6, 8, 10]];
    // `[7, 5, 9]` is deep on purpose, and deep chunks are exactly what
    // occlusion culling withholds — so there is no waiting for delivery here.
    // `ChunkFetch` is the explicit escape hatch and serves any *subscribed and
    // resident* position, culled or not; the retry covers the window where the
    // join burst has not generated a probe yet.
    let mut got = std::collections::HashMap::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while got.len() < probes.len() {
        assert!(
            std::time::Instant::now() < deadline,
            "only {} of {} probes came back from ChunkFetch",
            got.len(),
            probes.len()
        );
        c.send(&ClientMsg::ChunkFetch { positions: probes.clone() }).await;
        let until = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < until && got.len() < probes.len() {
            if let Ok(ServerMsg::Manifest { chunks }) =
                tokio::time::timeout(Duration::from_millis(200), c.next_msg()).await
            {
                for info in chunks {
                    if let ChunkInfo::Edited { pos, payload } = info {
                        got.insert(pos, payload);
                    }
                }
            }
        }
    }
    for pos in &probes {
        let (terrain, registry) = c.generator();
        let local = soils_protocol::encode_chunk(
            &terrain.generate(glam::IVec3::from_array(*pos), registry),
        );
        assert_eq!(
            got[pos], local,
            "server payload for {pos:?} differs from local generation — the client-gen \
             invariant is broken"
        );
    }

    // Edit within reach (spawn eye ~[282, 285, 268] → chunk [8, 8, 8]) and
    // refetch: the chunk must now classify Edited with the edit applied.
    let target = [282, 280, 268];
    c.edit(target, 5).await;
    c.recv_until(|msg| match msg {
        ServerMsg::EditAccepted { .. } => Some(()),
        ServerMsg::EditRejected { .. } => panic!("oracle edit rejected"),
        _ => None,
    })
    .await;
    c.send(&ClientMsg::ChunkFetch { positions: vec![[8, 8, 8]] }).await;
    let payload = c
        .recv_until(|msg| match msg {
            ServerMsg::Manifest { chunks } => chunks.into_iter().find_map(|i| match i {
                ChunkInfo::Edited { pos, payload } if pos == [8, 8, 8] => Some(payload),
                _ => None,
            }),
            _ => None,
        })
        .await;
    let vol = soils_protocol::decode_chunk(&payload).unwrap();
    assert_eq!(vol.get(282 & 31, 280 & 31, 268 & 31), 5, "edit missing from refetched payload");
}
