//! Records one player lighting a dark room: fly down into the carved chamber
//! ~100 voxels under spawn, then ring yourself with Lamp Blocks and watch each
//! one take another sector of the floor out of the dark.
//!
//! Ignored by default: it drives a real GPU client and OBS Studio.
//!
//! ```sh
//! cargo build --release -p soils-client
//! python scripts/obs_scene.py --pane "new-soils [lamplighter]"
//! python scripts/obs_record.py ensure
//! cargo test --release -p soils-server --test light_demo -- --ignored --nocapture
//! ```
//!
//! This is the take whose subject *is* the lighting, which makes it the one
//! that would show a regression in the flood. Two things are deliberately
//! switched off for it: the player's own lantern (`SOILS_PLAYER_LIGHT=0`, or a
//! level-12 emitter rides the camera and the room is never dark), and any
//! reliance on the sun (the chamber is sealed, so every photon on screen comes
//! from a block the player put there).
//!
//! The bot flies down *before* the recorder is cued, not after. Streaming the
//! chamber is most of what readiness waits for, and a take that opened on it
//! arriving would film the optimistic-open-sky correction rolling through —
//! the exact flicker `record::cue`'s light gate exists to keep off film.

mod common;

use common::{demo_budget, demo_secs, demo_var, recorder, workspace_root};

use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Beats in the bot's light script (`LIGHT_BEATS` in `soils-client::bot`).
/// The take is only worth publishing if every one of them fired.
const EXPECTED_BEATS: usize = 16;

/// Seconds of routine to record. The script runs 31.5 s from the start signal;
/// the flight down is pre-roll, so unlike the inventory demo no landing budget
/// is needed inside the take itself.
const TAKE_SECS: &str = "35";
/// Earliest the client may cue the recorder, and how long it waits for the
/// world to finish streaming before cueing anyway. `await_ready`'s deadline is
/// derived from both, so the two cannot drift apart.
const RECORD_AFTER: f32 = 20.0;
const RECORD_WAIT: f32 = 600.0;

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
        .env("SOILS_BOT", "light")
        .env("SOILS_BOT_START", start_file)
        .env("SOILS_READY_FILE", ready_file)
        .env("SOILS_RECORD_AFTER", demo_secs(RECORD_AFTER))
        // Wait for the world, do not time out into it. The first take of this
        // cued after 60s with 386 chunks still streaming: the player dropped out
        // of fly mode into terrain that did not exist yet and fell through the
        // world, so the whole recording was empty fog. A demo that opens on an
        // unfinished world is not a shorter demo, it is a broken one.
        .env("SOILS_RECORD_WAIT", demo_secs(RECORD_WAIT))
        .env("SOILS_RECORD_SECS", TAKE_SECS)
        .env("SOILS_RECORD_EXIT", "1")
        // Matches props_demo. A wider radius is more to stream before the take
        // can start and buys nothing at this pitch — the camera is looking at
        // the ground a metre away.
        .env("SOILS_RADIUS", demo_var("SOILS_DEMO_RADIUS", "2"))
        // Midday: the item on the ground has to be readable, and the drop is a
        // 0.3-unit cube.
        // Noon: exposure is `EV100_DAY` rather than the much darker
        // `EV100_NIGHT`, so the lamps read at the brightness they were tuned
        // for. Underground the sun contributes nothing either way — this only
        // sets the camera.
        .env("SOILS_DAYTIME", "0.0")
        // The whole point: no lantern on the camera, so the room is dark until
        // a lamp is placed in it.
        .env("SOILS_PLAYER_LIGHT", "0")
        // Derived from the same numbers that carve the room, because the spawn
        // height follows the terrain and only a delta means anything here.
        .env("SOILS_BOT_DESCENT", format!("{:.0}", soils_server::Chamber::DEMO.descent_from_spawn()))
        // GI is what gives the lamps their colour; the L0 glow is monochrome.
        // Overridable because it is expensive on a software rasteriser.
        .env("SOILS_GI", demo_var("SOILS_DEMO_GI", "1"))
        .env("SOILS_NOFOCUS", demo_var("SOILS_DEMO_NOFOCUS", "1"))
        .env("SOILS_VSYNC", "1")
        // The database is not part of what this films, and leaving it set would
        // make the take depend on a service being up.
        .env_remove("SOILS_STDB_URI")
        .env_remove("SOILS_STDB_TOKEN")
        // Captured so the test can prove the routine ran. Drained by a
        // thread from the moment of spawn — see `drain_stderr`.
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch bot client")
}

/// Read the client's stderr on a thread, into a buffer the test can inspect.
///
/// A piped stderr that nobody reads is a 64 KB buffer that fills and then
/// blocks the writer forever. This client fills it easily — winit repeats
/// "Cursor could not be confined" every frame on a display with no window
/// manager, and Bevy adds a `CommandQueue` warning per dropped command — so
/// the client would log a few lines, wedge on a write, and never reach the
/// point of signalling readiness. That is precisely what happened on CI: five
/// runs where the client printed `gpu caps` and then nothing at all for
/// three quarters of an hour.
///
/// The exit path below already knew this ("drain stderr *before* waiting"),
/// but `await_ready` runs first and nothing was reading during it. Draining
/// from spawn covers both waits and keeps the log for the beat assertions.
fn drain_stderr(kid: &mut Child) -> (Arc<Mutex<String>>, Option<std::thread::JoinHandle<()>>) {
    let buf = Arc::new(Mutex::new(String::new()));
    let handle = kid.stderr.take().map(|err| {
        let sink = Arc::clone(&buf);
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut g = sink.lock().unwrap();
                g.push_str(&line);
                g.push_str("\n");
            }
        })
    });
    (buf, handle)
}

fn await_ready(path: &std::path::Path, kid: &mut Child, log: &Arc<Mutex<String>>) {
    // Derived from the client's own budget rather than picked: it cues
    // unconditionally at RECORD_AFTER + RECORD_WAIT, so anything less than that
    // plus real slack is a race this side loses — and the failure then reads as
    // "client never signalled" rather than "we gave up first". It was 660
    // against a 620 cue: forty seconds, most of which the client spends
    // starting up. On CI that lost every time, for six runs.
    let deadline =
        std::time::Instant::now() + demo_budget((RECORD_AFTER + RECORD_WAIT) * 1.4);
    loop {
        if path.exists() {
            return;
        }
        if let Ok(Some(status)) = kid.try_wait() {
            panic!("client exited before the world was ready ({status})");
        }
        if std::time::Instant::now() >= deadline {
            // Show what the client was doing rather than only that it stopped.
            let _ = kid.kill();
            let captured = log.lock().unwrap().clone();
            let tail: Vec<&str> = captured.lines().rev().take(40).collect();
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
fn record_the_lit_room() {
    let root = workspace_root();
    let ready = root.join("target/soils-ready-lamplighter");
    let start = root.join("target/soils-bot-start-light");
    let _ = std::fs::remove_file(&ready);
    let _ = std::fs::remove_file(&start);

    // Check OBS *before* anything expensive. Streaming the world takes minutes,
    // and discovering a dead recorder after that wastes the whole run — which is
    // exactly how the fourth take was lost.
    recorder("status");

    let server = common::TestServer::start_with("lightdemo", |c| {
        c.chamber = Some(soils_server::Chamber::DEMO)
    });
    let addr = server.addr();
    println!("demo server on {addr}");

    let mut kid = spawn_bot(addr, "lamplighter", &ready, &start);
    let (log, drain) = drain_stderr(&mut kid);

    // Roll only once the world is on screen. Starting earlier films the join
    // burst, which is a different demo.
    await_ready(&ready, &mut kid, &log);
    recorder("start");
    std::fs::write(&start, "go").expect("write bot start file");
    println!("client ready; bot released");

    let status = kid.wait().expect("client");
    // The pipe EOFs when the client exits; join so the buffer is complete
    // before anything is asserted about it.
    if let Some(d) = drain {
        let _ = d.join();
    }
    let log = log.lock().unwrap().clone();
    println!("client exited: {status}");
    let recorded = recorder("stop");

    // A stalled routine produces a file of the right length full of nothing
    // happening. That is indistinguishable from a good take by size alone —
    // the first attempt at this recorded 34s of empty fog and passed.
    let beats = log.matches("bot: beat ").count();
    assert!(
        log.contains("light routine starts"),
        "the bot never reached the chamber:\n{log}"
    );
    assert_eq!(
        beats, EXPECTED_BEATS,
        "only {beats} of {EXPECTED_BEATS} script beats fired — the take shows a \
         routine that stalled partway through"
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
