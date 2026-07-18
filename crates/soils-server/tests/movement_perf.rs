//! Performance / load characteristics of the character-control + net-physics
//! path: the shared movement integrator's raw throughput, the server holding
//! its snapshot budget under many moving entities, faithful integration of
//! sustained input, and many concurrent players without the tick loop falling
//! behind. Load tests assert bounds (like `scenarios::snapshot_bandwidth`)
//! rather than wall-clock, which stays machine-independent; the one timing
//! smoke check uses a deliberately loose ceiling.

mod common;

use common::{Client, TestServer};
use glam::{IVec3, Vec2, Vec3};
use soils_protocol::ServerMsg;
use soils_sim::{PlayerInput, PlayerState, TICK_HZ, step_player};
use std::time::{Duration, Instant};

/// Hot-path throughput + numerical-stability smoke: half a million integrator
/// steps against a solid floor must stay finite and finish well under a loose
/// ceiling (guards against an O(n) blow-up in the swept-AABB collision).
#[test]
fn step_player_bulk_is_finite_and_fast() {
    // Infinite solid floor below y=64; player walks on top.
    let sampler = |v: IVec3| if v.y < 64 { 1u8 } else { 0u8 };
    let mut st = PlayerState {
        pos: Vec3::new(0.5, 66.0, 0.5),
        flying: false,
        ..Default::default()
    };
    let dt = 1.0 / TICK_HZ as f32;
    let n = 500_000u32;
    let start = Instant::now();
    for i in 0..n {
        let input = PlayerInput {
            move_axes: Vec2::new((i % 3) as f32 - 1.0, 1.0),
            yaw: i as f32 * 0.001,
            jump: i % 90 == 0,
            ..Default::default()
        };
        step_player(&mut st, &input, dt, &sampler);
    }
    let elapsed = start.elapsed();
    eprintln!(
        "{n} step_player calls in {elapsed:?} ({:.1} M/s), final {:?}",
        n as f64 / elapsed.as_secs_f64() / 1e6,
        st.pos
    );
    assert!(st.pos.is_finite() && st.vel.is_finite(), "sim went non-finite: {st:?}");
    assert!(st.pos.y >= 65.0, "player fell through the floor: y {}", st.pos.y);
    // Debug builds do ~1e6+ steps/s; 20 s for 5e5 is an enormous margin that
    // only trips on a genuine algorithmic regression.
    assert!(elapsed < Duration::from_secs(20), "hot path too slow: {elapsed:?}");
}

/// The per-tick snapshot byte budget holds even under a crowd of moving
/// entities: every packet stays within the server's hard cap (app.rs
/// SNAPSHOT_BUDGET = 410), so a busy world can never blow the bandwidth budget.
#[tokio::test]
async fn snapshot_budget_holds_under_many_moving_entities() {
    let server = TestServer::start_with("perf-budget", |cfg| cfg.critters = 8);
    let mut a = Client::join(server.addr(), "alice").await;
    a.await_self_pos().await; // let the join burst settle

    let (mut bytes, mut packets) = (0usize, 0u32);
    while packets < 60 {
        if let ServerMsg::Snapshot { tick, baseline_tick, payload, .. } = a.next_msg().await {
            assert!(
                payload.len() <= 410,
                "snapshot {} B exceeds the per-tick budget (410)",
                payload.len()
            );
            bytes += payload.len();
            packets += 1;
            let _ = a.tracker.apply(tick, baseline_tick, &payload);
        }
    }
    eprintln!("avg {} B/tick over {packets} packets (8 critters + self)", bytes / packets as usize);
}

/// Sustained, correctly-paced input is integrated in full — nothing is dropped
/// over several seconds. Sprint-fly (noclip) 256 ticks = 4 s; the server must
/// move the player the whole legit distance (32 u/s × 4 s = 128 u).
#[tokio::test]
async fn sustained_paced_input_is_integrated_in_full() {
    let server = TestServer::start("perf-sustained");
    let mut a = Client::join(server.addr(), "alice").await;
    let spawn = a.spawn;
    let net = a.self_entity;

    a.fly(256, 0.0, true).await; // forward = -Z at 32 u/s

    let reached = tokio::time::timeout(
        Duration::from_secs(20),
        a.await_entity(net, move |s| s.pos[2] < spawn[2] - 120.0),
    )
    .await;
    assert!(
        reached.is_ok(),
        "sustained paced input was dropped — server didn't integrate ~128 u of travel"
    );
}

/// Many players moving at once: the 20 Hz server tick keeps up and integrates
/// every client's inputs — none stall. Each of N connections flies forward
/// concurrently and must see its own server-echoed position advance.
#[tokio::test]
async fn many_clients_move_without_the_server_stalling() {
    const N: usize = 6;
    const TICKS: u32 = 96; // 1.5 s of travel = 12 u forward

    let server = TestServer::start("perf-crowd");
    let addr = server.addr();

    let mut tasks = Vec::new();
    for i in 0..N {
        let mut c = Client::join(addr, &format!("p{i}")).await;
        let spawn = c.spawn;
        let net = c.self_entity;
        // Each client drives and self-verifies inside its own task, so all N
        // move concurrently and the server tick loop is under real crowd load.
        tasks.push(tokio::spawn(async move {
            c.fly(TICKS, 0.0, false).await;
            tokio::time::timeout(
                Duration::from_secs(20),
                c.await_entity(net, move |s| s.pos[2] < spawn[2] - 6.0),
            )
            .await
            .is_ok()
        }));
    }

    let results = futures_util::future::join_all(tasks).await;
    let moved = results.into_iter().filter(|r| *r.as_ref().unwrap()).count();
    assert_eq!(moved, N, "only {moved}/{N} clients were integrated — the tick loop fell behind");
}
