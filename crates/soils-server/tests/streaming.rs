//! Streaming-throughput scenario: a fresh world's full radius-4 burst (729
//! chunks) must reach the client promptly. Guards the chunk pipeline's tick
//! pacing (TODO phase 5) against serializing the burst.

mod common;

use common::{Client, TestServer};

#[tokio::test]
async fn fresh_world_burst_streams_promptly() {
    let server = TestServer::start("burst");
    let mut c = Client::join(server.addr(), "burst").await;

    // The client's real first request: radius-4 cube around the spawn chunk,
    // nearest-first order not required for this measurement.
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
    assert_eq!(got.len(), 729);
    assert!(
        elapsed.as_secs_f32() < 3.0,
        "fresh 729-chunk burst took {elapsed:?}; the chunk pipeline is pacing waves too slowly"
    );
}
