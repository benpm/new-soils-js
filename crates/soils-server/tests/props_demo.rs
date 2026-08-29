//! Records both players' first-person views while they walk through a pile of
//! several hundred network-synced rigid bodies and into each other.
//!
//! Ignored by default: it drives two real GPU clients and OBS Studio.
//!
//! ```sh
//! cargo build --release -p soils-client
//! python scripts/obs_scene.py --pane "new-soils [alice]" --pane "new-soils [bob]"
//! python scripts/obs_record.py ensure
//! cargo test --release -p soils-server --test props_demo -- --ignored --nocapture
//! ```
//!
//! Unlike `demo.rs`, there is no spectator: each client *is* one of the players
//! (`SOILS_BOT`), because a first-person view has to come from that player's own
//! camera and prediction. Following someone else's replicated body would show
//! their interpolated, delayed position — the opposite of what they see.
//!
//! OBS composites the two windows side by side into one 2560x720 canvas, so the
//! comparison is frame-synchronous by construction rather than two recordings
//! that have to be lined up afterwards.
//!
//! Both bots wait on one shared start file, so their routines are choreographed
//! against a single signal instead of each client's own stream-in time.

mod common;

use common::{demo_budget, demo_secs, demo_var, recorder, workspace_root};

use std::process::{Child, Command};
use std::time::Duration;

/// Rigid bodies dropped near spawn. Matches `props.rs`, so the footage shows
/// exactly the scene the assertions cover.
const PROPS: u16 = 300;
/// Seconds of routine to record, once both clients report the world is up.
const TAKE_SECS: &str = "45";

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

/// Launch one bot client. `role` selects the mirrored half of the routine.
fn spawn_bot(
    addr: std::net::SocketAddr,
    name: &str,
    role: &str,
    ready_file: &std::path::Path,
    start_file: &std::path::Path,
) -> Child {
    let bin = client_binary()
        .expect("no client binary — run `cargo build --release -p soils-client` first");
    Command::new(bin)
        // Bevy resolves assets next to the executable when launched directly,
        // and they live under the client crate, not the workspace root.
        .env("BEVY_ASSET_ROOT", workspace_root().join("crates/soils-client"))
        .env("SOILS_AUTOLOGIN", addr.to_string())
        // Also becomes the window title, which is how OBS tells the two
        // clients of the same executable apart.
        .env("SOILS_NAME", name)
        .env("SOILS_BOT", role)
        .env("SOILS_BOT_START", start_file)
        .env("SOILS_READY_FILE", ready_file)
        .env("SOILS_RECORD_AFTER", demo_secs(20.0))
        .env("SOILS_RECORD_WAIT", demo_secs(60.0))
        .env("SOILS_RECORD_SECS", TAKE_SECS)
        .env("SOILS_RECORD_EXIT", "1")
        .env("SOILS_RADIUS", demo_var("SOILS_DEMO_RADIUS", "2"))
        .env("SOILS_DAYTIME", "0.0")
        .env("SOILS_NOFOCUS", "1")
        .env("SOILS_VSYNC", "1")
        .spawn()
        .expect("launch bot client")
}

fn await_ready(paths: &[std::path::PathBuf], kids: &mut [Child]) {
    let deadline = std::time::Instant::now() + demo_budget(300.0);
    loop {
        if paths.iter().all(|p| p.exists()) {
            return;
        }
        for (i, k) in kids.iter_mut().enumerate() {
            if let Ok(Some(status)) = k.try_wait() {
                panic!("client {i} exited before the world was ready ({status})");
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "clients never both signalled readiness"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "drives two GPU clients and OBS; run deliberately to record"]
fn record_two_first_person_views() {
    let root = workspace_root();
    let ready: Vec<std::path::PathBuf> = ["alice", "bob"]
        .iter()
        .map(|n| root.join(format!("target/soils-ready-{n}")))
        .collect();
    let start = root.join("target/soils-bot-start");
    for p in ready.iter().chain(std::iter::once(&start)) {
        let _ = std::fs::remove_file(p);
    }

    let server = common::TestServer::start_with("propsdemo", |c| {
        c.physics = true;
        c.props = PROPS;
    });
    let addr = server.addr();
    println!("demo server on {addr} with {PROPS} props");

    let mut kids = vec![
        spawn_bot(addr, "alice", "a", &ready[0], &start),
        spawn_bot(addr, "bob", "b", &ready[1], &start),
    ];

    // Roll only once *both* clients have the world on screen, then release the
    // bots together — the two takes are only comparable if the routines start
    // on the same signal.
    await_ready(&ready, &mut kids);
    recorder("start");
    std::fs::write(&start, "go").expect("write bot start file");
    println!("both clients ready; bots released");

    for (i, k) in kids.iter_mut().enumerate() {
        let status = k.wait().expect("client");
        println!("client {i} exited: {status}");
    }
    let recorded = recorder("stop");

    let path = recorded
        .lines()
        .find_map(|l| l.strip_prefix("recorded: "))
        .unwrap_or_else(|| panic!("obs_record.py stop reported no file:\n{recorded}"));
    let meta =
        std::fs::metadata(path).unwrap_or_else(|e| panic!("recording {path} is missing: {e}"));
    println!("recorded {path} ({:.1} MB)", meta.len() as f64 / 1e6);
    assert!(meta.len() > 200_000, "recording is suspiciously small ({} bytes)", meta.len());
}
