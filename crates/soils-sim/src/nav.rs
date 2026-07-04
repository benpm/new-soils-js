//! Server-side pathfinding, stages 1–2 (plan-game-systems §10): a per-chunk
//! walkability bit-grid derived from voxels, and a budgeted A* with jump/fall
//! costs. Pure functions over [`VoxelSampler`] like the rest of the sim, so
//! the server supplies its chunk map and tests supply closures.
//!
//! Terminology (shared by all stages):
//! - a cell is **passable** if a 2-high body can occupy it (air at the cell
//!   and the cell above);
//! - a cell is **walkable** if it is passable and stands on solid ground.
//!
//! Unloaded space reads as air (the [`VoxelSampler`] contract), so cells over
//! a missing floor are simply not walkable — paths never enter unloaded
//! terrain, which is the conservative behavior the server wants.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;

use glam::IVec3;
use soils_protocol::{CHUNK_SIZE, chunk_origin, voxel_index};

use crate::VoxelSampler;

const UP: IVec3 = IVec3::Y;
/// The four lateral step directions (4-connectivity; no corner cutting).
const LATERAL: [IVec3; 4] = [IVec3::X, IVec3::NEG_X, IVec3::Z, IVec3::NEG_Z];
/// Deepest drop an agent will pathfind over, in voxels (Minecraft-mob-like).
pub const MAX_FALL: i32 = 3;

/// Can a 2-high body occupy `p`?
pub fn passable(world: &impl VoxelSampler, p: IVec3) -> bool {
    !world.is_solid(p) && !world.is_solid(p + UP)
}

/// Can an agent stand at `p`?
pub fn walkable(world: &impl VoxelSampler, p: IVec3) -> bool {
    passable(world, p) && world.is_solid(p - UP)
}

/// Resolve a body position to a walkable path endpoint: `p`'s own column
/// scanned down up to `down` voxels first (bodies hover and fall), then the
/// eight neighbor columns nearest-first (a body can stand on a block *edge*,
/// leaving its center column floorless). `None` if nothing nearby is
/// standable — e.g. positions over unloaded space.
pub fn resolve_walkable(world: &impl VoxelSampler, p: IVec3, down: i32) -> Option<IVec3> {
    let mut cols: Vec<IVec3> = (-1..=1)
        .flat_map(|dz| (-1..=1).map(move |dx| IVec3::new(dx, 0, dz)))
        .collect();
    cols.sort_by_key(|d| d.x.abs() + d.z.abs());
    for d in cols {
        for k in 0..=down {
            let c = p + d - UP * k;
            if walkable(world, c) {
                return Some(c);
            }
        }
    }
    None
}

// ---------------- Stage 1: per-chunk walkability grid ----------------

const WORDS: usize = (CHUNK_SIZE * CHUNK_SIZE * CHUNK_SIZE) as usize / 64;

/// Per-chunk bit-set of walkable cells, cell order matching the voxel/light
/// layout (`voxel_index`). 4 KB; derived on demand and cached beside the chunk
/// keyed by its edit version (stage-2 integration).
#[derive(Clone, PartialEq)]
pub struct WalkGrid {
    words: [u64; WORDS],
}

impl Default for WalkGrid {
    fn default() -> Self {
        Self { words: [0; WORDS] }
    }
}

impl WalkGrid {
    pub fn get(&self, x: i32, y: i32, z: i32) -> bool {
        let i = voxel_index(x, y, z);
        self.words[i / 64] & (1 << (i % 64)) != 0
    }

    fn set(&mut self, x: i32, y: i32, z: i32) {
        let i = voxel_index(x, y, z);
        self.words[i / 64] |= 1 << (i % 64);
    }

    /// Number of walkable cells (cheap popcount; summaries and tests).
    pub fn count(&self) -> u32 {
        self.words.iter().map(|w| w.count_ones()).sum()
    }
}

/// Derive chunk `cpos`'s walkability grid from world voxels. Border rows
/// sample the vertical neighbor chunks through the sampler (missing below ⇒
/// no floor ⇒ not walkable; missing above ⇒ open headroom).
pub fn walk_grid(world: &impl VoxelSampler, cpos: IVec3) -> WalkGrid {
    let origin = chunk_origin(cpos);
    let mut grid = WalkGrid::default();
    for y in 0..CHUNK_SIZE {
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let p = origin + IVec3::new(x, y, z);
                // Scan bottom-up per column: a solid voxel means the cell
                // above it is a candidate; cheap early skip on solid cells.
                if walkable(world, p) {
                    grid.set(x, y, z);
                }
            }
        }
    }
    grid
}

// ---------------- Stage 2: budgeted local A* ----------------

/// Outcome of a budgeted search: a path (start..=goal cell positions), proof
/// there is none in the searched region, or an exhausted expansion budget
/// (caller retries later / falls back to the hierarchical layer).
#[derive(Debug, PartialEq)]
pub enum PathResult {
    Path(Vec<IVec3>),
    NoPath,
    Budget,
}

// Move costs in fixed-point quarters (×4), so falls can price per dropped
// voxel without floats in the heap.
const COST_WALK: u32 = 4;
const COST_JUMP: u32 = 6;
const COST_FALL_PER_VOXEL: u32 = 1;

/// Budgeted A* from `start` to `goal` (both must be walkable cells, or the
/// result is [`PathResult::NoPath`]). Moves are lateral steps, 1-up jumps
/// (needing 3-high clearance over the takeoff cell), and drops of up to
/// [`MAX_FALL`]. `max_expansions` bounds work per call — this is the per-tick
/// budget knob from the plan; short paths (< ~32 voxels) fit comfortably in a
/// few hundred expansions.
pub fn find_path(
    world: &impl VoxelSampler,
    start: IVec3,
    goal: IVec3,
    max_expansions: usize,
) -> PathResult {
    if !walkable(world, start) || !walkable(world, goal) {
        return PathResult::NoPath;
    }
    if start == goal {
        return PathResult::Path(vec![start]);
    }

    // Admissible heuristic: every move advances one lateral cell at least at
    // walk cost, so lateral manhattan distance is a lower bound.
    let h = |p: IVec3| ((p.x - goal.x).abs() + (p.z - goal.z).abs()) as u32 * COST_WALK;

    let mut open: BinaryHeap<Reverse<(u32, IVec3ToKey)>> = BinaryHeap::new();
    let mut best: HashMap<IVec3, (u32, IVec3)> = HashMap::new(); // g, parent
    best.insert(start, (0, start));
    open.push(Reverse((h(start), IVec3ToKey(start))));

    let mut expansions = 0;
    while let Some(Reverse((_, IVec3ToKey(cur)))) = open.pop() {
        let (g_cur, _) = best[&cur];
        if cur == goal {
            let mut path = vec![goal];
            let mut p = goal;
            while p != start {
                p = best[&p].1;
                path.push(p);
            }
            path.reverse();
            return PathResult::Path(path);
        }
        expansions += 1;
        if expansions > max_expansions {
            return PathResult::Budget;
        }

        for d in LATERAL {
            let n = cur + d;
            // Same-level step.
            if walkable(world, n) {
                relax(world, &mut open, &mut best, g_cur + COST_WALK, cur, n, h);
                continue;
            }
            // 1-up jump: destination walkable one higher, and the takeoff
            // column needs a third air voxel to rise through.
            let n_up = n + UP;
            if walkable(world, n_up) && !world.is_solid(cur + UP * 2) {
                relax(world, &mut open, &mut best, g_cur + COST_JUMP, cur, n_up, h);
            }
            // Drop: step into the (floorless) neighbor column and fall until
            // ground, every transited cell passable.
            if passable(world, n) {
                let mut fall = n;
                for k in 1..=MAX_FALL {
                    fall -= UP;
                    if !passable(world, fall) {
                        break;
                    }
                    if walkable(world, fall) {
                        let cost = g_cur + COST_WALK + k as u32 * COST_FALL_PER_VOXEL;
                        relax(world, &mut open, &mut best, cost, cur, fall, h);
                        break;
                    }
                }
            }
        }
    }
    PathResult::NoPath
}

/// Standard A* edge relaxation.
fn relax(
    _world: &impl VoxelSampler,
    open: &mut BinaryHeap<Reverse<(u32, IVec3ToKey)>>,
    best: &mut HashMap<IVec3, (u32, IVec3)>,
    g: u32,
    from: IVec3,
    to: IVec3,
    h: impl Fn(IVec3) -> u32,
) {
    let better = best.get(&to).is_none_or(|&(old_g, _)| g < old_g);
    if better {
        best.insert(to, (g, from));
        open.push(Reverse((g + h(to), IVec3ToKey(to))));
    }
}

/// `IVec3` doesn't implement `Ord`; wrap it so the heap can tie-break.
#[derive(PartialEq, Eq)]
struct IVec3ToKey(IVec3);

impl Ord for IVec3ToKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.to_array().cmp(&other.0.to_array())
    }
}

impl PartialOrd for IVec3ToKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny scene: solid voxels in a set, everything else air.
    fn scene(solids: &[IVec3]) -> impl VoxelSampler + '_ {
        move |v: IVec3| u8::from(solids.contains(&v))
    }

    /// A flat solid floor at y = `fy` (all x/z), everything above air.
    fn floor_world(fy: i32) -> impl VoxelSampler {
        move |v: IVec3| u8::from(v.y == fy)
    }

    /// Clip a world to |x|,|z| <= r (air outside): keeps the reachable set
    /// finite so exhausted searches return `NoPath`, not `Budget`.
    fn bounded(world: impl Fn(IVec3) -> u8, r: i32) -> impl VoxelSampler {
        move |v: IVec3| if v.x.abs() > r || v.z.abs() > r { 0 } else { world(v) }
    }

    #[test]
    fn walk_grid_marks_cells_above_a_slab() {
        // Slab across the chunk at local y = 4: exactly the y = 5 row is
        // walkable (air at 5 and 6, solid at 4).
        let world = |v: IVec3| u8::from(v.y == 4 && (0..32).contains(&v.x) && (0..32).contains(&v.z));
        let grid = walk_grid(&world, IVec3::ZERO);
        assert_eq!(grid.count(), 32 * 32);
        assert!(grid.get(0, 5, 0) && grid.get(31, 5, 31));
        assert!(!grid.get(0, 4, 0) && !grid.get(0, 6, 0));
    }

    #[test]
    fn walk_grid_requires_two_high_headroom() {
        // Slab at y=4 plus a ceiling at y=6 leaves only 1-high space between
        // them: the gap row (y=5) is not walkable. (The ceiling's own top,
        // y=7, legitimately is.)
        let world = |v: IVec3| u8::from(v.y == 4 || v.y == 6);
        let grid = walk_grid(&world, IVec3::ZERO);
        for (x, z) in [(0, 0), (13, 7), (31, 31)] {
            assert!(!grid.get(x, 5, z), "1-high gap must not be walkable");
            assert!(grid.get(x, 7, z), "row above the ceiling is walkable");
        }
        assert_eq!(grid.count(), 32 * 32);
    }

    #[test]
    fn walk_grid_borders_sample_neighbor_chunks() {
        // Floor is the top layer (y=31) of the chunk *below*: the y=0 row of
        // chunk (0,0,0) is walkable only because the sampler sees it.
        let world = |v: IVec3| u8::from(v.y == -1);
        let grid = walk_grid(&world, IVec3::ZERO);
        assert_eq!(grid.count(), 32 * 32);
        assert!(grid.get(7, 0, 9));
        // With nothing below (unloaded reads air), the row is not walkable.
        let empty = |_v: IVec3| 0u8;
        assert_eq!(walk_grid(&empty, IVec3::ZERO).count(), 0);
    }

    #[test]
    fn resolve_walkable_handles_hover_and_edge_standing() {
        // Hovering high above a floor: the own-column down-scan finds it.
        let world = floor_world(0);
        assert_eq!(
            resolve_walkable(&world, IVec3::new(4, 20, 4), 32),
            Some(IVec3::new(4, 1, 4))
        );
        // Standing on a block edge: the center column has no floor, but the
        // neighbor column over the block does.
        let solids = [IVec3::new(3, 0, 3)];
        let block = scene(&solids);
        assert_eq!(
            resolve_walkable(&block, IVec3::new(4, 1, 3), 2),
            Some(IVec3::new(3, 1, 3))
        );
        // Nothing standable anywhere nearby.
        let empty = |_v: IVec3| 0u8;
        assert_eq!(resolve_walkable(&empty, IVec3::new(0, 5, 0), 4), None);
    }

    #[test]
    fn path_across_a_flat_floor_is_a_straight_line() {
        let world = floor_world(0);
        let start = IVec3::new(0, 1, 0);
        let goal = IVec3::new(5, 1, 0);
        match find_path(&world, start, goal, 1000) {
            PathResult::Path(p) => {
                assert_eq!(p.len(), 6, "manhattan-straight path: {p:?}");
                assert_eq!(p[0], start);
                assert_eq!(*p.last().unwrap(), goal);
            }
            other => panic!("expected path, got {other:?}"),
        }
    }

    #[test]
    fn path_climbs_a_step_and_wont_scale_a_wall() {
        // Floor at y=0 with a one-block step up to a plateau (y=1 solid) for
        // x >= 3: agents jump up onto it.
        let world = |v: IVec3| u8::from(v.y == 0 || (v.y == 1 && v.x >= 3));
        let start = IVec3::new(0, 1, 0);
        let goal = IVec3::new(5, 2, 0);
        let PathResult::Path(p) = find_path(&world, start, goal, 1000) else {
            panic!("step should be climbable");
        };
        assert_eq!(*p.last().unwrap(), goal);
        // A 3-high cliff for x >= 3 is not climbable (jumps are 1-up only).
        let wall =
            bounded(|v: IVec3| u8::from(v.y == 0 || ((1..=3).contains(&v.y) && v.x >= 3)), 10);
        let high = IVec3::new(5, 4, 0);
        assert_eq!(find_path(&wall, start, high, 5000), PathResult::NoPath);
    }

    #[test]
    fn path_drops_off_ledges_but_not_cliffs() {
        // A plateau (solid y <= 2 for x < 3) two voxels above a floor
        // (y = 0): dropping down is allowed (2 <= MAX_FALL)...
        let world = bounded(
            |v: IVec3| u8::from((v.x < 3 && (0..=2).contains(&v.y)) || (v.x >= 3 && v.y == 0)),
            10,
        );
        let start = IVec3::new(0, 3, 0);
        let goal = IVec3::new(6, 1, 0);
        let PathResult::Path(p) = find_path(&world, start, goal, 1000) else {
            panic!("2-drop should be pathable");
        };
        assert_eq!(*p.last().unwrap(), goal);
        // ...but the reverse trip can't jump 2 up.
        assert_eq!(find_path(&world, goal, start, 5000), PathResult::NoPath);
        // A drop deeper than MAX_FALL is refused outright.
        let cliff = bounded(
            |v: IVec3| {
                u8::from(
                    (v.x < 3 && (0..=MAX_FALL + 1).contains(&v.y)) || (v.x >= 3 && v.y == 0),
                )
            },
            10,
        );
        let top = IVec3::new(0, MAX_FALL + 2, 0);
        assert_eq!(find_path(&cliff, top, goal, 5000), PathResult::NoPath);
    }

    #[test]
    fn path_routes_around_a_wall_gap() {
        // Floor plus a full-height wall at x=3 with a single gap at z=5.
        let world = |v: IVec3| {
            u8::from(v.y == 0 || (v.x == 3 && v.y >= 1 && v.y <= 4 && v.z != 5))
        };
        let start = IVec3::new(0, 1, 0);
        let goal = IVec3::new(6, 1, 0);
        let PathResult::Path(p) = find_path(&world, start, goal, 4000) else {
            panic!("gap should be findable");
        };
        assert!(p.contains(&IVec3::new(3, 1, 5)), "path must thread the gap: {p:?}");
    }

    #[test]
    fn unreachable_goal_is_no_path_and_budget_caps_work() {
        // Goal sealed in a box on a shared floor.
        let mut solids = vec![];
        let g = IVec3::new(5, 1, 5);
        for y in 0..=3 {
            for dz in -1..=1 {
                for dx in -1..=1 {
                    if (dx != 0 || dz != 0) || y == 0 || y == 3 {
                        solids.push(IVec3::new(5 + dx, y, 5 + dz));
                    }
                }
            }
        }
        // The box floor: keep g standing.
        solids.push(g - UP);
        let boxed = scene(&solids);
        // A shared (finite) floor outside the box.
        let world =
            bounded(|v: IVec3| if v.y == 0 { 1 } else { boxed.voxel(v) }, 12);
        assert_eq!(find_path(&world, IVec3::new(0, 1, 0), g, 10_000), PathResult::NoPath);
        // On an unbounded floor, a distant goal exhausts a small budget.
        let flat = floor_world(0);
        assert_eq!(
            find_path(&flat, IVec3::new(0, 1, 0), IVec3::new(200, 1, 0), 50),
            PathResult::Budget
        );
    }

    #[test]
    fn paths_are_step_valid() {
        // Every consecutive pair in a produced path must be one legal move:
        // lateral, 1-up, or a fall of at most MAX_FALL.
        let world = |v: IVec3| {
            u8::from(v.y == 0 || (v.y == 1 && v.x >= 4 && v.x <= 6) || (v.x == 2 && v.y == 1 && v.z != 3))
        };
        let PathResult::Path(p) =
            find_path(&world, IVec3::new(0, 1, 0), IVec3::new(8, 1, 6), 5000)
        else {
            panic!("expected a path");
        };
        for w in p.windows(2) {
            let d = w[1] - w[0];
            let lateral = d.x.abs() + d.z.abs();
            assert_eq!(lateral, 1, "one lateral cell per move: {d:?}");
            assert!(d.y <= 1 && d.y >= -MAX_FALL, "vertical move out of range: {d:?}");
            assert!(walkable(&world, w[1]), "path visits non-walkable cell {:?}", w[1]);
        }
    }
}
