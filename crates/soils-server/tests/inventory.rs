//! The inventory loop against the embedded server: the server owns the
//! inventory, placing spends an item, breaking yields one as a world entity,
//! and walking into that entity collects it.
//!
//! Everything here is about *authority*. The client is a mirror, so each
//! assertion reads the server's pushed `InventoryUpdate` rather than anything
//! the test computed for itself.

mod common;

use std::time::Duration;

use common::{Client, TestServer};
use soils_protocol::ClientMsg;
use soils_sim::{ItemKind, ItemStack};

const SPAWN_CHUNK: [i32; 3] = [8, 8, 8];
/// Long enough for a break, a fall of one block, and a pickup gate of
/// `PICKUP_DELAY_TICKS` (0.5 s at 20 Hz), with room for a loaded CI box.
const SETTLE: Duration = Duration::from_secs(10);

/// A 3x3 platform just under the player's feet, and the voxel on top of it to
/// experiment with.
///
/// Players spawn ~29 voxels above the surface in fly mode, so without a floor
/// every dropped item falls out of pickup range and the loop never closes.
/// That is correct behaviour and a useless test fixture.
fn work_site(spawn: [f32; 3]) -> (Vec<[i32; 3]>, [i32; 3]) {
    let (x, y, z) = (spawn[0].floor() as i32, spawn[1].floor() as i32, spawn[2].floor() as i32);
    let mut platform = Vec::new();
    for dx in -1..=1 {
        for dz in -1..=1 {
            platform.push([x + dx, y - 2, z + dz]);
        }
    }
    (platform, [x, y - 1, z])
}

/// Edits are rate-limited server-side (`EDIT_RATE`, 32/s). Sending faster just
/// gets them dropped, which reads as a logic failure rather than a pacing one.
async fn edit_paced(c: &mut Client, pos: [i32; 3], value: u8) -> u32 {
    let seq = c.edit(pos, value).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    seq
}

/// Lay the platform and wait for the last block to be charged for.
async fn build_platform(c: &mut Client, platform: &[[i32; 3]], block: u8) {
    for &p in platform {
        edit_paced(c, p, block).await;
    }
}

/// The first block kind the server stocked us with.
fn a_held_block(c: &Client) -> ItemKind {
    c.inventory()
        .iter()
        .flatten()
        .map(|s| s.kind)
        .find(|k| k.block().is_some())
        .expect("the server stocks new players with blocks")
}

#[tokio::test]
async fn a_new_player_is_given_a_starting_inventory() {
    let server = TestServer::start("inv-start");
    let mut a = Client::join(server.addr(), "alice").await;

    assert!(
        a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await,
        "the server must push an InventoryUpdate on join"
    );
    let blocks: Vec<_> = a.inventory().iter().flatten().filter(|s| s.kind.block().is_some()).collect();
    assert!(!blocks.is_empty(), "expected starter blocks, got {:?}", a.inventory());
    assert!(
        blocks.iter().all(|s| s.count <= s.kind.max_stack()),
        "no slot may exceed its kind's stack limit"
    );
}

#[tokio::test]
async fn placing_a_block_spends_it_and_breaking_it_gives_it_back() {
    let server = TestServer::start("inv-loop");
    let mut a = Client::join(server.addr(), "alice").await;
    a.await_chunk(SPAWN_CHUNK).await;
    assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "starter inventory");

    let kind = a_held_block(&a);
    let block = kind.block().unwrap();
    let (platform, target) = work_site(a.spawn);
    let initial = a.count_of(kind);
    build_platform(&mut a, &platform, block).await;
    // Settle on the exact post-platform count. Waiting for a loose condition
    // ("still has some") returns instantly against a mirror that has not
    // caught up yet, and every later delta is then measured from a stale
    // baseline — which is what made this test lie the first time.
    let before = initial - platform.len() as u32;
    assert!(
        a.await_inventory(|c| c.count_of(kind) == before, SETTLE).await,
        "the platform must cost exactly one block per voxel: expected {before}, have {}",
        a.count_of(kind)
    );

    // Place: the server must charge us for it.
    edit_paced(&mut a, target, block).await;
    assert!(
        a.await_inventory(|c| c.count_of(kind) == before - 1, SETTLE).await,
        "placing must spend exactly one {kind:?}: {before} -> {}",
        a.count_of(kind)
    );

    // Break it: the block comes back as an entity in the world, not straight
    // into the inventory.
    edit_paced(&mut a, target, 0).await;
    assert!(
        a.await_inventory(|c| !c.items_seen().is_empty(), SETTLE).await,
        "breaking a block must drop an item entity"
    );
    let (_, dropped) = a.items_seen()[0];
    assert_eq!(dropped.kind, kind, "the drop must be the block that was broken");
    assert_eq!(dropped.count, 1);

    // And walking into it (we are already standing over it) collects it.
    assert!(
        a.await_inventory(|c| c.count_of(kind) == before, SETTLE).await,
        "the dropped item must be picked up: {} != {before}",
        a.count_of(kind)
    );
    assert!(
        a.await_inventory(|c| c.items_seen().is_empty(), SETTLE).await,
        "a collected item must despawn"
    );
}

#[tokio::test]
async fn placing_a_block_you_do_not_have_is_refused() {
    let server = TestServer::start("inv-nostock");
    let mut a = Client::join(server.addr(), "alice").await;
    a.await_chunk(SPAWN_CHUNK).await;
    assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "starter inventory");

    // A real block the starter kit does not include. Asking whether the server
    // will conjure one is the whole point; emptying a stack first would test
    // the same rule through a much longer and flakier path.
    let held: Vec<u8> = a.inventory().iter().flatten().filter_map(|s| s.kind.block()).collect();
    let (_, registry) = a.generator();
    let unheld = (1..registry.len() as u8)
        .find(|id| !held.contains(id))
        .expect("blocks.yaml has more kinds than the starter kit");
    let kind = ItemKind::Block(unheld);
    assert_eq!(a.count_of(kind), 0, "precondition: we hold none of this block");

    let (_, target) = work_site(a.spawn);
    let seq = edit_paced(&mut a, target, unheld).await;
    let refused = a
        .recv_until(|msg| match msg {
            soils_protocol::ServerMsg::EditRejected { seq: s } if s == seq => Some(true),
            soils_protocol::ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(false),
            _ => None,
        })
        .await;
    assert!(refused, "placing block {unheld} with none held must be rejected");
    assert_eq!(a.count_of(kind), 0, "and must not go negative");

    // The refusal must not have touched the world either — otherwise the block
    // is placed and simply not billed.
    let held_block = a_held_block(&a).block().unwrap();
    let seq = edit_paced(&mut a, target, held_block).await;
    let accepted = a
        .recv_until(|msg| match msg {
            soils_protocol::ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(true),
            soils_protocol::ServerMsg::EditRejected { seq: s } if s == seq => Some(false),
            _ => None,
        })
        .await;
    assert!(accepted, "the target voxel must still be empty after a refused placement");
}

#[tokio::test]
async fn a_dropped_item_can_be_thrown_and_recollected() {
    let server = TestServer::start("inv-throw");
    let mut a = Client::join(server.addr(), "alice").await;
    a.await_chunk(SPAWN_CHUNK).await;
    assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "starter inventory");

    let kind = a_held_block(&a);
    let (platform, _) = work_site(a.spawn);
    let initial = a.count_of(kind);
    build_platform(&mut a, &platform, kind.block().unwrap()).await;
    let before = initial - platform.len() as u32;
    assert!(
        a.await_inventory(|c| c.count_of(kind) == before, SETTLE).await,
        "the platform must cost exactly one block per voxel"
    );
    let slot = a
        .inventory()
        .iter()
        .position(|s| s.is_some_and(|s| s.kind == kind))
        .expect("the stack we just found has a slot") as u16;

    a.send(&ClientMsg::DropItem { slot, count: 4 }).await;
    assert!(
        a.await_inventory(|c| c.count_of(kind) == before - 4, SETTLE).await,
        "throwing must remove the items from the inventory"
    );
    assert!(
        a.await_inventory(|c| c.items_seen().iter().any(|(_, s)| s.count == 4), SETTLE).await,
        "a thrown stack must appear in the world whole, not one entity per item"
    );

    // The throw gate keeps it uncollectable briefly; after that, standing over
    // it is enough. It was thrown one unit ahead, well inside pickup range.
    assert!(
        a.await_inventory(|c| c.count_of(kind) == before, Duration::from_secs(20)).await,
        "a thrown item must become collectable again: {} != {before}",
        a.count_of(kind)
    );
}

#[tokio::test]
async fn moving_a_stack_between_slots_conserves_it() {
    let server = TestServer::start("inv-move");
    let mut a = Client::join(server.addr(), "alice").await;
    assert!(a.await_inventory(|c| !c.inventory().is_empty(), SETTLE).await, "starter inventory");

    let total: u32 = a.inventory().iter().flatten().map(|s| s.count as u32).sum();
    let from = a.inventory().iter().position(|s| s.is_some()).unwrap() as u16;
    let to = a.inventory().iter().position(|s| s.is_none()).expect("an empty slot") as u16;
    let moved: ItemStack = a.inventory()[from as usize].unwrap();

    a.send(&ClientMsg::MoveItem { from, to }).await;
    assert!(
        a.await_inventory(
            |c| c.inventory().get(to as usize).is_some_and(|s| *s == Some(moved)),
            SETTLE
        )
        .await,
        "the stack must arrive in the target slot"
    );
    let after: u32 = a.inventory().iter().flatten().map(|s| s.count as u32).sum();
    assert_eq!(after, total, "moving must not create or destroy items");
}
