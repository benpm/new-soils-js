//! Records one player working the inventory loop: place a block, mine it back,
//! watch the item drop where it stood, walk onto it to collect it, and open the
//! inventory screen to see it counted.
//!
//! Ignored by default: it drives a real GPU client and OBS Studio.
//!
//! ```sh
//! cargo build --release -p soils-client
//! python scripts/obs_scene.py --pane "new-soils [miner]"
//! python scripts/obs_record.py ensure
//! cargo test --release -p soils-server --test inventory_demo -- --ignored --nocapture
//! ```
//!
//! One client, not two: the inventory is a first-person, single-player concern,
//! and the interesting frames are what *this* player's HUD and screen show. The
//! bot drives the same `ButtonInput` the keyboard and mouse write to, so what is
//! filmed is the path a person exercises — a bot with its own private edit path
//! would prove nothing about the one people use.

mod common;

use std::process::{Child, Command};
use std::time::Duration;

/// Seconds of routine to record. The bot's script (`SOILS_BOT=inv`) runs about
/// 24 s after landing, and landing from the spawn height costs ~5 s.
const TAKE_SECS: &str = "34";

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

fn spawn_bot(
    addr: std::net::SocketAddr,
    name: &str,
    ready_file: &std::path::Path,
    start_file: &std::path::Path,
) -> Child {
    let bin = client_binary()
        .expect("no client binary — run `cargo build --release -p soils-client` first");
    Command::new(bin)
        .env("BEVY_ASSET_ROOT", workspace_root().join("crates/soils-client"))
        .env("SOILS_AUTOLOGIN", addr.to_string())
        .env("SOILS_NAME", name)
        .env("SOILS_BOT", "inv")
        .env("SOILS_BOT_START", start_file)
        .env("SOILS_READY_FILE", ready_file)
        .env("SOILS_RECORD_AFTER", "20")
        // Wait for the world, do not time out into it. The first take of this
        // cued after 60s with 386 chunks still streaming: the player dropped out
        // of fly mode into terrain that did not exist yet and fell through the
        // world, so the whole recording was empty fog. A demo that opens on an
        // unfinished world is not a shorter demo, it is a broken one.
        .env("SOILS_RECORD_WAIT", "600")
        .env("SOILS_RECORD_SECS", TAKE_SECS)
        .env("SOILS_RECORD_EXIT", "1")
        // Matches props_demo. A wider radius is more to stream before the take
        // can start and buys nothing at this pitch — the camera is looking at
        // the ground a metre away.
        .env("SOILS_RADIUS", "2")
        // Midday: the item on the ground has to be readable, and the drop is a
        // 0.3-unit cube.
        .env("SOILS_DAYTIME", "0.0")
        .env("SOILS_NOFOCUS", "1")
        .env("SOILS_VSYNC", "1")
        // The database is not part of what this films, and leaving it set would
        // make the take depend on a service being up.
        .env_remove("SOILS_STDB_URI")
        .env_remove("SOILS_STDB_TOKEN")
        .spawn()
        .expect("launch bot client")
}

fn await_ready(path: &std::path::Path, kid: &mut Child) {
    // Must exceed the client's own SOILS_RECORD_WAIT, or this gives up first
    // and the failure reads as "client never signalled" rather than "the world
    // took longer than expected to stream".
    let deadline = std::time::Instant::now() + Duration::from_secs(660);
    loop {
        if path.exists() {
            return;
        }
        if let Ok(Some(status)) = kid.try_wait() {
            panic!("client exited before the world was ready ({status})");
        }
        assert!(std::time::Instant::now() < deadline, "client never signalled readiness");
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "drives a GPU client and OBS; run deliberately to record"]
fn record_the_inventory_loop() {
    let root = workspace_root();
    let ready = root.join("target/soils-ready-miner");
    let start = root.join("target/soils-bot-start");
    let _ = std::fs::remove_file(&ready);
    let _ = std::fs::remove_file(&start);

    let server = common::TestServer::start("invdemo");
    let addr = server.addr();
    println!("demo server on {addr}");

    let mut kid = spawn_bot(addr, "miner", &ready, &start);

    // Roll only once the world is on screen. Starting earlier films the join
    // burst, which is a different demo.
    await_ready(&ready, &mut kid);
    obs("start");
    std::fs::write(&start, "go").expect("write bot start file");
    println!("client ready; bot released");

    let status = kid.wait().expect("client");
    println!("client exited: {status}");
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
