//! Records one player working the inventory loop: place a block, mine it back,
//! watch the item drop where it stood, walk onto it to collect it, open the
//! inventory screen to see it counted, and finally put down a Wooden Crate and
//! right-click it open.
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

use common::{demo_budget, demo_secs, demo_var, recorder, workspace_root};

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Beats in the bot's inventory script (`INV_BEATS` in `soils-client::bot`).
/// The take is only worth publishing if every one of them fired.
const EXPECTED_BEATS: usize = 14;

/// Seconds of routine to record. The bot's script (`SOILS_BOT=inv`) runs about
/// 33 s after landing, and landing from the spawn height costs ~5 s.
const TAKE_SECS: &str = "40";

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
        .env("SOILS_RECORD_AFTER", demo_secs(20.0))
        // Wait for the world, do not time out into it. The first take of this
        // cued after 60s with 386 chunks still streaming: the player dropped out
        // of fly mode into terrain that did not exist yet and fell through the
        // world, so the whole recording was empty fog. A demo that opens on an
        // unfinished world is not a shorter demo, it is a broken one.
        .env("SOILS_RECORD_WAIT", demo_secs(600.0))
        .env("SOILS_RECORD_SECS", TAKE_SECS)
        .env("SOILS_RECORD_EXIT", "1")
        // Matches props_demo. A wider radius is more to stream before the take
        // can start and buys nothing at this pitch — the camera is looking at
        // the ground a metre away.
        .env("SOILS_RADIUS", demo_var("SOILS_DEMO_RADIUS", "2"))
        // Midday: the item on the ground has to be readable, and the drop is a
        // 0.3-unit cube.
        .env("SOILS_DAYTIME", "0.0")
        .env("SOILS_NOFOCUS", demo_var("SOILS_DEMO_NOFOCUS", "1"))
        .env("SOILS_VSYNC", "1")
        // The database is not part of what this films, and leaving it set would
        // make the take depend on a service being up.
        .env_remove("SOILS_STDB_URI")
        .env_remove("SOILS_STDB_TOKEN")
        // Captured so the test can prove the routine ran. Reading it is
        // deferred until after the client exits, which is safe only because
        // the pipe is drained in one go at the end — see the wait below.
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch bot client")
}

fn await_ready(path: &std::path::Path, kid: &mut Child) {
    // Must exceed the client's own SOILS_RECORD_WAIT, or this gives up first
    // and the failure reads as "client never signalled" rather than "the world
    // took longer than expected to stream".
    let deadline = std::time::Instant::now() + demo_budget(660.0);
    loop {
        if path.exists() {
            return;
        }
        if let Ok(Some(status)) = kid.try_wait() {
            panic!("client exited before the world was ready ({status})");
        }
        if std::time::Instant::now() >= deadline {
            // stderr is piped, and on this path it was previously dropped —
            // so a timeout said "never signalled readiness" and nothing about
            // why. Kill the client first: the pipe only reaches EOF once the
            // writer is gone, so reading it from a live child blocks forever.
            let _ = kid.kill();
            let mut log = String::new();
            if let Some(mut err) = kid.stderr.take() {
                let _ = err.read_to_string(&mut log);
            }
            let tail: Vec<&str> = log.lines().rev().take(40).collect();
            for line in tail.into_iter().rev() {
                println!("client| {line}");
            }
            panic!("client never signalled readiness (its last output is above)");
        }
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

    // Check OBS *before* anything expensive. Streaming the world takes minutes,
    // and discovering a dead recorder after that wastes the whole run — which is
    // exactly how the fourth take was lost.
    recorder("status");

    let server = common::TestServer::start("invdemo");
    let addr = server.addr();
    println!("demo server on {addr}");

    let mut kid = spawn_bot(addr, "miner", &ready, &start);

    // Roll only once the world is on screen. Starting earlier films the join
    // burst, which is a different demo.
    await_ready(&ready, &mut kid);
    recorder("start");
    std::fs::write(&start, "go").expect("write bot start file");
    println!("client ready; bot released");

    // Drain stderr *before* waiting: a client that fills the pipe buffer would
    // block forever on a write while we block forever on the exit.
    let mut log = String::new();
    if let Some(mut err) = kid.stderr.take() {
        let _ = err.read_to_string(&mut log);
    }
    let status = kid.wait().expect("client");
    println!("client exited: {status}");
    let recorded = recorder("stop");

    // A stalled routine produces a file of the right length full of nothing
    // happening. That is indistinguishable from a good take by size alone —
    // the first attempt at this recorded 34s of empty fog and passed.
    let beats = log.matches("bot: beat ").count();
    assert!(log.contains("inventory routine starts"), "the bot never landed:
{log}");
    assert_eq!(
        beats, EXPECTED_BEATS,
        "only {beats} of {EXPECTED_BEATS} script beats fired — the take shows a          routine that stalled partway through"
    );

    let path = recorded
        .lines()
        .find_map(|l| l.strip_prefix("recorded: "))
        .unwrap_or_else(|| panic!("obs_record.py stop reported no file:\n{recorded}"));
    let meta =
        std::fs::metadata(path).unwrap_or_else(|e| panic!("recording {path} is missing: {e}"));
    println!("recorded {path} ({:.1} MB)", meta.len() as f64 / 1e6);
    assert!(meta.len() > 200_000, "recording is suspiciously small ({} bytes)", meta.len());
}
