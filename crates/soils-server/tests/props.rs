//! Hundreds of network-synced rigid bodies, shared by two players.
//!
//! The engine already replicated a 3-cube demo stack. These tests take that to
//! a few hundred simultaneously-moving bodies and ask the questions that only
//! matter at that scale:
//!
//! * do two independently-connected clients converge on the *same* physics
//!   state, or does each drift into its own simulation?
//! * can a player actually shove the pile — the kinematic player proxy is the
//!   only path from player movement into the rigid-body world?
//! * what does replicating that many moving bodies cost on the wire?
//!
//! Each client runs on its own OS thread with its own runtime, so the server is
//! serving genuinely parallel connections while the solver is under load.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{Client, TestServer, spawn_peer};
use soils_protocol::{EntityState, ServerMsg};
use tokio::sync::Barrier;

/// Enough bodies that the pile is a load test rather than a scene, but still
/// well inside the snapshot codec's 4096-entity ceiling.
const PROPS: u16 = 300;
/// Chebyshev-8 reach means the pile lands within a few units of spawn.
const SPAWN_CHUNK: [i32; 3] = [8, 8, 8];
const EAST: f32 = -std::f32::consts::FRAC_PI_2;

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

async fn sync(b: &Barrier, what: &str) {
    tokio::time::timeout(Duration::from_secs(120), b.wait()).await.unwrap_or_else(|_| {
        panic!("timed out at barrier {what:?}: a peer thread failed or stalled")
    });
}

/// Collect the latest known position of every physics prop this client has
/// seen, waiting until at least `want` of them have been announced.
///
/// Props are learned from `EntitySpawn` and tracked through delta snapshots, so
/// this is the same path a real client uses — not a back door into the server.
async fn await_props(c: &mut Client, want: usize) -> HashMap<u32, [f32; 3]> {
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        // Drain first: that advances the tracker and the position table for
        // every prop at once, instead of awaiting each of several hundred
        // individually.
        c.drain_self_pos();
        let ids = c.props_seen();
        if ids.len() >= want {
            let out: HashMap<u32, [f32; 3]> =
                ids.iter().filter_map(|id| c.known_pos(*id).map(|p| (*id, p))).collect();
            if out.len() >= want {
                return out;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {} of {want} props announced",
            c.props_seen().len()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Idle until the pile stops moving, so "rest state" means something.
async fn await_settled(c: &mut Client) -> HashMap<u32, [f32; 3]> {
    let mut last = await_props(c, PROPS as usize).await;
    let mut still = 0;
    for _ in 0..60 {
        c.idle_for(Duration::from_millis(700)).await;
        let now = await_props(c, PROPS as usize).await;
        let moved = now
            .iter()
            .filter_map(|(id, p)| last.get(id).map(|q| dist(*p, *q)))
            .fold(0.0f32, f32::max);
        last = now;
        still = if moved < 0.05 { still + 1 } else { 0 };
        if still >= 3 {
            return last;
        }
    }
    // Returning the last reading here would hand back a mid-settle snapshot,
    // and a caller comparing before/after would then measure the pile's own
    // residual motion — letting a shove assertion pass without any shove.
    panic!("{PROPS} props never settled");
}

// ---------------------------------------------------------------------------

/// Two players, one pile: both clients must converge on the same rest state.
///
/// This is the actual meaning of "network-synced". The server owns the solver;
/// each client receives delta snapshots over its own connection and rebuilds
/// the world independently, so agreement across two connections is the thing
/// worth asserting — a client-side simulation drifting on its own would show up
/// here and nowhere else.
#[test]
fn hundreds_of_props_stay_synced_across_two_clients() {
    let server = TestServer::start_with("props-sync", |c| {
        c.physics = true;
        c.props = PROPS;
    });
    let addr = server.addr();
    let settled = Arc::new(Barrier::new(2));
    let done = Arc::new(Barrier::new(2));

    let peer = |name: &'static str| {
        let (settled, done) = (settled.clone(), done.clone());
        spawn_peer(addr, name, None, move |mut c| async move {
            c.await_chunk(SPAWN_CHUNK).await;
            let rest = await_settled(&mut c).await;
            sync(&settled, "settled").await;
            sync(&done, "done").await;
            assert!(
                rest.len() >= PROPS as usize,
                "{name} only tracked {} of {PROPS} props",
                rest.len()
            );
            rest
        })
    };

    // Both threads must exist before either is joined: joining the first
    // blocks the main thread, so spawning the second afterwards would leave
    // the first waiting at a barrier its partner can never reach.
    let ha = peer("alice");
    let hb = peer("bob");
    let a = ha.join().expect("alice thread");
    let b = hb.join().expect("bob thread");

    // Every prop both clients know about must be in the same place. The
    // tolerance is the snapshot codec's position quantization plus a tick of
    // interpolation slack, not a fudge factor for drift.
    let mut compared = 0;
    let mut worst = 0.0f32;
    for (id, pa) in &a {
        if let Some(pb) = b.get(id) {
            let d = dist(*pa, *pb);
            worst = worst.max(d);
            compared += 1;
        }
    }
    assert!(
        compared >= PROPS as usize * 9 / 10,
        "clients only shared {compared} of {PROPS} props"
    );
    assert!(
        worst < 0.5,
        "clients disagree on a prop by {worst} units — the two connections \
         rebuilt different worlds"
    );
    println!("{compared} props compared, worst disagreement {worst:.3} units");
}

/// A player shoves the pile, and the other player sees it move.
///
/// Player movement reaches the rigid-body world only through the kinematic
/// proxy, and that proxy is one-way: it pushes props without props pushing
/// back. So the assertion is that Alice's walk moves cubes *and* that Bob —
/// who never touched them — receives the same displacement.
#[test]
fn a_player_shoves_the_pile_and_the_other_player_sees_it() {
    let server = TestServer::start_with("props-shove", |c| {
        c.physics = true;
        c.props = PROPS;
    });
    let addr = server.addr();
    let settled = Arc::new(Barrier::new(2));
    let shoved = Arc::new(Barrier::new(2));

    // Alice lands and walks east through the pile.
    let alice = {
        let (settled, shoved) = (settled.clone(), shoved.clone());
        spawn_peer(addr, "alice", None, move |mut c| async move {
            c.await_chunk(SPAWN_CHUNK).await;
            await_settled(&mut c).await;
            sync(&settled, "settled").await;

            c.land().await;
            // Long enough to walk clean through the pile's footprint.
            for _ in 0..6 {
                c.walk(32, EAST).await;
            }
            c.idle_for(Duration::from_millis(800)).await;
            sync(&shoved, "shoved").await;
        })
    };

    // Bob only watches, from his own connection.
    let bob = {
        let (settled, shoved) = (settled.clone(), shoved.clone());
        spawn_peer(addr, "bob", None, move |mut c| async move {
            c.await_chunk(SPAWN_CHUNK).await;
            let before = await_settled(&mut c).await;
            sync(&settled, "settled").await;

            sync(&shoved, "shoved").await;
            let after = await_props(&mut c, PROPS as usize).await;

            let moved: Vec<f32> = after
                .iter()
                .filter_map(|(id, p)| before.get(id).map(|q| dist(*p, *q)))
                .filter(|d| *d > 0.25)
                .collect();
            (before.len(), moved)
        })
    };

    alice.join().expect("alice thread");
    let (tracked, moved) = bob.join().expect("bob thread");
    println!("bob tracked {tracked} props; {} moved after alice's walk", moved.len());
    assert!(
        !moved.is_empty(),
        "walking through {PROPS} rigid bodies moved none of them — the player \
         proxy is not reaching the physics world"
    );
    let furthest = moved.iter().cloned().fold(0.0f32, f32::max);
    assert!(furthest > 0.4, "the largest shove was only {furthest} units");
}

/// What replicating a settling pile actually costs.
///
/// Recorded rather than bounded against the ordinary per-snapshot budget: a few
/// hundred simultaneously-moving bodies is deliberately far outside it, and
/// pretending otherwise would either make the budget meaningless or this test
/// permanently red. The ceiling here only catches a codec regression.
#[test]
fn settling_pile_snapshot_cost_is_recorded() {
    let server = TestServer::start_with("props-cost", |c| {
        c.physics = true;
        c.props = PROPS;
    });
    let mut bytes = 0usize;
    let mut snapshots = 0usize;

    let peer = spawn_peer(server.addr(), "meter", None, move |mut c| async move {
        c.await_chunk(SPAWN_CHUNK).await;
        // Measure while the pile is still resolving — the worst case.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if let ServerMsg::Snapshot { payload, .. } = c.next_msg().await {
                bytes += payload.len();
                snapshots += 1;
            }
        }
        (bytes, snapshots)
    });

    let (bytes, snapshots) = peer.join().expect("meter thread");
    assert!(snapshots > 20, "only {snapshots} snapshots arrived");
    let mean = bytes as f64 / snapshots as f64;
    println!(
        "{PROPS} props: {snapshots} snapshots, {mean:.0} B mean payload, \
         {:.1} KB/s at 20 Hz",
        mean * 20.0 / 1024.0
    );
    // Well above the ordinary 410 B/tick budget, which a settling pile of this
    // size is deliberately outside — but close enough to the ~620 B actually
    // measured that a codec regression cannot slip past. The previous ceiling
    // was 32 kB, which nothing could ever have tripped.
    assert!(
        mean < 3_000.0,
        "mean snapshot payload {mean:.0} B for {PROPS} props looks like a codec \
         regression"
    );
}

/// The pile must not exceed what the codec can carry.
#[test]
fn prop_count_stays_inside_the_entity_ceiling() {
    // MAX_ENTITIES in the snapshot codec. Exceeding it makes `apply` reject the
    // payload outright, which would look like a silent replication failure.
    assert!(
        PROPS as usize + 8 < 4096,
        "{PROPS} props plus players would crowd the 4096-entity snapshot ceiling"
    );
}

/// Prop kinds are announced so a client can build the right body for them.
#[test]
fn props_announce_as_physics_cubes() {
    let server = TestServer::start_with("props-kind", |c| {
        c.physics = true;
        c.props = 8;
    });
    let peer = spawn_peer(server.addr(), "kinds", None, move |mut c| async move {
        c.await_chunk(SPAWN_CHUNK).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if c.props_seen().len() >= 8 {
                return true;
            }
            assert!(std::time::Instant::now() < deadline, "props never announced");
            c.drain_self_pos();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    assert!(peer.join().expect("kinds thread"));
}

/// Keep the unused-import lint honest about `EntityState`.
#[allow(dead_code)]
fn _uses_entity_state(s: &EntityState) -> [f32; 3] {
    s.pos
}
