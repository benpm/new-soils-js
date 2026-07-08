//! Baked voxel lighting (the L0 layer): a per-voxel grid of two 0-15 channels
//! packed into one byte — **skylight** (hi nibble, sun/sky reachability) and
//! **blocklight** (lo nibble, emissive blocks) — recomputed only when voxels
//! change, Minecraft-style.
//!
//! Rules:
//! - Opaque cells hold 0, except emitters, which hold their emission level in
//!   the block channel (they radiate but don't transmit).
//! - Blocklight spreads 6-way losing 1 per step.
//! - Skylight enters at level 15 from the open sky; a 15 beam travels straight
//!   down without loss, every other step loses 1.
//! - Propagation is confined to the caller's *domain* (loaded chunks / test
//!   bounds). Above-domain space is open sky where [`LightWorld::open_sky_above`]
//!   says so — the client answers "chunk above not loaded" with `true`, an
//!   optimistic guess corrected by [`reconcile_sky_below`] when it loads.
//!
//! [`relight_full`] is the from-scratch oracle; [`light_new_chunk`] and
//! [`apply_voxel_change`] are the incremental paths, property-tested to agree
//! with it.

use std::borrow::Cow;
use std::collections::VecDeque;

use glam::IVec3;
use soils_protocol::{CHUNK_CUBED, CHUNK_SIZE, chunk_origin, voxel_index};

pub const MAX_LIGHT: u8 = 15;

#[inline]
pub fn sky(packed: u8) -> u8 {
    packed >> 4
}

#[inline]
pub fn block(packed: u8) -> u8 {
    packed & 0x0F
}

#[inline]
pub fn pack(sky: u8, block: u8) -> u8 {
    (sky << 4) | (block & 0x0F)
}

/// One chunk's packed light values, parallel to `ChunkVolume`.
///
/// Most chunks are uniform — open-sky air is `sky=15` everywhere, enclosed rock
/// is `0` everywhere — so a `Uniform` variant stores those as a single byte and
/// lets the flood skip them, promoting to a full `Dense` grid only when a cell
/// actually diverges. `get` is transparent to the variant; `set` promotes.
#[derive(Clone)]
pub enum ChunkLight {
    Uniform(u8),
    Dense(Box<[u8]>),
}

impl ChunkLight {
    /// Every cell dark (`0`). The default for a freshly resident chunk.
    pub fn dark() -> Self {
        Self::Uniform(0)
    }

    /// Every cell fully sky-lit (`sky=15, block=0`).
    pub fn full_sky() -> Self {
        Self::Uniform(pack(MAX_LIGHT, 0))
    }

    /// Every cell `value`.
    pub fn uniform(value: u8) -> Self {
        Self::Uniform(value)
    }

    /// `Some(v)` iff every cell holds the same value `v`.
    #[inline]
    pub fn uniform_value(&self) -> Option<u8> {
        match self {
            Self::Uniform(v) => Some(*v),
            Self::Dense(_) => None,
        }
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32, z: i32) -> u8 {
        match self {
            Self::Uniform(v) => *v,
            Self::Dense(d) => d[voxel_index(x, y, z)],
        }
    }

    #[inline]
    pub fn set(&mut self, x: i32, y: i32, z: i32, value: u8) {
        match self {
            Self::Uniform(v) => {
                if *v == value {
                    return;
                }
                // Diverged: materialize a dense grid filled with the old value.
                let mut d = vec![*v; CHUNK_CUBED].into_boxed_slice();
                d[voxel_index(x, y, z)] = value;
                *self = Self::Dense(d);
            }
            Self::Dense(d) => d[voxel_index(x, y, z)] = value,
        }
    }

    /// A contiguous `CHUNK_CUBED`-byte view: borrowed for `Dense`, materialized
    /// on demand for `Uniform`. Prefer [`write_into`](Self::write_into) when a
    /// destination buffer already exists.
    pub fn as_dense_bytes(&self) -> Cow<'_, [u8]> {
        match self {
            Self::Uniform(v) => Cow::Owned(vec![*v; CHUNK_CUBED]),
            Self::Dense(d) => Cow::Borrowed(d),
        }
    }

    /// Fill `dst` (length `CHUNK_CUBED`) with this chunk's light.
    pub fn write_into(&self, dst: &mut [u8]) {
        match self {
            Self::Uniform(v) => dst.fill(*v),
            Self::Dense(d) => dst.copy_from_slice(d),
        }
    }

    /// Build from a dense buffer, collapsing to `Uniform` when every byte is
    /// equal (the common all-sky / all-dark result of a flood).
    pub fn from_bytes_collapsed(bytes: &[u8]) -> Self {
        match bytes.first() {
            Some(&first) if bytes.iter().all(|&b| b == first) => Self::Uniform(first),
            _ => Self::Dense(bytes.to_vec().into_boxed_slice()),
        }
    }

    /// Collapse a `Dense` chunk back to `Uniform` in place if all cells are equal.
    pub fn collapse(&mut self) {
        if let Self::Dense(d) = self
            && let Some(&first) = d.first()
            && d.iter().all(|&b| b == first)
        {
            *self = Self::Uniform(first);
        }
    }

    /// A mutable dense view, promoting `Uniform` to `Dense` first.
    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        if let Self::Uniform(v) = *self {
            *self = Self::Dense(vec![v; CHUNK_CUBED].into_boxed_slice());
        }
        match self {
            Self::Dense(d) => d,
            Self::Uniform(_) => unreachable!("promoted above"),
        }
    }
}

impl Default for ChunkLight {
    fn default() -> Self {
        Self::dark()
    }
}

/// The world a light flood runs against: voxel opacity/emission, light
/// read/write, and the computation domain.
pub trait LightWorld {
    /// Opaque at `v` (all non-Air blocks today).
    fn solid(&self, v: IVec3) -> bool;
    /// Emitted blocklight level 0-15 at `v` (see `BlockRegistry::light_table`).
    fn emission(&self, v: IVec3) -> u8;
    fn light(&self, v: IVec3) -> u8;
    fn set_light(&mut self, v: IVec3, packed: u8);
    /// Cells the flood may read/write (loaded chunks; test bounds).
    fn in_domain(&self, v: IVec3) -> bool;
    /// Whether the out-of-domain space directly above `v` counts as open sky
    /// (only consulted when `v + Y` is outside the domain).
    fn open_sky_above(&self, v: IVec3) -> bool;

    /// True if the chunk at `cpos` (chunk coords) is entirely air. The default
    /// scans every cell; implementors with the voxel volume to hand should
    /// override with a cheap check (e.g. `ChunkVolume::is_empty`) so the
    /// full-sky fast path in [`light_new_chunk`] stays O(1).
    fn chunk_all_air(&self, cpos: IVec3) -> bool {
        chunk_cells(cpos).all(|v| !self.solid(v))
    }

    /// Set every cell of `cpos` to `packed`. The default writes cell by cell;
    /// implementors backed by [`ChunkLight`] should override to store a single
    /// `Uniform` value in O(1).
    fn set_chunk_uniform(&mut self, cpos: IVec3, packed: u8) {
        for v in chunk_cells(cpos) {
            self.set_light(v, packed);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Sky,
    Block,
}

impl Channel {
    #[inline]
    fn get(self, packed: u8) -> u8 {
        match self {
            Channel::Sky => sky(packed),
            Channel::Block => block(packed),
        }
    }

    #[inline]
    fn set(self, packed: u8, v: u8) -> u8 {
        match self {
            Channel::Sky => pack(v, block(packed)),
            Channel::Block => pack(sky(packed), v),
        }
    }

    /// Level after one step in `dir` (the sky beam falls without loss).
    #[inline]
    fn step(self, level: u8, dir: IVec3) -> u8 {
        if self == Channel::Sky && dir == IVec3::NEG_Y && level == MAX_LIGHT {
            MAX_LIGHT
        } else {
            level.saturating_sub(1)
        }
    }
}

/// Neighbor order is fixed so floods are deterministic.
const DIRS: [IVec3; 6] = [
    IVec3::X,
    IVec3::NEG_X,
    IVec3::Y,
    IVec3::NEG_Y,
    IVec3::Z,
    IVec3::NEG_Z,
];

/// Raise-only BFS: seeds must already carry their light; spreads into
/// non-solid, in-domain cells whenever that increases their level.
pub fn propagate(world: &mut impl LightWorld, ch: Channel, mut queue: VecDeque<IVec3>) {
    while let Some(v) = queue.pop_front() {
        let level = ch.get(world.light(v));
        if level == 0 {
            continue;
        }
        for dir in DIRS {
            let n = v + dir;
            if !world.in_domain(n) || world.solid(n) {
                continue;
            }
            let new = ch.step(level, dir);
            let packed = world.light(n);
            if new > ch.get(packed) {
                world.set_light(n, ch.set(packed, new));
                queue.push_back(n);
            }
        }
    }
}

/// Two-queue removal: `removals` holds cells whose light was just deleted,
/// paired with the level they used to have. Clears everything that depended on
/// them and returns the surviving bright frontier to re-[`propagate`].
pub fn unpropagate(
    world: &mut impl LightWorld,
    ch: Channel,
    mut removals: VecDeque<(IVec3, u8)>,
) -> VecDeque<IVec3> {
    let mut reseeds = VecDeque::new();
    while let Some((v, old)) = removals.pop_front() {
        for dir in DIRS {
            let n = v + dir;
            if !world.in_domain(n) {
                continue;
            }
            let packed = world.light(n);
            let nl = ch.get(packed);
            if nl == 0 {
                continue;
            }
            // An emitter is its own source; never treat it as dependent.
            if ch == Channel::Block && world.emission(n) >= nl {
                reseeds.push_back(n);
                continue;
            }
            let beam = ch == Channel::Sky && dir == IVec3::NEG_Y && old == MAX_LIGHT && nl == MAX_LIGHT;
            if nl < old || beam {
                world.set_light(n, ch.set(packed, 0));
                removals.push_back((n, nl));
            } else {
                reseeds.push_back(n);
            }
        }
    }
    reseeds
}

/// Iterate every cell of a chunk (local order x-fastest).
fn chunk_cells(cpos: IVec3) -> impl Iterator<Item = IVec3> {
    let origin = chunk_origin(cpos);
    (0..CHUNK_SIZE).flat_map(move |y| {
        (0..CHUNK_SIZE).flat_map(move |z| {
            (0..CHUNK_SIZE).map(move |x| origin + IVec3::new(x, y, z))
        })
    })
}

/// Seed a cell's channels from its own properties (emission; direct sky when
/// the domain ends above it), returning per-channel queues to extend.
fn seed_cell(
    world: &mut impl LightWorld,
    v: IVec3,
    sky_seeds: &mut VecDeque<IVec3>,
    block_seeds: &mut VecDeque<IVec3>,
) {
    let e = world.emission(v);
    if e > 0 {
        let packed = world.light(v);
        if e > block(packed) {
            world.set_light(v, Channel::Block.set(packed, e));
        }
        block_seeds.push_back(v);
    }
    if !world.solid(v) && !world.in_domain(v + IVec3::Y) && world.open_sky_above(v) {
        let packed = world.light(v);
        world.set_light(v, Channel::Sky.set(packed, MAX_LIGHT));
        sky_seeds.push_back(v);
    }
}

/// From-scratch (re)light of a set of chunks — the oracle the incremental
/// paths are tested against. Zeroes the listed chunks, seeds emitters and
/// open-sky top cells, and floods. Light does not enter from outside the
/// listed set (callers pass the whole domain).
pub fn relight_full(world: &mut impl LightWorld, chunks: &[IVec3]) {
    for &c in chunks {
        for v in chunk_cells(c) {
            world.set_light(v, 0);
        }
    }
    let mut sky_seeds = VecDeque::new();
    let mut block_seeds = VecDeque::new();
    for &c in chunks {
        for v in chunk_cells(c) {
            seed_cell(world, v, &mut sky_seeds, &mut block_seeds);
        }
    }
    propagate(world, Channel::Sky, sky_seeds);
    propagate(world, Channel::Block, block_seeds);
}

/// Fast path for [`light_new_chunk`]: if the chunk at `cpos` is entirely air,
/// receives `sky=15` across its whole top face (open sky above, or an already
/// `15` neighbor), and takes no block light on any of its six faces, then every
/// cell is `(sky=15, block=0)`. Set it uniformly and spill that light outward
/// via `propagate` — identical to what the general flood would produce, but
/// without the per-cell zero, the seed scan, or the within-chunk BFS. Returns
/// `false` (no change) when any condition fails, so the caller runs the full path.
fn try_full_sky(world: &mut impl LightWorld, cpos: IVec3) -> bool {
    if !world.chunk_all_air(cpos) {
        return false;
    }
    let origin = chunk_origin(cpos);
    let s = CHUNK_SIZE;
    for a in 0..s {
        for b in 0..s {
            // Top face must deliver 15: open sky, or a neighbor already at 15.
            let top_cell = origin + IVec3::new(a, s - 1, b);
            let above = top_cell + IVec3::Y;
            let top_ok = if world.in_domain(above) {
                let l = world.light(above);
                sky(l) >= MAX_LIGHT && block(l) == 0
            } else {
                world.open_sky_above(top_cell)
            };
            if !top_ok {
                return false;
            }
            // No block light may enter from any face (it would break uniformity).
            for nb in [
                origin + IVec3::new(-1, a, b),
                origin + IVec3::new(s, a, b),
                origin + IVec3::new(a, -1, b),
                origin + IVec3::new(a, b, -1),
                origin + IVec3::new(a, b, s),
            ] {
                if world.in_domain(nb) && block(world.light(nb)) > 0 {
                    return false;
                }
            }
        }
    }

    world.set_chunk_uniform(cpos, pack(MAX_LIGHT, 0));

    // Spill the chunk's border light outward exactly as the full flood would
    // (interior cells are already at the 15 max, so propagate does no work there).
    let mut sky_seeds = VecDeque::new();
    for a in 0..s {
        for b in 0..s {
            sky_seeds.push_back(origin + IVec3::new(0, a, b));
            sky_seeds.push_back(origin + IVec3::new(s - 1, a, b));
            sky_seeds.push_back(origin + IVec3::new(a, 0, b));
            sky_seeds.push_back(origin + IVec3::new(a, s - 1, b));
            sky_seeds.push_back(origin + IVec3::new(a, b, 0));
            sky_seeds.push_back(origin + IVec3::new(a, b, s - 1));
        }
    }
    propagate(world, Channel::Sky, sky_seeds);
    true
}

/// Light a chunk that just entered the domain: seed its own emitters and
/// open-sky top, pull light in across all six faces from loaded neighbors, and
/// flood (spilling back out into neighbors where this chunk brightens them).
/// Follow with [`reconcile_sky_below`] so an optimistically sky-lit chunk
/// below gets darkened by this chunk's terrain.
pub fn light_new_chunk(world: &mut impl LightWorld, cpos: IVec3) {
    // "Assume full light": an all-air chunk that receives sky=15 across its
    // whole top and no block light on any face is uniformly (sky=15, block=0).
    // Set it in one shot and spill outward, skipping the per-cell zero + seed
    // scan + within-chunk BFS. Oracle-equivalent to the general path below.
    if try_full_sky(world, cpos) {
        return;
    }

    for v in chunk_cells(cpos) {
        world.set_light(v, 0);
    }

    let mut sky_seeds = VecDeque::new();
    let mut block_seeds = VecDeque::new();
    for v in chunk_cells(cpos) {
        seed_cell(world, v, &mut sky_seeds, &mut block_seeds);
    }

    // Boundary inflow: every out-of-chunk face neighbor that is lit becomes a
    // seed; propagation carries its light into this chunk.
    let origin = chunk_origin(cpos);
    let s = CHUNK_SIZE;
    for a in 0..s {
        for b in 0..s {
            for v in [
                origin + IVec3::new(-1, a, b),
                origin + IVec3::new(s, a, b),
                origin + IVec3::new(a, -1, b),
                origin + IVec3::new(a, s, b),
                origin + IVec3::new(a, b, -1),
                origin + IVec3::new(a, b, s),
            ] {
                if !world.in_domain(v) {
                    continue;
                }
                let packed = world.light(v);
                if sky(packed) > 0 {
                    sky_seeds.push_back(v);
                }
                if block(packed) > 0 {
                    block_seeds.push_back(v);
                }
            }
        }
    }

    propagate(world, Channel::Sky, sky_seeds);
    propagate(world, Channel::Block, block_seeds);
}

/// After `cpos` was lit, fix the chunk directly below it: while that chunk was
/// the top of its column it may have assumed open sky. Remove any of its
/// skylight now contradicted by `cpos`'s bottom layer.
pub fn reconcile_sky_below(world: &mut impl LightWorld, cpos: IVec3) {
    let origin = chunk_origin(cpos);
    let mut removals = VecDeque::new();
    for z in 0..CHUNK_SIZE {
        for x in 0..CHUNK_SIZE {
            let top = origin + IVec3::new(x, 0, z); // bottom layer of `cpos`
            let under = top - IVec3::Y; // top layer of the chunk below
            if !world.in_domain(under) {
                continue;
            }
            let packed = world.light(under);
            let have = sky(packed);
            if have == 0 {
                continue;
            }
            let feed = if world.solid(top) { 0 } else { sky(world.light(top)) };
            let expected = Channel::Sky.step(feed, IVec3::NEG_Y);
            if have > expected {
                world.set_light(under, Channel::Sky.set(packed, 0));
                removals.push_back((under, have));
            }
        }
    }
    if !removals.is_empty() {
        let reseeds = unpropagate(world, Channel::Sky, removals);
        propagate(world, Channel::Sky, reseeds);
    }
}

/// Incrementally fix light after the voxel at `v` changed (the voxel data must
/// already reflect the edit).
pub fn apply_voxel_change(world: &mut impl LightWorld, v: IVec3) {
    let old = world.light(v);
    if world.solid(v) {
        // Placed a block (possibly an emitter).
        let e = world.emission(v);
        world.set_light(v, pack(0, e));
        if sky(old) > 0 {
            let reseeds =
                unpropagate(world, Channel::Sky, VecDeque::from([(v, sky(old))]));
            propagate(world, Channel::Sky, reseeds);
        }
        let mut reseeds = if block(old) > 0 {
            unpropagate(world, Channel::Block, VecDeque::from([(v, block(old))]))
        } else {
            VecDeque::new()
        };
        if e > 0 {
            reseeds.push_back(v);
        }
        propagate(world, Channel::Block, reseeds);
    } else {
        // Broke a block (possibly an emitter: its own stored level goes away).
        world.set_light(v, 0);
        let mut block_reseeds = if block(old) > 0 {
            unpropagate(world, Channel::Block, VecDeque::from([(v, block(old))]))
        } else {
            VecDeque::new()
        };
        let mut sky_seeds = VecDeque::new();
        for dir in DIRS {
            let n = v + dir;
            if !world.in_domain(n) {
                continue;
            }
            let packed = world.light(n);
            if sky(packed) > 0 {
                sky_seeds.push_back(n);
            }
            if block(packed) > 0 {
                block_reseeds.push_back(n);
            }
        }
        // The domain may end right above: the new hole sees the sky directly.
        if !world.in_domain(v + IVec3::Y) && world.open_sky_above(v) {
            world.set_light(v, Channel::Sky.set(world.light(v), MAX_LIGHT));
            sky_seeds.push_back(v);
        }
        propagate(world, Channel::Sky, sky_seeds);
        propagate(world, Channel::Block, block_reseeds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soils_protocol::chunk_of;
    use std::collections::HashMap;

    /// A boxed test world: voxels + light in `chunks_x × chunks_y × chunks_z`
    /// chunks starting at the origin; everything above the box is open sky.
    struct TestWorld {
        voxels: HashMap<IVec3, u8>,
        light: HashMap<IVec3, u8>,
        emitters: HashMap<u8, u8>, // block id -> level
        max: IVec3,                // exclusive, in voxels
        sky_above: bool,
    }

    impl TestWorld {
        fn new(chunks: IVec3) -> Self {
            Self {
                voxels: HashMap::new(),
                light: HashMap::new(),
                emitters: HashMap::new(),
                max: chunks * CHUNK_SIZE,
                sky_above: true,
            }
        }

        fn set_voxel(&mut self, x: i32, y: i32, z: i32, id: u8) {
            self.voxels.insert(IVec3::new(x, y, z), id);
        }

        fn chunk_list(&self) -> Vec<IVec3> {
            let c = chunk_of(self.max - IVec3::ONE) + IVec3::ONE;
            let mut out = Vec::new();
            for y in 0..c.y {
                for z in 0..c.z {
                    for x in 0..c.x {
                        out.push(IVec3::new(x, y, z));
                    }
                }
            }
            out
        }

        fn sky_at(&self, x: i32, y: i32, z: i32) -> u8 {
            sky(*self.light.get(&IVec3::new(x, y, z)).unwrap_or(&0))
        }

        fn block_at(&self, x: i32, y: i32, z: i32) -> u8 {
            block(*self.light.get(&IVec3::new(x, y, z)).unwrap_or(&0))
        }
    }

    impl LightWorld for TestWorld {
        fn solid(&self, v: IVec3) -> bool {
            self.voxels.get(&v).copied().unwrap_or(0) != 0
        }
        fn emission(&self, v: IVec3) -> u8 {
            let id = self.voxels.get(&v).copied().unwrap_or(0);
            self.emitters.get(&id).copied().unwrap_or(0)
        }
        fn light(&self, v: IVec3) -> u8 {
            self.light.get(&v).copied().unwrap_or(0)
        }
        fn set_light(&mut self, v: IVec3, packed: u8) {
            self.light.insert(v, packed);
        }
        fn in_domain(&self, v: IVec3) -> bool {
            v.cmpge(IVec3::ZERO).all() && v.cmplt(self.max).all()
        }
        fn open_sky_above(&self, _v: IVec3) -> bool {
            self.sky_above
        }
    }

    #[test]
    fn open_world_is_fully_sky_lit() {
        let mut w = TestWorld::new(IVec3::new(1, 1, 1));
        let chunks = w.chunk_list();
        relight_full(&mut w, &chunks);
        assert_eq!(w.sky_at(0, 0, 0), 15);
        assert_eq!(w.sky_at(16, 31, 16), 15);
        assert_eq!(w.sky_at(31, 0, 31), 15);
    }

    #[test]
    fn skylight_under_platform_attenuates_from_edges() {
        let mut w = TestWorld::new(IVec3::new(1, 1, 1));
        // A wide platform at y=20 spanning x/z 4..=27.
        for x in 4..=27 {
            for z in 4..=27 {
                w.set_voxel(x, 20, z, 1);
            }
        }
        let chunks = w.chunk_list();
        relight_full(&mut w, &chunks);
        // Above the platform: direct sky.
        assert_eq!(w.sky_at(16, 21, 16), 15);
        // Just inside the platform edge at platform level - 1: light walked
        // down the rim (one down-step off the 15 column keeps 15? No — the rim
        // cell below y=20 at x=3 is under open sky (15); one step sideways in
        // costs 1.
        assert_eq!(w.sky_at(4, 19, 16), 14);
        // Deep centre below the platform: 15 - distance to the nearest open
        // column (x=28, one past the platform's 4..=27 span → 12 steps).
        let d = 28 - 16;
        assert_eq!(w.sky_at(16, 19, 16) as i32, (15 - d).max(0));
    }

    #[test]
    fn emitter_makes_a_diamond() {
        let mut w = TestWorld::new(IVec3::new(1, 1, 1));
        w.emitters.insert(9, 14);
        w.sky_above = false; // isolate blocklight
        w.set_voxel(16, 16, 16, 9);
        let chunks = w.chunk_list();
        relight_full(&mut w, &chunks);
        assert_eq!(w.block_at(16, 16, 16), 14, "emitter holds its level");
        assert_eq!(w.block_at(17, 16, 16), 13);
        assert_eq!(w.block_at(16, 20, 16), 10, "manhattan distance 4");
        assert_eq!(w.block_at(16 + 13, 16, 16), 1, "14 - distance 13");
        assert_eq!(w.block_at(16 + 14, 16, 16), 0, "level 14 reaches 13 cells");
        assert_eq!(w.sky_at(16, 31, 16), 0, "no sky in this test");
    }

    #[test]
    fn wall_blocks_blocklight() {
        let mut w = TestWorld::new(IVec3::new(1, 1, 1));
        w.emitters.insert(9, 10);
        w.sky_above = false;
        w.set_voxel(10, 10, 10, 9);
        // A full wall two cells to the +x side.
        for y in 0..32 {
            for z in 0..32 {
                w.set_voxel(12, y, z, 1);
            }
        }
        let chunks = w.chunk_list();
        relight_full(&mut w, &chunks);
        assert_eq!(w.block_at(11, 10, 10), 9);
        assert_eq!(w.block_at(12, 10, 10), 0, "opaque cell holds none");
        // Behind the wall: light must walk around (over the top at y=31 is
        // open? The wall spans all y/z, so nothing gets through in-domain).
        assert_eq!(w.block_at(13, 10, 10), 0);
    }

    #[test]
    fn placing_block_cuts_the_sky_beam() {
        let mut w = TestWorld::new(IVec3::new(1, 1, 1));
        let chunks = w.chunk_list();
        relight_full(&mut w, &chunks);
        assert_eq!(w.sky_at(16, 10, 16), 15);

        w.set_voxel(16, 25, 16, 1);
        apply_voxel_change(&mut w, IVec3::new(16, 25, 16));

        // Below the new block the beam is gone, but neighbors still carry 15,
        // so the column refills sideways at 14.
        assert_eq!(w.sky_at(16, 25, 16), 0, "opaque cell");
        assert_eq!(w.sky_at(16, 24, 16), 14);

        // And the incremental result matches a full relight.
        let mut fresh = TestWorld::new(IVec3::new(1, 1, 1));
        fresh.voxels = w.voxels.clone();
        let chunks = fresh.chunk_list();
        relight_full(&mut fresh, &chunks);
        assert_eq!(w.light, fresh.light);
    }

    #[test]
    fn incremental_matches_full_relight_under_edit_storm() {
        let mut w = TestWorld::new(IVec3::new(1, 1, 1));
        w.emitters.insert(9, 14);
        w.emitters.insert(11, 15);
        // Floor.
        for x in 0..32 {
            for z in 0..32 {
                w.set_voxel(x, 8, z, 1);
            }
        }
        let chunks = w.chunk_list();
        relight_full(&mut w, &chunks);

        // Deterministic LCG for reproducibility (no rand dep).
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for _ in 0..120 {
            let x = (next() % 32) as i32;
            let y = (next() % 24) as i32; // keep some open sky above
            let z = (next() % 32) as i32;
            let id = match next() % 4 {
                0 => 0u8, // break
                1 => 9,   // diamond emitter
                2 => 11,  // ruby emitter
                _ => 1,   // stone
            };
            w.set_voxel(x, y, z, id);
            apply_voxel_change(&mut w, IVec3::new(x, y, z));
        }

        let mut fresh = TestWorld::new(IVec3::new(1, 1, 1));
        fresh.emitters = w.emitters.clone();
        fresh.voxels = w.voxels.clone();
        let chunks = fresh.chunk_list();
        relight_full(&mut fresh, &chunks);
        for (v, l) in &fresh.light {
            assert_eq!(
                w.light.get(v).copied().unwrap_or(0),
                *l,
                "mismatch at {v:?} (incremental vs oracle)"
            );
        }
        for (v, l) in &w.light {
            assert_eq!(fresh.light.get(v).copied().unwrap_or(0), *l, "extra light at {v:?}");
        }
    }

    #[test]
    fn new_chunk_above_darkens_optimistic_chunk_below() {
        // Two stacked chunks. Light the bottom one first with nothing above:
        // it assumes open sky. Then the top chunk (with a solid slab) loads.
        let mut w = TestWorld::new(IVec3::new(1, 2, 1));
        // Solid slab across the top chunk's mid-height.
        for x in 0..32 {
            for z in 0..32 {
                w.set_voxel(x, 48, z, 1);
            }
        }
        // Phase 1: only the bottom chunk exists in-domain.
        w.max = IVec3::new(32, 32, 32);
        light_new_chunk(&mut w, IVec3::new(0, 0, 0));
        assert_eq!(w.sky_at(16, 31, 16), 15, "assumed open sky");

        // Phase 2: the top chunk loads; its slab occludes the column.
        w.max = IVec3::new(32, 64, 32);
        light_new_chunk(&mut w, IVec3::new(0, 1, 0));
        reconcile_sky_below(&mut w, IVec3::new(0, 1, 0));

        assert_eq!(w.sky_at(16, 49, 16), 15, "above the slab");
        assert_eq!(w.sky_at(16, 47, 16), 0, "under the wide slab, far from edges");
        assert_eq!(w.sky_at(16, 16, 16), 0, "bottom chunk darkened");

        // Agreement with the oracle over both chunks.
        let mut fresh = TestWorld::new(IVec3::new(1, 2, 1));
        fresh.voxels = w.voxels.clone();
        let chunks = fresh.chunk_list();
        relight_full(&mut fresh, &chunks);
        for (v, l) in &fresh.light {
            assert_eq!(w.light.get(v).copied().unwrap_or(0), *l, "mismatch at {v:?}");
        }
    }

    #[test]
    fn boundary_inflow_lights_new_chunk_from_neighbor() {
        // Two side-by-side chunks, no sky; an emitter near the shared face.
        let mut w = TestWorld::new(IVec3::new(2, 1, 1));
        w.sky_above = false;
        w.emitters.insert(11, 15);
        w.set_voxel(30, 16, 16, 11);
        // Light only the left chunk first (right not in domain yet).
        w.max = IVec3::new(32, 32, 32);
        light_new_chunk(&mut w, IVec3::new(0, 0, 0));
        // Now the right chunk loads: inflow should carry the glow across.
        w.max = IVec3::new(64, 32, 32);
        light_new_chunk(&mut w, IVec3::new(1, 0, 0));
        assert_eq!(w.block_at(33, 16, 16), 12, "3 steps from the level-15 emitter");
    }

    #[test]
    fn chunk_light_enum_get_set_and_collapse() {
        let mut cl = ChunkLight::dark();
        assert_eq!(cl.uniform_value(), Some(0));
        assert_eq!(cl.get(3, 4, 5), 0);
        cl.set(3, 4, 5, 0); // same value: stays uniform
        assert_eq!(cl.uniform_value(), Some(0));
        cl.set(3, 4, 5, 0xF0); // diverge: promotes to dense
        assert_eq!(cl.uniform_value(), None);
        assert_eq!(cl.get(3, 4, 5), 0xF0);
        assert_eq!(cl.get(0, 0, 0), 0);
        let bytes = cl.as_dense_bytes();
        assert_eq!(bytes.len(), CHUNK_CUBED);
        assert_eq!(bytes[voxel_index(3, 4, 5)], 0xF0);
        cl.set(3, 4, 5, 0); // back to all-zero...
        cl.collapse(); // ...collapses to uniform
        assert_eq!(cl.uniform_value(), Some(0));

        assert_eq!(ChunkLight::from_bytes_collapsed(&[7u8; CHUNK_CUBED]).uniform_value(), Some(7));
        let mut mixed = vec![7u8; CHUNK_CUBED];
        mixed[10] = 8;
        assert_eq!(ChunkLight::from_bytes_collapsed(&mixed).uniform_value(), None);
        assert_eq!(ChunkLight::from_bytes_collapsed(&mixed).get(0, 1, 0), 7);
    }

    #[test]
    fn full_sky_fast_path_equals_oracle() {
        // A single all-air open-sky chunk, lit incrementally.
        let mut w = TestWorld::new(IVec3::new(1, 1, 1));
        light_new_chunk(&mut w, IVec3::new(0, 0, 0));
        for &(x, y, z) in &[(0, 0, 0), (16, 16, 16), (31, 31, 31), (0, 31, 0)] {
            assert_eq!(w.sky_at(x, y, z), 15);
            assert_eq!(w.block_at(x, y, z), 0);
        }
        let mut fresh = TestWorld::new(IVec3::new(1, 1, 1));
        let chunks = fresh.chunk_list();
        relight_full(&mut fresh, &chunks);
        assert_eq!(w.light, fresh.light);
    }

    #[test]
    fn full_sky_column_matches_oracle_top_down() {
        // A vertical column of all-air chunks, lit top-down like the client.
        let mut w = TestWorld::new(IVec3::new(1, 3, 1));
        for cy in (0..3).rev() {
            light_new_chunk(&mut w, IVec3::new(0, cy, 0));
        }
        let mut fresh = TestWorld::new(IVec3::new(1, 3, 1));
        let chunks = fresh.chunk_list();
        relight_full(&mut fresh, &chunks);
        assert_eq!(w.light, fresh.light);
    }

    #[test]
    fn full_sky_bails_when_block_light_enters() {
        // Two side-by-side chunks under open sky; the right one has a max-level
        // emitter on the shared face. Lighting the (all-air) left chunk must see
        // the block inflow, fall back to the full flood, and match the oracle —
        // not collapse to a naive uniform sky.
        let mut w = TestWorld::new(IVec3::new(2, 1, 1));
        w.emitters.insert(11, 15);
        w.set_voxel(32, 16, 16, 11); // right chunk, local (0,16,16), on the face
        light_new_chunk(&mut w, IVec3::new(1, 0, 0)); // right (has emitter)
        light_new_chunk(&mut w, IVec3::new(0, 0, 0)); // left (must bail)

        // Left saw block light across the face and is not naive-uniform.
        assert_eq!(w.sky_at(16, 16, 16), 15, "still sky-lit");
        assert!(w.block_at(31, 16, 16) > 0, "block light crossed into the left chunk");

        let mut fresh = TestWorld::new(IVec3::new(2, 1, 1));
        fresh.emitters = w.emitters.clone();
        fresh.voxels = w.voxels.clone();
        let chunks = fresh.chunk_list();
        relight_full(&mut fresh, &chunks);
        for (v, l) in &fresh.light {
            assert_eq!(w.light.get(v).copied().unwrap_or(0), *l, "mismatch at {v:?}");
        }
        for (v, l) in &w.light {
            assert_eq!(fresh.light.get(v).copied().unwrap_or(0), *l, "extra at {v:?}");
        }
    }
}
