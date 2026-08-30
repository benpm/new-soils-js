//! Server-side scripting end-to-end: a script loaded from a scripts dir mutates
//! authoritative world state, and those mutations reach a real protocol client
//! through the normal replication path (edit broadcast + entity snapshots).
//!
//! Uses a precompiled `.wat` fixture so the test needs no Node/`asc` toolchain.

mod common;

use common::{Client, TestServer};
use soils_protocol::{ClientMsg, ServerMsg, SlotRef};
use soils_sim::ItemKind;
use std::path::PathBuf;
use std::time::Duration;

/// The chunk containing the spawn point (mirrors `scenarios.rs`).
const SPAWN_CHUNK: [i32; 3] = [8, 8, 8];
/// A voxel within edit reach of the spawn eye.
const NEAR_VOXEL: [i32; 3] = [282, 280, 268];

/// Fixture script: spawns one critter near spawn on its first tick, and caps any
/// player edit with a stone block (id 3) directly above it.
const FIXTURE_WAT: &str = r#"(module
  (import "soils" "edit_voxel" (func $edit (param i32 i32 i32 i32)))
  (import "soils" "spawn" (func $spawn (param i32 f32 f32 f32) (result i32)))
  (global $spawned (mut i32) (i32.const 0))
  (func (export "on_tick") (param i32 f32)
    (if (i32.eqz (global.get $spawned)) (then
      (global.set $spawned (i32.const 1))
      (drop (call $spawn (i32.const 1) (f32.const 282) (f32.const 279) (f32.const 268))))))
  (func (export "on_edit") (param i32 i32 i32 i32 i32 i32)
    (call $edit (local.get 0) (i32.add (local.get 1) (i32.const 1)) (local.get 2) (i32.const 3))))"#;

/// Fixture that deletes whatever a player edits, one voxel below their edit.
///
/// Deliberately a *script* edit: script commands go straight through
/// `World::edit`, bypassing reach, rate and inventory checks, which is why the
/// container spill was missing on this path for so long.
const ERASER_WAT: &str = r#"(module
  (import "soils" "edit_voxel" (func $edit (param i32 i32 i32 i32)))
  (func (export "on_edit") (param i32 i32 i32 i32 i32 i32)
    (call $edit (local.get 0) (i32.sub (local.get 1) (i32.const 1)) (local.get 2) (i32.const 0))))"#;

/// Write a fixture into a fresh temp scripts dir; returns the dir path.
fn scripts_dir_with(tag: &str, wat: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soils-scripts-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scripts dir");
    std::fs::write(dir.join("fixture.wat"), wat).expect("write fixture");
    dir
}

fn scripts_dir(tag: &str) -> PathBuf {
    scripts_dir_with(tag, FIXTURE_WAT)
}

#[tokio::test]
async fn script_on_edit_broadcasts_a_downstream_edit() {
    let dir = scripts_dir("onedit");
    let server = TestServer::start_with("script-edit", |c| c.scripts_dir = Some(dir.clone()));
    let mut a = Client::join(server.addr(), "alice").await;

    // Ensure the spawn chunk (and thus the edit target) is resident.
    a.await_chunk(SPAWN_CHUNK).await;

    // Player places a block; the server accepts it (editor's own edit is not
    // echoed back to the editor).
    let seq = a.edit(NEAR_VOXEL, 5).await;
    a.recv_until(|m| match m {
        ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(()),
        _ => None,
    })
    .await;

    // The script's on_edit reaction places stone one voxel above, broadcast to
    // everyone in the world (including the editor). This is the downstream
    // event the script produced landing on the network world state.
    let above = [NEAR_VOXEL[0], NEAR_VOXEL[1] + 1, NEAR_VOXEL[2]];
    a.recv_until(|m| match m {
        ServerMsg::Edit { pos, value } if pos == above && value == 3 => Some(()),
        _ => None,
    })
    .await;

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn script_on_tick_spawns_a_replicated_entity() {
    let dir = scripts_dir("ontick");
    let server = TestServer::start_with("script-spawn", |c| c.scripts_dir = Some(dir.clone()));
    let mut a = Client::join(server.addr(), "alice").await;
    let self_net = a.self_entity;

    // The script spawns a critter near spawn on its first tick; the client
    // learns of it through the normal interest/EntitySpawn path.
    let kind = a
        .recv_until(|m| match m {
            ServerMsg::EntitySpawn { id, kind, .. } if id != self_net => Some(kind),
            _ => None,
        })
        .await;
    assert_eq!(kind, soils_sim::KIND_CRITTER, "script-spawned entity replicates as a critter");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A script that deletes a container block must give its contents back, exactly
/// as a player breaking it does.
///
/// Script edits bypass reach, rate and inventory checks and go straight to
/// `World::edit`, and for a long time they bypassed the spill too — so a script
/// that removed a chest left its contents on the block-data page with no block
/// in front of them: invisible, unreachable, and inherited by whatever was
/// built on that voxel next. Both paths now share `release_block_data`.
#[tokio::test]
async fn a_script_that_deletes_a_container_spills_it() {
    let dir = scripts_dir_with("eraser", ERASER_WAT);
    let server = TestServer::start_with("script-spill", |c| c.scripts_dir = Some(dir.clone()));
    let mut a = Client::join(server.addr(), "alice").await;

    assert!(
        a.await_inventory(|c| !c.inventory().is_empty(), Duration::from_secs(10)).await,
        "the server must stock a new player before anything can be stored"
    );
    let crate_id = a
        .generator()
        .1
        .id_of("Wooden Crate")
        .expect("Wooden Crate must exist in blocks.yaml");
    let grass = a.generator().1.id_of("Grass").expect("Grass must exist");

    // Put a crate down and fill it, one voxel *above* where the script will
    // strike, so the player's own edit is what triggers `on_edit`.
    let spawn = a.spawn;
    let at = [spawn[0].floor() as i32, spawn[1].floor() as i32 - 3, spawn[2].floor() as i32];
    let trigger = [at[0], at[1] + 1, at[2]];
    let _ = a.await_chunk([at[0] >> 5, at[1] >> 5, at[2] >> 5]).await;
    a.place(at, crate_id).await;
    tokio::time::sleep(Duration::from_millis(60)).await;

    a.send(&ClientMsg::OpenContainer { pos: at }).await;
    assert!(
        a.await_inventory(|c| c.container().is_some(), Duration::from_secs(10)).await,
        "the crate must open"
    );
    let kind = ItemKind::Block(grass);
    let slot = a.pack_slot_of(kind).expect("a pack slot holds grass");
    a.send(&ClientMsg::TransferItem { from: SlotRef::Pack(slot), count: 9 }).await;
    assert!(a.await_inventory(|c| c.container_count(kind) == 9, Duration::from_secs(10)).await);

    // Editing `trigger` fires `on_edit`, and the script erases the voxel below
    // it — the crate.
    let dropped_before = a.items_seen().len();
    a.edit(trigger, grass).await;

    assert!(
        a.await_inventory(|c| c.container().is_none(), Duration::from_secs(10)).await,
        "the script deleting the block must close the panel, as a break does"
    );
    assert!(
        a.await_inventory(|c| c.items_seen().len() > dropped_before, Duration::from_secs(10)).await,
        "the contents must be spawned as world items"
    );
    let spilled: u32 = a
        .items_seen()
        .into_iter()
        .filter(|(_, s)| s.kind == kind)
        .map(|(_, s)| s.count as u32)
        .sum();
    assert_eq!(spilled, 9, "every stored item must come back, exactly once");

    let _ = std::fs::remove_dir_all(&dir);
}
