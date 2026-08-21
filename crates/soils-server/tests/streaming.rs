//! Streaming-throughput scenario: a fresh world's full radius-4 burst (729
//! chunks) must reach the client promptly. Guards the chunk pipeline's tick
//! pacing (TODO phase 5) against serializing the burst.

mod common;

use common::{Client, TestServer};
use soils_protocol::{ChunkInfo, ClientMsg, ServerMsg};

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
    let t0 = std::time::Instant::now();
    let got = c.collect_chunks(&wave).await;
    let elapsed = t0.elapsed();
    println!("729-chunk fresh burst streamed in {} ms", elapsed.as_millis());
    assert_eq!(got.payloads.len(), 729);
    assert!(
        elapsed.as_secs_f32() < 3.0,
        "fresh 729-chunk burst took {elapsed:?}; the chunk pipeline is pacing waves too slowly"
    );

    // Manifest streaming gates: a fresh world is all pristine — nothing ships
    // a payload, and the whole burst is a few KB of positions.
    println!("729-chunk burst manifest cost {} KB", got.wire_bytes / 1024);
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
    let _ = c.collect_chunks(&probes).await; // wait until they're all resident

    c.send(&ClientMsg::ChunkFetch { positions: probes.clone() }).await;
    let mut got = std::collections::HashMap::new();
    while got.len() < probes.len() {
        if let ServerMsg::Manifest { chunks } = c.next_msg().await {
            for info in chunks {
                if let ChunkInfo::Edited { pos, payload } = info {
                    got.insert(pos, payload);
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
