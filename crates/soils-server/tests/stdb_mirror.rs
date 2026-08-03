//! End-to-end: an edit made through the *game* server reaches SpacetimeDB.
//!
//! Auto-skips unless a host is configured, in the same style as the `asc`-gated
//! scripting test:
//!
//! ```sh
//! SOILS_STDB_URI=http://127.0.0.1:3000 \
//! SOILS_STDB_DB=soils \
//! SOILS_STDB_TOKEN=<token from `spacetime login show --token`> \
//!   cargo test -p soils-server --test stdb_mirror
//! ```

mod common;

use std::time::{Duration, Instant};

use common::{Client, TestServer};
use soils_protocol::chunk_key::{pack_chunk_key, world_id_for};
use soils_server::StdbConfig;
use soils_stdb::module_bindings::{
    DbConnection, chunk_blob_table::ChunkBlobTableAccess,
    game_server_table::GameServerTableAccess, player_profile_table::PlayerProfileTableAccess,
    presence_table::PresenceTableAccess,
};
use spacetimedb_sdk::{DbContext, Table};

/// Matches `soils_server`'s `DEFAULT_WORLD`.
const DEFAULT_WORLD: &str = "default";
/// A voxel within edit reach of spawn, as in `scenarios.rs`.
const NEAR_VOXEL: [i32; 3] = [282, 280, 268];
/// The chunk `NEAR_VOXEL` lives in.
const SPAWN_CHUNK: [i32; 3] = [8, 8, 8];

/// An independent read-only connection used to observe what the server wrote.
fn observer(cfg: &StdbConfig) -> Option<DbConnection> {
    let mut b = DbConnection::builder().with_uri(&cfg.uri).with_database_name(&cfg.database);
    if let Some(t) = &cfg.token {
        b = b.with_token(Some(t.clone()));
    }
    let conn = b.build().ok()?;
    conn.run_threaded();
    conn.subscription_builder().subscribe([
        "SELECT * FROM chunk_blob".to_string(),
        "SELECT * FROM player_profile".to_string(),
        "SELECT * FROM presence".to_string(),
        "SELECT * FROM game_server".to_string(),
    ]);
    std::thread::sleep(Duration::from_millis(400));
    Some(conn)
}

// The SpacetimeDB SDK connects via `tokio::task::block_in_place`, which panics
// on the default current-thread test runtime.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_reaches_spacetimedb() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB mirror test");
        return;
    };
    let Some(obs) = observer(&cfg) else {
        eprintln!("skipping: could not reach SpacetimeDB at {}", cfg.uri);
        return;
    };

    let key = pack_chunk_key(
        world_id_for(DEFAULT_WORLD),
        SPAWN_CHUNK[0],
        SPAWN_CHUNK[1],
        SPAWN_CHUNK[2],
    )
    .expect("spawn chunk is representable");

    // Any residue from an earlier run would make this pass vacuously.
    let preexisting = obs.db().chunk_blob().iter().find(|b| b.chunk_key == key).map(|b| b.version);

    let cfg_for_server = cfg.clone();
    let server = TestServer::start_with("stdbmirror", move |c| {
        c.stdb = Some(cfg_for_server);
    });

    let mut client = Client::join(server.addr(), "mirror").await;
    // The server rejects edits to chunks that aren't resident yet, so let the
    // spawn chunk stream in first.
    client.await_chunk(SPAWN_CHUNK).await;

    // Wait for the ack too: a rejected edit leaves nothing dirty to mirror.
    let seq = client.edit(NEAR_VOXEL, 1).await;
    client
        .recv_until(|msg| match msg {
            soils_protocol::ServerMsg::EditAccepted { seq: s, .. } if s == seq => Some(()),
            soils_protocol::ServerMsg::EditRejected { seq: s } if s == seq => {
                panic!("edit {s} was rejected; the test voxel must be in reach and resident")
            }
            _ => None,
        })
        .await;

    // The server registers itself in the browser registry on its first tick.
    let deadline = Instant::now() + Duration::from_secs(10);
    while obs.db().game_server().iter().next().is_none() {
        assert!(Instant::now() < deadline, "server never registered a game_server row");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // Disconnect first so the server observes the logout and runs the profile /
    // presence path; shutting down with the client still attached would skip it.
    drop(client);
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Then push the dirty chunk through the persistence path (which is what
    // mirrors it) instead of waiting on the 30 s flush interval.
    server.handle.shutdown();

    let deadline = Instant::now() + Duration::from_secs(25);
    let row = loop {
        if let Some(row) = obs.db().chunk_blob().iter().find(|b| b.chunk_key == key)
            && preexisting != Some(row.version)
        {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "edited chunk {SPAWN_CHUNK:?} (key {key}) never reached SpacetimeDB. \
             Is SOILS_STDB_TOKEN's identity in the module's server_identity allowlist?"
        );
        std::thread::sleep(Duration::from_millis(150));
    };

    assert_eq!(
        (row.world_id, row.cx, row.cy, row.cz),
        (world_id_for(DEFAULT_WORLD), SPAWN_CHUNK[0], SPAWN_CHUNK[1], SPAWN_CHUNK[2]),
        "chunk key unpacked to the wrong position"
    );

    // The mirrored payload must be a real chunk carrying the edit.
    let volume = soils_protocol::decode_chunk(&row.payload).expect("mirrored payload decodes");
    let (lx, ly, lz) = (NEAR_VOXEL[0] & 31, NEAR_VOXEL[1] & 31, NEAR_VOXEL[2] & 31);
    assert_eq!(volume.get(lx, ly, lz), 1, "the mirrored chunk should carry the edited voxel");

    // Disconnecting saves the player's last position and clears presence.
    let profile = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(p) = obs.db().player_profile().iter().find(|p| p.account == "mirror") {
                break p;
            }
            assert!(Instant::now() < deadline, "no player_profile row for 'mirror' after logout");
            std::thread::sleep(Duration::from_millis(150));
        }
    };
    assert_eq!(profile.world_id, world_id_for(DEFAULT_WORLD));
    assert!(
        profile.y > 0.0,
        "profile should carry a real position, got ({}, {}, {})",
        profile.x,
        profile.y,
        profile.z
    );

    // Presence is dropped on a clean disconnect (the reaper only covers crashes).
    let deadline = Instant::now() + Duration::from_secs(15);
    while obs.db().presence().iter().any(|p| p.account == "mirror") {
        assert!(Instant::now() < deadline, "presence row for 'mirror' outlived a clean logout");
        std::thread::sleep(Duration::from_millis(150));
    }
}
