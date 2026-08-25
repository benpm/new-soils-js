//! A login flood must cost the flooder, not the server.
//!
//! Password checking is Argon2id, which is memory-hard *by design* — that is
//! what makes a stolen verifier expensive to crack, and it is also why one
//! thread per pending login hands anyone who can open a socket an unbounded
//! memory amplifier. These tests pin the properties that make a flood
//! survivable from outside the process: the tick never stalls, and every
//! client gets an answer. The concurrency cap itself is asserted in `app.rs`,
//! where the pool can be observed directly.
//!
//! Neither needs SpacetimeDB: the pool sits in front of both account stores.

mod common;

use common::{Client, TestServer};
use soils_protocol::{ClientMsg, ServerMsg};
use std::time::Duration;

/// About the size of a real join burst — the concurrent-client tests join a
/// hundred at once — and well inside `AUTH_BACKLOG`.
const FLOOD: usize = 96;

/// Establish every connection first, then fire the passwords together — the
/// shape an actual denial of service takes, and the only version of this that
/// measures hashing rather than TCP setup.
async fn flood(addr: std::net::SocketAddr, victim: &str) -> Vec<Client> {
    let mut clients = Vec::new();
    for _ in 0..FLOOD {
        clients.push(Client::connect(addr).await);
    }
    for c in &mut clients {
        c.send(&ClientMsg::Login {
            name: victim.to_string(),
            password: "not-it".into(),
            signup: false,
            protocol: soils_protocol::PROTOCOL_VERSION,
        })
        .await;
    }
    clients
}

/// Every client in a flood gets an answer. Sounds trivial; it is the property
/// the pool most easily breaks — an honest hundred-player join burst must
/// queue, not be turned away, and a request that is neither queued nor
/// answered leaves that client hanging forever.
///
/// The backlog is deliberately far larger than this flood. A queued request is
/// a name and a password; the *hash* is what costs 19 MB, and that is bounded
/// by the worker count instead (see `the_auth_pool_bounds_concurrent_hashing`
/// in `app.rs`, which asserts it directly — it cannot be seen from out here).
#[tokio::test(flavor = "multi_thread")]
async fn every_login_in_a_flood_is_answered() {
    let server = TestServer::start("authflood-refuse");
    let addr = server.addr();

    let mut victim = Client::connect(addr).await;
    victim.login("floodtarget").await;

    let mut clients = flood(addr, "floodtarget").await;

    let mut busy = 0;
    for c in &mut clients {
        let message = tokio::time::timeout(
            Duration::from_secs(60),
            c.recv_until(|m| match m {
                ServerMsg::LoginError { message } => Some(message),
                _ => None,
            }),
        )
        .await
        .expect("every flooding client must get an answer, not hang");
        if message.contains("busy") {
            busy += 1;
        }
    }
    assert_eq!(
        busy, 0,
        "{busy}/{FLOOD} logins were refused as 'server busy'; a burst this size is an          ordinary join, and the backlog exists so it queues instead"
    );
}

/// The other half of the contract: whatever the pool *does* accept still runs
/// off the tick thread. Measured as the longest gap between consecutive ticks,
/// because total elapsed time is not discriminating — an earlier version of
/// this measurement passed with the hashing back inline.
#[tokio::test(flavor = "multi_thread")]
async fn a_login_flood_does_not_stall_the_tick() {
    let server = TestServer::start("authflood");
    let addr = server.addr();

    let mut victim = Client::connect(addr).await;
    victim.login("stalltarget").await;
    let mut watcher = Client::connect(addr).await;
    watcher.login("stallwatch").await;
    watcher.await_server_ticks(4).await;

    let clients = flood(addr, "stalltarget").await;

    /// Two Argon2 verifies would exceed this; a flood of them would bury it.
    const MAX_GAP: Duration = Duration::from_millis(250);
    let mut worst = Duration::ZERO;
    for _ in 0..40 {
        let t = std::time::Instant::now();
        watcher.await_server_ticks(1).await;
        worst = worst.max(t.elapsed());
    }
    assert!(
        worst < MAX_GAP,
        "the tick stalled for {worst:?} under a login flood; \
         password hashing is back on the tick thread"
    );
    drop(clients);
}
