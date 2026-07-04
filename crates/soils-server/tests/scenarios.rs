//! Scripted multi-client network scenarios against the embedded server: actor
//! visibility, movement authority (inputs, not positions), edit authority
//! (seq/ack), world isolation, subscriptions, and persistence. These pin the
//! protocol semantics so later phases can refactor against them.

mod common;

use common::{Client, TestServer};
use soils_protocol::{ClientMsg, InputFrame, ServerMsg};

/// The chunk containing the spawn point (players spawn ~29 voxels above the
/// surface, so nearby editable space is air — placements work, reach holds).
const SPAWN_CHUNK: [i32; 3] = [8, 8, 8];
/// A voxel within edit reach (Chebyshev ≤ 8) of the spawn eye position.
const NEAR_VOXEL: [i32; 3] = [282, 280, 268];
/// `NEAR_VOXEL`'s coordinates within `SPAWN_CHUNK`.
const NEAR_LOCAL: (i32, i32, i32) = (26, 24, 12);

#[tokio::test]
async fn entities_move_by_inputs_and_despawn_on_disconnect() {
    let server = TestServer::start("actors");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;
    let (a_net, spawn) = (a.self_entity, a.spawn);

    // B sees A's player entity spawn without A doing anything (server owns
    // entities; kind comes from the shared registry).
    b.recv_until(|msg| match msg {
        ServerMsg::EntitySpawn { id, kind, .. }
            if id == a_net && kind == soils_sim::KIND_PLAYER =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // A holds forward for 16 ticks facing -Z: the *server* integrates this to
    // 16/64 s × 8 u/s = 2.0 units, and B observes exactly that.
    a.fly(16, 0.0, false).await;
    let moved = b
        .recv_until(|msg| match msg {
            ServerMsg::EntityUpdate { entities } => entities
                .into_iter()
                .find(|s| s.id == a_net && s.pos[2] < spawn[2] - 1.5),
            _ => None,
        })
        .await;
    assert!(
        (moved.pos[2] - (spawn[2] - 2.0)).abs() < 0.5
            && (moved.pos[0] - spawn[0]).abs() < 0.1
            && (moved.pos[1] - spawn[1]).abs() < 0.1,
        "server-integrated position should be ~2 units -Z from spawn, got {:?}",
        moved.pos
    );

    // A disconnects; B must see the entity despawn.
    drop(a);
    b.recv_until(|msg| match msg {
        ServerMsg::EntityDespawn { id } if id == a_net => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn critters_replicate_and_wander() {
    let server = TestServer::start_with("critters", |cfg| cfg.critters = 3);
    let mut a = Client::join(server.addr(), "alice").await;

    // All three ambient critters spawn into our interest, with registry kind.
    let mut critters = std::collections::HashSet::new();
    while critters.len() < 3 {
        let id = a
            .recv_until(|msg| match msg {
                ServerMsg::EntitySpawn { id, kind, .. }
                    if kind == soils_sim::KIND_CRITTER =>
                {
                    Some(id)
                }
                _ => None,
            })
            .await;
        critters.insert(id);
    }

    // They are simulated server-side: one of them measurably moves (they walk
    // and fall under gravity once their terrain is resident).
    let watch = *critters.iter().next().unwrap();
    let first = a
        .recv_until(|msg| match msg {
            ServerMsg::EntityUpdate { entities } => {
                entities.into_iter().find(|s| s.id == watch).map(|s| s.pos)
            }
            _ => None,
        })
        .await;
    a.recv_until(|msg| match msg {
        ServerMsg::EntityUpdate { entities } => entities
            .into_iter()
            .find(|s| {
                s.id == watch
                    && (s.pos[0] - first[0]).abs()
                        + (s.pos[1] - first[1]).abs()
                        + (s.pos[2] - first[2]).abs()
                        > 0.5
            })
            .map(|_| ()),
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn input_flooding_cannot_speed_hack() {
    let server = TestServer::start("flood");
    let mut a = Client::join(server.addr(), "alice").await;
    let spawn = a.spawn;

    // 640 forward frames in one message = 10 s of movement (80 units) if the
    // server trusted them. The input token bucket admits only a small burst;
    // the rest are dropped, not queued.
    let frames: Vec<InputFrame> = (1..=640)
        .map(|seq| {
            let input = soils_sim::PlayerInput {
                move_axes: glam::Vec2::new(0.0, 1.0),
                yaw: 0.0,
                ..Default::default()
            };
            let (buttons, flags, yaw) = soils_sim::pack_input(&input);
            InputFrame { seq, buttons, flags, yaw }
        })
        .collect();
    a.send(&ClientMsg::Inputs { frames }).await;

    // A Time broadcast (1 Hz) guarantees the flood was processed ticks ago;
    // the next self-position echo is post-flood.
    a.recv_until(|msg| match msg {
        ServerMsg::Time { .. } => Some(()),
        _ => None,
    })
    .await;
    let pos = a.await_self_pos().await;
    let moved = (pos[2] - spawn[2]).abs();
    assert!(
        moved < 8.0,
        "flooded inputs moved the player {moved} units — rate cap failed (80 if fully trusted)"
    );
}

#[tokio::test]
async fn edits_are_acked_and_replicate_without_echo() {
    let server = TestServer::start("edits");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;
    a.await_chunk(SPAWN_CHUNK).await;

    // A's in-reach edit is accepted (seq round-trip) and reaches B as a plain
    // Edit broadcast.
    let seq = a.edit(NEAR_VOXEL, 5).await;
    a.recv_until(|msg| match msg {
        ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(()),
        _ => None,
    })
    .await;
    let got = b
        .recv_until(|msg| match msg {
            ServerMsg::Edit { pos, value } => Some((pos, value)),
            _ => None,
        })
        .await;
    assert_eq!(got, (NEAR_VOXEL, 5));

    // B replies with its own edit. Per-connection order means the FIRST plain
    // edit A receives must be B's — no echo of A's own.
    let pb = [NEAR_VOXEL[0] + 1, NEAR_VOXEL[1], NEAR_VOXEL[2]];
    b.edit(pb, 7).await;
    let got = a
        .recv_until(|msg| match msg {
            ServerMsg::Edit { pos, value } => Some((pos, value)),
            _ => None,
        })
        .await;
    assert_eq!(got, (pb, 7), "first edit at A must be B's, never an echo of A's own");
}

#[tokio::test]
async fn out_of_reach_edits_are_rejected() {
    let server = TestServer::start("reach");
    let mut a = Client::join(server.addr(), "alice").await;
    a.await_chunk(SPAWN_CHUNK).await;

    // ~60 voxels below the player: resident, but far outside REACH=8.
    let seq = a.edit([282, 224, 268], 5).await;
    a.recv_until(|msg| match msg {
        ServerMsg::EditRejected { seq: s } if s == seq => Some(()),
        _ => None,
    })
    .await;

    // Unknown block ids are rejected too, even in reach.
    let seq = a.edit(NEAR_VOXEL, 250).await;
    a.recv_until(|msg| match msg {
        ServerMsg::EditRejected { seq: s } if s == seq => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn warp_isolates_edits_and_actors_by_world() {
    let server = TestServer::start("warp");
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;
    // C stays in "default" as the observer that confirms broadcasts fired.
    let mut c = Client::join(server.addr(), "carol").await;

    a.await_chunk(SPAWN_CHUNK).await;

    // B warps away; same-world clients see B's entity leave interest.
    b.send(&ClientMsg::Warp { world: "elsewhere".into() }).await;
    b.recv_until(|msg| match msg {
        ServerMsg::Warp { .. } => Some(()),
        _ => None,
    })
    .await;
    let b_net = b.self_entity;
    a.recv_until(|msg| match msg {
        ServerMsg::EntityDespawn { id } if id == b_net => Some(()),
        _ => None,
    })
    .await;

    // A edits in "default" while B is elsewhere. C (same world) receiving it
    // proves the broadcast fired; B must not get it.
    a.edit(NEAR_VOXEL, 5).await;
    c.recv_until(|msg| match msg {
        ServerMsg::Edit { pos, value } if pos == NEAR_VOXEL && value == 5 => Some(()),
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
    let pb = [NEAR_VOXEL[0] + 1, NEAR_VOXEL[1], NEAR_VOXEL[2]];
    a.edit(pb, 7).await;
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
    a.await_chunk(SPAWN_CHUNK).await;

    // Shrinking the view radius unloads the shell beyond radius+1 on its own.
    a.send(&ClientMsg::ViewRadius { radius: 2 }).await;
    a.recv_until(|msg| match msg {
        ServerMsg::ChunkUnload { pos }
            if (pos[0] - 8).abs().max((pos[1] - 8).abs()).max((pos[2] - 8).abs()) > 3 =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // Sprint-fly east for 128 ticks = 2 s × 32 u/s = 64 units = 2 chunks: the
    // window recenters, streaming terrain ahead...
    a.fly(128, -std::f32::consts::FRAC_PI_2, true).await;
    a.recv_until(|msg| match msg {
        ServerMsg::Bundle { chunks } if chunks.iter().any(|c| c.pos[0] >= 11) => Some(()),
        ServerMsg::Chunk { pos, .. } if pos[0] >= 11 => Some(()),
        _ => None,
    })
    .await;
    // ...and dropping the west edge.
    a.recv_until(|msg| match msg {
        ServerMsg::ChunkUnload { pos } if pos[0] <= 6 => Some(()),
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
        a.await_chunk(SPAWN_CHUNK).await;
        let seq = a.edit(NEAR_VOXEL, 5).await;
        a.recv_until(|msg| match msg {
            ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(()),
            _ => None,
        })
        .await;
        // TestServer::drop shuts down synchronously and waits for the flush.
    }

    // Session 2: a fresh server on the same data dir must serve the edit.
    let server = TestServer::start_at(dir.clone(), "restart2");
    let mut c = Client::join(server.addr(), "carol").await;
    let vol = c.await_chunk(SPAWN_CHUNK).await;
    assert_eq!(
        vol.get(NEAR_LOCAL.0, NEAR_LOCAL.1, NEAR_LOCAL.2),
        5,
        "edit lost across restart"
    );
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
