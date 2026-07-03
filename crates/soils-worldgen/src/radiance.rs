//! Reference implementation of the radiance-cascades global-illumination math,
//! kept pure and unit-tested so the GPU compute shader (`radiance.wgsl`) has a
//! CPU oracle to match — exactly as `greedy.rs` is the oracle for the GPU
//! greedy mesher.
//!
//! # Radiance cascades in one paragraph
//!
//! Global illumination is a hierarchy of *probe grids* ("cascades"). Cascade 0
//! has many probes but each stores only a few coarse directions over a short
//! ray *interval* right next to it; each higher cascade halves probe density
//! per axis (⅛ the probes in 3D) while quadrupling the directions and pushing
//! the ray interval further out. This is the *penumbra hypothesis*: nearby
//! light needs spatial precision, distant light needs angular precision. Each
//! probe/direction traces only its own short interval; a top-down **merge**
//! then telescopes the intervals into a full radiance field at cascade 0.
//!
//! Everything here is deliberately branch-light and uses only operations that
//! translate directly to WGSL, so the shader can be a line-by-line port.

/// RGB radiance plus a visibility (transmittance) term along an interval.
/// `vis == 1.0` means the interval hit nothing (fully open); `vis == 0.0`
/// means it terminated on an opaque surface (or the sky at the top cascade).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Radiance {
    pub rgb: [f32; 3],
    pub vis: f32,
}

impl Radiance {
    pub const OPEN: Radiance = Radiance { rgb: [0.0; 3], vis: 1.0 };

    pub fn terminal(rgb: [f32; 3]) -> Radiance {
        Radiance { rgb, vis: 0.0 }
    }
}

/// Merge a *near* interval with the *far* interval that continues past it
/// (the far one is already-merged radiance from the next cascade up). The near
/// interval occludes the far one by its visibility: this is the core radiance-
/// cascades interval-merge equation.
pub fn merge(near: Radiance, far: Radiance) -> Radiance {
    Radiance {
        rgb: [
            near.rgb[0] + near.vis * far.rgb[0],
            near.rgb[1] + near.vis * far.rgb[1],
            near.rgb[2] + near.vis * far.rgb[2],
        ],
        vis: near.vis * far.vis,
    }
}

/// Number of directions per probe at cascade `c` = `(base_res << c)^2`.
pub fn dir_res(base_res: u32, c: u32) -> u32 {
    base_res << c
}

/// Probe spacing (in voxels) at cascade `c` = `base_spacing << c`.
pub fn probe_spacing(base_spacing: u32, c: u32) -> u32 {
    base_spacing << c
}

/// Ray-interval `[start, end)` (in voxels) for cascade `c`, with interval
/// length doubling each cascade: lengths are `base, 2·base, 4·base, …`, so
/// `start(c) = base·(2^c − 1)` and they telescope with no gaps or overlaps.
pub fn interval(base_len: f32, c: u32) -> (f32, f32) {
    let start = base_len * ((1u32 << c) as f32 - 1.0);
    let end = base_len * ((1u32 << (c + 1)) as f32 - 1.0);
    (start, end)
}

/// Decode an octahedral `(u, v)` in `[0,1]²` to a unit direction on the sphere.
/// The octahedral map is an equal-topology sphere parameterisation that (unlike
/// lat/long) has no pole singularities, so adjacent direction texels stay
/// roughly equal-solid-angle — what the cascade merge's 4:1 angular fold needs.
pub fn octa_decode(u: f32, v: f32) -> [f32; 3] {
    // Map [0,1] -> [-1,1].
    let ox = 2.0 * u - 1.0;
    let oy = 2.0 * v - 1.0;
    let z = 1.0 - ox.abs() - oy.abs();
    let (mut x, mut y) = (ox, oy);
    if z < 0.0 {
        // Fold the lower hemisphere back over the octahedron's edges.
        x = (1.0 - oy.abs()) * ox.signum();
        y = (1.0 - ox.abs()) * oy.signum();
    }
    let len = (x * x + y * y + z * z).sqrt();
    [x / len, y / len, z / len]
}

/// Direction for texel `(ix, iy)` of a `res × res` octahedral grid (texel
/// centres).
pub fn dir_for_texel(ix: u32, iy: u32, res: u32) -> [f32; 3] {
    let u = (ix as f32 + 0.5) / res as f32;
    let v = (iy as f32 + 0.5) / res as f32;
    octa_decode(u, v)
}

/// A dense occupancy + emission grid — the "scene" the reference tracer marches.
/// The GPU uses a 3D texture with the same semantics.
pub struct LightGrid {
    pub size: i32,
    /// `true` where a voxel is opaque.
    pub occ: Vec<bool>,
    /// Per-voxel emitted radiance (0 for non-emitters).
    pub emission: Vec<[f32; 3]>,
}

impl LightGrid {
    pub fn new(size: i32) -> Self {
        let n = (size * size * size) as usize;
        Self { size, occ: vec![false; n], emission: vec![[0.0; 3]; n] }
    }

    fn idx(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        if x < 0 || y < 0 || z < 0 || x >= self.size || y >= self.size || z >= self.size {
            return None;
        }
        Some(((y * self.size + z) * self.size + x) as usize)
    }

    pub fn set_solid(&mut self, x: i32, y: i32, z: i32, emission: [f32; 3]) {
        if let Some(i) = self.idx(x, y, z) {
            self.occ[i] = true;
            self.emission[i] = emission;
        }
    }

    fn sample(&self, x: i32, y: i32, z: i32) -> Option<(bool, [f32; 3])> {
        self.idx(x, y, z).map(|i| (self.occ[i], self.emission[i]))
    }
}

/// March the interval `[t0, t1)` of a ray through the grid by fixed steps of
/// `step` voxels (a simple, GPU-friendly ray-march rather than exact DDA;
/// `step` around 0.5 is plenty for voxel-scale detail). Returns the radiance of
/// the first opaque voxel entered (terminal), or [`Radiance::OPEN`] if the
/// interval clears without hitting anything.
pub fn trace_interval(
    grid: &LightGrid,
    origin: [f32; 3],
    dir: [f32; 3],
    t0: f32,
    t1: f32,
    step: f32,
) -> Radiance {
    let mut t = t0;
    while t < t1 {
        let p = [origin[0] + dir[0] * t, origin[1] + dir[1] * t, origin[2] + dir[2] * t];
        let v = [p[0].floor() as i32, p[1].floor() as i32, p[2].floor() as i32];
        if let Some((solid, emission)) = grid.sample(v[0], v[1], v[2]) {
            if solid {
                return Radiance::terminal(emission);
            }
        }
        t += step;
    }
    Radiance::OPEN
}

/// Hemispherical irradiance at a surface with unit normal `n`, given a function
/// that returns incoming radiance for a direction. Integrates `L·max(0, n·ω)`
/// over a `res × res` octahedral direction set (cosine-weighted, normalised by
/// the summed weights so a uniform white environment integrates back to white).
pub fn gather_irradiance(
    n: [f32; 3],
    res: u32,
    incoming: impl Fn([f32; 3]) -> [f32; 3],
) -> [f32; 3] {
    let mut acc = [0.0f32; 3];
    let mut wsum = 0.0f32;
    for iy in 0..res {
        for ix in 0..res {
            let d = dir_for_texel(ix, iy, res);
            let ndotl = (d[0] * n[0] + d[1] * n[1] + d[2] * n[2]).max(0.0);
            if ndotl <= 0.0 {
                continue;
            }
            let l = incoming(d);
            acc[0] += l[0] * ndotl;
            acc[1] += l[1] * ndotl;
            acc[2] += l[2] * ndotl;
            wsum += ndotl;
        }
    }
    if wsum > 0.0 {
        [acc[0] / wsum, acc[1] / wsum, acc[2] / wsum]
    } else {
        [0.0; 3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: [f32; 3], b: [f32; 3], eps: f32) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() < eps)
    }

    #[test]
    fn merge_occludes_far_behind_near() {
        // A near surface fully blocks whatever is behind it.
        let near = Radiance::terminal([1.0, 0.0, 0.0]);
        let far = Radiance::terminal([0.0, 0.0, 1.0]);
        let m = merge(near, far);
        assert_eq!(m.rgb, [1.0, 0.0, 0.0]);
        assert_eq!(m.vis, 0.0);
    }

    #[test]
    fn merge_passes_far_through_open_near() {
        // An open near interval lets the far radiance through unchanged.
        let m = merge(Radiance::OPEN, Radiance::terminal([0.2, 0.4, 0.6]));
        assert!(approx(m.rgb, [0.2, 0.4, 0.6], 1e-6));
        assert_eq!(m.vis, 0.0);
    }

    #[test]
    fn octahedral_directions_are_unit_and_cover_both_hemispheres() {
        let mut any_up = false;
        let mut any_down = false;
        for iy in 0..8 {
            for ix in 0..8 {
                let d = dir_for_texel(ix, iy, 8);
                let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                assert!((len - 1.0).abs() < 1e-5, "direction not unit length: {len}");
                any_up |= d[1] > 0.3;
                any_down |= d[1] < -0.3;
            }
        }
        assert!(any_up && any_down, "octahedral set must cover the whole sphere");
    }

    #[test]
    fn cascade_intervals_telescope_without_gaps() {
        // end(c) must equal start(c+1) so merged intervals tile [0, reach).
        let base = 2.0;
        for c in 0..4 {
            let (_s, e) = interval(base, c);
            let (s1, _e1) = interval(base, c + 1);
            assert!((e - s1).abs() < 1e-5, "gap between cascade {c} and {}", c + 1);
        }
        assert_eq!(interval(base, 0).0, 0.0);
    }

    #[test]
    fn higher_cascades_have_more_directions_and_sparser_probes() {
        assert_eq!(dir_res(4, 0), 4);
        assert_eq!(dir_res(4, 1), 8); // 4x the directions (8x8 vs 4x4)
        assert_eq!(probe_spacing(2, 0), 2);
        assert_eq!(probe_spacing(2, 1), 4); // half the probe density per axis
    }

    #[test]
    fn ray_hits_emissive_voxel_and_reads_its_light() {
        let mut grid = LightGrid::new(16);
        grid.set_solid(10, 4, 4, [3.0, 0.0, 0.0]); // red lamp down the +x axis
        let origin = [4.5, 4.5, 4.5];
        let hit = trace_interval(&grid, origin, [1.0, 0.0, 0.0], 0.0, 12.0, 0.25);
        assert_eq!(hit.vis, 0.0, "ray should terminate on the lamp");
        assert!(approx(hit.rgb, [3.0, 0.0, 0.0], 1e-4));
    }

    #[test]
    fn ray_misses_when_pointed_away() {
        let mut grid = LightGrid::new(16);
        grid.set_solid(10, 4, 4, [3.0, 0.0, 0.0]);
        let origin = [4.5, 4.5, 4.5];
        // Point -x, away from the lamp: nothing in the interval.
        let miss = trace_interval(&grid, origin, [-1.0, 0.0, 0.0], 0.0, 12.0, 0.25);
        assert_eq!(miss, Radiance::OPEN);
    }

    #[test]
    fn wall_between_probe_and_lamp_blocks_the_light() {
        let mut grid = LightGrid::new(16);
        grid.set_solid(10, 4, 4, [3.0, 0.0, 0.0]); // lamp
        grid.set_solid(7, 4, 4, [0.0, 0.0, 0.0]); // opaque wall in front of it
        let origin = [4.5, 4.5, 4.5];
        let hit = trace_interval(&grid, origin, [1.0, 0.0, 0.0], 0.0, 12.0, 0.25);
        // First opaque voxel entered is the (non-emitting) wall, so no light.
        assert_eq!(hit.vis, 0.0);
        assert!(approx(hit.rgb, [0.0, 0.0, 0.0], 1e-6));
    }

    #[test]
    fn irradiance_of_uniform_white_environment_is_white() {
        // A constant white incoming field must integrate back to ~white,
        // independent of the normal (cosine weights are normalised).
        let irr = gather_irradiance([0.0, 1.0, 0.0], 16, |_d| [1.0, 1.0, 1.0]);
        assert!(approx(irr, [1.0, 1.0, 1.0], 1e-4), "got {irr:?}");
    }

    #[test]
    fn irradiance_faces_the_light() {
        // A lamp directly overhead lights an up-facing surface more than a
        // down-facing one.
        let up = gather_irradiance([0.0, 1.0, 0.0], 16, |d| {
            if d[1] > 0.7 { [5.0, 5.0, 5.0] } else { [0.0; 3] }
        });
        let down = gather_irradiance([0.0, -1.0, 0.0], 16, |d| {
            if d[1] > 0.7 { [5.0, 5.0, 5.0] } else { [0.0; 3] }
        });
        assert!(up[0] > down[0], "up-facing surface should catch overhead light");
        assert!(down[0] < 1e-4, "down-facing surface sees no overhead light");
    }
}
