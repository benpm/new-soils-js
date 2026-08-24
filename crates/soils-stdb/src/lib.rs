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
pub use spacetimedb_sdk::Identity;
// Brings `unsubscribe` into scope for the generated `SubscriptionHandle`.
use spacetimedb_sdk::SubscriptionHandle as _;
use spacetimedb_sdk::{DbContext, Table};

use module_bindings::{
    DbConnection, RemoteReducers,
    chat_message_table::ChatMessageTableAccess, chunk_blob_table::ChunkBlobTableAccess,
    game_server_table::GameServerTableAccess, player_profile_table::PlayerProfileTableAccess,
    world_table::WorldTableAccess,
};
// Each reducer is generated as its own snake_case extension trait; they must be
// in scope for `reducers.<name>(..)` to resolve.
use module_bindings::{
    heartbeat, heartbeat_presence, link_identity, mark_absent, mark_present, put_chunk_blob,
    register_account, save_profile, send_chat, set_password, upsert_world, verify_login,
};

pub use module_bindings::{ChatMessage, ChunkBlob, GameServer, PlayerProfile, World};

/// What a game server reads back: profiles, to restore a returning player's
/// position.
///
/// `account` is absent because it is a *private* table and has no client-side
/// accessor at all — passwords are checked by the `verify_login` reducer,
/// inside the database, so the verifier never crosses the wire.
pub const SERVER_SUBSCRIPTIONS: &[&str] = &["SELECT * FROM player_profile"];

/// What a game *client* reads: the lobby. Excludes `chunk_blob`, which would
/// stream the stored world into a player's memory. (`account` is private, so
/// it is not excluded here so much as unreachable.)
pub const CLIENT_SUBSCRIPTIONS: &[&str] =
    &["SELECT * FROM game_server", "SELECT * FROM world", "SELECT * FROM chat_message"];

/// Where a reducer's verdict is sent once the module has run it.
///
/// Most commands are fire-and-forget, but authentication is a question: the
/// answer only exists after the module has done the hashing. SpacetimeDB
/// delivers a reducer's outcome to the connection that called it and to no
/// one else, so this is a private round-trip, not a broadcast.
pub type Reply = Sender<Result<(), String>>;

/// Work handed to the SpacetimeDB thread.
#[derive(Debug, Clone)]
pub enum StdbCmd {
    /// Create-or-refresh a world; the module rejects a generator change under
    /// an existing name rather than silently invalidating stored chunks.
    UpsertWorld {
        /// Server-chosen stable id (`soils_protocol::chunk_key::world_id_for`).
        world_id: u16,
        name: String,
        seed: i64,
        world_type: u8,
        graph_hash: u64,
        daytime: f32,
    },
    /// Write a coalesced chunk payload (`soils_protocol::chunk_codec` bytes).
    ///
    /// `writer_epoch` identifies the writing server *process*: chunk versions
    /// are in-memory counters that restart when a chunk is evicted and
    /// reloaded, so the module's stale-write guard only compares them within
    /// one epoch.
    PutChunkBlob { key: u64, payload: Vec<u8>, version: u32, writer_epoch: u64 },
    /// Create an account, or migrate one that already exists locally. The
    /// module hashes; `reply` carries its verdict back to the caller.
    RegisterAccount { name: String, password: String, reply: Reply },
    /// Check a password. The comparison happens inside the module.
    VerifyLogin { name: String, password: String, reply: Reply },
    /// Replace an account's password.
    SetPassword { name: String, password: String, reply: Reply },
    /// Bind a SpacetimeDB identity to an account the server has authenticated.
    LinkIdentity { account: String, identity: Identity },
    /// Post a chat message as this connection's own identity.
    SendChat { world_id: u16, text: String },
    /// Persist a player's last known position, keyed by account name.
    SaveProfile {
        account: String,
        world_id: u16,
        x: f32,
        y: f32,
        z: f32,
        yaw: f32,
        view_radius: u8,
    },
    /// Refresh this server's registry row (server browser + liveness).
    Heartbeat { server_id: u32, name: String, addr: String, player_count: u32 },
    /// Record an account as online on this server.
    MarkPresent { account: String, server_id: u32, world_id: u16 },
    /// Refresh presence for this server's whole roster in one transaction, and
    /// drop rows for anyone no longer on it.
    ///
    /// `MarkPresent` alone is not enough to stay online: the module reaps any
    /// presence row older than its liveness TTL, so a row written at login and
    /// never refreshed disappears while the player is still connected.
    HeartbeatPresence { server_id: u32, world_id: u16, accounts: Vec<String> },
    /// Drop an account's presence row on a clean disconnect.
    MarkAbsent { account: String },
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
    worker: Option<std::thread::JoinHandle<()>>,
    /// Set once the subscription has delivered its first snapshot, so a read
    /// can tell "no such row" from "cache not warm yet".
    ready: Arc<AtomicBool>,
    /// The live connection, published by the worker once it is up.
    ///
    /// Reads go straight to the SDK's client-side cache, which the worker keeps
    /// current from its subscriptions. That is safe from the ECS thread and,
    /// more importantly, synchronous: the login path needs a player's saved
    /// position *now*, and cannot wait on a round trip mid-tick.
    conn: Arc<std::sync::RwLock<Option<Arc<DbConnection>>>>,
}

impl StdbLink {
    /// Spawn the worker and begin connecting, subscribing to what a game
    /// server needs. Non-blocking: the caller learns the outcome through
    /// [`StdbEvent::Connected`] / `ConnectError`.
    pub fn connect(uri: &str, database: &str, token: Option<String>) -> Self {
        Self::connect_with(uri, database, token, SERVER_SUBSCRIPTIONS)
    }

    /// As [`connect`](Self::connect), with an explicit subscription set.
    ///
    /// Servers and clients read different halves of the schema, and a
    /// subscription is what fills the local cache — so the caller has to say
    /// which half it wants rather than paying for both.
    pub fn connect_with(
        uri: &str,
        database: &str,
        token: Option<String>,
        subscriptions: &[&str],
    ) -> Self {
        let (cmd_tx, cmd_rx) = unbounded::<StdbCmd>();
        let (event_tx, event_rx) = unbounded::<StdbEvent>();
        let running = Arc::new(AtomicBool::new(true));

        let uri = uri.to_string();
        let database = database.to_string();
        let subs: Vec<String> = subscriptions.iter().map(|s| s.to_string()).collect();
        let flag = running.clone();
        let conn = Arc::new(std::sync::RwLock::new(None));
        let published = conn.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let signal = ready.clone();
        let worker = std::thread::Builder::new()
            .name("soils-stdb".into())
            .spawn(move || {
                worker(uri, database, token, subs, cmd_rx, event_tx, flag, published, signal)
            })
            .expect("spawn soils-stdb thread");

        Self { cmd_tx, event_rx, running, worker: Some(worker), conn, ready }
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

    /// Block until the profile cache has its first snapshot, or `timeout`
    /// elapses. Returns whether it is ready.
    ///
    /// Worth waiting for at startup: reads come from the local cache, so a
    /// login arriving before the first snapshot would silently fall back to the
    /// world spawn point and quietly lose the player's saved position.
    pub fn wait_ready(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while !self.ready.load(Ordering::Relaxed) {
            if std::time::Instant::now() >= deadline || !self.is_running() {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        true
    }

    /// Whether the first subscription snapshot has arrived.
    ///
    /// Authentication no longer depends on this — passwords are checked by a
    /// reducer, not against a cache — but restoring a player's saved position
    /// does, and a login that beats the snapshot would silently drop them at
    /// the world spawn instead.
    pub fn accounts_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Check a password against the stored account, inside the database.
    ///
    /// Blocks for the round-trip, so it must not be called from a thread that
    /// owes anyone latency — Argon2 is deliberately expensive, and the point of
    /// running it in the module is that it costs the *database* that time
    /// rather than the simulation tick.
    ///
    /// A timeout is reported as an error rather than a rejection, and the
    /// caller decides what to do about it; treating "the database did not
    /// answer" as "wrong password" would lock every player out of a healthy
    /// server the moment the network hiccuped.
    pub fn verify_login(
        &self,
        name: &str,
        password: &str,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        self.ask(timeout, |reply| StdbCmd::VerifyLogin {
            name: name.to_string(),
            password: password.to_string(),
            reply,
        })
    }

    /// Create an account. Idempotent when the password already matches, which
    /// is what makes migrating a local account file safe to repeat.
    pub fn register_account(
        &self,
        name: &str,
        password: &str,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        self.ask(timeout, |reply| StdbCmd::RegisterAccount {
            name: name.to_string(),
            password: password.to_string(),
            reply,
        })
    }

    /// Replace an account's password. The caller is responsible for having
    /// authorised the change.
    pub fn set_password(
        &self,
        name: &str,
        password: &str,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        self.ask(timeout, |reply| StdbCmd::SetPassword {
            name: name.to_string(),
            password: password.to_string(),
            reply,
        })
    }

    /// Send a command that expects an answer, and wait for it.
    fn ask(
        &self,
        timeout: std::time::Duration,
        build: impl FnOnce(Reply) -> StdbCmd,
    ) -> Result<(), String> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.send(build(tx)).map_err(|e| format!("account store unavailable: {e}"))?;
        match rx.recv_timeout(timeout) {
            Ok(verdict) => verdict,
            // Disconnected means the worker dropped the command without
            // answering — a session that ended mid-flight.
            Err(_) => Err("account store did not answer".to_string()),
        }
    }

    /// Every stored chunk for one world, for restoring a server whose region
    /// files are gone.
    ///
    /// Takes out a one-off subscription rather than keeping `chunk_blob` in the
    /// standing set: a running server serves chunks from region files, which
    /// stay authoritative, so subscribing permanently would hold the whole
    /// stored world in memory to answer a question asked once at startup.
    ///
    /// Returns what arrived before `timeout`. A partial restore is still better
    /// than none — the missing chunks regenerate as pristine terrain, which is
    /// exactly what they would have done with no database at all.
    pub fn fetch_world_chunks(
        &self,
        world_id: u16,
        timeout: std::time::Duration,
    ) -> Vec<ChunkBlob> {
        let guard = match self.conn.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let Some(conn) = guard.as_ref() else { return Vec::new() };

        let applied = Arc::new(AtomicBool::new(false));
        let flag = applied.clone();
        let handle = conn
            .subscription_builder()
            .on_applied(move |_| flag.store(true, Ordering::Relaxed))
            .subscribe([format!("SELECT * FROM chunk_blob WHERE world_id = {world_id}")]);

        let deadline = std::time::Instant::now() + timeout;
        while !applied.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let rows: Vec<ChunkBlob> =
            conn.db().chunk_blob().iter().filter(|b| b.world_id == world_id).collect();

        // Read the rows out *first*, then drop the query: unsubscribing clears
        // them from the cache. Dropping the handle would not have been enough —
        // it does not unsubscribe, so the query stayed live for the rest of the
        // process and held the whole stored world in memory, which is precisely
        // what taking a one-off subscription was meant to avoid.
        if let Err(e) = handle.unsubscribe() {
            eprintln!("stdb: could not release the chunk restore subscription: {e}");
        }
        rows
    }

    /// Live game servers, most populated first.
    ///
    /// Complements the UDP LAN discovery rather than replacing it: discovery
    /// still finds servers on a local network with no database in the picture.
    pub fn servers(&self) -> Vec<GameServer> {
        let guard = self.conn.read().ok();
        let Some(conn) = guard.as_ref().and_then(|g| g.as_ref()) else { return Vec::new() };
        let mut rows: Vec<GameServer> = conn.db().game_server().iter().collect();
        rows.sort_by(|a, b| b.players.cmp(&a.players).then_with(|| a.name.cmp(&b.name)));
        rows
    }

    /// Known worlds, by name.
    pub fn worlds(&self) -> Vec<World> {
        let guard = self.conn.read().ok();
        let Some(conn) = guard.as_ref().and_then(|g| g.as_ref()) else { return Vec::new() };
        let mut rows: Vec<World> = conn.db().world().iter().collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// The most recent chat for a world, oldest first, capped at `limit`.
    pub fn chat(&self, world_id: u16, limit: usize) -> Vec<ChatMessage> {
        let guard = self.conn.read().ok();
        let Some(conn) = guard.as_ref().and_then(|g| g.as_ref()) else { return Vec::new() };
        let mut rows: Vec<ChatMessage> =
            conn.db().chat_message().iter().filter(|m| m.world_id == world_id).collect();
        rows.sort_by_key(|m| m.at);
        if rows.len() > limit {
            rows.drain(..rows.len() - limit);
        }
        rows
    }

    /// This connection's own identity, once the handshake has completed.
    pub fn identity(&self) -> Option<Identity> {
        let guard = self.conn.read().ok()?;
        guard.as_ref()?.try_identity()
    }

    /// A player's saved profile, or `None` if the cache is not warm yet or the
    /// account has never logged out.
    ///
    /// All three are indistinguishable here and all mean the same thing to the
    /// caller: spawn them at the world spawn point. A returning player losing
    /// their position is a small regression; blocking the tick to find out is
    /// not an acceptable alternative.
    pub fn profile(&self, account: &str) -> Option<PlayerProfile> {
        if !self.ready.load(Ordering::Relaxed) {
            return None;
        }
        let guard = self.conn.read().ok()?;
        let conn = guard.as_ref()?;
        conn.db().player_profile().iter().find(|p| p.account == account)
    }
}

impl Drop for StdbLink {
    /// Wait for the worker to finish draining before returning.
    ///
    /// The flush that matters most is the one at shutdown, where the server
    /// pushes its dirty chunks out on the way down. Previously this only set a
    /// flag, so the process could exit while the worker was still delivering —
    /// losing exactly that flush.
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Closing the command channel is what tells the worker to drain and
        // exit; it cannot see that while this handle still holds a sender.
        let (dead, _) = unbounded();
        let _ = std::mem::replace(&mut self.cmd_tx, dead);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("spacetimedb worker panicked during shutdown");
        }
    }
}

fn worker(
    uri: String,
    database: String,
    token: Option<String>,
    subscriptions: Vec<String>,
    cmd_rx: Receiver<StdbCmd>,
    event_tx: Sender<StdbEvent>,
    running: Arc<AtomicBool>,
    publish: Arc<std::sync::RwLock<Option<Arc<DbConnection>>>>,
    ready: Arc<AtomicBool>,
) {
    // Reconnect loop. A database restart used to end the link for the lifetime
    // of the process, which for a long-running server meant losing the mirror
    // permanently over a momentary outage.
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        match session(
            &uri,
            &database,
            token.clone(),
            &subscriptions,
            &cmd_rx,
            &event_tx,
            &running,
            &publish,
            &ready,
        ) {
            SessionEnd::Shutdown => break,
            SessionEnd::Lost => {
                ready.store(false, Ordering::Relaxed);
                if let Ok(mut slot) = publish.write() {
                    *slot = None;
                }
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let _ = event_tx.send(StdbEvent::Disconnected(Some(format!(
                    "retrying in {:.0}s",
                    backoff.as_secs_f32()
                ))));
                std::thread::sleep(backoff);
                // Capped so a long outage does not turn into a long silence.
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
            }
        }
    }
    running.store(false, Ordering::Relaxed);
}

/// Why a session ended.
enum SessionEnd {
    /// The link is shutting down; do not reconnect.
    Shutdown,
    /// The connection failed or dropped.
    Lost,
}

/// One connection's lifetime: connect, subscribe, pump commands.
#[allow(clippy::too_many_arguments)]
fn session(
    uri: &str,
    database: &str,
    token: Option<String>,
    subscriptions: &[String],
    cmd_rx: &Receiver<StdbCmd>,
    event_tx: &Sender<StdbEvent>,
    running: &Arc<AtomicBool>,
    publish: &Arc<std::sync::RwLock<Option<Arc<DbConnection>>>>,
    ready: &Arc<AtomicBool>,
) -> SessionEnd {
    let mut builder = DbConnection::builder().with_uri(uri).with_database_name(database);
    if let Some(token) = token {
        builder = builder.with_token(Some(token));
    }

    let conn = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            let _ = event_tx.send(StdbEvent::ConnectError(e.to_string()));
            return SessionEnd::Lost;
        }
    };

    // Drives the connection's I/O on its own thread; `conn` stays usable here.
    let handle = conn.run_threaded();

    let applied = ready.clone();
    conn.subscription_builder()
        .on_applied(move |_| applied.store(true, Ordering::Relaxed))
        .subscribe(subscriptions.to_vec());
    // Shared so the ECS thread can read the cache; the worker keeps using it
    // through the Arc.
    let conn = Arc::new(conn);
    if let Ok(mut slot) = publish.write() {
        *slot = Some(conn.clone());
    }

    // The identity lands once the handshake completes, which is *after*
    // `build()` returns — so poll briefly rather than declaring failure on the
    // first look.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let identity = loop {
        if let Some(id) = conn.try_identity() {
            break Some(id);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    match identity {
        Some(id) => {
            let _ = event_tx.send(StdbEvent::Connected(id));
        }
        None => {
            let _ = event_tx
                .send(StdbEvent::ConnectError("handshake did not yield an identity".into()));
            return SessionEnd::Lost;
        }
    }

    // Run until every sender is gone, then drain what's left. Exiting purely on
    // the `running` flag would discard commands already queued — and the flush
    // that matters most is the one at shutdown, where the server pushes its
    // dirty chunks out on the way down.
    let mut shutdown;
    loop {
        match cmd_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(cmd) => apply(conn.reducers(), cmd, event_tx),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if !running.load(Ordering::Relaxed) {
                    shutdown = true;
                    break;
                }
                if !conn.is_active() {
                    // The host went away. Anything still queued is kept for the
                    // next session rather than applied to a dead connection.
                    return SessionEnd::Lost;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                shutdown = true;
                break;
            }
        }
    }
    while let Ok(cmd) = cmd_rx.try_recv() {
        apply(conn.reducers(), cmd, event_tx);
    }

    // Reducer calls are queued on the connection, so let them reach the host
    // before tearing it down; otherwise a shutdown flush is sent and dropped.
    std::thread::sleep(std::time::Duration::from_millis(250));
    let _ = conn.disconnect();
    let _ = handle.join();
    if shutdown { SessionEnd::Shutdown } else { SessionEnd::Lost }
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

/// Forward a reducer's own verdict to whoever is waiting on it.
///
/// The outer `Result` is the SDK failing to deliver the call at all; the inner
/// one is the module's answer. Both mean "not authenticated", but only the
/// inner one is the module speaking, so its message is the one worth showing.
fn answer(
    reply: Reply,
) -> impl FnOnce(&module_bindings::ReducerEventContext, Result<Result<(), String>, spacetimedb_sdk::__codegen::InternalError>)
+ Send
+ 'static {
    move |_ctx, outcome| {
        let _ = reply.send(match outcome {
            Ok(inner) => inner,
            Err(e) => Err(format!("account store error: {e}")),
        });
    }
}

/// Answer the waiter ourselves when the call could not even be sent, so a
/// login blocks for the round-trip and not for the whole timeout.
fn fail_reply<E: std::fmt::Display>(reply: &Reply, r: Result<(), E>) {
    if let Err(e) = r {
        let _ = reply.send(Err(format!("account store unavailable: {e}")));
    }
}

fn apply(reducers: &RemoteReducers, cmd: StdbCmd, event_tx: &Sender<StdbEvent>) {
    let report = |reducer, r| report(event_tx, reducer, r);

    match cmd {
        StdbCmd::UpsertWorld { world_id, name, seed, world_type, graph_hash, daytime } => report(
            "upsert_world",
            reducers.upsert_world(world_id, name, seed, world_type, graph_hash, daytime),
        ),
        StdbCmd::PutChunkBlob { key, payload, version, writer_epoch } => report(
            "put_chunk_blob",
            reducers.put_chunk_blob(key, payload, version, writer_epoch),
        ),
        StdbCmd::RegisterAccount { name, password, reply } => {
            let r = reducers.register_account_then(name, password, answer(reply.clone()));
            fail_reply(&reply, r);
        }
        StdbCmd::VerifyLogin { name, password, reply } => {
            let r = reducers.verify_login_then(name, password, answer(reply.clone()));
            fail_reply(&reply, r);
        }
        StdbCmd::SetPassword { name, password, reply } => {
            let r = reducers.set_password_then(name, password, answer(reply.clone()));
            fail_reply(&reply, r);
        }
        StdbCmd::LinkIdentity { account, identity } => {
            report("link_identity", reducers.link_identity(account, identity))
        }
        StdbCmd::SendChat { world_id, text } => {
            report("send_chat", reducers.send_chat(world_id, text))
        }
        StdbCmd::SaveProfile { account, world_id, x, y, z, yaw, view_radius } => report(
            "save_profile",
            reducers.save_profile(account, world_id, x, y, z, yaw, view_radius),
        ),
        StdbCmd::Heartbeat { server_id, name, addr, player_count } => {
            report("heartbeat", reducers.heartbeat(server_id, name, addr, player_count))
        }
        StdbCmd::MarkPresent { account, server_id, world_id } => {
            report("mark_present", reducers.mark_present(account, server_id, world_id))
        }
        StdbCmd::MarkAbsent { account } => {
            report("mark_absent", reducers.mark_absent(account))
        }
        StdbCmd::HeartbeatPresence { server_id, world_id, accounts } => report(
            "heartbeat_presence",
            reducers.heartbeat_presence(server_id, world_id, accounts),
        ),
    }
}

/// Blocking helper for tests and tools: read a chunk blob out of the client
/// cache once a subscription has delivered it.
pub fn chunk_blob_from_cache(conn: &DbConnection, key: u64) -> Option<ChunkBlob> {
    conn.db().chunk_blob().iter().find(|b| b.chunk_key == key)
}
