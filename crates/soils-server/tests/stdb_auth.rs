//! Accounts live in SpacetimeDB when one is configured.
//!
//! Auto-skips without a host, like the other SpacetimeDB tests:
//!
//! ```sh
//! SOILS_STDB_URI=http://127.0.0.1:3000 SOILS_STDB_TOKEN=<token> \
//!   cargo test -p soils-server --test stdb_auth
//! ```

mod common;

use std::time::{Duration, Instant};

use common::{Client, TestServer};
use soils_protocol::ServerMsg;
use soils_server::StdbConfig;
use soils_stdb::module_bindings::{DbConnection, account_table::AccountTableAccess};
use spacetimedb_sdk::{DbContext, Table};

/// A read-only connection used to observe what the server wrote.
fn observer(cfg: &StdbConfig) -> Option<DbConnection> {
    let mut b = DbConnection::builder().with_uri(&cfg.uri).with_database_name(&cfg.database);
    if let Some(t) = &cfg.token {
        b = b.with_token(Some(t.clone()));
    }
    let conn = b.build().ok()?;
    conn.run_threaded();
    conn.subscription_builder().subscribe(["SELECT * FROM account".to_string()]);
    std::thread::sleep(Duration::from_millis(400));
    Some(conn)
}

/// A name unique to this run, so repeated runs against one database do not
/// collide on an account that already exists.
fn unique_name(tag: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{tag}{}", n % 1_000_000)
}

fn await_account(obs: &DbConnection, name: &str) -> soils_stdb::Account {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(a) = obs.db().account().iter().find(|a| a.name == name) {
            return a;
        }
        assert!(Instant::now() < deadline, "account '{name}' never reached SpacetimeDB");
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Signup writes an account to the database, and the stored verifier is an
/// Argon2 hash rather than the password or a bare digest.
#[tokio::test(flavor = "multi_thread")]
async fn signup_registers_an_account_in_spacetimedb() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB auth test");
        return;
    };
    let Some(obs) = observer(&cfg) else {
        eprintln!("skipping: could not reach SpacetimeDB at {}", cfg.uri);
        return;
    };

    let name = unique_name("newbie");
    let cfg_for_server = cfg.clone();
    let server = TestServer::start_with("stdbauth", move |c| {
        c.stdb = Some(cfg_for_server);
    });

    let client = Client::join(server.addr(), &name).await;
    drop(client);

    let account = await_account(&obs, &name);
    assert!(
        account.verifier.starts_with("$argon2"),
        "expected an argon2 PHC verifier, got {:?}",
        account.verifier
    );
    assert!(
        !account.verifier.contains(&name),
        "the verifier must not embed the account name in the clear"
    );
    assert!(account.identity.is_none(), "nothing has linked an identity yet");
}

/// The password is actually checked against what the database stored — a
/// second login with the wrong password must be refused, and with the right
/// one accepted.
#[tokio::test(flavor = "multi_thread")]
async fn stored_verifier_is_what_login_checks() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB auth test");
        return;
    };
    let Some(obs) = observer(&cfg) else {
        return;
    };

    let name = unique_name("guard");
    let cfg_for_server = cfg.clone();
    let server = TestServer::start_with("stdbauth2", move |c| {
        c.stdb = Some(cfg_for_server);
    });

    // Sign up with a real password.
    let mut c = Client::connect(server.addr()).await;
    c.send(&soils_protocol::ClientMsg::Login {
        name: name.clone(),
        password: "hunter2".into(),
        signup: true,
        protocol: soils_protocol::PROTOCOL_VERSION,
    })
    .await;
    c.recv_until(|m| match m {
        ServerMsg::Init { .. } => Some(()),
        ServerMsg::LoginError { message } => panic!("signup refused: {message}"),
        _ => None,
    })
    .await;
    drop(c);
    await_account(&obs, &name);

    // Wrong password is refused.
    let mut bad = Client::connect(server.addr()).await;
    bad.send(&soils_protocol::ClientMsg::Login {
        name: name.clone(),
        password: "not-it".into(),
        signup: false,
        protocol: soils_protocol::PROTOCOL_VERSION,
    })
    .await;
    let refused = bad
        .recv_until(|m| match m {
            ServerMsg::LoginError { message } => Some(message),
            ServerMsg::Init { .. } => panic!("a wrong password was accepted"),
            _ => None,
        })
        .await;
    assert!(refused.to_lowercase().contains("password"), "unexpected refusal: {refused}");
    drop(bad);

    // The right one still works.
    let mut good = Client::connect(server.addr()).await;
    good.send(&soils_protocol::ClientMsg::Login {
        name: name.clone(),
        password: "hunter2".into(),
        signup: false,
        protocol: soils_protocol::PROTOCOL_VERSION,
    })
    .await;
    good.recv_until(|m| match m {
        ServerMsg::Init { .. } => Some(()),
        ServerMsg::LoginError { message } => panic!("correct password refused: {message}"),
        _ => None,
    })
    .await;
}

/// An account that already exists in the local file migrates into the database
/// on its next login, and keeps working afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_account_migrates_on_next_login() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to run the SpacetimeDB auth test");
        return;
    };
    let Some(obs) = observer(&cfg) else {
        return;
    };

    let name = unique_name("legacy");
    let dir = std::env::temp_dir().join(format!("soils-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // First run: no database, so the account is created locally only.
    {
        let server = TestServer::start_at(dir.clone(), "stdbmigrate-local");
        let mut c = Client::connect(server.addr()).await;
        c.send(&soils_protocol::ClientMsg::Login {
            name: name.clone(),
            password: "hunter2".into(),
            signup: true,
            protocol: soils_protocol::PROTOCOL_VERSION,
        })
        .await;
        c.recv_until(|m| match m {
            ServerMsg::Init { .. } => Some(()),
            ServerMsg::LoginError { message } => panic!("local signup refused: {message}"),
            _ => None,
        })
        .await;
    }
    assert!(
        obs.db().account().iter().all(|a| a.name != name),
        "the account should not be in the database before the migrating login"
    );

    // Second run: same data dir, now with a database. Logging in migrates it.
    let cfg_for_server = cfg.clone();
    let server = TestServer::start_at_with(dir.clone(), "stdbmigrate-db", move |c| {
        c.stdb = Some(cfg_for_server);
    });
    let mut c = Client::connect(server.addr()).await;
    c.send(&soils_protocol::ClientMsg::Login {
        name: name.clone(),
        password: "hunter2".into(),
        signup: false,
        protocol: soils_protocol::PROTOCOL_VERSION,
    })
    .await;
    c.recv_until(|m| match m {
        ServerMsg::Init { .. } => Some(()),
        ServerMsg::LoginError { message } => panic!("migrating login refused: {message}"),
        _ => None,
    })
    .await;
    drop(c);

    let account = await_account(&obs, &name);
    assert!(
        account.verifier.starts_with("$argon2"),
        "a migrated account should be rehashed with argon2, got {:?}",
        account.verifier
    );
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
