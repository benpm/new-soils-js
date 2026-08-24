//! Accounts live in SpacetimeDB when one is configured.
//!
//! These assert *behaviour*, not stored bytes, and they have to: `account` is a
//! private table, so nothing outside the module can read a verifier — including
//! this test. That is the property under test in
//! [`a_client_cannot_read_the_account_table`], and the reason every other test
//! here proves storage by logging in again from a server with no local account
//! file rather than by looking the row up.
//!
//! Auto-skips without a host, like the other SpacetimeDB tests:
//!
//! ```sh
//! SOILS_STDB_URI=http://127.0.0.1:3000 SOILS_STDB_TOKEN=<token> \
//!   cargo test -p soils-server --test stdb_auth
//! ```

mod common;

use std::time::Duration;

use common::{Client, TestServer};
use soils_protocol::{ClientMsg, ServerMsg};
use soils_server::StdbConfig;

/// A name unique to this run, so repeated runs against one database do not
/// collide on an account that already exists.
fn unique_name(tag: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{tag}{}", n % 1_000_000)
}

/// A scratch data directory, so a server starts with no local account file and
/// can only answer from the database.
fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("soils-{tag}-{}", unique_name("")));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn login(
    addr: std::net::SocketAddr,
    name: &str,
    password: &str,
    signup: bool,
) -> Result<(), String> {
    let mut c = Client::connect(addr).await;
    c.send(&ClientMsg::Login {
        name: name.to_string(),
        password: password.to_string(),
        signup,
        protocol: soils_protocol::PROTOCOL_VERSION,
    })
    .await;
    c.recv_until(|m| match m {
        ServerMsg::Init { .. } => Some(Ok(())),
        ServerMsg::LoginError { message } => Some(Err(message)),
        _ => None,
    })
    .await
}

/// Signing up stores the account in the database, not merely on the server that
/// took the signup: a *different* server, with its own empty data directory,
/// accepts the same credentials.
#[tokio::test(flavor = "multi_thread")]
async fn signup_registers_an_account_in_spacetimedb() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB auth test");
        return;
    };

    let name = unique_name("newbie");
    let one = cfg.clone();
    let dir_a = fresh_dir("auth-a");
    {
        let server = TestServer::start_at_with(dir_a.clone(), "stdbauth", move |c| {
            c.stdb = Some(one);
        });
        login(server.addr(), &name, "hunter2", true).await.expect("signup");
    }

    // A second server, sharing nothing but the database.
    let two = cfg.clone();
    let dir_b = fresh_dir("auth-b");
    let server = TestServer::start_at_with(dir_b.clone(), "stdbauth-b", move |c| {
        c.stdb = Some(two);
    });
    login(server.addr(), &name, "hunter2", false)
        .await
        .expect("the account should be readable from the database by any server");

    // ...and it really is checking the password, not just the name.
    let refused = login(server.addr(), &name, "not-it", false)
        .await
        .expect_err("a wrong password must be refused");
    assert!(refused.to_lowercase().contains("password"), "unexpected refusal: {refused}");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// No client may read `account`. This is the whole reason verification happens
/// inside the module: SpacetimeDB has only public and private tables, `public`
/// means *every* connected identity can subscribe, and as of 2.7.1 row-level
/// security is gated behind `unstable` and documented in its own source as not
/// enforced. A public `account` would hand every player every Argon2 hash to
/// crack offline at their leisure.
///
/// The first half of this guarantee is enforced at compile time and is
/// invisible here: a private table generates no client-side accessor, so there
/// is no `db().account()` to call. What is checked below is the other half —
/// that naming the table in raw subscription SQL is refused too.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_cannot_read_the_account_table() {
    use spacetimedb_sdk::DbContext;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB auth test");
        return;
    };

    // Make sure there is at least one account to fail to read.
    let name = unique_name("secret");
    let one = cfg.clone();
    let dir = fresh_dir("auth-priv");
    let server = TestServer::start_at_with(dir.clone(), "stdbpriv", move |c| {
        c.stdb = Some(one);
    });
    login(server.addr(), &name, "hunter2", true).await.expect("signup");

    // Deliberately *no* token: an ordinary player's connection.
    let conn = soils_stdb::module_bindings::DbConnection::builder()
        .with_uri(&cfg.uri)
        .with_database_name(&cfg.database)
        .build()
        .expect("anonymous connect");
    conn.run_threaded();

    let refused = Arc::new(AtomicBool::new(false));
    let applied = Arc::new(AtomicBool::new(false));
    let (r, a) = (refused.clone(), applied.clone());
    conn.subscription_builder()
        .on_error(move |_, _| r.store(true, Ordering::Relaxed))
        .on_applied(move |_| a.store(true, Ordering::Relaxed))
        .subscribe(["SELECT * FROM account".to_string()]);

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !refused.load(Ordering::Relaxed)
        && !applied.load(Ordering::Relaxed)
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        refused.load(Ordering::Relaxed),
        "a client subscription to the private `account` table was not refused (applied={})",
        applied.load(Ordering::Relaxed)
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An account that already exists in the local file migrates into the database
/// on its next login, and is then usable from a server that has never seen it.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_account_migrates_on_next_login() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB auth test");
        return;
    };

    let name = unique_name("legacy");
    let dir = fresh_dir("migrate");

    // First run: no database, so the account exists only in the local file.
    {
        let server = TestServer::start_at(dir.clone(), "stdbmigrate-local");
        login(server.addr(), &name, "hunter2", true).await.expect("local signup");
    }

    // A database-backed server that has never seen this account rejects it,
    // which is what makes the migration below observable at all.
    let before = cfg.clone();
    let elsewhere = fresh_dir("migrate-elsewhere");
    {
        let server = TestServer::start_at_with(elsewhere.clone(), "stdbmigrate-pre", move |c| {
            c.stdb = Some(before);
        });
        login(server.addr(), &name, "hunter2", false)
            .await
            .expect_err("the account should not be in the database yet");
    }

    // Second run: same data dir, now with a database. Logging in migrates it —
    // this is the only moment a plaintext is in hand to re-register with.
    let during = cfg.clone();
    {
        let server = TestServer::start_at_with(dir.clone(), "stdbmigrate-db", move |c| {
            c.stdb = Some(during);
        });
        login(server.addr(), &name, "hunter2", false).await.expect("migrating login");
    }

    // Now the stranger accepts it.
    let after = cfg.clone();
    let server = TestServer::start_at_with(elsewhere.clone(), "stdbmigrate-post", move |c| {
        c.stdb = Some(after);
    });
    login(server.addr(), &name, "hunter2", false)
        .await
        .expect("the migrated account should now be in the database");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// A login must not stall the simulation. Argon2 is deliberately slow — that
/// is what makes a stolen verifier expensive — and the database adds a round
/// trip on top, so the check runs on a worker thread.
///
/// Measured as the **longest gap between consecutive ticks**, not as total
/// elapsed time. Total time is not discriminating: forty logins spread over a
/// couple of seconds of connection setup leave most of the cost outside any
/// short measurement window, and an earlier version of this test passed
/// happily with the hashing back on the tick thread. A stall shows up as one
/// long gap, so that is what to look for.
///
/// The connections are established first and the logins fired together, which
/// is both the sharper measurement and the shape an actual denial of service
/// would take.
///
/// The flood is made of *failed* logins on purpose: successful ones would join
/// forty players, and the chunk streaming that follows would dominate the
/// measurement and prove nothing about hashing. A rejected login still pays a
/// full Argon2 verify and then stops.
#[tokio::test(flavor = "multi_thread")]
async fn logins_do_not_stall_the_tick() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB auth test");
        return;
    };

    let dir = fresh_dir("auth-tick");
    let one = cfg.clone();
    let server = TestServer::start_at_with(dir.clone(), "stdbtick", move |c| {
        c.stdb = Some(one);
    });
    let addr = server.addr();

    // The account the flood will guess at, and an established player to watch
    // the tick through.
    let victim = unique_name("target");
    login(addr, &victim, "hunter2", true).await.expect("signup");
    let mut watcher = Client::join(addr, &unique_name("watch")).await;
    watcher.await_server_ticks(4).await;

    /// Longest a single tick may take. Two Argon2 verifies would exceed this;
    /// forty would bury it.
    const MAX_GAP: Duration = Duration::from_millis(250);
    const FLOOD: usize = 40;

    // Establish every connection *before* sending any password, so the
    // hashing all lands at once instead of trickling in behind TCP setup.
    let mut floods = Vec::new();
    for _ in 0..FLOOD {
        floods.push(Client::connect(addr).await);
    }
    for c in &mut floods {
        c.send(&ClientMsg::Login {
            name: victim.clone(),
            password: "not-it".into(),
            signup: false,
            protocol: soils_protocol::PROTOCOL_VERSION,
        })
        .await;
    }

    let mut worst = Duration::ZERO;
    for _ in 0..FLOOD {
        let t = std::time::Instant::now();
        watcher.await_server_ticks(1).await;
        worst = worst.max(t.elapsed());
    }

    assert!(
        worst < MAX_GAP,
        "the tick stalled for {worst:?} while {FLOOD} logins were being checked;          password hashing is back on the tick thread"
    );

    drop(floods);
    drop(watcher);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
