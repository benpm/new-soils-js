//! Records a video of two players colliding, blocking, and standing on each
//! other's head — the visual counterpart to the assertions in `concurrent.rs`.
//!
//! Ignored by default: it drives a real GPU client and OBS Studio, so it
//! belongs to a deliberate recording session rather than to `cargo test`.
//!
//! ```sh
//! cargo build --release -p soils-client
//! python scripts/obs_record.py ensure
//! cargo test --release -p soils-server --test demo -- --ignored --nocapture
//! ```
//!
//! Run it in **release**. A debug-built embedded server cannot generate and
//! light chunks fast enough for the world to be on screen at all, so the take
//! opens on an empty void.
//!
//! Three processes cooperate: this test hosts the server and scripts the two
//! participants, a third client joins purely to watch, and OBS captures that
//! client's window. A spectator is necessary rather than convenient — a
//! first-person camera cannot show two bodies meeting, and a hand-driven third
//! player would not be reproducible.
//!
//! The spectator signals `SOILS_READY_FILE` once the world has actually
//! streamed and meshed; that is the cue to start recording. Waiting a fixed
//! interval instead would open the take on empty sky, which is how the first
//! attempts at this failed.
//!
//! `SOILS_DEMO_NETSIM` (default `120,40,0.05`) sets the *spectator's* link.
//! Degrading the observer is the point: the interaction is resolved
//! server-side, so what a bad link changes is whether the watcher still sees
//! smooth motion. That is exactly what the recording is for. Set it to
//! `0,0,0` for a clean-link take to compare against.

mod common;

use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{TestServer, recorder, spawn_peer, workspace_root};
use tokio::sync::Barrier;

const WEST: f32 = std::f32::consts::FRAC_PI_2;
const EAST: f32 = -std::f32::consts::FRAC_PI_2;
const SPAWN_CHUNK: [i32; 3] = [8, 8, 8];
/// Hard ceiling on the performance, in case the spectator never exits.
const MAX_TAKE_SECS: u64 = 400;
/// Length of the recorded take, handed to the spectator.
const TAKE_SECS_STR: &str = "40";

/// The client binary: release if it has been built (much smoother capture, and
/// a debug Bevy client is too slow to judge motion by), debug otherwise.
/// `SOILS_CLIENT_BIN` overrides.
/// Barrier wait with a deadline. Without one, a participant that stalls leaves
/// the other waiting forever and the recording captures a motionless stage —
/// which is exactly what happened the first time.
async fn sync(b: &Barrier, what: &str) {
    match tokio::time::timeout(Duration::from_secs(90), b.wait()).await {
        Ok(_) => println!("  [{what}] both ready"),
        Err(_) => panic!("timed out at barrier {what:?}"),
    }
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

/// Launch the spectating client, parked so both participants are in frame.
fn spawn_spectator(
    addr: std::net::SocketAddr,
    ready_file: &std::path::Path,
    eye: [f32; 3],
    at: [f32; 3],
) -> Option<Child> {
    let bin = client_binary()?;
    let spectate = format!("{},{},{},{},{},{}", eye[0], eye[1], eye[2], at[0], at[1], at[2]);
    let netsim =
        std::env::var("SOILS_DEMO_NETSIM").unwrap_or_else(|_| "120,40,0.05".to_string());
    println!("spectator: {}", bin.display());
    Command::new(bin)
        // Bevy resolves assets next to the *executable* when it is launched
        // directly, so a bare target/release/soils-client finds no shaders and
        // renders an empty void with no error beyond a few asset-path lines.
        // Assets live under the client crate, not the workspace root.
        .env("BEVY_ASSET_ROOT", workspace_root().join("crates/soils-client"))
        .env("SOILS_AUTOLOGIN", addr.to_string())
        .env("SOILS_NAME", "camera")
        .env("SOILS_SPECTATE", spectate)
        .env("SOILS_READY_FILE", ready_file)
        .env("SOILS_RECORD_AFTER", "20")
        // `pending` counts the local view box, which does not reach zero while
        // the server is still filling the outer ring, so cap the wait.
        .env("SOILS_RECORD_WAIT", "45")
        .env("SOILS_RECORD_SECS", TAKE_SECS_STR)
        // A small view radius: the action is within ~10 units of spawn, and
        // radius 4 is 729 chunks for a debug-built server to generate before
        // anything is worth filming.
        .env("SOILS_RADIUS", "2")
        // Pin noon: a take long enough to be worth watching otherwise drifts
        // into night and the participants go dark.
        .env("SOILS_DAYTIME", "0.0")
        .env("SOILS_RECORD_EXIT", "1")
        .env("SOILS_NETSIM", netsim)
        // Visible (so it renders through the normal swapchain) but never
        // stealing focus.
        .env("SOILS_NOFOCUS", "1")
        .env("SOILS_VSYNC", "1")
        .spawn()
        .map_err(|e| eprintln!("could not launch client: {e}"))
        .ok()
}

/// Block until the spectator reports the world is on screen.
fn await_ready(path: &std::path::Path, spectator: &mut Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    while !path.exists() {
        if let Ok(Some(status)) = spectator.try_wait() {
            panic!("spectator exited before the world was ready ({status})");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "spectator never signalled readiness at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "drives a real GPU client and OBS; run deliberately to record"]
fn record_two_player_demo() {
    let ready_file = workspace_root().join("target/soils-demo-ready");
    let _ = std::fs::remove_file(&ready_file);

    let server = TestServer::start("demo");
    let addr = server.addr();
    println!("demo server on {addr}");

    // Framed on the platform the participants build, from a three-quarter view
    // so blocking (horizontal) and stacking (vertical) both read clearly.
    // Failing loudly matters here: silently returning would report a passing
    // test that recorded nothing.
    let mut spectator =
        spawn_spectator(addr, &ready_file, [288.0, 284.5, 274.0], [281.0, 282.3, 268.0])
            .expect("no client binary — run `cargo build --release -p soils-client` first");

    let built = Arc::new(Barrier::new(2));
    let ready = Arc::new(Barrier::new(2));
    // The participants perform until the camera stops, not for a guessed
    // duration: the spectator spends its first minute or two streaming the
    // world in, and bots on a fixed timer finish and disconnect before the
    // recording window ever opens — leaving an empty stage on film.
    let stop = Arc::new(AtomicBool::new(false));

    // Bob: the obstacle. Lands on the spawn column and holds it.
    let bob = {
        let (built, ready, stop) = (built.clone(), ready.clone(), stop.clone());
        spawn_peer(addr, "bob", None, move |mut c| async move {
            c.await_chunk(SPAWN_CHUNK).await;
            println!("bob: spawn chunk resident");
            sync(&built, "built").await;
            let rest = c.land().await;
            println!("bob: landed at {rest:?}");
            sync(&ready, "ready").await;
            // Hold still for the whole take: idle frames keep gravity applied,
            // and a silent client would simply freeze mid-air.
            let end = tokio::time::Instant::now() + Duration::from_secs(MAX_TAKE_SECS);
            while !stop.load(Ordering::Relaxed) && tokio::time::Instant::now() < end {
                c.idle_for(Duration::from_secs(1)).await;
            }
        })
    };

    // Alice: builds the stage, then charges Bob, then climbs onto him.
    let alice = {
        let (built, ready, stop) = (built.clone(), ready.clone(), stop.clone());
        spawn_peer(addr, "alice", None, move |mut c| async move {
            c.await_chunk(SPAWN_CHUNK).await;
            let spawn = c.spawn;
            let (sx, sy, sz) =
                (spawn[0].floor() as i32, spawn[1].floor() as i32, spawn[2].floor() as i32);
            // A flat stage, so nothing in the shot depends on the terrain that
            // happens to be under the spawn point.
            for x in sx - 6..=sx + 6 {
                for z in sz - 2..=sz + 2 {
                    let seq = c.edit([x, sy - 5, z], 1).await;
                    c.recv_until(|m| match m {
                        soils_protocol::ServerMsg::EditAccepted { seq: s, .. } if s == seq => {
                            Some(())
                        }
                        // Placement spends the block. Without this arm a stock
                        // shortfall hangs here rather than failing.
                        soils_protocol::ServerMsg::EditRejected { seq: s } if s == seq => {
                            panic!("stage block at {x},{z} rejected — out of reach or out of stock")
                        }
                        _ => None,
                    })
                    .await;
                }
            }
            println!("alice: stage built ({} blocks)", 13 * 5);
            sync(&built, "built").await;

            c.fly(24, WEST, false).await; // clear of the spawn column, noclip
            let rest = c.land().await;
            let start_x = rest[0];
            println!("alice: landed at {rest:?}");
            sync(&ready, "ready").await;

            // Loop the whole routine for the length of the take. Looping
            // rather than running it once matters: the spectator spends its
            // first seconds streaming the world in, and a one-shot routine
            // would be over before the recording window opens.
            let end = tokio::time::Instant::now() + Duration::from_secs(MAX_TAKE_SECS);
            let mut pass = 0;
            while !stop.load(Ordering::Relaxed) && tokio::time::Instant::now() < end {
                pass += 1;
                println!("alice: pass {pass} at {:?}", c.current_self_pos().await);
                // Charge Bob twice: 5 units of walk into a body 3 away, so
                // she is stopped by him rather than arriving next to him.
                for _ in 0..2 {
                    c.walk(40, EAST).await;
                    c.idle_for(Duration::from_millis(400)).await;
                    c.walk(24, WEST).await; // 3 units back, matching the gap
                    c.idle_for(Duration::from_millis(400)).await;
                }

                // Climb: fly up and over Bob, drop onto his head, jump on it.
                c.toggle_fly().await;
                c.drive(28, |_| soils_sim::PlayerInput {
                    move_axes: glam::Vec2::new(0.0, 1.0),
                    yaw: EAST,
                    up: true,
                    ..Default::default()
                })
                .await;
                c.land().await; // onto Bob, not the stage
                for _ in 0..3 {
                    c.drive(1, |_| soils_sim::PlayerInput { jump: true, ..Default::default() })
                        .await;
                    c.idle_for(Duration::from_millis(450)).await;
                }

                // Step west off his head, back to the starting mark. Walking
                // (not flying) keeps her on the slab; the previous version
                // added a further fly leg and walked her off the edge.
                c.walk(24, WEST).await;
                c.idle_for(Duration::from_millis(400)).await;

                // Drift guard: the legs above are balanced, but a blocked
                // charge is not exactly reversible, so re-centre rather than
                // letting a slow westward creep reach the edge.
                let here = c.current_self_pos().await;
                if here[0] < start_x - 1.5 {
                    c.walk(16, EAST).await;
                } else if here[0] > start_x + 1.5 {
                    c.walk(16, WEST).await;
                }
            }
        })
    };

    // Roll only once the world is actually on screen, so the take never opens
    // on empty sky.
    await_ready(&ready_file, &mut spectator);
    recorder("start");

    // The spectator ends the take (SOILS_RECORD_SECS then exit); the
    // participants keep performing until it does.
    let status = spectator.wait().expect("spectator client");
    let recorded = recorder("stop");
    println!("spectator exited: {status}");
    stop.store(true, Ordering::Relaxed);
    let _ = alice.join();
    let _ = bob.join();

    let path = recorded
        .lines()
        .find_map(|l| l.strip_prefix("recorded: "))
        .unwrap_or_else(|| panic!("obs_record.py stop did not report a file:\n{recorded}"));
    let meta = std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("recording {path} is missing: {e}"));
    println!("recorded {path} ({:.1} MB)", meta.len() as f64 / 1e6);
    assert!(meta.len() > 200_000, "recording is suspiciously small ({} bytes)", meta.len());
}
