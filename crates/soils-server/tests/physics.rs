//! Networked Avian physics: the server drops a stack of spinning rigid-body
//! cubes once a player is in-world (SOILS_PHYSICS demo). These pin the
//! authoritative-physics replication path: bodies fall, their orientation
//! ships (rotation added to the snapshot codec), and two clients converge on
//! the same rest state.

mod common;

use common::{Client, TestServer};
use soils_protocol::{ClientMsg, EntityState, ServerMsg};

fn vel_sq(s: &EntityState) -> f32 {
    s.velocity.iter().map(|v| v * v).sum()
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Wait for the demo cube stack and return the first cube's NetId.
async fn await_demo_cube(c: &mut Client) -> u32 {
    c.recv_until(|msg| match msg {
        ServerMsg::EntitySpawn { id, kind, .. } if kind == soils_sim::KIND_PHYSICS_CUBE => {
            Some(id)
        }
        _ => None,
    })
    .await
}

#[tokio::test]
async fn physics_cube_falls_and_replicates_rotation() {
    let server = TestServer::start_with("physics-fall", |cfg| cfg.physics = true);
    let mut a = Client::join(server.addr(), "alice").await;

    let cube = await_demo_cube(&mut a).await;

    // Falls under gravity: y drops well below where it spawned.
    let first = a.await_entity(cube, |_| true).await;
    a.await_entity(cube, |s| s.pos[1] < first.pos[1] - 1.0).await;

    // Tumbles: it was spawned with angular velocity, so the replicated
    // orientation leaves identity ([0,0,0,1] → w drops below ~1).
    let rotated = a.await_entity(cube, |s| s.rot[3].abs() < 0.99).await;

    // The quaternion round-trips as (roughly) unit-length through quantization.
    let n = rotated.rot.iter().map(|c| c * c).sum::<f32>().sqrt();
    assert!((n - 1.0).abs() < 0.05, "orientation not unit-length: {:?}", rotated.rot);
}

#[tokio::test]
async fn two_clients_agree_on_physics_cube_rest_state() {
    let server = TestServer::start_with("physics-rest", |cfg| cfg.physics = true);
    let mut a = Client::join(server.addr(), "alice").await;
    let mut b = Client::join(server.addr(), "bob").await;

    let cube = await_demo_cube(&mut a).await;
    let first = a.await_entity(cube, |_| true).await;

    // Settled = has fallen a good distance and is nearly still. Both clients
    // derive from the same authoritative snapshots, so their rest positions
    // must match within quantization + interp-tick slack.
    let settled = move |s: &EntityState| s.pos[1] < first.pos[1] - 2.0 && vel_sq(s) < 0.05;
    let rest_a = a.await_entity(cube, settled).await;
    let rest_b = b.await_entity(cube, settled).await;

    assert!(
        dist(rest_a.pos, rest_b.pos) < 0.3,
        "clients disagree on rest position: {:?} vs {:?}",
        rest_a.pos,
        rest_b.pos
    );
}

#[tokio::test]
async fn spawn_cube_command_creates_a_replicated_cube() {
    let server = TestServer::start_with("physics-spawn", |cfg| cfg.physics = true);
    let mut a = Client::join(server.addr(), "alice").await;

    // Get past the demo stack (3 cubes) so a new cube is unambiguous.
    let mut cubes = std::collections::HashSet::new();
    while cubes.len() < 3 {
        cubes.insert(await_demo_cube(&mut a).await);
    }

    // Request a cube a couple of metres in front of us (within reach).
    let s = a.spawn;
    a.send(&ClientMsg::SpawnCube { pos: [s[0], s[1], s[2] - 2.0] }).await;

    // A new, distinct physics cube must be spawned and replicated to us.
    let new_id = loop {
        let id = await_demo_cube(&mut a).await;
        if !cubes.contains(&id) {
            break id;
        }
    };
    assert!(!cubes.contains(&new_id), "commanded cube should be a new entity");
}
