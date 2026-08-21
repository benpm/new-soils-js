//! Server-side scripting: JIT-run AssemblyScript (or precompiled `.wasm`/`.wat`)
//! modules inside the game tick, letting scripts read world state and emit
//! [`ScriptCommand`]s that the embedder applies to the authoritative world.
//!
//! The embedder ([`soils-server`]) creates one [`ScriptRuntime`] over a scripts
//! directory, calls [`ScriptRuntime::poll`] to pick up file changes (hot
//! reload), then each tick calls [`ScriptRuntime::run`] with a [`ScriptWorld`]
//! read view + the tick's [`ScriptEvent`]s, and applies the returned commands.
//!
//! Isolation: each script gets its own wasm `Store`, a per-call **fuel** budget
//! (a runaway loop traps and the script is disabled, never stalling the tick),
//! and a memory ceiling. A trapping script's buffered commands are discarded.

mod compile;
mod host;

pub use compile::Asc;
pub use host::{ScriptCommand, ScriptHost, ScriptWorld};

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::SystemTime;
use wasmtime::{Config, Engine, Linker, Module, Store, TypedFunc, WasmParams};

/// A game event delivered to scripts' reaction callbacks before `on_tick`.
#[derive(Debug, Clone)]
pub enum ScriptEvent {
    /// A voxel edit that already happened (from a player). `by` is the editor's
    /// client id. Script-originated edits do NOT produce this (no recursion).
    Edit { x: i32, y: i32, z: i32, old: u8, new: u8, by: u32 },
    PlayerJoin { netid: u32 },
    PlayerLeave { netid: u32 },
}

/// Per-callback instruction budget. Tiny scalar scripts use a few thousand;
/// this bounds a pathological loop to well under a tick at 20 Hz.
const FUEL_PER_CALL: u64 = 5_000_000;

type OnTick = TypedFunc<(i32, f32), ()>;
type OnEdit = TypedFunc<(i32, i32, i32, i32, i32, i32), ()>;
type OnId = TypedFunc<i32, ()>;

struct LoadedScript {
    path: PathBuf,
    mtime: SystemTime,
    store: Store<ScriptHost>,
    on_tick: Option<OnTick>,
    on_edit: Option<OnEdit>,
    on_join: Option<OnId>,
    on_leave: Option<OnId>,
    /// Set when a call traps; the script is skipped until reloaded.
    disabled: bool,
}

impl LoadedScript {
    /// Call one export with a fresh fuel budget. On trap: log, disable, return
    /// false. `f` is borrowed (a cheap handle cloned out of the `Option`) so it
    /// doesn't alias the `&mut self` used for the store.
    fn invoke<P: WasmParams>(&mut self, f: &TypedFunc<P, ()>, args: P) -> bool {
        let _ = self.store.set_fuel(FUEL_PER_CALL);
        match f.call(&mut self.store, args) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[script] {} disabled after trap: {e}", self.path.display());
                self.disabled = true;
                false
            }
        }
    }
}

/// A `.ts` compile running on a background thread (asc spawns Node).
struct Pending {
    path: PathBuf,
    mtime: SystemTime,
    rx: Receiver<Result<Vec<u8>, String>>,
}

pub struct ScriptRuntime {
    engine: Engine,
    linker: Linker<ScriptHost>,
    dir: PathBuf,
    /// Where compiled `.ts` output lands.
    cache: PathBuf,
    seed: i64,
    asc: Option<Asc>,
    scripts: Vec<LoadedScript>,
    pending: Vec<Pending>,
}

impl ScriptRuntime {
    /// Build a runtime over `dir`. `seed` roots each script's deterministic rng.
    /// Never fails: a missing dir or absent `asc` just means fewer scripts load.
    pub fn new(dir: impl Into<PathBuf>, seed: i64) -> anyhow::Result<Self> {
        let mut cfg = Config::new();
        cfg.consume_fuel(true);
        let engine = Engine::new(&cfg)?;
        let linker = host::build_linker(&engine)?;
        let dir = dir.into();
        let cache = dir.join(".cache");
        let asc = Asc::detect();
        if asc.is_none() {
            eprintln!("[script] no `asc` found — .ts scripts skipped (set SOILS_ASC or install assemblyscript). .wasm/.wat still load.");
        }
        let mut rt = Self { engine, linker, dir, cache, seed, asc, scripts: Vec::new(), pending: Vec::new() };
        rt.poll();
        Ok(rt)
    }

    pub fn script_count(&self) -> usize {
        self.scripts.iter().filter(|s| !s.disabled).count()
    }

    /// Rescan the directory: load new/changed `.wasm`/`.wat` synchronously and
    /// kick off `.ts` compiles on background threads; adopt finished compiles;
    /// drop scripts whose file disappeared. Cheap enough to call every tick.
    pub fn poll(&mut self) {
        // Adopt any finished background compiles first.
        let mut ready: Vec<(PathBuf, SystemTime, Vec<u8>)> = Vec::new();
        self.pending.retain_mut(|p| match p.rx.try_recv() {
            Ok(Ok(bytes)) => {
                ready.push((p.path.clone(), p.mtime, bytes));
                false
            }
            Ok(Err(diag)) => {
                eprintln!("[script] compile failed: {}\n{diag}", p.path.display());
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
        });
        for (path, mtime, bytes) in ready {
            match Module::new(&self.engine, &bytes) {
                Ok(m) => self.install(path, mtime, m),
                Err(e) => eprintln!("[script] bad module {}: {e}", path.display()),
            }
        }

        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let mut seen: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(ext, "ts" | "wasm" | "wat") {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else { continue };
            seen.push(path.clone());
            let current = self.scripts.iter().find(|s| s.path == path).map(|s| s.mtime);
            let compiling = self.pending.iter().any(|p| p.path == path && p.mtime == mtime);
            if current == Some(mtime) || compiling {
                continue; // up to date or already in flight
            }
            match ext {
                "ts" => self.spawn_compile(path, mtime),
                _ => match Module::from_file(&self.engine, &path) {
                    Ok(m) => self.install(path, mtime, m),
                    Err(e) => eprintln!("[script] bad module {}: {e}", path.display()),
                },
            }
        }
        // Drop scripts whose source file is gone.
        self.scripts.retain(|s| seen.contains(&s.path));
    }

    fn spawn_compile(&mut self, path: PathBuf, mtime: SystemTime) {
        let Some(asc) = self.asc.clone() else { return };
        let _ = std::fs::create_dir_all(&self.cache);
        let out = self.cache.join(format!(
            "{}.wasm",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("script")
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        let src = path.clone();
        std::thread::Builder::new()
            .name("soils-asc".into())
            .spawn(move || {
                let _ = tx.send(asc.compile(&src, &out));
            })
            .ok();
        self.pending.push(Pending { path, mtime, rx });
    }

    /// Instantiate `module` and register it (replacing any prior load of `path`).
    fn install(&mut self, path: PathBuf, mtime: SystemTime, module: Module) {
        let mut store = Store::new(&self.engine, ScriptHost::new(self.seed));
        store.limiter(|h| h.limits());
        let instance = match self.linker.instantiate(&mut store, &module) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[script] instantiate {} failed: {e}", path.display());
                return;
            }
        };
        // Optional lifecycle setup: on_init() runs once with no world view.
        if let Ok(f) = instance.get_typed_func::<(), ()>(&mut store, "on_init") {
            store.data_mut().enter(&NoWorld, 0);
            let _ = store.set_fuel(FUEL_PER_CALL);
            if let Err(e) = f.call(&mut store, ()) {
                eprintln!("[script] {} on_init trapped: {e}", path.display());
            }
            let _ = store.data_mut().take(); // on_init world edits are ignored by design
            store.data_mut().leave();
        }
        let on_tick = instance.get_typed_func::<(i32, f32), ()>(&mut store, "on_tick").ok();
        let on_edit = instance
            .get_typed_func::<(i32, i32, i32, i32, i32, i32), ()>(&mut store, "on_edit")
            .ok();
        let on_join = instance.get_typed_func::<i32, ()>(&mut store, "on_player_join").ok();
        let on_leave = instance.get_typed_func::<i32, ()>(&mut store, "on_player_leave").ok();

        let script = LoadedScript { path: path.clone(), mtime, store, on_tick, on_edit, on_join, on_leave, disabled: false };
        if let Some(slot) = self.scripts.iter_mut().find(|s| s.path == path) {
            *slot = script;
        } else {
            self.scripts.push(script);
        }
        eprintln!("[script] loaded {}", path.display());
    }

    /// Run every enabled script for one tick: dispatch `events` to reaction
    /// callbacks, then `on_tick`, and return all emitted commands (in script
    /// order). `world` is borrowed only for the duration of this call.
    pub fn run(&mut self, world: &dyn ScriptWorld, tick: u64, dt: f32, events: &[ScriptEvent]) -> Vec<ScriptCommand> {
        let mut cmds = Vec::new();
        for s in &mut self.scripts {
            if s.disabled {
                continue;
            }
            s.store.data_mut().enter(world, tick);
            let mut ok = true;
            // Event reactions first. Clone the (cheap) TypedFunc handle out of
            // the Option so it doesn't alias the &mut store inside `invoke`.
            if let Some(f) = s.on_edit.clone() {
                for ev in events {
                    if let ScriptEvent::Edit { x, y, z, old, new, by } = *ev {
                        if !s.invoke(&f, (x, y, z, old as i32, new as i32, by as i32)) {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if ok {
                if let Some(f) = s.on_join.clone() {
                    for ev in events {
                        if let ScriptEvent::PlayerJoin { netid } = *ev {
                            if !s.invoke(&f, netid as i32) {
                                ok = false;
                                break;
                            }
                        }
                    }
                }
            }
            if ok {
                if let Some(f) = s.on_leave.clone() {
                    for ev in events {
                        if let ScriptEvent::PlayerLeave { netid } = *ev {
                            if !s.invoke(&f, netid as i32) {
                                ok = false;
                                break;
                            }
                        }
                    }
                }
            }
            if ok {
                if let Some(f) = s.on_tick.clone() {
                    ok = s.invoke(&f, (tick as i32, dt));
                }
            }
            let out = s.store.data_mut().take();
            s.store.data_mut().leave();
            if ok {
                cmds.extend(out); // trapped scripts' partial output is discarded
            }
        }
        cmds
    }
}

/// Empty read view for `on_init` (no live world during instantiation).
struct NoWorld;
impl ScriptWorld for NoWorld {
    fn voxel(&self, _x: i32, _y: i32, _z: i32) -> u8 {
        0
    }
    fn entity_count(&self) -> usize {
        0
    }
    fn entity_field(&self, _index: usize, _field: i32) -> f32 {
        0.0
    }
}

#[cfg(test)]
mod tests;
