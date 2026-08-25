//! Genuinely concurrent clients: every player runs on its own OS thread with
//! its own runtime, so the server's connection handling, input integration and
//! replication are exercised in real parallel rather than interleaved on one
//! executor.
//!
//! Covers player-vs-player collision (blocking, stacking, jumping off a peer's
//! head), the same under artificial network conditions, and a 100-client load
//! test.
//!
//! Two facts about the sim shape every test here:
//!
//! * **Fly mode is noclip.** Players spawn flying, and a flying player passes
//!   through voxels and peers alike. Anything about collision must first put
//!   the player on the ground with [`Client::land`].
//! * **The server only steps a player on ticks it receives input for.** A
//!   silent client is frozen, not falling, so "stand still" means sending idle
//!   frames, not sending nothing.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::{Client, TestServer, spawn_peer};
use soils_protocol::netsim::NetSim;
use soils_protocol::{ClientMsg, ServerMsg};
use tokio::sync::Barrier;

/// Facing constants: yaw rotates `-Z` about `+Y`.
const NORTH: f32 = 0.0; // -Z
const SOUTH: f32 = std::f32::consts::PI; // +Z
const EAST: f32 = -std::f32::consts::FRAC_PI_2; // +X
const WEST: f32 = std::f32::consts::FRAC_PI_2; // -X

/// The chunk containing the spawn point.
const SPAWN_CHUNK: [i32; 3] = [8, 8, 8];

fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Barrier wait with a deadline.
///
/// A peer thread that panics never reaches its barrier, and an unguarded
/// `wait()` would then hang the whole test binary — burying the real assertion
/// failure under a timeout. Failing here instead keeps the first error visible.
async fn sync(b: &Barrier, what: &str) {
    tokio::time::timeout(Duration::from_secs(60), b.wait()).await.unwrap_or_else(|_| {
        panic!("timed out at barrier {what:?}: a peer thread failed or stalled")
    });
}

/// The other player's NetId, learned from replication rather than shared
/// through the test — the same way a real client learns it.
async fn await_peer(c: &mut Client) -> u32 {
    c.await_peer_player().await
}

/// Place `blocks`, waiting for each ack. A rejected edit fails loudly: silently
/// building nothing would make every downstream collision assertion vacuous.
async fn build(c: &mut Client, blocks: &[[i32; 3]]) {
    for b in blocks {
        let seq = c.edit(*b, 1).await;
        c.recv_until(|msg| match msg {
            ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(()),
            ServerMsg::EditRejected { seq: s } if s == seq => {
                panic!("edit at {b:?} rejected — out of reach, or chunk not resident")
            }
            _ => None,
        })
        .await;
    }
}

/// A flat platform to stand on, so collision tests do not depend on whatever
/// the terrain generator put under the spawn point. Spans `±rx` by `±rz`
/// voxels at `y`, all within edit reach (Chebyshev 8) of the spawn eye.
fn platform(spawn: [f32; 3], rx: i32, rz: i32, drop: i32) -> (Vec<[i32; 3]>, i32) {
    let (sx, sy, sz) =
        (spawn[0].floor() as i32, spawn[1].floor() as i32, spawn[2].floor() as i32);
    let y = sy - drop;
    let mut blocks = Vec::new();
    for x in sx - rx..=sx + rx {
        for z in sz - rz..=sz + rz {
            blocks.push([x, y, z]);
        }
    }
    (blocks, y)
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Two players, two threads, moving at the same time: each must observe the
/// other's server-integrated position through the delta-snapshot stream.
#[test]
fn two_players_on_separate_threads_observe_each_other_moving() {
    let server = TestServer::start("concurrent-pair");
    let addr = server.addr();
    let gate = Arc::new(Barrier::new(2));

    // Each flies away from the other along Z, then checks the peer went the
    // opposite way. 16 ticks at 8 u/s = 2.0 units.
    let leg = |name: &'static str, yaw: f32, sign: f32| {
        let gate = gate.clone();
        spawn_peer(addr, name, None, move |mut c| async move {
            let spawn = c.spawn;
            let peer = await_peer(&mut c).await;
            sync(&gate, "gate").await; // both sides know each other before anyone moves

            c.fly(16, yaw, false).await;

            let moved = c.await_entity(peer, |s| (s.pos[2] - spawn[2]) * -sign > 1.5).await;
            assert!(
                (moved.pos[0] - spawn[0]).abs() < 0.2,
                "{name}'s peer drifted in x: {:?}",
                moved.pos
            );
            let me = c.await_self_pos().await;
            assert!(
                ((me[2] - spawn[2]) * sign - 2.0).abs() < 0.6,
                "{name} should have moved ~2 units, got {:?} from {:?}",
                me,
                spawn
            );
            (me, moved.pos)
        })
    };

    let a = leg("alice", NORTH, -1.0);
    let b = leg("bob", SOUTH, 1.0);
    let (a_self, a_saw) = a.join().expect("alice thread");
    let (b_self, b_saw) = b.join().expect("bob thread");

    // Each side's view of the other must match that side's own authoritative
    // position: one server, one truth, two independently-driven connections.
    assert!(
        dist_xz(a_self, b_saw) < 0.5,
        "bob saw alice at {b_saw:?}, alice's own echo was {a_self:?}"
    );
    assert!(
        dist_xz(b_self, a_saw) < 0.5,
        "alice saw bob at {a_saw:?}, bob's own echo was {b_self:?}"
    );
}

// ---------------------------------------------------------------------------
// Player-vs-player collision
// ---------------------------------------------------------------------------

/// A walking player is stopped by another player's body.
#[test]
fn players_block_each_other_when_walking() {
    let server = TestServer::start("concurrent-block");
    let addr = server.addr();
    let built = Arc::new(Barrier::new(2));
    let landed = Arc::new(Barrier::new(2));
    let done = Arc::new(Barrier::new(2));

    // Alice builds the floor, moves clear *while still flying*, then walks
    // back into Bob. Separating in the air matters: everyone spawns on the
    // same point, and two players who drop from it land stacked, not side by
    // side (which is its own test below).
    let alice = {
        let (built, landed, done) = (built.clone(), landed.clone(), done.clone());
        spawn_peer(addr, "alice", None, move |mut c| async move {
            let peer = await_peer(&mut c).await;
            c.await_chunk(SPAWN_CHUNK).await;
            let (blocks, _) = platform(c.spawn, 4, 2, 5);
            build(&mut c, &blocks).await;
            sync(&built, "built").await;

            c.fly(24, WEST, false).await; // 3 units clear, noclip
            let start = c.land().await;
            sync(&landed, "landed").await;

            let bob = c.current_entity_pos(peer).await;
            assert!(
                bob[0] - start[0] > 1.5,
                "alice should be clear of bob before charging: alice {start:?} bob {bob:?}"
            );
            assert!(
                (bob[1] - start[1]).abs() < 0.5,
                "both should be standing on the platform, not stacked: alice {start:?} bob {bob:?}"
            );

            // 48 ticks east = 6 units unobstructed, which would carry her well
            // past Bob. She must instead stop against him.
            c.walk(48, EAST).await;
            c.hold(16, soils_sim::PlayerInput::default()).await;
            c.await_server_ticks(4).await; // the echo, not a guess
            let end = c.current_self_pos().await;
            sync(&done, "done").await;

            assert!(
                end[0] > start[0] + 1.0,
                "alice did not advance toward bob: {start:?} -> {end:?}"
            );
            assert!(
                end[0] < bob[0] - 0.4,
                "alice walked through bob: stopped at {end:?}, bob at {bob:?}"
            );
            assert!(
                end[0] > bob[0] - 1.6,
                "alice stopped far short of bob — something else blocked her: \
                 {end:?}, bob at {bob:?}"
            );
            end
        })
    };

    // Bob holds the spawn point, idling so gravity keeps him planted.
    let bob = {
        let (built, landed, done) = (built.clone(), landed.clone(), done.clone());
        spawn_peer(addr, "bob", None, move |mut c| async move {
            await_peer(&mut c).await;
            sync(&built, "built").await;

            let rest = c.land().await;
            sync(&landed, "landed").await;

            // Stand still while Alice charges.
            c.hold(80, soils_sim::PlayerInput::default()).await;
            c.await_server_ticks(4).await;
            let held = c.current_self_pos().await;
            sync(&done, "done").await;

            assert!(
                dist_xz(held, rest) < 0.3,
                "bob should not have been shoved: {rest:?} -> {held:?}"
            );
            held
        })
    };

    let a = alice.join().expect("alice thread");
    let b = bob.join().expect("bob thread");
    // Bodies are 0.6 wide, so touching centres sit ~0.6 apart.
    let gap = dist_xz(a, b);
    assert!(
        (0.45..1.2).contains(&gap),
        "alice should be resting against bob, gap was {gap} (alice {a:?}, bob {b:?})"
    );
}

/// A player falling onto another lands on their head, and can jump off it —
/// which is only possible if peer contact sets `grounded`.
#[test]
fn a_player_stands_and_jumps_on_another_players_head() {
    let server = TestServer::start("concurrent-stack");
    let addr = server.addr();
    let built = Arc::new(Barrier::new(2));
    let bob_down = Arc::new(Barrier::new(2));
    let done = Arc::new(Barrier::new(2));
    // Bob's resting eye height, published for Alice to check herself against.
    let bob_y = Arc::new(AtomicU32::new(0));

    // Bob drops to the platform first and stays there; Alice stays in the air
    // directly above him (both spawn on the same point) and then falls.
    let bob = {
        let (built, bob_down, done, bob_y) =
            (built.clone(), bob_down.clone(), done.clone(), bob_y.clone());
        spawn_peer(addr, "bob", None, move |mut c| async move {
            await_peer(&mut c).await;
            c.await_chunk(SPAWN_CHUNK).await;
            let (blocks, _) = platform(c.spawn, 3, 3, 5);
            build(&mut c, &blocks).await;
            sync(&built, "built").await;

            let rest = c.land().await;
            bob_y.store(rest[1].to_bits(), Ordering::SeqCst);
            sync(&bob_down, "bob_down").await;

            // Hold position while Alice lands on him and jumps.
            c.hold(240, soils_sim::PlayerInput::default()).await;
            let held = c.current_self_pos().await;
            sync(&done, "done").await;
            assert!(
                (held[1] - rest[1]).abs() < 0.2,
                "bob should not be pushed down by the weight: {rest:?} -> {held:?}"
            );
            held
        })
    };

    let alice = {
        let (built, bob_down, done, bob_y) =
            (built.clone(), bob_down.clone(), done.clone(), bob_y.clone());
        spawn_peer(addr, "alice", None, move |mut c| async move {
            await_peer(&mut c).await;
            sync(&built, "built").await;
            sync(&bob_down, "bob_down").await;

            let bob_eye = f32::from_bits(bob_y.load(Ordering::SeqCst));
            // Feet rest on Bob's head; the eye sits a body-height above his.
            let expect = bob_eye + soils_sim::EYE_TO_HEAD + soils_sim::EYE_TO_FEET;

            let rest = c.land().await;
            assert!(
                (rest[1] - expect).abs() < 0.3,
                "alice should rest on bob's head at ~{expect}, got {} (bob eye {bob_eye})",
                rest[1]
            );
            assert!(
                rest[1] > bob_eye + 1.0,
                "alice fell through bob to the platform: {} vs bob {bob_eye}",
                rest[1]
            );

            // Jumping requires `grounded`, which here can only have come from
            // standing on Bob.
            c.drive(1, |_| soils_sim::PlayerInput { jump: true, ..Default::default() }).await;
            let apex = {
                let mut best = rest[1];
                for _ in 0..12 {
                    c.hold(4, soils_sim::PlayerInput::default()).await;
                    best = best.max(c.await_self_pos().await[1]);
                }
                best
            };
            assert!(
                apex > rest[1] + 0.5,
                "alice never left bob's head: apex {apex} vs rest {}",
                rest[1]
            );

            // ...and comes back down onto him, not through him.
            let back = c.settle().await;
            sync(&done, "done").await;
            assert!(
                (back[1] - expect).abs() < 0.3,
                "alice should land back on bob's head at ~{expect}, got {}",
                back[1]
            );
            (rest, apex, back)
        })
    };

    let (rest, apex, back) = alice.join().expect("alice thread");
    let b = bob.join().expect("bob thread");
    assert!(
        rest[1] > b[1] + 1.0 && back[1] > b[1] + 1.0,
        "alice ({rest:?} -> apex {apex} -> {back:?}) should be stacked above bob ({b:?})"
    );
}

// ---------------------------------------------------------------------------
// Adverse network conditions
// ---------------------------------------------------------------------------

/// The same interaction over a bad link: 120 ms one-way with 40 ms of gaussian
/// jitter and 5% loss on the lanes built to absorb it. Collision is resolved
/// server-side, so a hostile link may delay the outcome but must not change it.
#[test]
fn players_still_block_each_other_over_a_lossy_jittery_link() {
    let server = TestServer::start("concurrent-netsim");
    let addr = server.addr();
    let built = Arc::new(Barrier::new(2));
    let landed = Arc::new(Barrier::new(2));
    let done = Arc::new(Barrier::new(2));
    let link = |seed| Some(NetSim::new(
        Duration::from_millis(120),
        Duration::from_millis(40),
        0.05,
        seed,
    ));

    let alice = {
        let (built, landed, done) = (built.clone(), landed.clone(), done.clone());
        spawn_peer(addr, "alice", link(17), move |mut c| async move {
            let peer = await_peer(&mut c).await;
            c.await_chunk(SPAWN_CHUNK).await;
            let (blocks, _) = platform(c.spawn, 4, 2, 5);
            build(&mut c, &blocks).await;
            sync(&built, "built").await;

            c.fly(24, WEST, false).await;
            let start = c.land().await;
            sync(&landed, "landed").await;

            let bob = c.current_entity_pos(peer).await;
            c.walk(48, EAST).await;
            c.hold(16, soils_sim::PlayerInput::default()).await;
            c.await_server_ticks(4).await;
            let end = c.current_self_pos().await;
            sync(&done, "done").await;

            assert!(
                end[0] > start[0] + 0.5,
                "alice made no progress under loss: {start:?} -> {end:?}"
            );
            assert!(
                end[0] < bob[0] - 0.4,
                "a lossy link must not let alice through bob: {end:?} vs {bob:?}"
            );
            end
        })
    };

    let bob = {
        let (built, landed, done) = (built.clone(), landed.clone(), done.clone());
        spawn_peer(addr, "bob", link(23), move |mut c| async move {
            await_peer(&mut c).await;
            sync(&built, "built").await;
            let rest = c.land().await;
            sync(&landed, "landed").await;
            c.hold(80, soils_sim::PlayerInput::default()).await;
            c.await_server_ticks(4).await;
            let held = c.current_self_pos().await;
            sync(&done, "done").await;
            assert!(dist_xz(held, rest) < 0.4, "bob drifted under loss: {rest:?} -> {held:?}");
            held
        })
    };

    let a = alice.join().expect("alice thread");
    let b = bob.join().expect("bob thread");
    let gap = dist_xz(a, b);
    assert!(gap > 0.4, "alice ended up inside bob over a bad link: gap {gap}");
}

// ---------------------------------------------------------------------------
// Load
// ---------------------------------------------------------------------------

/// 100 clients, 100 threads, all joining and moving at once.
///
/// Each gets a small view radius: the point is connection, login, input and
/// replication throughput under many simultaneous players, not to stream 100
/// copies of a 9³-chunk join burst.
#[test]
fn a_hundred_clients_join_and_move_concurrently() {
    const N: usize = 100;
    let server = TestServer::start("concurrent-100");
    let addr = server.addr();
    let gate = Arc::new(Barrier::new(N));

    let peers: Vec<_> = (0..N)
        .map(|i| {
            let gate = gate.clone();
            spawn_peer(addr, format!("player{i:03}"), None, move |mut c| async move {
                c.send(&ClientMsg::ViewRadius { radius: 1, full_streams: false }).await;
                let (id, net, spawn) = (c.id, c.self_entity, c.spawn);

                // Everyone waits for the full lobby, so the movement below is
                // genuinely simultaneous rather than staggered by join order.
                sync(&gate, "gate").await;

                // Fan out: each player flies a different bearing, so they end
                // up spread rather than stacked (and cannot block each other —
                // fly mode is noclip).
                let yaw = i as f32 / N as f32 * std::f32::consts::TAU;
                c.fly(16, yaw, false).await;

                // Wait for the server to report the flight rather than
                // sampling immediately: under a hundred simultaneous clients
                // the snapshot for any one of them can lag several ticks.
                let end = c
                    .await_self_where(|p| dist_xz(p, spawn) > 1.4, "player to finish flying")
                    .await;
                let moved = dist_xz(end, spawn);
                assert!(
                    moved < 2.6,
                    "player{i:03} moved {moved}, further than 16 ticks of input allows \
                     ({spawn:?} -> {end:?})"
                );
                (id, net)
            })
        })
        .collect();

    let mut ids = std::collections::HashSet::new();
    let mut nets = std::collections::HashSet::new();
    for (i, h) in peers.into_iter().enumerate() {
        let (id, net) = h.join().unwrap_or_else(|_| panic!("player{i:03} thread panicked"));
        assert!(ids.insert(id), "duplicate player id {id}");
        assert!(nets.insert(net), "duplicate entity NetId {net}");
    }
    assert_eq!(ids.len(), N, "every client should have logged in");
}

/// 100 clients under adverse network conditions, checking the server keeps
/// every one of them served rather than starving the slow ones.
#[test]
fn a_hundred_clients_survive_adverse_network_conditions() {
    const N: usize = 100;
    let server = TestServer::start("concurrent-100-netsim");
    let addr = server.addr();
    let gate = Arc::new(Barrier::new(N));

    let peers: Vec<_> = (0..N)
        .map(|i| {
            let gate = gate.clone();
            // A spread of link qualities, so this is not one uniform delay:
            // latency 40-240 ms, jitter 10-60 ms, loss 0-6%.
            let sim = NetSim::new(
                Duration::from_millis(40 + (i as u64 % 5) * 50),
                Duration::from_millis(10 + (i as u64 % 6) * 10),
                (i % 7) as f64 * 0.01,
                1000 + i as u64,
            );
            spawn_peer(addr, format!("laggy{i:03}"), Some(sim), move |mut c| async move {
                c.send(&ClientMsg::ViewRadius { radius: 1, full_streams: false }).await;
                let spawn = c.spawn;
                sync(&gate, "gate").await;

                let yaw = i as f32 / N as f32 * std::f32::consts::TAU;
                c.fly(32, yaw, false).await;

                // The server integrates inputs, not positions, so loss costs
                // distance (dropped frames are re-sent in the next bundle, but
                // only the last 3 survive) while jitter costs nothing. Assert
                // real progress in the right direction rather than an exact
                // distance.
                let end = c
                    .await_self_where(|p| dist_xz(p, spawn) > 1.0, "laggy player to move")
                    .await;
                let moved = dist_xz(end, spawn);
                assert!(moved < 5.0, "laggy{i:03} moved {moved}, further than any input allows");
                moved
            })
        })
        .collect();

    let moved: Vec<f32> = peers
        .into_iter()
        .enumerate()
        .map(|(i, h)| h.join().unwrap_or_else(|_| panic!("laggy{i:03} thread panicked")))
        .collect();
    assert_eq!(moved.len(), N);
    let mean = moved.iter().sum::<f32>() / N as f32;
    assert!(mean > 2.0, "mean travel {mean} suggests the server starved these clients");
}
