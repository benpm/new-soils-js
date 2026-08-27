//! Draw-distance stress: one client at a large view radius, against the deep
//! terrain the continental octave produces.
//!
//! The point of the exercise is occlusion culling. A radius-8 subscription is
//! 17³ = 4913 chunks, and in terrain with hundreds of voxels of relief most of
//! them are buried where nothing can see or reach them. The server generates
//! all of them but withholds the sealed ones (`World::sealed`), so what the
//! client pays to receive, generate, mesh and keep resident is only the shell
//! it can actually look at.
//!
//! Two properties matter and both are asserted here:
//!
//! * **Safety** — a chunk is only ever withheld when every received chunk
//!   beside it shows a solid wall towards it. If that ever fails the player
//!   sees a hole into nothing, or walks into unloaded space.
//! * **Payoff** — the cull actually removes a large share of the set. Without
//!   this the feature could silently degrade to a no-op and only show up as a
//!   frame-rate regression much later.

mod common;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use common::{Client, TestServer};
use glam::IVec3;
use soils_protocol::{CHUNK_SIZE, ChunkVolume, ClientMsg, ServerMsg};

/// Radius for the routine run. Small enough that a debug-profile worldgen
/// burst finishes in about the time the other multi-client tests take, deep
/// enough that the cube has a buried interior to hide.
const RADIUS: i32 = 5;
/// The radius the server clamps to (`MAX_RADIUS`) — the worst case a client
/// can ask for, and what the ignored stress run uses. 4913 chunks is minutes
/// of generation in a debug build, which is why it is opt-in.
const MAX_RADIUS: i32 = 8;
/// Give up waiting once no *manifest* has arrived for this long: the
/// subscription is served, or something is wedged.
///
/// Measured on manifests alone, not on the connection: snapshots go out every
/// tick, so the socket itself is never idle and a plain read timeout would
/// wait forever.
const QUIET: Duration = Duration::from_millis(1500);
/// Per-read timeout while draining. Short, so the manifest-quiet check gets
/// re-evaluated promptly between snapshots.
const READ_STEP: Duration = Duration::from_millis(200);
/// Overall ceiling. Generous — a debug-profile worldgen burst of thousands of
/// chunks is not fast — and only there so a wedge fails as a test rather than
/// hanging CI.
const DEADLINE: Duration = Duration::from_secs(600);

/// The six axis neighbours of a chunk.
const DIRS: [IVec3; 6] = [
    IVec3::new(1, 0, 0),
    IVec3::new(-1, 0, 0),
    IVec3::new(0, 1, 0),
    IVec3::new(0, -1, 0),
    IVec3::new(0, 0, 1),
    IVec3::new(0, 0, -1),
];

/// Is the boundary layer of `vol` facing `dir` solid all the way across? This
/// is the client-side mirror of the server's `face_mask`, written out
/// independently so a bug in that one cannot make the test agree with it.
fn face_solid(vol: &ChunkVolume, dir: IVec3) -> bool {
    let n = CHUNK_SIZE - 1;
    (0..CHUNK_SIZE).all(|a| {
        (0..CHUNK_SIZE).all(|b| {
            let (x, y, z) = match (dir.x, dir.y, dir.z) {
                (1, 0, 0) => (n, a, b),
                (-1, 0, 0) => (0, a, b),
                (0, 1, 0) => (a, n, b),
                (0, -1, 0) => (a, 0, b),
                (0, 0, 1) => (a, b, n),
                _ => (a, b, 0),
            };
            vol.get(x, y, z) != soils_protocol::AIR
        })
    })
}

#[tokio::test]
async fn draw_distance_streams_only_what_can_be_seen() {
    stream_and_check("drawdist", RADIUS, 0.12).await;
}

/// The real stress run: the maximum radius the protocol allows. Minutes of
/// worldgen in a debug build, far less in release —
/// `cargo test --release -p soils-server --test draw_distance -- --ignored
/// --nocapture` prints the cull ratio and the wall clock.
#[tokio::test]
#[ignore]
async fn max_draw_distance_stress() {
    stream_and_check("drawdist-max", MAX_RADIUS, 0.20).await;
}

/// Subscribe at `radius`, drain until quiet, then check that nothing visible
/// was withheld and that at least `min_culled` of the cube was.
async fn stream_and_check(tag: &str, radius: i32, min_culled: f64) {
    let server = TestServer::start(tag);
    let mut c = Client::join(server.addr(), "surveyor").await;

    let eye = c.current_self_pos().await;
    let center = IVec3::new(eye[0] as i32 >> 5, eye[1] as i32 >> 5, eye[2] as i32 >> 5);

    let started = Instant::now();
    c.send(&ClientMsg::ViewRadius { radius: radius as u8, full_streams: false }).await;

    // Drain until the server stops pushing. Chunks are not requested one by
    // one — the server owns the subscription — so "done" is "quiet", not "the
    // set I asked for arrived": the whole point is that some of it never will.
    let mut received: HashMap<IVec3, ChunkVolume> = HashMap::new();
    let mut last_manifest = Instant::now();
    while last_manifest.elapsed() < QUIET {
        if let Ok(msg) = tokio::time::timeout(READ_STEP, c.next_msg()).await
            && let ServerMsg::Manifest { chunks } = &msg
        {
            last_manifest = Instant::now();
            for info in chunks {
                let vol = c.materialize(info);
                received.insert(IVec3::from_array(info.pos()), vol);
            }
        }
        assert!(
            started.elapsed() < DEADLINE,
            "still streaming after {DEADLINE:?}; {} chunks so far",
            received.len()
        );
    }
    let elapsed = started.elapsed();

    // The subscription the server built around the player.
    let mut wanted: HashSet<IVec3> = HashSet::new();
    for dx in -radius..=radius {
        for dy in -radius..=radius {
            for dz in -radius..=radius {
                wanted.insert(center + IVec3::new(dx, dy, dz));
            }
        }
    }

    // The client may hold chunks from the smaller default radius it had before
    // the request, and the subscription recentres if the player drifts; judge
    // only the cube we know about.
    let inside: HashSet<IVec3> = received.keys().copied().filter(|p| wanted.contains(p)).collect();
    let withheld: Vec<IVec3> = wanted.difference(&inside).copied().collect();

    eprintln!(
        "radius {radius}: {} of {} chunks sent, {} withheld ({:.1}% culled) in {elapsed:?}",
        inside.len(),
        wanted.len(),
        withheld.len(),
        withheld.len() as f64 / wanted.len() as f64 * 100.0,
    );

    // --- safety: nothing visible was withheld -------------------------------
    //
    // For every chunk the client did get, any neighbour it did *not* get must
    // be hidden behind a solid wall. That is exactly the invariant the server
    // claims when it culls, checked from the receiving end against the voxels
    // the client actually holds.
    let missing: HashSet<IVec3> = withheld.iter().copied().collect();
    let mut holes = Vec::new();
    for (&pos, vol) in &received {
        for dir in DIRS {
            let n = pos + dir;
            if !missing.contains(&n) {
                continue;
            }
            if !face_solid(vol, dir) {
                holes.push((pos, n));
            }
        }
    }
    assert!(
        holes.is_empty(),
        "{} withheld chunks are visible through a hole in their neighbour, e.g. {:?}",
        holes.len(),
        &holes[..holes.len().min(5)]
    );

    // --- payoff: the cull is worth having -----------------------------------
    //
    // The centre of the cube in this terrain is mostly underground. The bar is
    // deliberately low: it should catch the cull regressing to a no-op without
    // pinning a ratio that a legitimate terrain retune would move.
    let culled_frac = withheld.len() as f64 / wanted.len() as f64;
    assert!(
        culled_frac > min_culled,
        "occlusion culling withheld only {:.1}% of a radius-{radius} subscription \
         (expected over {:.0}%); it is supposed to hide the buried interior",
        culled_frac * 100.0,
        min_culled * 100.0,
    );

    // And it must not have eaten the world: the player's own surroundings are
    // always sent (`CULL_KEEP`), so the shell around them is intact.
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                let p = center + IVec3::new(dx, dy, dz);
                assert!(received.contains_key(&p), "chunk {p:?} beside the player was withheld");
            }
        }
    }
}

// Note on the exposure path: breaking a seal from a client is impossible by
// construction, so it is pinned as a unit test on `World` instead
// (`breaking_a_boundary_layer_unseals_the_chunk_behind_it`). `CULL_KEEP` keeps
// the chunks around the player subscribed and edit reach is a handful of
// voxels, so the nearest withheld chunk is always tens of voxels out of range —
// a player can never put a hole in a layer that is hiding one.
