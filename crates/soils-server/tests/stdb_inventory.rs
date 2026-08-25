//! Inventory survives logout when a database is configured.
//!
//! The point of these is the *handover*: the server owns the inventory during a
//! session, and SpacetimeDB owns it between sessions. Both tests prove it the
//! same way the account tests do — by coming back on a server with a **fresh
//! data directory**, so nothing local could be answering.
//!
//! Auto-skips without a host, like the other SpacetimeDB tests:
//!
//! ```sh
//! SOILS_STDB_URI=http://127.0.0.1:3000 SOILS_STDB_TOKEN=<token> \
//!   cargo test -p soils-server --test stdb_inventory
//! ```

mod common;

use std::time::Duration;

use common::{Client, TestServer};
use soils_protocol::ClientMsg;
use soils_server::StdbConfig;

const SETTLE: Duration = Duration::from_secs(10);
/// Long enough for a logout write to reach the database and come back through
/// the next server's subscription.
const HANDOVER: Duration = Duration::from_secs(6);

fn unique_name(tag: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{tag}{}", n % 1_000_000)
}

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("soils-{tag}-{}", unique_name("")));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The first block kind the server stocked us with.
fn a_held_block(c: &Client) -> soils_sim::ItemKind {
    c.inventory()
        .iter()
        .flatten()
        .map(|s| s.kind)
        .find(|k| k.block().is_some())
        .expect("the server stocks new players with blocks")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inventory_survives_logout_and_a_different_server() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB inventory test");
        return;
    };

    let name = unique_name("keeper");
    let (kind, spent, before) = {
        let one = cfg.clone();
        let server =
            TestServer::start_at_with(fresh_dir("inv-a"), "stdbinv-a", move |c| {
                c.stdb = Some(one);
            });
        let mut a = Client::connect(server.addr()).await;
        a.login(&name).await;
        assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "starter kit");

        // Spend a distinctive number of one block, so what comes back is
        // provably *this* inventory and not a freshly issued starter kit.
        let kind = a_held_block(&a);
        let block = kind.block().unwrap();
        let start = a.count_of(kind);
        let (x, y, z) = (a.spawn[0] as i32, a.spawn[1] as i32, a.spawn[2] as i32);
        for i in 0..5 {
            a.edit([x + i, y - 2, z], block).await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let want = start - 5;
        assert!(
            a.await_inventory(|c| c.count_of(kind) == want, SETTLE).await,
            "five placements must cost five blocks: {} != {want}",
            a.count_of(kind)
        );
        drop(a); // disconnect: the server writes the inventory out
        tokio::time::sleep(HANDOVER).await;
        server.handle.shutdown_and_wait();
        (kind, 5, want)
    };

    // A *different* server, with an empty data directory. Anything it knows
    // about this player came from the database.
    let two = cfg.clone();
    let server = TestServer::start_at_with(fresh_dir("inv-b"), "stdbinv-b", move |c| {
        c.stdb = Some(two);
    });
    let mut b = Client::connect(server.addr()).await;
    b.login(&name).await;
    assert!(
        b.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await,
        "the returning player must be sent an inventory"
    );
    assert!(
        b.await_inventory(|c| c.count_of(kind) == before, SETTLE).await,
        "expected the {spent} spent blocks to still be missing ({before} of {kind:?}), got {}",
        b.count_of(kind)
    );
}

/// The failure this guards is subtle and expensive: if a returning player is
/// re-stocked *in addition* to what they stored, every reconnect mints a new
/// starter kit and the inventory is an infinite source of blocks.
#[tokio::test(flavor = "multi_thread")]
async fn reconnecting_does_not_mint_a_second_starter_kit() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB inventory test");
        return;
    };

    let name = unique_name("nodupe");
    let one = cfg.clone();
    let server = TestServer::start_at_with(fresh_dir("inv-c"), "stdbinv-c", move |c| {
        c.stdb = Some(one);
    });

    let mut a = Client::connect(server.addr()).await;
    a.login(&name).await;
    assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "starter kit");
    let kind = a_held_block(&a);
    let first = a.count_of(kind);
    let total_first: u32 = a.inventory().iter().flatten().map(|s| s.count as u32).sum();
    drop(a);
    tokio::time::sleep(HANDOVER).await;

    let mut b = Client::connect(server.addr()).await;
    b.login(&name).await;
    assert!(b.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "restored");
    // Give any erroneous second stocking time to land before measuring.
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(b.count_of(kind), first, "a reconnect must not add to a held stack");
    let total_second: u32 = b.inventory().iter().flatten().map(|s| s.count as u32).sum();
    assert_eq!(total_second, total_first, "a reconnect must not mint items");
}

/// A player who has never logged out anywhere still gets stocked. Without this,
/// "restore" and "stock" could both be skipped and the bug would look like an
/// empty inventory rather than a missing branch.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_time_player_is_still_stocked() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB inventory test");
        return;
    };

    let server = TestServer::start_at_with(fresh_dir("inv-d"), "stdbinv-d", move |c| {
        c.stdb = Some(cfg);
    });
    let mut a = Client::connect(server.addr()).await;
    a.login(&unique_name("brandnew")).await;
    assert!(
        a.await_inventory(|c| c.inventory().iter().flatten().any(|s| s.kind.block().is_some()), SETTLE)
            .await,
        "a player with nothing stored must be stocked, not left empty"
    );
}

/// Placing what was restored must work — a restored inventory has to be a real
/// inventory the authority will spend from, not just bytes the UI can draw.
#[tokio::test(flavor = "multi_thread")]
async fn restored_items_can_still_be_placed() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB inventory test");
        return;
    };

    let name = unique_name("builder");
    let one = cfg.clone();
    let server = TestServer::start_at_with(fresh_dir("inv-e"), "stdbinv-e", move |c| {
        c.stdb = Some(one);
    });

    let mut a = Client::connect(server.addr()).await;
    a.login(&name).await;
    assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "starter kit");
    drop(a);
    tokio::time::sleep(HANDOVER).await;

    let mut b = Client::connect(server.addr()).await;
    b.login(&name).await;
    b.await_chunk([8, 8, 8]).await;
    assert!(b.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "restored");

    let kind = a_held_block(&b);
    let before = b.count_of(kind);
    let (x, y, z) = (b.spawn[0] as i32, b.spawn[1] as i32, b.spawn[2] as i32);
    let seq = b.edit([x, y - 2, z], kind.block().unwrap()).await;
    assert!(b.edit_verdict(seq).await, "a restored block must be placeable");
    assert!(
        b.await_inventory(|c| c.count_of(kind) == before - 1, SETTLE).await,
        "and placing it must spend it"
    );
}
