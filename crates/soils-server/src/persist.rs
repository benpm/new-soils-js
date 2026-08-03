//! Background chunk persistence: a dedicated writer thread that drains save
//! jobs off the request/connection path so disk I/O never blocks worldgen,
//! streaming, or edits.
//!
//! Freshly generated chunks and edits are *enqueued* (a cheap clone + channel
//! send, safe to do while holding the `World` mutex) and written later by the
//! writer thread, which coalesces all currently-queued jobs and writes each
//! region file at most once per drain (see [`region::save_many`]).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel};
use std::thread::JoinHandle;

use glam::IVec3;
use soils_protocol::ChunkVolume;
use soils_stdb::{StdbCmd, StdbLink};

use crate::region;

/// One chunk to persist, carrying its world's region directory so a single
/// writer can serve every world.
pub struct SaveJob {
    pub dir: PathBuf,
    pub pos: IVec3,
    pub volume: ChunkVolume,
    /// Lands in the region header's `EDITED_FLAG` (drives manifest classes).
    pub edited: bool,
    /// `soils_protocol::chunk_key` of this chunk, when SpacetimeDB mirroring is
    /// on. `None` leaves the job disk-only.
    pub stdb_key: Option<u64>,
}

enum Msg {
    Save(SaveJob),
    /// Drain everything queued, then ack so the caller knows the flush is done.
    Shutdown(SyncSender<()>),
}

/// A cheap, cloneable sender used by each `World` to enqueue saves.
#[derive(Clone)]
pub struct PersistHandle {
    tx: Sender<Msg>,
}

impl PersistHandle {
    /// Queue a chunk for background persistence. Never blocks on disk; the only
    /// cost is cloning the volume (done by the caller) and a channel send. If
    /// the writer has gone away the job is silently dropped.
    pub fn enqueue(&self, dir: PathBuf, pos: IVec3, volume: ChunkVolume, edited: bool) {
        self.enqueue_with_key(dir, pos, volume, edited, None);
    }

    /// As [`enqueue`](Self::enqueue), but also mirrors the chunk into
    /// SpacetimeDB under `stdb_key` when mirroring is enabled.
    pub fn enqueue_with_key(
        &self,
        dir: PathBuf,
        pos: IVec3,
        volume: ChunkVolume,
        edited: bool,
        stdb_key: Option<u64>,
    ) {
        let _ = self.tx.send(Msg::Save(SaveJob { dir, pos, volume, edited, stdb_key }));
    }
}

/// Owns the writer thread. Kept outside the shared server state so it can be
/// joined on shutdown; dropping it just closes the channel (the thread then
/// drains and exits on its own).
pub struct Persister {
    tx: Sender<Msg>,
    handle: Option<JoinHandle<()>>,
}

impl Persister {
    pub fn new() -> Self {
        Self::with_stdb(None)
    }

    /// As [`new`](Self::new), but every save carrying a `stdb_key` is also
    /// mirrored into SpacetimeDB.
    ///
    /// Region files stay the source of truth: the mirror is written *after* a
    /// successful disk write, and a SpacetimeDB failure is logged rather than
    /// propagated, so losing the database can never lose a chunk.
    pub fn with_stdb(stdb: Option<Arc<StdbLink>>) -> Self {
        let (tx, rx) = channel::<Msg>();
        let handle = std::thread::Builder::new()
            .name("soils-chunk-writer".into())
            .spawn(move || writer_loop(rx, stdb))
            .expect("spawn chunk writer thread");
        Self { tx, handle: Some(handle) }
    }

    /// A sender to clone into each `World`.
    pub fn handle(&self) -> PersistHandle {
        PersistHandle { tx: self.tx.clone() }
    }

    /// Flush all queued jobs and stop the writer thread. Blocks until the final
    /// drain has hit disk, so a clean shutdown never loses queued writes.
    pub fn shutdown(mut self) {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(0);
        if self.tx.send(Msg::Shutdown(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn writer_loop(rx: Receiver<Msg>, stdb: Option<Arc<StdbLink>>) {
    // Block for the next job, then greedily drain everything already queued and
    // write it in one coalesced pass — so the queue stays near-empty and a
    // fresh-world burst collapses into a few region-file writes.
    while let Ok(first) = rx.recv() {
        let mut batch: Vec<SaveJob> = Vec::new();
        let mut ack: Option<SyncSender<()>> = None;
        match first {
            Msg::Save(job) => batch.push(job),
            Msg::Shutdown(a) => ack = Some(a),
        }
        loop {
            match rx.try_recv() {
                Ok(Msg::Save(job)) => batch.push(job),
                Ok(Msg::Shutdown(a)) => ack = Some(a),
                Err(_) => break,
            }
        }

        flush_batch(batch, stdb.as_deref());

        if let Some(a) = ack {
            let _ = a.send(());
            return;
        }
    }
}

/// Write a drained batch, grouping by world directory so each region file is
/// opened once. A failing region is logged and skipped so one bad write can't
/// kill the writer.
fn flush_batch(batch: Vec<SaveJob>, stdb: Option<&StdbLink>) {
    use std::collections::HashMap;
    let mut by_dir: HashMap<PathBuf, Vec<(IVec3, ChunkVolume, bool, Option<u64>)>> = HashMap::new();
    for job in batch {
        by_dir.entry(job.dir).or_default().push((job.pos, job.volume, job.edited, job.stdb_key));
    }
    for (dir, chunks) in by_dir {
        let refs: Vec<(IVec3, &ChunkVolume, bool)> =
            chunks.iter().map(|(p, v, e, _)| (*p, v, *e)).collect();
        if let Err(e) = region::save_many(&dir, &refs) {
            eprintln!("chunk writer: failed to persist {} chunks in {dir:?}: {e}", refs.len());
            // Disk is authoritative; don't mirror what we failed to store.
            continue;
        }
        let Some(stdb) = stdb else { continue };
        for (_, volume, edited, key) in &chunks {
            // Pristine chunks are reproducible from GenParams and are already
            // skipped on disk — mirroring them would waste the database too.
            let (Some(key), true) = (key, *edited) else { continue };
            let payload = soils_protocol::encode_chunk(volume);
            if let Err(e) = stdb.send(StdbCmd::PutChunkBlob {
                key: *key,
                payload,
                // The region file carries no version counter, so the mirror
                // uses a monotonic clock stamp: later writes always win the
                // module's stale-write check.
                version: stamp(),
                edits_through: 0,
            }) {
                eprintln!("chunk writer: spacetimedb mirror unavailable: {e}");
            }
        }
    }
}

/// Seconds since the unix epoch, saturating into `u32`. Monotonic enough to
/// order chunk writes for the module's stale-version guard.
fn stamp() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}
