//! Recording support: tells an external recorder when the world is actually on
//! screen, and ends the take.
//!
//! The client does not encode video itself. Video comes from OBS, driven by
//! `scripts/obs_record.py` — an earlier version of this module saved a PNG per
//! frame, which perturbed the very frame clock the recording exists to judge
//! and needed a timestamp-aware mux to undo the damage. OBS captures the
//! window at a real 60 fps without touching the render loop.
//!
//! What the client still owns is *when*: only it knows whether the surrounding
//! world has streamed and meshed. How long that takes depends on view radius,
//! disk cache and whether the server is a debug build, so any fixed delay the
//! recorder might guess is wrong somewhere — and a take that opens on an empty
//! void is worse than no take.
//!
//! Environment:
//! * `SOILS_READY_FILE=<path>` — enable; the file is created once the world is
//!   ready, which is the recorder's cue to start.
//! * `SOILS_RECORD_AFTER=<secs>` — earliest cue (default 6).
//! * `SOILS_RECORD_WAIT=<secs>` — cue anyway after this long, so a stream that
//!   never settles still yields evidence (default 120).
//! * `SOILS_RECORD_SECS=<secs>` — how long to stay alive after the cue
//!   (default 20).
//! * `SOILS_RECORD_EXIT=1` — quit when that elapses, ending the take.

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::time::Real;

/// How long the light backlog must sit at zero before the world counts as
/// ready.
///
/// One frame is not enough. `record::cue` has no ordering against
/// `demand::process_demands` or `gpu_light::plan_light_jobs`, so a single zero
/// reading can be the gap between one frame's drain and the next frame's
/// intake.
const LIGHT_SETTLE_SECS: f32 = 1.0;

#[derive(Resource)]
pub struct CaptureCue {
    ready_file: PathBuf,
    after: f32,
    wait: f32,
    secs: f32,
    /// When the cue fired.
    started: Option<f32>,
    /// When the light backlog last became empty, or `None` if it is not.
    light_quiet_since: Option<f32>,
    done: bool,
}

fn env_f32(key: &str, default: f32) -> f32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The configured cue, if `SOILS_READY_FILE` is set.
pub fn configured() -> Option<CaptureCue> {
    let ready_file = PathBuf::from(std::env::var("SOILS_READY_FILE").ok()?);
    if let Some(dir) = ready_file.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        error!("SOILS_READY_FILE: cannot create {}: {e}", dir.display());
        return None;
    }
    // A stale file from a previous run would cue the recorder instantly.
    let _ = std::fs::remove_file(&ready_file);
    Some(CaptureCue {
        ready_file,
        after: env_f32("SOILS_RECORD_AFTER", 6.0),
        wait: env_f32("SOILS_RECORD_WAIT", 120.0),
        secs: env_f32("SOILS_RECORD_SECS", 20.0),
        started: None,
        light_quiet_since: None,
        done: false,
    })
}

/// Signal readiness once the world is up, then end the take.
pub fn cue(
    // `Time<Real>`, not the default virtual clock. Bevy clamps virtual delta to
    // `max_delta` (250 ms) so a slow frame cannot spiral the simulation, which
    // means that below 4 fps virtual time advances *slower than wall time* —
    // at ~2 fps, half. Every number here is wall-clock by definition ("wait 600
    // seconds for the world", "record for 40 seconds"), and the test harness
    // times out against a real-time budget, so reading the virtual clock made
    // the cue fire late in proportion to how slow the renderer was. That is
    // why a software-rasterised take timed out while the same budgets passed
    // on a GPU.
    time: Res<Time<Real>>,
    streaming: Res<crate::player::Streaming>,
    light: Res<crate::light::LightQueue>,
    light_ready: Option<Res<crate::gpu_light::LightReady>>,
    mut cue: ResMut<CaptureCue>,
    mut exit: MessageWriter<AppExit>,
) {
    if cue.done {
        return;
    }
    let now = time.elapsed_secs();
    if now < cue.after {
        return;
    }

    // Streaming being finished is not the same as the world being *lit*.
    // `process_demands` drops a chunk from the pending set when it dispatches
    // generation and pushes it onto the light queue, so `pending` reads zero
    // while the flood for those chunks has not run — and an unflooded chunk is
    // drawn on the optimistic open-sky assumption, i.e. as full daylight.
    // Underground that is exactly the phantom-daylight window this demo exists
    // to show is gone, so the cue waits for the flood to drain as well.
    //
    // `LightReady` matters too: `plan_light_jobs` returns early until the
    // pipelines compile, so the queue can be non-empty *and* undrainable — an
    // empty reading before then means nothing has started, not that everything
    // finished.
    let (queued_chunks, queued_edits, _) = light.backlog();
    let flood_idle = light_ready.is_some() && queued_chunks == 0 && queued_edits == 0;
    if !flood_idle {
        cue.light_quiet_since = None;
    } else if cue.light_quiet_since.is_none() {
        cue.light_quiet_since = Some(now);
    }
    let lit = cue.light_quiet_since.is_some_and(|t| now - t >= LIGHT_SETTLE_SECS);

    let start = match cue.started {
        Some(t) => t,
        None => {
            let ready = streaming.pending == 0 && lit;
            if !ready && now < cue.after + cue.wait {
                return; // still streaming, or still flooding
            }
            if !ready {
                // Both halves, so a timed-out cue says which one never settled.
                warn!(
                    "SOILS_READY_FILE: cueing with {} chunks streaming and {}+{} light \
                     jobs queued — the take may open on an unfinished or unlit world",
                    streaming.pending, queued_chunks, queued_edits
                );
            }
            if let Err(e) = std::fs::write(&cue.ready_file, format!("{now:.2}")) {
                error!("SOILS_READY_FILE: cannot write {}: {e}", cue.ready_file.display());
            }
            info!("world ready at {now:.1}s — cued the recorder for {}s", cue.secs);
            cue.started = Some(now);
            now
        }
    };

    if now > start + cue.secs {
        cue.done = true;
        info!("take finished after {:.1}s", now - start);
        if std::env::var("SOILS_RECORD_EXIT").as_deref() == Ok("1") {
            exit.write(AppExit::Success);
        }
    }
}
