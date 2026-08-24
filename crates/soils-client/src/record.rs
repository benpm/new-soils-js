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

#[derive(Resource)]
pub struct CaptureCue {
    ready_file: PathBuf,
    after: f32,
    wait: f32,
    secs: f32,
    /// When the cue fired.
    started: Option<f32>,
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
        done: false,
    })
}

/// Signal readiness once the world is up, then end the take.
pub fn cue(
    time: Res<Time>,
    streaming: Res<crate::player::Streaming>,
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

    let start = match cue.started {
        Some(t) => t,
        None => {
            let ready = streaming.pending == 0;
            if !ready && now < cue.after + cue.wait {
                return; // still streaming; nothing worth filming yet
            }
            if !ready {
                warn!(
                    "SOILS_READY_FILE: cueing with {} chunks still streaming — the \
                     take may open on an unfinished world",
                    streaming.pending
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
