//! Putting items into a block and getting them back out, against the embedded
//! server.
//!
//! The point of these is conservation and authority. A container is the first
//! place items live outside a player, so every test here asks the same
//! question in a different way: did anything get duplicated, and did anything
//! disappear? The client is a mirror throughout — every assertion reads what
//! the server pushed, never what the test computed.

mod common;

use std::time::Duration;

use common::{Client, TestServer};
use soils_protocol::{ClientMsg, SlotRef};
use soils_sim::ItemKind;

/// Long enough for a place, an open round-trip, and a settle on a loaded box.
const SETTLE: Duration = Duration::from_secs(10);

/// Edits are rate-limited server-side (`EDIT_RATE`, 32/s); sending faster just
/// gets them dropped, which reads as a logic failure rather than a pacing one.
async fn edit_paced(c: &mut Client, pos: [i32; 3], value: u8) {
    c.edit(pos, value).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
}

fn block_id(c: &mut Client, name: &str) -> u8 {
    c.generator().1.id_of(name).unwrap_or_else(|| panic!("{name} must exist in blocks.yaml"))
}

/// A voxel just under the player's feet, once the server can actually be edited
/// there.
///
/// Edits are refused against a chunk the server has not made resident, and
/// after a join it has not yet: the subscription streams in over the next few
/// ticks. Waiting for the manifest entry is waiting for exactly that — the
/// server only lists a chunk it has adopted.
async fn work_voxel(c: &mut Client) -> [i32; 3] {
    let spawn = c.spawn;
    let at = [spawn[0].floor() as i32, spawn[1].floor() as i32 - 2, spawn[2].floor() as i32];
    let _ = c.await_chunk([at[0] >> 5, at[1] >> 5, at[2] >> 5]).await;
    at
}

/// Place a crate one block below the player's feet and open it. Returns the
/// crate's position and the block id used.
async fn open_a_crate(c: &mut Client) -> ([i32; 3], u8) {
    assert!(
        c.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await,
        "the server must stock a new player before anything can be stored"
    );
    let crate_id = block_id(c, "Wooden Crate");
    let at = work_voxel(c).await;
    c.place(at, crate_id).await;

    c.send(&ClientMsg::OpenContainer { pos: at }).await;
    assert!(
        c.await_inventory(|c| c.container().is_some(), SETTLE).await,
        "placing a container block and asking to open it must produce a ContainerUpdate"
    );
    (at, crate_id)
}

#[tokio::test]
async fn a_crate_holds_what_is_put_into_it_and_gives_it_back() {
    let server = TestServer::start("chest-roundtrip");
    let mut a = Client::join(server.addr(), "alice").await;
    let (_, _) = open_a_crate(&mut a).await;

    let kind = ItemKind::Block(block_id(&mut a, "Cobblestone"));
    let held = a.count_of(kind);
    assert!(held > 0, "the starter kit includes Cobblestone");

    let slot = a.pack_slot_of(kind).expect("a pack slot holds it");
    a.send(&ClientMsg::TransferItem { from: SlotRef::Pack(slot), count: 10 }).await;
    // Both sides, in one wait: the container update is sent immediately and the
    // inventory update at the end of the tick, so checking them separately
    // reads the pack before it has been told anything.
    assert!(
        a.await_inventory(|c| c.container_count(kind) == 10 && c.count_of(kind) == held - 10, SETTLE)
            .await,
        "the crate should hold 10 and the pack be short 10; crate {:?}, pack {}",
        a.container(),
        a.count_of(kind),
    );

    let cslot = a.container_slot_of(kind).expect("the crate holds it");
    a.send(&ClientMsg::TransferItem { from: SlotRef::Container(cslot), count: 10 }).await;
    assert!(
        a.await_inventory(|c| c.count_of(kind) == held && c.container_count(kind) == 0, SETTLE)
            .await,
        "taking it back must restore the pack exactly and empty the crate"
    );
}

/// The failure this whole file exists to catch: an item that is in both places
/// at once, or in neither.
#[tokio::test]
async fn a_transfer_never_creates_or_destroys_an_item() {
    let server = TestServer::start("chest-conserve");
    let mut a = Client::join(server.addr(), "alice").await;
    open_a_crate(&mut a).await;

    let kind = ItemKind::Block(block_id(&mut a, "Log"));
    let total = a.count_of(kind);
    assert!(total > 0);

    // Several partial moves in both directions, each one settled before the
    // next so a dropped update cannot be mistaken for a lost item.
    for (i, count) in [7u16, 30, 3, 64, 1].into_iter().enumerate() {
        let from = if i % 2 == 0 {
            SlotRef::Pack(a.pack_slot_of(kind).expect("pack holds some"))
        } else {
            SlotRef::Container(a.container_slot_of(kind).expect("crate holds some"))
        };
        let before = a.count_of(kind);
        a.send(&ClientMsg::TransferItem { from, count }).await;
        assert!(
            a.await_inventory(|c| c.count_of(kind) != before, SETTLE).await,
            "transfer {i} moved nothing"
        );
        assert_eq!(
            a.count_of(kind) + a.container_count(kind),
            total,
            "after transfer {i}: pack {} + crate {} should still be {total}",
            a.count_of(kind),
            a.container_count(kind),
        );
    }
}

/// A chest that ate its contents on break would be worse than one that never
/// opened.
#[tokio::test]
async fn breaking_a_full_crate_spills_everything_and_closes_it() {
    let server = TestServer::start("chest-break");
    let mut a = Client::join(server.addr(), "alice").await;
    let (at, _) = open_a_crate(&mut a).await;

    let kind = ItemKind::Block(block_id(&mut a, "Grass"));
    let slot = a.pack_slot_of(kind).expect("a pack slot holds it");
    a.send(&ClientMsg::TransferItem { from: SlotRef::Pack(slot), count: 25 }).await;
    assert!(a.await_inventory(|c| c.container_count(kind) == 25, SETTLE).await);

    let dropped_before = a.items_seen().len();
    edit_paced(&mut a, at, 0).await;

    assert!(
        a.await_inventory(|c| c.container().is_none(), SETTLE).await,
        "breaking the block must close it for everyone viewing it"
    );
    // Two spawns: the crate itself, and the 25 Grass it was holding.
    assert!(
        a.await_inventory(|c| c.items_seen().len() >= dropped_before + 2, SETTLE).await,
        "the contents must come back out as world items, saw {:?}",
        a.items_seen()
    );
    let spilled: u32 = a
        .items_seen()
        .into_iter()
        .filter(|(_, s)| s.kind == kind)
        .map(|(_, s)| s.count as u32)
        .sum();
    assert_eq!(spilled, 25, "every stored item must come back, exactly once");
}

#[tokio::test]
async fn an_ordinary_block_is_not_a_container() {
    let server = TestServer::start("chest-plain");
    let mut a = Client::join(server.addr(), "alice").await;
    assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await);

    let stone = block_id(&mut a, "Cobblestone");
    let at = work_voxel(&mut a).await;
    a.place(at, stone).await;

    a.send(&ClientMsg::OpenContainer { pos: at }).await;
    // Nothing comes back — and "nothing" has to be proven by waiting, not by
    // asserting immediately.
    assert!(
        !a.await_inventory(|c| c.container().is_some(), Duration::from_secs(2)).await,
        "a block with no container spec must not open"
    );
}

/// Reach is re-checked on every transfer, not just on open — otherwise a chest
/// stays lootable for as long as the client keeps quiet about walking away.
#[tokio::test]
async fn a_crate_out_of_reach_cannot_be_looted() {
    let server = TestServer::start("chest-reach");
    let mut a = Client::join(server.addr(), "alice").await;
    let _ = open_a_crate(&mut a).await;

    let kind = ItemKind::Block(block_id(&mut a, "Leaves"));
    let held = a.count_of(kind);

    // Fly well past REACH (8 voxels) without telling the server anything about
    // the container.
    a.fly(120, 0.0, true).await;

    let slot = a.pack_slot_of(kind).expect("pack holds it");
    a.send(&ClientMsg::TransferItem { from: SlotRef::Pack(slot), count: 5 }).await;
    assert!(
        a.await_inventory(|c| c.container().is_none(), SETTLE).await,
        "walking away must close the container"
    );
    assert_eq!(a.count_of(kind), held, "and nothing may move into it from across the map");
}

/// Two players in one chest is the case that forces the whole design: the
/// server owns the contents, and a transfer names what to move rather than
/// where to put it, because neither client can be right about the destination.
#[tokio::test]
async fn both_viewers_of_one_crate_see_every_change() {
    let server = TestServer::start("chest-shared");
    let mut a = Client::join(server.addr(), "alice").await;
    let (at, _) = open_a_crate(&mut a).await;

    let mut b = Client::join(server.addr(), "bob").await;
    assert!(b.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await);
    b.send(&ClientMsg::OpenContainer { pos: at }).await;
    assert!(
        b.await_inventory(|c| c.container().is_some(), SETTLE).await,
        "a second player may open the same crate"
    );

    let kind = ItemKind::Block(block_id(&mut a, "Dirt"));
    let slot = a.pack_slot_of(kind).expect("pack holds it");
    a.send(&ClientMsg::TransferItem { from: SlotRef::Pack(slot), count: 12 }).await;

    assert!(
        b.await_inventory(|c| c.container_count(kind) == 12, SETTLE).await,
        "bob must see alice's deposit without asking, got {:?}",
        b.container()
    );

    // And bob can take it, leaving alice's own view correct.
    let cslot = b.container_slot_of(kind).expect("the crate holds it");
    b.send(&ClientMsg::TransferItem { from: SlotRef::Container(cslot), count: 12 }).await;
    assert!(
        b.await_inventory(|c| c.count_of(kind) > 0 && c.container_count(kind) == 0, SETTLE).await
    );
    assert!(
        a.await_inventory(|c| c.container_count(kind) == 0, SETTLE).await,
        "alice must see the crate empty again"
    );
}

/// The storage claim, end to end: contents written through the page cache to a
/// region-adjacent file, and streamed back after the process that wrote them is
/// gone.
#[tokio::test]
async fn crate_contents_survive_a_server_restart() {
    let data_dir =
        std::env::temp_dir().join(format!("soils-test-chest-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);

    let kind;
    let at;
    {
        let server = TestServer::start_at(data_dir.clone(), "chest-restart");
        let mut a = Client::join(server.addr(), "alice").await;
        let (pos, _) = open_a_crate(&mut a).await;
        at = pos;
        kind = ItemKind::Block(block_id(&mut a, "Stone Bricks"));

        let slot = a.pack_slot_of(kind).expect("pack holds it");
        a.send(&ClientMsg::TransferItem { from: SlotRef::Pack(slot), count: 40 }).await;
        assert!(a.await_inventory(|c| c.container_count(kind) == 40, SETTLE).await);
        drop(a);
        // `TestServer::drop` flushes and joins the writer thread, so the bytes
        // are on disk by the time this scope ends.
    }

    {
        let server = TestServer::start_at(data_dir.clone(), "chest-restart");
        let mut a = Client::join(server.addr(), "alice").await;
        assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await);
        a.send(&ClientMsg::OpenContainer { pos: at }).await;
        assert!(
            a.await_inventory(|c| c.container_count(kind) == 40, SETTLE).await,
            "the crate must come back off disk with its contents, got {:?}",
            a.container()
        );
    }

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn placing_over_a_crate_does_not_orphan_its_contents() {
    // Nothing on the server requires the target voxel to be air before a
    // place, so a client can send one straight onto a container block. Keyed
    // on "this was a break" rather than "the old block is gone", the spill
    // never ran: the page entry stayed behind with no container in front of
    // it — invisible, unreachable, and inherited by whatever was built on
    // that voxel next.
    let server = TestServer::start("chest-overbuild");
    let mut a = Client::join(server.addr(), "alice").await;
    let (at, crate_id) = open_a_crate(&mut a).await;

    let kind = ItemKind::Block(block_id(&mut a, "Grass"));
    let slot = a.pack_slot_of(kind).expect("a pack slot holds it");
    a.send(&ClientMsg::TransferItem { from: SlotRef::Pack(slot), count: 12 }).await;
    assert!(a.await_inventory(|c| c.container_count(kind) == 12, SETTLE).await);

    let dropped_before = a.items_seen().len();
    // Place a *different* block onto the crate's own voxel.
    let stone = block_id(&mut a, "Cobblestone");
    edit_paced(&mut a, at, stone).await;

    assert!(
        a.await_inventory(|c| c.container().is_none(), SETTLE).await,
        "building over the block must close the panel, as breaking it does"
    );
    // The spill is a set of entity spawns; wait for them rather than reading
    // the moment the panel closes.
    assert!(
        a.await_inventory(|c| c.items_seen().len() > dropped_before, SETTLE).await,
        "the contents must be spawned as world items"
    );
    let spilled: u32 = a
        .items_seen()
        .into_iter()
        .filter(|(_, s)| s.kind == kind)
        .map(|(_, s)| s.count as u32)
        .sum();
    assert_eq!(spilled, 12, "the contents must come back out, not be orphaned");

    // And the voxel must no longer answer as a container: if the page entry
    // had survived, a crate rebuilt here would inherit the old contents.
    edit_paced(&mut a, at, 0).await;
    edit_paced(&mut a, at, crate_id).await;
    a.send(&ClientMsg::OpenContainer { pos: at }).await;
    assert!(
        a.await_inventory(|c| c.container().is_some(), SETTLE).await,
        "a fresh crate here must open"
    );
    assert_eq!(
        a.container_count(kind),
        0,
        "a crate built where one was overbuilt must be empty, not inherit the old contents"
    );
}
