//! Native SpacetimeDB client for new-soils.
//!
//! Wraps the generated [`module_bindings`] in a [`StdbLink`]: a worker thread
//! plus channels, deliberately the same shape as the existing transport seam in
//! `soils_server::NewConn`. The ECS stays unaware of SpacetimeDB — it sends
//! [`StdbCmd`]s and drains [`StdbEvent`]s, exactly as it already does for
//! WebSocket/WebTransport clients.
//!
//! No Bevy dependency, so the server (headless `bevy_ecs`) and, later, the
//! client (full Bevy) can both use it.

pub mod module_bindings;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use spacetimedb_sdk::{DbContext, Identity, Table};

use module_bindings::{DbConnection, RemoteReducers, chunk_blob_table::ChunkBlobTableAccess};
// Each reducer is generated as its own snake_case extension trait; they must be
// in scope for `reducers.<name>(..)` to resolve.
use module_bindings::{
    heartbeat, prune_edits, put_chunk_blob, save_profile, submit_edits, upsert_world,
};

pub use module_bindings::{ChunkBlob, ChunkEdit, PackedEdit, World};

/// Batch cap for [`StdbCmd::SubmitEdits`], mirroring the module's
/// `MAX_EDITS_PER_CALL`. Larger batches risk exhausting the reducer's fuel
/// budget, which rolls back the *entire* transaction — so the caller must
/// chunk rather than gamble.
pub const MAX_EDITS_PER_CALL: usize = 4096;

/// Work handed to the SpacetimeDB thread.
#[derive(Debug, Clone)]
pub enum StdbCmd {
    /// Create-or-refresh a world; the module rejects a generator change under
    /// an existing name rather than silently invalidating stored chunks.
    UpsertWorld { name: String, seed: i64, world_type: u8, graph_hash: u64, daytime: f32 },
    /// Append voxel edits to the journal.
    SubmitEdits { tick: u64, edits: Vec<PackedEdit> },
    /// Write a coalesced chunk payload (`soils_protocol::chunk_codec` bytes).
    PutChunkBlob { key: u64, payload: Vec<u8>, version: u32, edits_through: u64 },
    /// Drop journal rows already folded into a blob.
    PruneEdits { key: u64, up_to_id: u64 },
    /// Persist a player's last known position.
    SaveProfile {
        identity: Identity,
        world_id: u16,
        x: f32,
        y: f32,
        z: f32,
        yaw: f32,
        view_radius: u8,
    },
    /// Refresh the server registry row and connected players' presence.
    Heartbeat {
        server_id: u32,
        name: String,
        addr: String,
        players: Vec<Identity>,
        world_id: u16,
    },
}

/// Notifications from the SpacetimeDB thread.
#[derive(Debug, Clone)]
pub enum StdbEvent {
    Connected(Identity),
    Disconnected(Option<String>),
    ConnectError(String),
    /// A reducer call failed. Carries the reducer name and the module's error.
    ReducerFailed { reducer: &'static str, error: String },
}

/// Handle onto the SpacetimeDB worker thread.
pub struct StdbLink {
    cmd_tx: Sender<StdbCmd>,
    event_rx: Receiver<StdbEvent>,
    running: Arc<AtomicBool>,
}

impl StdbLink {
    /// Spawn the worker and begin connecting. Non-blocking: the caller learns
    /// the outcome through [`StdbEvent::Connected`] / `ConnectError`.
    pub fn connect(uri: &str, database: &str, token: Option<String>) -> Self {
        let (cmd_tx, cmd_rx) = unbounded::<StdbCmd>();
        let (event_tx, event_rx) = unbounded::<StdbEvent>();
        let running = Arc::new(AtomicBool::new(true));

        let uri = uri.to_string();
        let database = database.to_string();
        let flag = running.clone();
        std::thread::Builder::new()
            .name("soils-stdb".into())
            .spawn(move || worker(uri, database, token, cmd_rx, event_tx, flag))
            .expect("spawn soils-stdb thread");

        Self { cmd_tx, event_rx, running }
    }

    /// Queue a command. Fails only once the worker has stopped.
    pub fn send(&self, cmd: StdbCmd) -> Result<(), String> {
        self.cmd_tx.send(cmd).map_err(|_| "spacetimedb worker stopped".to_string())
    }

    /// Drain pending events without blocking.
    pub fn drain(&self) -> Vec<StdbEvent> {
        let mut out = Vec::new();
        loop {
            match self.event_rx.try_recv() {
                Ok(e) => out.push(e),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for StdbLink {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn worker(
    uri: String,
    database: String,
    token: Option<String>,
    cmd_rx: Receiver<StdbCmd>,
    event_tx: Sender<StdbEvent>,
    running: Arc<AtomicBool>,
) {
    let mut builder = DbConnection::builder().with_uri(&uri).with_database_name(&database);
    if let Some(token) = token {
        builder = builder.with_token(Some(token));
    }

    let conn = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            let _ = event_tx.send(StdbEvent::ConnectError(e.to_string()));
            running.store(false, Ordering::Relaxed);
            return;
        }
    };

    // Drives the connection's I/O on its own thread; `conn` stays usable here.
    let handle = conn.run_threaded();

    match conn.try_identity() {
        Some(id) => {
            let _ = event_tx.send(StdbEvent::Connected(id));
        }
        None => {
            let _ = event_tx.send(StdbEvent::ConnectError("no identity after build".into()));
        }
    }

    while running.load(Ordering::Relaxed) {
        match cmd_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(cmd) => apply(conn.reducers(), cmd, &event_tx),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = conn.disconnect();
    let _ = handle.join();
    running.store(false, Ordering::Relaxed);
}

fn report<E: std::fmt::Display>(
    event_tx: &Sender<StdbEvent>,
    reducer: &'static str,
    r: Result<(), E>,
) {
    if let Err(e) = r {
        let _ = event_tx.send(StdbEvent::ReducerFailed { reducer, error: e.to_string() });
    }
}

fn apply(reducers: &RemoteReducers, cmd: StdbCmd, event_tx: &Sender<StdbEvent>) {
    let report = |reducer, r| report(event_tx, reducer, r);

    match cmd {
        StdbCmd::UpsertWorld { name, seed, world_type, graph_hash, daytime } => report(
            "upsert_world",
            reducers.upsert_world(name, seed, world_type, graph_hash, daytime),
        ),
        StdbCmd::SubmitEdits { tick, edits } => {
            // Split oversized batches rather than letting the module reject
            // them wholesale.
            for chunk in edits.chunks(MAX_EDITS_PER_CALL) {
                report("submit_edits", reducers.submit_edits(tick, chunk.to_vec()));
            }
        }
        StdbCmd::PutChunkBlob { key, payload, version, edits_through } => report(
            "put_chunk_blob",
            reducers.put_chunk_blob(key, payload, version, edits_through),
        ),
        StdbCmd::PruneEdits { key, up_to_id } => {
            report("prune_edits", reducers.prune_edits(key, up_to_id))
        }
        StdbCmd::SaveProfile { identity, world_id, x, y, z, yaw, view_radius } => report(
            "save_profile",
            reducers.save_profile(identity, world_id, x, y, z, yaw, view_radius),
        ),
        StdbCmd::Heartbeat { server_id, name, addr, players, world_id } => {
            report("heartbeat", reducers.heartbeat(server_id, name, addr, players, world_id))
        }
    }
}

/// Blocking helper for tests and tools: read a chunk blob out of the client
/// cache once a subscription has delivered it.
pub fn chunk_blob_from_cache(conn: &DbConnection, key: u64) -> Option<ChunkBlob> {
    conn.db().chunk_blob().iter().find(|b| b.chunk_key == key)
}
