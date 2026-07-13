//! Error-path and edge-case robustness: malformed/hostile/degenerate client
//! behavior must be rejected or ignored, never crash or wedge the server. Each
//! test provokes a bad path, then proves the server is still fully functional
//! (a fresh client joins and moves, or the same connection recovers).

mod common;

use common::{Client, TestServer};
use soils_protocol::{ClientMsg, InputFrame, ServerMsg};
use std::time::Duration;

/// The chunk containing the spawn point (shared with `scenarios.rs`).
const SPAWN_CHUNK: [i32; 3] = [8, 8, 8];
/// A voxel within edit reach (Chebyshev ≤ 8) of the spawn eye.
const NEAR_VOXEL: [i32; 3] = [282, 280, 268];

/// Wait for the server to echo any state for `net`, bounded so a wedged server
/// fails the test instead of hanging it. Returns true if the echo arrived.
async fn responds(c: &mut Client, net: u32) -> bool {
    tokio::time::timeout(Duration::from_secs(15), c.await_entity(net, |_| true)).await.is_ok()
}

#[tokio::test]
async fn empty_username_is_rejected_and_server_survives() {
    let server = TestServer::start("rob-empty");
    let mut bad = Client::connect(server.addr()).await;
    bad.send(&ClientMsg::Login { name: String::new(), password: String::new(), signup: true })
        .await;
    let msg = bad
        .recv_until(|m| match m {
            ServerMsg::LoginError { message } => Some(message),
            ServerMsg::Init { .. } => panic!("empty username was accepted"),
            _ => None,
        })
        .await;
    assert!(msg.to_lowercase().contains("username"), "unexpected reason: {msg}");

    // The server is unharmed: a real client still joins and moves.
    let mut good = Client::join(server.addr(), "real").await;
    let net = good.self_entity;
    good.fly(16, 0.0, false).await;
    assert!(responds(&mut good, net).await, "server stopped responding after a bad login");
}

#[tokio::test]
async fn duplicate_username_is_rejected() {
    let server = TestServer::start("rob-dup");
    // First signup claims the name (harness uses an empty password).
    let _alice = Client::join(server.addr(), "dup").await;

    // A second signup for the same name with a *different* password is denied
    // (identical credentials would instead act as a re-login).
    let mut b = Client::connect(server.addr()).await;
    b.send(&ClientMsg::Login { name: "dup".into(), password: "other".into(), signup: true })
        .await;
    let msg = b
        .recv_until(|m| match m {
            ServerMsg::LoginError { message } => Some(message),
            ServerMsg::Init { .. } => panic!("duplicate username was accepted"),
            _ => None,
        })
        .await;
    assert!(msg.to_lowercase().contains("taken"), "unexpected reason: {msg}");
}

#[tokio::test]
async fn login_to_missing_account_is_rejected() {
    let server = TestServer::start("rob-missing");
    let mut c = Client::connect(server.addr()).await;
    c.send(&ClientMsg::Login { name: "ghost".into(), password: "pw".into(), signup: false })
        .await;
    let msg = c
        .recv_until(|m| match m {
            ServerMsg::LoginError { message } => Some(message),
            ServerMsg::Init { .. } => panic!("login to a missing account was accepted"),
            _ => None,
        })
        .await;
    assert!(msg.to_lowercase().contains("account"), "unexpected reason: {msg}");
}

#[tokio::test]
async fn pre_auth_messages_are_ignored_then_connection_works() {
    let server = TestServer::start("rob-preauth");
    let mut c = Client::connect(server.addr()).await;

    // Blast gameplay messages before authenticating — the server must drop them
    // (the auth gate), not panic on a missing player entity.
    let (buttons, flags, yaw) = soils_sim::pack_input(&soils_sim::PlayerInput {
        move_axes: glam::Vec2::new(0.0, 1.0),
        ..Default::default()
    });
    c.send(&ClientMsg::Inputs {
        ack_tick: 0,
        frames: (1..=8).map(|seq| InputFrame { seq, buttons, flags, yaw }).collect(),
    })
    .await;
    c.send(&ClientMsg::Edit { seq: 1, pos: NEAR_VOXEL, value: 5 }).await;
    c.send(&ClientMsg::ViewRadius { radius: 3 }).await;

    // Now log in on the same connection: it must work normally.
    c.login("late").await;
    let net = c.self_entity;
    assert!(responds(&mut c, net).await, "connection was poisoned by pre-auth traffic");
}

#[tokio::test]
async fn edit_flooding_is_rate_capped() {
    let server = TestServer::start("rob-editflood");
    let mut a = Client::join(server.addr(), "alice").await;
    a.await_chunk(SPAWN_CHUNK).await;

    // 100 in-reach edits of a known block, fired back-to-back. All are valid but
    // the per-client edit token bucket (EDIT_RATE = 32) admits only a burst; the
    // rest come back rejected — the same anti-abuse shape as input flooding.
    let mut sent = 0;
    'flood: for dx in 0..5 {
        for dy in 0..5 {
            for dz in 0..4 {
                a.edit([278 + dx, 281 + dy, 264 + dz], 5).await;
                sent += 1;
                if sent == 100 {
                    break 'flood;
                }
            }
        }
    }

    let (mut accepted, mut rejected) = (0u32, 0u32);
    while accepted + rejected < 100 {
        match a.next_msg().await {
            ServerMsg::EditAccepted { .. } => accepted += 1,
            ServerMsg::EditRejected { .. } => rejected += 1,
            _ => {}
        }
    }
    eprintln!("edit flood: {accepted} accepted / {rejected} rejected of 100");
    assert!(accepted >= 1, "no edits applied at all");
    assert!(accepted <= 40, "{accepted} edits applied — rate cap failed (all 100 valid)");
    assert!(rejected >= 40, "only {rejected} rejected — rate cap didn't engage");
}

#[tokio::test]
async fn warp_to_a_new_world_creates_it_on_demand() {
    let server = TestServer::start("rob-warp");
    let mut a = Client::join(server.addr(), "alice").await;
    a.await_chunk(SPAWN_CHUNK).await;

    // Warp to a never-seen world name: the server generates it on demand.
    a.send(&ClientMsg::Warp { world: "a-brand-new-world".into() }).await;
    a.recv_until(|m| match m {
        ServerMsg::Warp { .. } => Some(()),
        _ => None,
    })
    .await;

    // The on-demand world is generated and streamed: its spawn chunk arrives.
    // The default world's copy of this chunk was already consumed above, so this
    // one is unambiguously the new world's — proving terrain was generated (not
    // a crash or an empty stream) after the warp.
    let streamed = tokio::time::timeout(Duration::from_secs(20), a.await_chunk(SPAWN_CHUNK)).await;
    assert!(streamed.is_ok(), "on-demand world never streamed its terrain");
}

#[tokio::test]
async fn oversized_view_radius_is_clamped_and_stable() {
    let server = TestServer::start("rob-radius");
    let mut a = Client::join(server.addr(), "alice").await;

    // Max out the (u8) radius; the server clamps to MAX_RADIUS and must neither
    // OOM nor wedge streaming the world.
    a.send(&ClientMsg::ViewRadius { radius: u8::MAX }).await;

    // Still ticking (a Time broadcast lands) and still integrating movement.
    a.recv_until(|m| match m {
        ServerMsg::Time { .. } => Some(()),
        _ => None,
    })
    .await;
    let net = a.self_entity;
    a.fly(16, 0.0, false).await;
    assert!(responds(&mut a, net).await, "server wedged after an oversized view radius");
}

#[tokio::test]
async fn rapid_connect_disconnect_is_clean() {
    let server = TestServer::start("rob-churn");
    // Churn connections: each joins (spawns a player) then drops (despawn +
    // account cleanup). Leaked entities or a poisoned client map would surface
    // as a later failure.
    for i in 0..16 {
        let c = Client::join(server.addr(), &format!("churn{i}")).await;
        drop(c);
    }
    let mut z = Client::join(server.addr(), "survivor").await;
    let net = z.self_entity;
    z.fly(16, 0.0, false).await;
    assert!(responds(&mut z, net).await, "server unusable after connection churn");
}
