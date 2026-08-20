//! 2D noise functions ported from `crates/soils-terrainlab/noise.glsl` (MIT,
//! © @lumiey), used by [`crate::graph::NodeKind::Noise`] and `FractalNoise`.
//!
//! # Why f32
//!
//! These are **hash-based** noises: the value in a cell is derived from a hash
//! of that cell's integer coordinate. To stay character-identical to the WGSL
//! port in `soils-terrainlab::wgsl_gen` (which is `f32`), **everything here is
//! `f32`** — the graph casts `x`/`z` to `f32` before calling in. The lattice
//! hash ([`ihash`]) works in `u32`, so it is bit-exact and identical on CPU and
//! GPU (see its docs for why we avoid GLSL's `floatBitsToUint` scheme).
//!
//! Each mode's natural `~0..1` GLSL output is remapped to signed `~[-1, 1]` to
//! match the codebase's noise convention (raw noise, scaled downstream by
//! `ScaleBias`); `Gabor`/`Wavelet` are already signed.

use crate::graph::NoiseMode;

// --- tiny vec2 helpers (match WGSL builtin semantics) ---
type V = [f32; 2];
#[inline]
fn floor2(p: V) -> V {
    [p[0].floor(), p[1].floor()]
}
/// `x - floor(x)` — WGSL `fract`, NOT Rust's `f32::fract` (which keeps sign).
#[inline]
fn fract1(x: f32) -> f32 {
    x - x.floor()
}
#[inline]
fn fract2(p: V) -> V {
    [fract1(p[0]), fract1(p[1])]
}
#[inline]
fn dot2(a: V, b: V) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}
#[inline]
fn add(a: V, b: V) -> V {
    [a[0] + b[0], a[1] + b[1]]
}
#[inline]
fn sub(a: V, b: V) -> V {
    [a[0] - b[0], a[1] - b[1]]
}
#[inline]
fn adds(a: V, s: f32) -> V {
    [a[0] + s, a[1] + s]
}
#[inline]
fn subs(a: V, s: f32) -> V {
    [a[0] - s, a[1] - s]
}
#[inline]
fn muls(a: V, s: f32) -> V {
    [a[0] * s, a[1] * s]
}
#[inline]
fn len2(a: V) -> f32 {
    dot2(a, a).sqrt()
}
#[inline]
fn normalize2(a: V) -> V {
    let l = len2(a);
    [a[0] / l, a[1] / l]
}
#[inline]
fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
/// Manual Hermite smoothstep — used for the *reversed-edge* cases (e0 > e1),
/// which WGSL's builtin `smoothstep` leaves undefined, so we spell it out
/// identically on both sides.
#[inline]
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 1 / 2^32, exact in f32.
const INV_U32: f32 = 1.0 / 4294967296.0;

/// Integer avalanche hash of a cell coordinate (murmur3 fmix32 finalizer).
///
/// We deliberately do NOT use `noise.glsl`'s `floatBitsToUint(p * bigConstant)`:
/// the hash callers always pass an *integer* cell coordinate formed as
/// `floor(p) + offset`, and a GPU compiler is free to fma-contract
/// `(floor(p) + offset) * bigConstant`, changing the product's low mantissa bits
/// — which a bit-reinterpret hash amplifies into a completely different value,
/// breaking CPU/GPU agreement. Hashing the integer lattice instead is immune to
/// that (integer ops are exact and identical on both sides) and still yields the
/// uniform per-cell values the noise functions expect.
#[inline]
fn ihash(ix: i32, iy: i32) -> u32 {
    let mut h =
        (ix as u32).wrapping_mul(3242174889).wrapping_add((iy as u32).wrapping_mul(2447445413));
    h ^= h >> 16;
    h = h.wrapping_mul(2246822507);
    h ^= h >> 13;
    h = h.wrapping_mul(3266489909);
    h ^= h >> 16;
    h
}
/// Numerical-Recipes LCG step, to decorrelate extra hash outputs from one seed.
#[inline]
fn lcg(h: u32) -> u32 {
    h.wrapping_mul(1664525).wrapping_add(1013904223)
}

// --- hashes: integer-lattice, `[0, 1]` (mirror the WGSL `nm_*` twins) ---
#[inline]
fn hash12(p: V) -> f32 {
    ihash(p[0] as i32, p[1] as i32) as f32 * INV_U32
}
#[inline]
fn hash22(p: V) -> V {
    let h = ihash(p[0] as i32, p[1] as i32);
    let h2 = lcg(h);
    [h as f32 * INV_U32, h2 as f32 * INV_U32]
}
#[inline]
fn hash32(p: V) -> [f32; 3] {
    let h = ihash(p[0] as i32, p[1] as i32);
    let h2 = lcg(h);
    let h3 = lcg(h2);
    [h as f32 * INV_U32, h2 as f32 * INV_U32, h3 as f32 * INV_U32]
}

// --- noise modes (each returns signed ~[-1, 1]) ---

fn value12(p: V) -> f32 {
    let i = floor2(p);
    let mut f = sub(p, i);
    f = [f[0] * f[0] * (3.0 - 2.0 * f[0]), f[1] * f[1] * (3.0 - 2.0 * f[1])];
    mix(
        mix(hash12(i), hash12(add(i, [1.0, 0.0])), f[0]),
        mix(hash12(add(i, [0.0, 1.0])), hash12(add(i, [1.0, 1.0])), f[0]),
        f[1],
    )
}

fn perlin12(p: V) -> f32 {
    let i = floor2(p);
    let f = sub(p, i);
    let u = [
        f[0] * f[0] * f[0] * (10.0 + f[0] * (6.0 * f[0] - 15.0)),
        f[1] * f[1] * f[1] * (10.0 + f[1] * (6.0 * f[1] - 15.0)),
    ];
    let g = |c: V| normalize2(subs(hash22(add(i, c)), 0.5));
    let a = dot2(g([0.0, 0.0]), sub(f, [0.0, 0.0]));
    let b = dot2(g([1.0, 0.0]), sub(f, [1.0, 0.0]));
    let c = dot2(g([0.0, 1.0]), sub(f, [0.0, 1.0]));
    let d = dot2(g([1.0, 1.0]), sub(f, [1.0, 1.0]));
    mix(mix(a, b, u[0]), mix(c, d, u[0]), u[1]) * 0.7 + 0.5
}

/// Perlin gradient noise + analytic derivative: `[value, d/dx, d/dz]`.
fn perlin12d(p: V) -> [f32; 3] {
    let i = floor2(p);
    let f = fract2(p);
    let u = [
        f[0] * f[0] * f[0] * (f[0] * (f[0] * 6.0 - 15.0) + 10.0),
        f[1] * f[1] * f[1] * (f[1] * (f[1] * 6.0 - 15.0) + 10.0),
    ];
    let du = [
        30.0 * f[0] * f[0] * (f[0] * (f[0] - 2.0) + 1.0),
        30.0 * f[1] * f[1] * (f[1] * (f[1] - 2.0) + 1.0),
    ];
    let grad = |c: V| subs(muls(hash22(add(i, c)), 2.0), 1.0);
    let (ga, gb, gc, gd) =
        (grad([0.0, 0.0]), grad([1.0, 0.0]), grad([0.0, 1.0]), grad([1.0, 1.0]));
    let va = dot2(ga, sub(f, [0.0, 0.0]));
    let vb = dot2(gb, sub(f, [1.0, 0.0]));
    let vc = dot2(gc, sub(f, [0.0, 1.0]));
    let vd = dot2(gd, sub(f, [1.0, 1.0]));
    let value = va + u[0] * (vb - va) + u[1] * (vc - va) + u[0] * u[1] * (va - vb - vc + vd);
    // derivative = g_a + u.x*(g_b-g_a) + u.y*(g_c-g_a) + u.x*u.y*(...) + du*(...)
    let k = va - vb - vc + vd;
    let dx = ga[0]
        + u[0] * (gb[0] - ga[0])
        + u[1] * (gc[0] - ga[0])
        + u[0] * u[1] * (ga[0] - gb[0] - gc[0] + gd[0])
        + du[0] * (u[1] * k + vb - va);
    let dz = ga[1]
        + u[0] * (gb[1] - ga[1])
        + u[1] * (gc[1] - ga[1])
        + u[0] * u[1] * (ga[1] - gb[1] - gc[1] + gd[1])
        + du[1] * (u[0] * k + vc - va);
    [value, dx, dz]
}

fn simplex12(p: V) -> f32 {
    let i = floor2(adds(p, (p[0] + p[1]) * 0.366025));
    let a = adds(sub(p, i), (i[0] + i[1]) * 0.211324);
    let m = if a[0] >= a[1] { 1.0 } else { 0.0 };
    let o = [m, 1.0 - m];
    let b = adds(sub(a, o), 0.211324);
    let c = subs(a, 0.577351);
    let h = [
        (0.5 - dot2(a, a)).max(0.0),
        (0.5 - dot2(b, b)).max(0.0),
        (0.5 - dot2(c, c)).max(0.0),
    ];
    let n = [
        h[0] * h[0] * h[0] * h[0] * dot2(a, subs(hash22(i), 0.5)),
        h[1] * h[1] * h[1] * h[1] * dot2(b, subs(hash22(add(i, o)), 0.5)),
        h[2] * h[2] * h[2] * h[2] * dot2(c, subs(hash22(adds(i, 1.0)), 0.5)),
    ];
    (n[0] + n[1] + n[2]) * 70.0 + 0.5
}

fn worley12(mut p: V) -> f32 {
    let i = floor2(p);
    p = sub(p, i);
    let mut w = 1e6f32;
    for x in -1..=1 {
        for y in -1..=1 {
            let g = [x as f32, y as f32];
            let h = hash12(add(i, g));
            let c = [p[0] - g[0] - h, p[1] - g[1] - h];
            w = w.min(dot2(c, c));
        }
    }
    1.0 - w.sqrt()
}

fn voronoi12(x: V, s: f32) -> f32 {
    let s = 1.0 / s.max(1e-3);
    let p = floor2(x);
    let f = sub(x, p);
    let mut va = 0.0;
    let mut wt = 0.0;
    for xi in -1..=1 {
        for yi in -1..=1 {
            let g = [xi as f32, yi as f32];
            let o = hash32(add(p, g));
            let d = len2([g[0] - f[0] + o[0], g[1] - f[1] + o[1]]);
            let ww = smoothstep(1.414, 0.0, d).powf(s);
            va += o[2] * ww;
            wt += ww;
        }
    }
    // Low smoothness raises the exponent so far that every weight can
    // underflow to 0.0; guard the division or the field goes NaN.
    va / wt.max(1e-6)
}

fn gabor12(p: V) -> f32 {
    const KF: f32 = 8.0;
    let i = floor2(p);
    let mut f = sub(p, i);
    f = [f[0] * f[0] * (3.0 - 2.0 * f[0]), f[1] * f[1] * (3.0 - 2.0 * f[1])];
    // Range-reduce before `sin` (identically to the WGSL twin): the argument
    // is world-scale, and GPU `sin` precision is only specified on [-pi, pi].
    let s = |c: V| {
        let a = KF * dot2(p, hash22(add(i, c)));
        (a - (a * 0.15915494).round() * 6.2831855).sin()
    };
    mix(
        mix(s([0.0, 0.0]), s([1.0, 0.0]), f[0]),
        mix(s([0.0, 1.0]), s([1.0, 1.0]), f[0]),
        f[1],
    )
}

fn crater12(p: V) -> f32 {
    let f = fract2(p);
    let p = floor2(p);
    let mut va = 0.0;
    let mut wt = 0.0;
    for i in -2..=2 {
        for j in -2..=2 {
            let g = [i as f32, j as f32];
            let o = hash22(add(p, g));
            let d = len2([f[0] - g[0] - o[0], f[1] - g[1] - o[1]]);
            let w = (-4.0 * d).exp();
            va += w * (6.28 * d.max(0.06).sqrt()).sin();
            wt += w;
        }
    }
    (va / wt).abs()
}

/// fBm of the 2D Perlin derivative field (used by [`wool12`]), 6 octaves.
fn fbm_wool(mut p: V) -> V {
    let mut s = [0.0, 0.0];
    let mut m = 0.0;
    let mut a = 1.0;
    for _ in 0..6 {
        let nd = perlin12d(p);
        s = add(s, muls([nd[1], nd[2]], a));
        m += a;
        a *= 0.5;
        p = muls(p, 2.0);
    }
    [s[0] / m, s[1] / m]
}

fn wool12(p: V) -> f32 {
    let n = fbm_wool(p);
    n[0].abs().max(n[1].abs())
}

/// fBm summing the full `perlin12d` vec3 (value + derivative), 6 octaves; its
/// `.yz` derivative warps the domain for [`stone12`].
fn fbm_stone(mut p: V) -> [f32; 3] {
    let mut s = [0.0, 0.0, 0.0];
    let mut a = 1.0;
    for _ in 0..6 {
        let nd = perlin12d(p);
        s = [s[0] + a * nd[0], s[1] + a * nd[1], s[2] + a * nd[2]];
        a *= 0.5;
        p = muls(p, 2.0);
    }
    s
}

/// fBm of scalar `perlin12`, `octaves` layers.
fn fbm12(mut p: V, octaves: u32) -> f32 {
    let mut s = 0.0;
    let mut m = 0.0;
    let mut a = 1.0;
    for _ in 0..octaves {
        s += a * perlin12(p);
        m += a;
        a *= 0.5;
        p = muls(p, 2.0);
    }
    s / m
}

fn stone12(p: V) -> f32 {
    let d = fbm_stone(p);
    fbm12(add(p, muls([d[1], d[2]], 0.4)), 6)
}

fn wavelet12(mut p: V, phase: f32) -> f32 {
    const SCALE: f32 = 1.24;
    let (mut d, mut s, mut m) = (0.0f32, 1.0f32, 0.0f32);
    for i in 0..4 {
        let fi = i as f32;
        let q0 = muls(p, s);
        let mut g = fract2([q0[0].floor() * 123.34, q0[1].floor() * 233.53]);
        let gd = g[0] * (g[0] + 23.234) + g[1] * (g[1] + 23.234);
        g = adds(g, gd);
        let a = fract1(g[0] * g[1]) * 1000.0;
        let (ca, sa) = (a.cos(), a.sin());
        let r = subs(fract2(q0), 0.5);
        let q = [r[0] * ca - r[1] * sa, r[0] * sa + r[1] * ca];
        d += (q[0] * 10.0 + phase).sin() * smoothstep(0.25, 0.0, dot2(q, q)) / s;
        p = [0.54 * p[0] - 0.84 * p[1] + fi, 0.84 * p[0] + 0.54 * p[1] + fi];
        m += 1.0 / s;
        s *= SCALE;
    }
    d / m
}

/// Evaluate a single mode at `(x, z)`. `param` is the smoothness for `Voronoi`
/// and the phase for `Wavelet`; ignored by the other modes. Output is signed
/// `~[-1, 1]` (the GLSL `~0..1` modes are remapped `v*2 - 1` here).
pub fn eval_mode(mode: NoiseMode, x: f32, z: f32, param: f32) -> f32 {
    let p = [x, z];
    match mode {
        NoiseMode::Value => value12(p) * 2.0 - 1.0,
        NoiseMode::Perlin => perlin12(p) * 2.0 - 1.0,
        NoiseMode::Simplex => simplex12(p) * 2.0 - 1.0,
        NoiseMode::Worley => worley12(p) * 2.0 - 1.0,
        NoiseMode::Voronoi => voronoi12(p, param) * 2.0 - 1.0,
        NoiseMode::Gabor => gabor12(p),
        NoiseMode::Crater => crater12(p) * 2.0 - 1.0,
        NoiseMode::Wool => wool12(p) * 2.0 - 1.0,
        NoiseMode::Stone => stone12(p) * 2.0 - 1.0,
        NoiseMode::Wavelet => wavelet12(p, param),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mode is deterministic, finite, and roughly within the signed band.
    #[test]
    fn modes_are_finite_and_bounded() {
        for mode in NoiseMode::ALL {
            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            for j in 0..40 {
                for i in 0..40 {
                    let x = (i as f32 - 20.0) * 0.37;
                    let z = (j as f32 - 20.0) * 0.41;
                    let param = 0.5; // smoothness / phase
                    let v = eval_mode(mode, x, z, param);
                    assert!(v.is_finite(), "{mode:?} non-finite at ({x},{z})");
                    // Determinism.
                    assert_eq!(v, eval_mode(mode, x, z, param), "{mode:?} not deterministic");
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            // Loose bound: signed noise should stay well within [-3, 3].
            assert!(lo >= -3.0 && hi <= 3.0, "{mode:?} range [{lo}, {hi}] out of band");
            // And it should actually vary.
            assert!(hi - lo > 0.05, "{mode:?} is nearly constant ([{lo}, {hi}])");
        }
    }

    /// The hashes wrap like the WGSL `u32` port (no debug-overflow panic) and
    /// stay in `[0, 1]`.
    #[test]
    fn hashes_in_unit_range() {
        for k in 0..100 {
            let p = [k as f32 * 12.9898, k as f32 * 78.233];
            let a = hash12(p);
            assert!((0.0..=1.0).contains(&a), "hash12 out of range: {a}");
            let b = hash22(p);
            assert!((0.0..=1.0).contains(&b[0]) && (0.0..=1.0).contains(&b[1]));
            let c = hash32(p);
            assert!(c.iter().all(|v| (0.0..=1.0).contains(v)));
        }
    }
}
