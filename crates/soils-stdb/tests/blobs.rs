//! A0 spike, as a permanent regression test: the assumptions the SpacetimeDB
//! schema rests on, checked against a real 2.7 host.
//!
//! Auto-skips when no host is reachable, mirroring the `asc`-gated scripting
//! test and the GPU-adapter-gated oracle tests. Point it at a host with:
//!
//! ```sh
//! SOILS_STDB_URI=http://127.0.0.1:3000 SOILS_STDB_DB=soils cargo test -p soils-stdb
//! ```

use std::time::{Duration, Instant};

use soils_protocol::{
    ChunkVolume,
    chunk_key::{pack_chunk_key, world_id_for},
    encode_chunk,
};
use soils_stdb::module_bindings::{
    DbConnection, chunk_blob_table::ChunkBlobTableAccess, put_chunk_blob, upsert_world,
    world_table::WorldTableAccess,
};
use spacetimedb_sdk::{DbContext, Table};

const WORLD: &str = "blobtest";

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Connect, or return `None` so the test skips.
///
/// `SOILS_STDB_TOKEN` must carry an identity present in the module's
/// `server_identity` allowlist, since these exercise server-only reducers.
/// Get one with `spacetime login show --token`.
fn connect() -> Option<DbConnection> {
    let uri = env("SOILS_STDB_URI")?;
    let db = env("SOILS_STDB_DB").unwrap_or_else(|| "soils".into());
    let mut builder = DbConnection::builder().with_uri(&uri).with_database_name(&db);
    if let Some(token) = env("SOILS_STDB_TOKEN") {
        builder = builder.with_token(Some(token));
    }
    let conn = builder.build().ok()?;
    conn.run_threaded();
    Some(conn)
}

/// Block until `f` holds or the deadline passes.
fn wait_for(conn: &DbConnection, timeout: Duration, mut f: impl FnMut(&DbConnection) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f(conn) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f(conn)
}

/// Subscribe to everything and wait for the initial snapshot.
fn subscribe_all(conn: &DbConnection) {
    conn.subscription_builder()
        .subscribe(["SELECT * FROM chunk_blob".to_string(), "SELECT * FROM world".to_string()]);
    // The cache is empty until the subscription applies; give it a moment.
    std::thread::sleep(Duration::from_millis(300));
}

/// A chunk whose payload is deliberately large: 128 distinct block ids in a
/// pattern LZ4 can't collapse, which forces the codec off its uniform and
/// small-palette tiers.
fn bulky_chunk(salt: u8) -> ChunkVolume {
    let mut v = ChunkVolume::empty();
    let mut state: u32 = 0x9E37_79B9 ^ (salt as u32);
    for z in 0..32 {
        for y in 0..32 {
            for x in 0..32 {
                // xorshift keeps this deterministic per salt but incompressible.
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                v.set(x, y, z, (state % 128) as u8);
            }
        }
    }
    v
}

#[test]
fn large_chunk_payload_round_trips() {
    let Some(conn) = connect() else {
        eprintln!("skipping: set SOILS_STDB_URI to run against a live SpacetimeDB");
        return;
    };
    subscribe_all(&conn);

    conn.reducers
        .upsert_world(world_id_for(WORLD), WORLD.into(), 4242, 0, 7, 0.25)
        .expect("upsert_world queued");
    assert!(
        wait_for(&conn, Duration::from_secs(10), |c| {
            c.db().world().iter().any(|w| w.name == WORLD)
        }),
        "world row never arrived — is this identity in server_identity?"
    );
    let world_id = conn.db().world().iter().find(|w| w.name == WORLD).unwrap().world_id;

    let volume = bulky_chunk(1);
    let payload = encode_chunk(&volume);
    assert!(
        payload.len() > 992,
        "payload {} B is below SpacetimeDB's 992 B blob threshold — this test \
         would not exercise the blob store",
        payload.len()
    );
    eprintln!("payload: {} B", payload.len());

    let key = pack_chunk_key(world_id, 3, -2, 5).expect("in range");
    conn.reducers.put_chunk_blob(key, payload.clone(), 1, 0).expect("put queued");

    assert!(
        wait_for(&conn, Duration::from_secs(15), |c| {
            c.db().chunk_blob().iter().any(|b| b.chunk_key == key)
        }),
        "chunk_blob row never arrived"
    );

    let row = conn.db().chunk_blob().iter().find(|b| b.chunk_key == key).unwrap();
    assert_eq!(row.payload, payload, "payload came back altered");
    assert_eq!((row.world_id, row.cx, row.cy, row.cz), (world_id, 3, -2, 5), "key unpacked wrong");

    // And it must still decode to the original voxels.
    let back = soils_protocol::decode_chunk(&row.payload).expect("payload decodes");
    for (i, (a, b)) in volume.as_bytes().iter().zip(back.as_bytes()).enumerate() {
        assert_eq!(a, b, "voxel {i} differs after a database round trip");
    }
}

#[test]
fn identical_payloads_are_accepted_under_distinct_keys() {
    let Some(conn) = connect() else {
        eprintln!("skipping: set SOILS_STDB_URI to run against a live SpacetimeDB");
        return;
    };
    subscribe_all(&conn);

    conn.reducers
        .upsert_world(world_id_for(WORLD), WORLD.into(), 4242, 0, 7, 0.25)
        .expect("upsert_world queued");
    assert!(wait_for(&conn, Duration::from_secs(10), |c| {
        c.db().world().iter().any(|w| w.name == WORLD)
    }));
    let world_id = conn.db().world().iter().find(|w| w.name == WORLD).unwrap().world_id;

    // The blob store is content-addressed and refcounted, so two keys holding
    // the same bytes should dedupe internally while both remaining readable.
    let payload = encode_chunk(&bulky_chunk(9));
    let a = pack_chunk_key(world_id, 10, 0, 0).unwrap();
    let b = pack_chunk_key(world_id, 11, 0, 0).unwrap();
    conn.reducers.put_chunk_blob(a, payload.clone(), 1, 0).unwrap();
    conn.reducers.put_chunk_blob(b, payload.clone(), 1, 0).unwrap();

    assert!(
        wait_for(&conn, Duration::from_secs(15), |c| {
            let d = c.db();
            d.chunk_blob().iter().any(|r| r.chunk_key == a)
                && d.chunk_blob().iter().any(|r| r.chunk_key == b)
        }),
        "both rows should be present"
    );
    let d = conn.db();
    let ra = d.chunk_blob().iter().find(|r| r.chunk_key == a).unwrap();
    let rb = d.chunk_blob().iter().find(|r| r.chunk_key == b).unwrap();
    assert_eq!(ra.payload, rb.payload);
    assert_eq!(ra.payload, payload);
}

#[test]
fn stale_version_write_is_rejected() {
    let Some(conn) = connect() else {
        eprintln!("skipping: set SOILS_STDB_URI to run against a live SpacetimeDB");
        return;
    };
    subscribe_all(&conn);

    conn.reducers.upsert_world(world_id_for(WORLD), WORLD.into(), 4242, 0, 7, 0.25).unwrap();
    assert!(wait_for(&conn, Duration::from_secs(10), |c| {
        c.db().world().iter().any(|w| w.name == WORLD)
    }));
    let world_id = conn.db().world().iter().find(|w| w.name == WORLD).unwrap().world_id;

    let key = pack_chunk_key(world_id, 20, 0, 0).unwrap();
    let v5 = encode_chunk(&bulky_chunk(5));
    conn.reducers.put_chunk_blob(key, v5.clone(), 5, 0).unwrap();
    assert!(wait_for(&conn, Duration::from_secs(15), |c| {
        c.db().chunk_blob().iter().any(|r| r.chunk_key == key && r.version == 5)
    }));

    // A late flush carrying an older version must not roll the chunk back.
    let v2 = encode_chunk(&bulky_chunk(2));
    conn.reducers.put_chunk_blob(key, v2, 2, 0).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let row = conn.db().chunk_blob().iter().find(|r| r.chunk_key == key).unwrap();
    assert_eq!(row.version, 5, "stale write must be refused");
    assert_eq!(row.payload, v5, "stale write must not alter the payload");
}
