//! Records the SpacetimeDB layer working: two clients on a database-backed
//! server, seeing each other in the registry and talking to each other.
//!
//! Ignored by default — it drives two real GPU clients and OBS.
//!
//! ```sh
//! cargo build --release -p soils-client
//! python scripts/obs_scene.py --pane "new-soils [ana]" --pane "new-soils [bo]"
//! python scripts/obs_record.py ensure
//! SOILS_STDB_URI=http://127.0.0.1:3000 SOILS_STDB_TOKEN=<token> \
//!   cargo test --release -p soils-server --test stdb_demo -- --ignored --nocapture
//! ```
//!
//! Skips without a database, since a recording of the SpacetimeDB layer with no
//! SpacetimeDB would be a recording of nothing.

mod common;

use std::process::{Child, Command};
use std::time::Duration;

use soils_server::StdbConfig;

/// Seconds of routine to record once both clients report the world is up.
const TAKE_SECS: &str = "30";

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<pkg> lives two levels below the workspace root")
        .to_path_buf()
}

fn client_binary() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("SOILS_CLIENT_BIN") {
        return Some(p.into());
    }
    let exe = if cfg!(windows) { "soils-client.exe" } else { "soils-client" };
    let root = workspace_root();
    ["target/release", "target/debug"]
        .iter()
        .map(|d| root.join(d).join(exe))
        .find(|p| p.exists())
}

fn obs(action: &str) -> String {
    let script = workspace_root().join("scripts/obs_record.py");
    let out = Command::new("python")
        .arg(&script)
        .arg(action)
        .current_dir(workspace_root())
        .output()
        .unwrap_or_else(|e| panic!("could not run {}: {e}", script.display()));
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success(),
        "obs_record.py {action} failed:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    println!("obs {action}: {text}");
    text
}

/// Launch one client, pointed at both the game server and the database.
fn spawn_client(
    addr: std::net::SocketAddr,
    cfg: &StdbConfig,
    name: &str,
    role: &str,
    ready_file: &std::path::Path,
    start_file: &std::path::Path,
) -> Child {
    let bin = client_binary()
        .expect("no client binary — run `cargo build --release -p soils-client` first");
    let mut cmd = Command::new(bin);
    cmd.env("BEVY_ASSET_ROOT", workspace_root().join("crates/soils-client"))
        .env("SOILS_AUTOLOGIN", addr.to_string())
        .env("SOILS_NAME", name)
        .env("SOILS_BOT", role)
        .env("SOILS_BOT_START", start_file)
        .env("SOILS_READY_FILE", ready_file)
        .env("SOILS_RECORD_AFTER", "15")
        .env("SOILS_RECORD_WAIT", "45")
        .env("SOILS_RECORD_SECS", TAKE_SECS)
        .env("SOILS_RECORD_EXIT", "1")
        .env("SOILS_RADIUS", "2")
        .env("SOILS_DAYTIME", "0.0")
        .env("SOILS_NOFOCUS", "1")
        .env("SOILS_VSYNC", "1")
        // The same variables the server reads, so one setting points both at
        // one database — this is what lights up the lobby and chat.
        .env("SOILS_STDB_URI", &cfg.uri)
        .env("SOILS_STDB_DB", &cfg.database);
    // No token: each client takes an anonymous identity of its own. Sharing the
    // server's would make both clients one identity — and that identity is in
    // the module's server allowlist.
    //
    // Removed explicitly, not merely left unset: a child inherits this
    // process's environment, and the recording session is started with the
    // server's token exported. Both clients silently connected *as the server*
    // until this was added, so every chat line was attributed to whichever
    // account linked last.
    cmd.env_remove("SOILS_STDB_TOKEN").env_remove("SOILS_STDB_CLIENT_TOKEN");
    cmd.spawn().expect("launch client")
}

fn await_ready(paths: &[std::path::PathBuf], kids: &mut [Child]) {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        if paths.iter().all(|p| p.exists()) {
            return;
        }
        for (i, k) in kids.iter_mut().enumerate() {
            if let Ok(Some(status)) = k.try_wait() {
                panic!("client {i} exited before the world was ready ({status})");
            }
        }
        assert!(std::time::Instant::now() < deadline, "clients never signalled readiness");
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "drives two GPU clients and OBS; run deliberately to record"]
fn record_spacetimedb_lobby_and_chat() {
    let Some(cfg) = StdbConfig::from_env() else {
        eprintln!("skipping: set SOILS_STDB_URI to record the SpacetimeDB demo");
        return;
    };

    let root = workspace_root();
    let ready: Vec<std::path::PathBuf> = ["ana", "bo"]
        .iter()
        .map(|n| root.join(format!("target/soils-ready-{n}")))
        .collect();
    let start = root.join("target/soils-bot-start");
    for p in ready.iter().chain(std::iter::once(&start)) {
        let _ = std::fs::remove_file(p);
    }

    let cfg_for_server = cfg.clone();
    let server = common::TestServer::start_with("stdbdemo", move |c| {
        c.stdb = Some(cfg_for_server);
    });
    let addr = server.addr();
    println!("demo server on {addr}, mirroring to {}", cfg.uri);

    let mut kids = vec![
        spawn_client(addr, &cfg, "ana", "a", &ready[0], &start),
        spawn_client(addr, &cfg, "bo", "b", &ready[1], &start),
    ];

    await_ready(&ready, &mut kids);
    obs("start");
    std::fs::write(&start, "go").expect("write bot start file");
    println!("both clients ready; recording");

    for (i, k) in kids.iter_mut().enumerate() {
        let status = k.wait().expect("client");
        println!("client {i} exited: {status}");
    }
    let recorded = obs("stop");

    let path = recorded
        .lines()
        .find_map(|l| l.strip_prefix("recorded: "))
        .unwrap_or_else(|| panic!("obs_record.py stop reported no file:\n{recorded}"));
    let meta =
        std::fs::metadata(path).unwrap_or_else(|e| panic!("recording {path} is missing: {e}"));
    println!("recorded {path} ({:.1} MB)", meta.len() as f64 / 1e6);
    assert!(meta.len() > 200_000, "recording is suspiciously small ({} bytes)", meta.len());
}
