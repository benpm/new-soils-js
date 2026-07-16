//! The host side of the script ABI: the [`ScriptHost`] store data, the read
//! view scripts consult ([`ScriptWorld`]), the commands they emit
//! ([`ScriptCommand`]), and the wasmtime [`Linker`] wiring the two together.
//!
//! The ABI is deliberately **scalar-only** (i32/f32) so no AssemblyScript
//! loader/GC bridge is needed — the host never has to read a guest string or
//! typed array. Reads (`get_voxel`, `entity_*`) resolve against a scoped
//! [`ScriptWorld`] borrow that is only live during a synchronous export call;
//! writes are buffered into [`ScriptHost::out`] and applied by the embedder
//! after the call returns.

use wasmtime::{Caller, Engine, Linker, StoreLimits, StoreLimitsBuilder};

/// Read view over live game state, implemented by the embedder (the server)
/// for the duration of a single script call. Unloaded voxels read as air (0),
/// matching the shared `soils-sim` sampler contract.
pub trait ScriptWorld {
    fn voxel(&self, x: i32, y: i32, z: i32) -> u8;
    fn entity_count(&self) -> usize;
    /// `field`: 0=netid, 1=kind, 2=x, 3=y, 4=z. Out-of-range → 0.
    fn entity_field(&self, index: usize, field: i32) -> f32;
}

/// A world-state mutation requested by a script, applied by the embedder after
/// the call so wasm never re-enters the ECS mid-borrow.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptCommand {
    Edit { x: i32, y: i32, z: i32, id: u8 },
    Spawn { kind: u16, x: f32, y: f32, z: f32 },
    Despawn { netid: u32 },
    SetVel { netid: u32, x: f32, y: f32, z: f32 },
    SetPos { netid: u32, x: f32, y: f32, z: f32 },
}

/// Per-tick memory ceiling for a script instance (guards a runaway allocator).
const MEM_LIMIT: usize = 16 << 20;

/// `Store<ScriptHost>` data. One per loaded script.
pub struct ScriptHost {
    /// Scoped read view. `None` outside a call; a `'static`-erased pointer to a
    /// borrow that is only valid for the synchronous span of one export call
    /// (set by [`Self::enter`], cleared by [`Self::leave`]).
    world: Option<*const (dyn ScriptWorld + 'static)>,
    /// Commands emitted during the current call, drained by [`Self::take`].
    pub out: Vec<ScriptCommand>,
    /// World seed (deterministic-rng root) and current server tick.
    pub seed: i64,
    pub tick: u64,
    /// splitmix64 state, reseeded from (seed, tick) at the start of each call so
    /// `rng()` is replay-deterministic.
    rng_state: u64,
    limits: StoreLimits,
}

impl ScriptHost {
    pub fn new(seed: i64) -> Self {
        Self {
            world: None,
            out: Vec::new(),
            seed,
            tick: 0,
            rng_state: 0,
            limits: StoreLimitsBuilder::new().memory_size(MEM_LIMIT).build(),
        }
    }

    /// Accessor used by `Store::limiter`.
    pub fn limits(&mut self) -> &mut StoreLimits {
        &mut self.limits
    }

    /// Begin a call: install the scoped world borrow and reseed the rng. The
    /// caller MUST call [`Self::leave`] before the borrow `w` ends.
    ///
    /// # Safety
    /// The pointer stored here outlives the borrow's named lifetime only as a
    /// raw pointer; it is never dereferenced after [`Self::leave`], and scripts
    /// run single-threaded, so no aliasing/UAF is observable.
    pub fn enter(&mut self, w: &dyn ScriptWorld, tick: u64) {
        let ptr: *const dyn ScriptWorld = w;
        // Erase the borrow lifetime to store the fat pointer; only valid until `leave`.
        self.world = Some(unsafe {
            std::mem::transmute::<*const dyn ScriptWorld, *const (dyn ScriptWorld + 'static)>(ptr)
        });
        self.tick = tick;
        self.rng_state = (self.seed as u64) ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        self.out.clear();
    }

    pub fn leave(&mut self) {
        self.world = None;
    }

    pub fn take(&mut self) -> Vec<ScriptCommand> {
        std::mem::take(&mut self.out)
    }

    fn world(&self) -> Option<&dyn ScriptWorld> {
        // SAFETY: only Some during a call, between `enter` and `leave`, where the
        // borrow is guaranteed live.
        self.world.map(|p| unsafe { &*p })
    }

    /// Deterministic uniform in [0, 1). splitmix64 → 24-bit mantissa.
    fn next_rng(&mut self) -> f32 {
        self.rng_state = self.rng_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        ((z >> 40) as f32) / ((1u32 << 24) as f32)
    }
}

/// `*const dyn` makes the struct non-Send/Sync automatically, which is what we
/// want: the embedder holds it as a Bevy **non-send** resource, pinning script
/// systems to the single ECS thread.
///
/// Build a [`Linker`] exposing the `"soils"` host module plus the required
/// AssemblyScript `env.abort` import.
pub fn build_linker(engine: &Engine) -> anyhow::Result<Linker<ScriptHost>> {
    let mut l = Linker::new(engine);

    // --- reads (consult the scoped world view) ---
    l.func_wrap("soils", "get_voxel", |c: Caller<'_, ScriptHost>, x: i32, y: i32, z: i32| -> i32 {
        c.data().world().map(|w| w.voxel(x, y, z) as i32).unwrap_or(0)
    })?;
    l.func_wrap("soils", "entity_count", |c: Caller<'_, ScriptHost>| -> i32 {
        c.data().world().map(|w| w.entity_count() as i32).unwrap_or(0)
    })?;
    l.func_wrap("soils", "entity_field", |c: Caller<'_, ScriptHost>, i: i32, f: i32| -> f32 {
        match c.data().world() {
            Some(w) if i >= 0 => w.entity_field(i as usize, f),
            _ => 0.0,
        }
    })?;
    l.func_wrap("soils", "seed", |c: Caller<'_, ScriptHost>| -> i32 { c.data().seed as i32 })?;
    l.func_wrap("soils", "tick", |c: Caller<'_, ScriptHost>| -> i32 { c.data().tick as i32 })?;
    l.func_wrap("soils", "rng", |mut c: Caller<'_, ScriptHost>| -> f32 { c.data_mut().next_rng() })?;

    // --- writes (buffered, applied after the call) ---
    l.func_wrap("soils", "edit_voxel", |mut c: Caller<'_, ScriptHost>, x: i32, y: i32, z: i32, id: i32| {
        c.data_mut().out.push(ScriptCommand::Edit { x, y, z, id: id as u8 });
    })?;
    l.func_wrap("soils", "spawn", |mut c: Caller<'_, ScriptHost>, kind: i32, x: f32, y: f32, z: f32| -> i32 {
        let h = c.data_mut();
        h.out.push(ScriptCommand::Spawn { kind: kind as u16, x, y, z });
        h.out.len() as i32 // provisional local handle (real NetId assigned on apply)
    })?;
    l.func_wrap("soils", "despawn", |mut c: Caller<'_, ScriptHost>, net: i32| {
        c.data_mut().out.push(ScriptCommand::Despawn { netid: net as u32 });
    })?;
    l.func_wrap("soils", "set_velocity", |mut c: Caller<'_, ScriptHost>, net: i32, x: f32, y: f32, z: f32| {
        c.data_mut().out.push(ScriptCommand::SetVel { netid: net as u32, x, y, z });
    })?;
    l.func_wrap("soils", "set_pos", |mut c: Caller<'_, ScriptHost>, net: i32, x: f32, y: f32, z: f32| {
        c.data_mut().out.push(ScriptCommand::SetPos { netid: net as u32, x, y, z });
    })?;
    l.func_wrap("soils", "log", |_c: Caller<'_, ScriptHost>, level: i32, code: i32| {
        // Scalar debug hook (no strings in v1). Kept quiet at level 0.
        if level != 0 {
            eprintln!("[script] log level={level} code={code}");
        }
    })?;

    // --- AssemblyScript runtime import: env.abort traps the call ---
    l.func_wrap(
        "env",
        "abort",
        |_c: Caller<'_, ScriptHost>, _msg: i32, _file: i32, line: i32, col: i32| -> anyhow::Result<()> {
            Err(anyhow::anyhow!("assemblyscript abort at line {line}:{col}"))
        },
    )?;

    Ok(l)
}
