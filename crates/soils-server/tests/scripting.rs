//! Server-side scripting end-to-end: a script loaded from a scripts dir mutates
//! authoritative world state, and those mutations reach a real protocol client
//! through the normal replication path (edit broadcast + entity snapshots).
//!
//! Uses a precompiled `.wat` fixture so the test needs no Node/`asc` toolchain.

mod common;

use common::{Client, TestServer};
use soils_protocol::ServerMsg;
use std::path::PathBuf;

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

/// Write the fixture into a fresh temp scripts dir; returns the dir path.
fn scripts_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soils-scripts-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scripts dir");
    std::fs::write(dir.join("fixture.wat"), FIXTURE_WAT).expect("write fixture");
    dir
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
