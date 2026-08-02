//! Deterministic gradient noise over the [`crate::fx`] Q16.16 domain — the
//! worldgen v2 noise core, bit-exact between this Rust implementation and the
//! WGSL mirror in [`WGSL_PRELUDE`] on every platform and GPU driver.
//!
//! Perlin-style lattice gradient noise: corner hashes from iterated PCG
//! (u32 wrapping ops — exactly specified in Rust and WGSL), gradients with
//! components in {-1, 0, 1} so corner dot products are pure adds/subs, quintic
//! fade + lerp through [`fx`]. Character-equivalent to the old f64 simplex
//! (the port precedent: JS → Rust already changed the noise once); outputs are
//! normalized so the observed range roughly matches the old simplex's ±0.75
//! envelope, keeping amplitude/threshold tuning meaningful.

use crate::fx::{self, Fx, ONE};

/// PCG-RXS-M-XS round (Jarzynski & Olano, "Hash Functions for GPU Rendering").
#[inline]
pub fn pcg(v: u32) -> u32 {
    let s = v.wrapping_mul(747796405).wrapping_add(2891336453);
    let w = ((s >> ((s >> 28).wrapping_add(4))) ^ s).wrapping_mul(277803737);
    (w >> 22) ^ w
}

#[inline]
fn h2(seed: u32, x: i32, y: i32) -> u32 {
    pcg(seed ^ pcg((x as u32) ^ pcg(y as u32)))
}

#[inline]
fn h3(seed: u32, x: i32, y: i32, z: i32) -> u32 {
    pcg(seed ^ pcg((x as u32) ^ pcg((y as u32) ^ pcg(z as u32))))
}

/// Dot of one of 8 gradient directions (axes + diagonals) with offset (dx, dy).
#[inline]
fn grad2(h: u32, dx: Fx, dy: Fx) -> Fx {
    match h & 7 {
        0 => dx + dy,
        1 => dy - dx,
        2 => dx - dy,
        3 => -dx - dy,
        4 => dx,
        5 => -dx,
        6 => dy,
        _ => -dy,
    }
}

/// Dot of one of the 12 cube-edge gradient directions with offset (dx, dy, dz).
#[inline]
fn grad3(h: u32, dx: Fx, dy: Fx, dz: Fx) -> Fx {
    match h % 12 {
        0 => dx + dy,
        1 => dy - dx,
        2 => dx - dy,
        3 => -dx - dy,
        4 => dx + dz,
        5 => dz - dx,
        6 => dx - dz,
        7 => -dx - dz,
        8 => dy + dz,
        9 => dz - dy,
        10 => dy - dz,
        _ => -dy - dz,
    }
}

/// Raw-range normalizers, chosen from the measured |max| over 4M samples
/// (see `measure_range` below) so the output envelope is ~±0.75 like the old
/// simplex. Part of the deterministic spec — same constants in WGSL.
pub const NORM2: Fx = 49152; // 0.750: raw 2D |max| measured 1.000 over 4M samples
pub const NORM3: Fx = 49736; // 0.759: raw 3D |max| measured 0.988 over 4M samples

fn noise2_raw(seed: u32, u: Fx, v: Fx) -> Fx {
    let (xi, yi) = (fx::floor(u), fx::floor(v));
    let (fu, fv) = (fx::frac(u), fx::frac(v));
    let d = |cx: i32, cy: i32| grad2(h2(seed, xi + cx, yi + cy), fu - cx * ONE, fv - cy * ONE);
    let (su, sv) = (fx::fade(fu), fx::fade(fv));
    let a = fx::lerp(d(0, 0), d(1, 0), su);
    let b = fx::lerp(d(0, 1), d(1, 1), su);
    fx::lerp(a, b, sv)
}

/// 2D gradient noise at Q16.16 coordinate (u, v). Output ~[-0.75, 0.75].
pub fn noise2(seed: u32, u: Fx, v: Fx) -> Fx {
    fx::mul(noise2_raw(seed, u, v), NORM2)
}

fn noise3_raw(seed: u32, u: Fx, v: Fx, w: Fx) -> Fx {
    let (xi, yi, zi) = (fx::floor(u), fx::floor(v), fx::floor(w));
    let (fu, fv, fw) = (fx::frac(u), fx::frac(v), fx::frac(w));
    let d = |cx: i32, cy: i32, cz: i32| {
        grad3(
            h3(seed, xi + cx, yi + cy, zi + cz),
            fu - cx * ONE,
            fv - cy * ONE,
            fw - cz * ONE,
        )
    };
    let (su, sv, sw) = (fx::fade(fu), fx::fade(fv), fx::fade(fw));
    let x00 = fx::lerp(d(0, 0, 0), d(1, 0, 0), su);
    let x10 = fx::lerp(d(0, 1, 0), d(1, 1, 0), su);
    let x01 = fx::lerp(d(0, 0, 1), d(1, 0, 1), su);
    let x11 = fx::lerp(d(0, 1, 1), d(1, 1, 1), su);
    let y0 = fx::lerp(x00, x10, sv);
    let y1 = fx::lerp(x01, x11, sv);
    fx::lerp(y0, y1, sw)
}

/// 3D gradient noise at Q16.16 coordinate (u, v, w). Output ~[-0.75, 0.75].
pub fn noise3(seed: u32, u: Fx, v: Fx, w: Fx) -> Fx {
    fx::mul(noise3_raw(seed, u, v, w), NORM3)
}

/// WGSL mirror of `fx` + this module. Prepended to generated worldgen shaders;
/// the terrainlab GPU parity test asserts bit-exact equality against the Rust
/// implementations, noise included.
pub const WGSL_PRELUDE: &str = r#"
// Q16.16 fixed point — mirrors soils_worldgen::fx / noise_det exactly.
const FX_ONE: i32 = 65536;
const FX_HALF: i32 = 32768;

// (a * b) / 2^16 truncating toward zero, via sign-magnitude 16-bit limbs.
fn fx_mul(a: i32, b: i32) -> i32 {
    let neg = (a < 0) != (b < 0);
    let ua = u32(select(a, -a, a < 0));
    let ub = u32(select(b, -b, b < 0));
    let ah = ua >> 16u; let al = ua & 0xffffu;
    let bh = ub >> 16u; let bl = ub & 0xffffu;
    let r = ((ah * bh) << 16u) + ah * bl + al * bh + ((al * bl) >> 16u);
    let ri = i32(r);
    return select(ri, -ri, neg);
}

fn fx_floor(v: i32) -> i32 { return v >> 16u; }
fn fx_frac(v: i32) -> i32 { return v & 0xffff; }
fn fx_lerp(a: i32, b: i32, t: i32) -> i32 { return a + fx_mul(b - a, t); }
fn fx_clamp(v: i32, lo: i32, hi: i32) -> i32 { return min(max(v, lo), hi); }
fn fx_abs(v: i32) -> i32 { return select(v, -v, v < 0); }
fn fx_round(v: i32) -> i32 {
    if (v >= 0) { return (v + FX_HALF) >> 16u; }
    return -((-v + FX_HALF) >> 16u);
}

fn fx_fade(t: i32) -> i32 {
    let t2 = fx_mul(t, t);
    let t3 = fx_mul(t2, t);
    let a = fx_mul(t, 6 * FX_ONE) - 15 * FX_ONE;
    return fx_mul(t3, fx_mul(t, a) + 10 * FX_ONE);
}

fn dn_pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

fn dn_h2(seed: u32, x: i32, y: i32) -> u32 {
    return dn_pcg(seed ^ dn_pcg(bitcast<u32>(x) ^ dn_pcg(bitcast<u32>(y))));
}

fn dn_h3(seed: u32, x: i32, y: i32, z: i32) -> u32 {
    return dn_pcg(seed ^ dn_pcg(bitcast<u32>(x) ^ dn_pcg(bitcast<u32>(y) ^ dn_pcg(bitcast<u32>(z)))));
}

fn dn_grad2(h: u32, dx: i32, dy: i32) -> i32 {
    switch (h & 7u) {
        case 0u: { return dx + dy; }
        case 1u: { return dy - dx; }
        case 2u: { return dx - dy; }
        case 3u: { return -dx - dy; }
        case 4u: { return dx; }
        case 5u: { return -dx; }
        case 6u: { return dy; }
        default: { return -dy; }
    }
}

fn dn_grad3(h: u32, dx: i32, dy: i32, dz: i32) -> i32 {
    switch (h % 12u) {
        case 0u: { return dx + dy; }
        case 1u: { return dy - dx; }
        case 2u: { return dx - dy; }
        case 3u: { return -dx - dy; }
        case 4u: { return dx + dz; }
        case 5u: { return dz - dx; }
        case 6u: { return dx - dz; }
        case 7u: { return -dx - dz; }
        case 8u: { return dy + dz; }
        case 9u: { return dz - dy; }
        case 10u: { return dy - dz; }
        default: { return -dy - dz; }
    }
}

const DN_NORM2: i32 = 49152;
const DN_NORM3: i32 = 49736;

fn dn_noise2(seed: u32, u: i32, v: i32) -> i32 {
    let xi = fx_floor(u); let yi = fx_floor(v);
    let fu = fx_frac(u); let fv = fx_frac(v);
    let n00 = dn_grad2(dn_h2(seed, xi, yi), fu, fv);
    let n10 = dn_grad2(dn_h2(seed, xi + 1, yi), fu - FX_ONE, fv);
    let n01 = dn_grad2(dn_h2(seed, xi, yi + 1), fu, fv - FX_ONE);
    let n11 = dn_grad2(dn_h2(seed, xi + 1, yi + 1), fu - FX_ONE, fv - FX_ONE);
    let su = fx_fade(fu); let sv = fx_fade(fv);
    let a = fx_lerp(n00, n10, su);
    let b = fx_lerp(n01, n11, su);
    return fx_mul(fx_lerp(a, b, sv), DN_NORM2);
}

fn dn_noise3(seed: u32, u: i32, v: i32, w: i32) -> i32 {
    let xi = fx_floor(u); let yi = fx_floor(v); let zi = fx_floor(w);
    let fu = fx_frac(u); let fv = fx_frac(v); let fw = fx_frac(w);
    let d000 = dn_grad3(dn_h3(seed, xi, yi, zi), fu, fv, fw);
    let d100 = dn_grad3(dn_h3(seed, xi + 1, yi, zi), fu - FX_ONE, fv, fw);
    let d010 = dn_grad3(dn_h3(seed, xi, yi + 1, zi), fu, fv - FX_ONE, fw);
    let d110 = dn_grad3(dn_h3(seed, xi + 1, yi + 1, zi), fu - FX_ONE, fv - FX_ONE, fw);
    let d001 = dn_grad3(dn_h3(seed, xi, yi, zi + 1), fu, fv, fw - FX_ONE);
    let d101 = dn_grad3(dn_h3(seed, xi + 1, yi, zi + 1), fu - FX_ONE, fv, fw - FX_ONE);
    let d011 = dn_grad3(dn_h3(seed, xi, yi + 1, zi + 1), fu, fv - FX_ONE, fw - FX_ONE);
    let d111 = dn_grad3(dn_h3(seed, xi + 1, yi + 1, zi + 1), fu - FX_ONE, fv - FX_ONE, fw - FX_ONE);
    let su = fx_fade(fu); let sv = fx_fade(fv); let sw = fx_fade(fw);
    let x00 = fx_lerp(d000, d100, su);
    let x10 = fx_lerp(d010, d110, su);
    let x01 = fx_lerp(d001, d101, su);
    let x11 = fx_lerp(d011, d111, su);
    let y0 = fx_lerp(x00, x10, sv);
    let y1 = fx_lerp(x01, x11, sv);
    return fx_mul(fx_lerp(y0, y1, sw), DN_NORM3);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::from_f32;

    /// Prints raw (pre-normalization) range stats; run with
    /// `cargo test -p soils-worldgen measure_range -- --ignored --nocapture`
    /// when retuning NORM2/NORM3.
    #[test]
    #[ignore]
    fn measure_range() {
        let (mut max2, mut max3) = (0i32, 0i32);
        for i in 0..4_000_000u32 {
            let u = (pcg(i) % (4096 * 65536)) as i32 - 2048 * 65536;
            let v = (pcg(i ^ 0x9e3779b9) % (4096 * 65536)) as i32 - 2048 * 65536;
            let w = (pcg(i ^ 0x517cc1b7) % (4096 * 65536)) as i32 - 2048 * 65536;
            max2 = max2.max(noise2_raw(7, u, v).abs());
            max3 = max3.max(noise3_raw(7, u, v, w).abs());
        }
        println!("raw |max|: 2d {} 3d {}", max2 as f64 / 65536.0, max3 as f64 / 65536.0);
    }

    fn golden_samples() -> [Fx; 5] {
        [
            noise2(0, 0, 0),
            noise2(7, from_f32(0.5), from_f32(0.5)),
            noise2(7, from_f32(123.375), from_f32(-45.25)),
            noise3(7, from_f32(1.5), from_f32(2.5), from_f32(3.5)),
            noise3(42, from_f32(-10.125), from_f32(0.625), from_f32(99.875)),
        ]
    }

    /// Prints the goldens; run when a deliberate algorithm change re-pins them.
    #[test]
    #[ignore]
    fn print_golden_values() {
        println!("pcg(0)={} pcg(1)={} samples={:?}", pcg(0), pcg(1), golden_samples());
    }

    #[test]
    fn deterministic_golden_values() {
        // Pinned outputs: any change to hash, gradients, fade, or norms is a
        // WORLDGEN_ALGO_VERSION bump and must be deliberate.
        assert_eq!(pcg(0), 129708002);
        assert_eq!(pcg(1), 2831084092);
        assert_eq!(golden_samples(), GOLDEN, "noise outputs drifted");
    }

    // Captured from the first correct run with the final NORM constants.
    const GOLDEN: [Fx; 5] = [0, -18432, 22920, 18651, 4992];

    #[test]
    fn lattice_points_are_zero() {
        // At integer coordinates every offset fraction is 0, so the axis
        // gradient dots are 0 -> noise must be exactly 0.
        for &(x, y) in &[(0, 0), (5, -3), (-100, 77)] {
            assert_eq!(noise2(9, x * ONE, y * ONE), 0);
            assert_eq!(noise3(9, x * ONE, y * ONE, (x - y) * ONE), 0);
        }
    }

    #[test]
    fn output_range_is_bounded() {
        for i in 0..200_000u32 {
            let u = (pcg(i) % (1024 * 65536)) as i32 - 512 * 65536;
            let v = (pcg(i ^ 1) % (1024 * 65536)) as i32 - 512 * 65536;
            let w = (pcg(i ^ 2) % (1024 * 65536)) as i32 - 512 * 65536;
            let n2 = noise2(3, u, v).abs();
            let n3 = noise3(3, u, v, w).abs();
            assert!(n2 <= from_f32(0.80), "2d out of envelope: {}", fx::to_f32(n2));
            assert!(n3 <= from_f32(0.85), "3d out of envelope: {}", fx::to_f32(n3));
        }
    }

    #[test]
    fn continuity_across_cell_borders() {
        // Step one raw Q16.16 tick across an integer boundary: the value jump
        // must be tiny (fade has zero derivative at the ends).
        for &seed in &[1u32, 99] {
            for &c in &[0i32, 7, -13] {
                let a = noise2(seed, c * ONE - 1, from_f32(0.37));
                let b = noise2(seed, c * ONE + 1, from_f32(0.37));
                assert!((a - b).abs() < 64, "2d jump at x={c}: {a} vs {b}");
                let a3 = noise3(seed, from_f32(0.3), c * ONE - 1, from_f32(0.6));
                let b3 = noise3(seed, from_f32(0.3), c * ONE + 1, from_f32(0.6));
                assert!((a3 - b3).abs() < 64, "3d jump at y={c}");
            }
        }
    }
}
