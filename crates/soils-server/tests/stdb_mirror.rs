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

    let cfg_for_server = cfg.clone();
    let server = TestServer::start_with("stdbmirror", move |c| {
        c.stdb = Some(cfg_for_server);
    });

    let mut client = Client::join(server.addr(), "mirror").await;

    // Derive the edit target from where the client actually spawned, not a
    // fixed voxel. Profile restore means a returning player starts wherever
    // they logged out, so a hardcoded target drifts out of edit reach after the
    // first run.
    let (sx, sy, sz) = (
        client.spawn[0].floor() as i32,
        client.spawn[1].floor() as i32,
        client.spawn[2].floor() as i32,
    );
    let near_voxel = [sx, sy - 5, sz];
    let spawn_chunk = [near_voxel[0] >> 5, near_voxel[1] >> 5, near_voxel[2] >> 5];
    let key = pack_chunk_key(
        world_id_for(DEFAULT_WORLD),
        spawn_chunk[0],
        spawn_chunk[1],
        spawn_chunk[2],
    )
    .expect("spawn chunk is representable");

    // Residue from an earlier run against the same database would otherwise
    // make this pass vacuously. Keyed on `updated_at` rather than `version`:
    // the version is the chunk's edit counter, so a repeat of this exact run
    // produces the same number and nothing would look new.
    let preexisting =
        obs.db().chunk_blob().iter().find(|b| b.chunk_key == key).map(|b| b.updated_at);

    // The server rejects edits to chunks that aren't resident yet, so let the
    // spawn chunk stream in first.
    client.await_chunk(spawn_chunk).await;

    // Wait for the ack too: a rejected edit leaves nothing dirty to mirror.
    let seq = client.edit(near_voxel, 1).await;
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

    // Presence must survive being online, not merely appear at login. The
    // module reaps rows older than its 30 s liveness TTL, so a row written once
    // at login and never refreshed vanishes while the player is still
    // connected — and a test that only checks presence is *gone* after logout
    // passes either way.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !obs.db().presence().iter().any(|p| p.account == "mirror") {
        assert!(Instant::now() < deadline, "no presence row for 'mirror' while online");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let first = obs
        .db()
        .presence()
        .iter()
        .find(|p| p.account == "mirror")
        .expect("present")
        .heartbeat;
    // Long enough to cover more than one server heartbeat period.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let refreshed = obs
        .db()
        .presence()
        .iter()
        .find(|p| p.account == "mirror")
        .expect("presence row was reaped while the player was still online")
        .heartbeat;
    assert!(
        refreshed > first,
        "presence heartbeat never advanced ({first:?} -> {refreshed:?}); the row          will be reaped out from under an online player"
    );

    // Fly clear of the spawn point before logging out. Without this the saved
    // profile sits on top of the world spawn and the resume check below cannot
    // tell a restored position from a default one.
    let first_spawn = client.spawn;
    client.fly(48, 0.0, false).await;
    let moved = client.current_self_pos().await;
    let travelled = ((moved[0] - first_spawn[0]).powi(2)
        + (moved[1] - first_spawn[1]).powi(2)
        + (moved[2] - first_spawn[2]).powi(2))
    .sqrt();
    assert!(travelled > 2.0, "expected to fly clear of spawn, only moved {travelled}");

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
            && preexisting != Some(row.updated_at)
        {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "edited chunk {spawn_chunk:?} (key {key}) never reached SpacetimeDB. \
             Is SOILS_STDB_TOKEN's identity in the module's server_identity allowlist?"
        );
        std::thread::sleep(Duration::from_millis(150));
    };

    assert_eq!(
        (row.world_id, row.cx, row.cy, row.cz),
        (world_id_for(DEFAULT_WORLD), spawn_chunk[0], spawn_chunk[1], spawn_chunk[2]),
        "chunk key unpacked to the wrong position"
    );

    // The mirrored payload must be a real chunk carrying the edit.
    let volume = soils_protocol::decode_chunk(&row.payload).expect("mirrored payload decodes");
    let (lx, ly, lz) = (near_voxel[0] & 31, near_voxel[1] & 31, near_voxel[2] & 31);
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

    // A saved profile is only worth writing if it is read back. Start a fresh
    // server against the same database and log the same account in: it should
    // resume at the stored position, not the world spawn point.
    let saved = [profile.x, profile.y, profile.z];
    // Release the first server before starting the second: TestServer holds a
    // process-wide gate for its whole scope (parallel embedded servers starve
    // the shared rayon pool), so constructing another while it is alive
    // deadlocks.
    drop(server);
    let server = TestServer::start_with("stdbresume", move |c| {
        c.stdb = Some(cfg);
    });
    let resumed = Client::join(server.addr(), "mirror").await;
    let spawned = resumed.spawn;
    let drift = ((spawned[0] - saved[0]).powi(2)
        + (spawned[1] - saved[1]).powi(2)
        + (spawned[2] - saved[2]).powi(2))
    .sqrt();
    let from_default = ((spawned[0] - first_spawn[0]).powi(2)
        + (spawned[1] - first_spawn[1]).powi(2)
        + (spawned[2] - first_spawn[2]).powi(2))
    .sqrt();
    drop(resumed);
    assert!(
        drift < 0.5,
        "expected to resume at the saved profile {saved:?}, spawned at {spawned:?} \
         ({drift} away) — the profile is being written but not read"
    );
    // And prove it is the profile doing the work, not a coincidence.
    assert!(
        from_default > 2.0,
        "resumed at {spawned:?}, which is the same place the first session \
         started ({first_spawn:?}) — the profile read cannot be doing anything"
    );
}
