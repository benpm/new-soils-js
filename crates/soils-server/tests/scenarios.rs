//! Scripted multi-client network scenarios against the embedded server: actor
//! visibility, edit replication, movement authority, world isolation, and
//! concurrent chunk generation. These pin the current protocol semantics so
//! the server rework phases (TODO 5–11) can refactor against them.

mod common;

use common::{Client, TestServer};
use soils_protocol::{ClientMsg, ServerMsg};

/// A chunk well below the surface (~y=256): reliably solid, so edits apply.
const DEEP_CHUNK: [i32; 3] = [8, 6, 8];
/// A voxel inside [`DEEP_CHUNK`].
const DEEP_VOXEL: [i32; 3] = [8 * 32, 6 * 32, 8 * 32];

#[tokio::test]
async fn actors_are_visible_and_removed_on_disconnect() {
    let server = TestServer::start("actors");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;

    // A moves; B must observe A at the new position (with velocity) via the
    // periodic ActorUpdate broadcast.
    let target = [a.spawn[0] + 10.0, a.spawn[1], a.spawn[2]];
    a.send(&ClientMsg::Move { pos: target, velocity: [1.0, 0.0, 0.0] }).await;
    let a_id = a.id;
    let seen = b
        .recv_until(|msg| match msg {
            ServerMsg::ActorUpdate { actors } => {
                actors.into_iter().find(|s| s.id == a_id && s.pos == target)
            }
            _ => None,
        })
        .await;
    assert_eq!(seen.velocity, [1.0, 0.0, 0.0]);

    // A disconnects; B must be told the actor is gone.
    drop(a);
    b.recv_until(|msg| match msg {
        ServerMsg::ActorRemove { id } if id == a_id => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn edits_replicate_to_peers_without_echo() {
    let server = TestServer::start("edits");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;

    // Load the target chunk server-side (edits to unloaded chunks are dropped).
    let vol = a.await_chunk(DEEP_CHUNK).await;
    assert!(!vol.is_empty(), "deep chunk should be solid");

    // A's edit reaches B.
    a.send(&ClientMsg::Edit { pos: DEEP_VOXEL, value: 5 }).await;
    let got = b
        .recv_until(|msg| match msg {
            ServerMsg::Edit { pos, value } => Some((pos, value)),
            _ => None,
        })
        .await;
    assert_eq!(got, (DEEP_VOXEL, 5));

    // B replies with its own edit. Per-connection message order means the
    // FIRST edit A ever receives must be B's — proving A got no echo of its
    // own edit (the server excludes the sender from the broadcast).
    let pb = [DEEP_VOXEL[0] + 1, DEEP_VOXEL[1], DEEP_VOXEL[2]];
    b.send(&ClientMsg::Edit { pos: pb, value: 7 }).await;
    let got = a
        .recv_until(|msg| match msg {
            ServerMsg::Edit { pos, value } => Some((pos, value)),
            _ => None,
        })
        .await;
    assert_eq!(got, (pb, 7), "first edit at A must be B's, never an echo of A's own");
}

#[tokio::test]
async fn implausible_moves_are_rejected_and_corrected() {
    let server = TestServer::start("moves");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;

    // A legal step is accepted and becomes visible to B.
    let legal = [a.spawn[0] + 20.0, a.spawn[1], a.spawn[2]];
    a.send(&ClientMsg::Move { pos: legal, velocity: [0.0; 3] }).await;
    let a_id = a.id;
    b.recv_until(|msg| match msg {
        ServerMsg::ActorUpdate { actors } => {
            actors.into_iter().find(|s| s.id == a_id && s.pos == legal).map(|_| ())
        }
        _ => None,
    })
    .await;

    // A teleport far beyond MAX_STEP is rejected: the server snaps A back to
    // the last accepted position...
    let teleport = [legal[0] + 1000.0, legal[1], legal[2]];
    a.send(&ClientMsg::Move { pos: teleport, velocity: [0.0; 3] }).await;
    let corrected = a
        .recv_until(|msg| match msg {
            ServerMsg::Position { pos } => Some(pos),
            _ => None,
        })
        .await;
    assert_eq!(corrected, legal);

    // ...and the teleported position never enters the actor state: whatever
    // update B sees next still reports the last accepted position.
    let seen = b
        .recv_until(|msg| match msg {
            ServerMsg::ActorUpdate { actors } => actors.into_iter().find(|s| s.id == a_id),
            _ => None,
        })
        .await;
    assert_eq!(seen.pos, legal, "rejected teleport must not leak into actor broadcasts");
}

#[tokio::test]
async fn warp_isolates_edits_and_actors_by_world() {
    let server = TestServer::start("warp");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;
    // C stays in "default" as the observer that confirms broadcasts fired.
    let mut c = Client::join(server.addr(), "carol").await;

    a.await_chunk(DEEP_CHUNK).await;

    // B warps away; same-world clients are told B's actor left.
    b.send(&ClientMsg::Warp { world: "elsewhere".into() }).await;
    b.recv_until(|msg| match msg {
        ServerMsg::Warp { .. } => Some(()),
        _ => None,
    })
    .await;
    let b_id = b.id;
    a.recv_until(|msg| match msg {
        ServerMsg::ActorRemove { id } if id == b_id => Some(()),
        _ => None,
    })
    .await;

    // A edits in "default" while B is elsewhere. C (same world) receiving it
    // proves the broadcast fired; B must not get it.
    a.send(&ClientMsg::Edit { pos: DEEP_VOXEL, value: 5 }).await;
    c.recv_until(|msg| match msg {
        ServerMsg::Edit { pos, value } if pos == DEEP_VOXEL && value == 5 => Some(()),
        _ => None,
    })
    .await;

    // Fence: the global broadcast stream is FIFO per subscriber, and Time goes
    // to every world. C notes the first daytime after the edit; once B has seen
    // a Time at least that late, B's forwarder has provably already processed
    // (and dropped) the edit broadcast — closing the race with the warp back.
    let fence = c
        .recv_until(|msg| match msg {
            ServerMsg::Time { daytime } => Some(daytime),
            _ => None,
        })
        .await;
    b.recv_until(|msg| match msg {
        ServerMsg::Time { daytime } if daytime >= fence => Some(()),
        _ => None,
    })
    .await;

    // B warps back; the next edit must arrive, and per-connection ordering
    // means the FIRST edit B ever receives is this one — the edit made while
    // B was elsewhere never leaked.
    b.send(&ClientMsg::Warp { world: "default".into() }).await;
    b.recv_until(|msg| match msg {
        ServerMsg::Warp { .. } => Some(()),
        _ => None,
    })
    .await;
    let pb = [DEEP_VOXEL[0] + 1, DEEP_VOXEL[1], DEEP_VOXEL[2]];
    a.send(&ClientMsg::Edit { pos: pb, value: 7 }).await;
    let got = b
        .recv_until(|msg| match msg {
            ServerMsg::Edit { pos, value } => Some((pos, value)),
            _ => None,
        })
        .await;
    assert_eq!(got, (pb, 7), "edit made while B was in another world leaked through");
}

#[tokio::test]
async fn concurrent_requests_serve_identical_chunks() {
    let server = TestServer::start("concurrent");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;

    // Both clients' join bursts race over the same fresh region. Generation
    // runs off the tick, so the adoption guard must dedupe: both clients get
    // byte-identical chunks for every position.
    let wave: Vec<[i32; 3]> =
        (4..8).flat_map(|x| (6..9).map(move |y| [x, y, 8])).collect();
    let (got_a, got_b) =
        tokio::join!(a.collect_chunks(&wave), b.collect_chunks(&wave));
    for pos in &wave {
        assert_eq!(
            got_a.get(pos),
            got_b.get(pos),
            "chunk {pos:?} differs between concurrent clients"
        );
    }
}

#[tokio::test]
async fn moving_restreams_ahead_and_unloads_behind() {
    let server = TestServer::start("subs");
    let mut a = Client::join(server.addr(), "alice").await;

    // Let part of the join burst land, then march east well past the
    // hysteresis margin (each step stays under the anti-teleport MAX_STEP).
    a.await_chunk([8, 8, 8]).await;
    for i in 1..=8 {
        let pos = [282.0 + 60.0 * i as f32, 285.0, 268.0];
        a.send(&ClientMsg::Move { pos, velocity: [0.0; 3] }).await;
    }

    // 480 voxels east = 15 chunks: the spawn-side column (x <= 8) is far
    // outside radius+1 of the new center, so it must unload...
    a.recv_until(|msg| match msg {
        ServerMsg::ChunkUnload { pos } if pos[0] <= 8 => Some(()),
        _ => None,
    })
    .await;
    // ...and terrain around the destination (x = 282+480 >> 5 = 23) streams
    // in without any client request.
    a.recv_until(|msg| match msg {
        ServerMsg::Bundle { chunks } if chunks.iter().any(|c| c.pos[0] >= 22) => Some(()),
        ServerMsg::Chunk { pos, .. } if pos[0] >= 22 => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn edits_survive_server_restart_via_dirty_flush() {
    let dir =
        std::env::temp_dir().join(format!("soils-test-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // Session 1: edit a voxel, then shut down. Edits only mark chunks dirty
    // (no per-edit persistence); the shutdown flush must save them.
    {
        let server = TestServer::start_at(dir.clone(), "restart");
        let mut a = Client::join(server.addr(), "alice").await;
        a.await_chunk(DEEP_CHUNK).await;
        a.send(&ClientMsg::Edit { pos: DEEP_VOXEL, value: 5 }).await;
        // Round-trip: our own edit isn't echoed, so use a second client's edit
        // as the fence that the server processed ours.
        let mut b = Client::join(server.addr(), "bob").await;
        b.send(&ClientMsg::Edit {
            pos: [DEEP_VOXEL[0] + 1, DEEP_VOXEL[1], DEEP_VOXEL[2]],
            value: 6,
        })
        .await;
        a.recv_until(|msg| match msg {
            ServerMsg::Edit { value: 6, .. } => Some(()),
            _ => None,
        })
        .await;
        // TestServer::drop shuts down and waits for the flush.
    }

    // Session 2: a fresh server on the same data dir must serve the edit.
    let server = TestServer::start_at(dir.clone(), "restart2");
    let mut c = Client::join(server.addr(), "carol").await;
    let vol = c.await_chunk(DEEP_CHUNK).await;
    assert_eq!(vol.get(0, 0, 0), 5, "edit lost across restart");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
