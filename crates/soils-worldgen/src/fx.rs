//! Q16.16 fixed-point math, bit-exact across every CPU and GPU.
//!
//! Worldgen v2 evaluates the terrain graph in this domain so the client, the
//! server, and the GPU gen shader produce byte-identical chunks from a seed.
//! Floats can't do that: f64 vs f32 aside, WGSL permits contraction and
//! reassociation per driver, so identical float source isn't bit-stable.
//! i32/u32 arithmetic is exactly specified everywhere.
//!
//! Every op here has an exact WGSL mirror in [`crate::noise_det::WGSL_PRELUDE`];
//! nothing may be added to one side without the other (the GPU parity test is
//! the gate). Two deliberate spec choices that make the mirror cheap:
//! - [`mul`] truncates toward **zero** (i64 `/ 65536`), because the GPU
//!   emulates the wide product in sign-magnitude 16-bit limbs.
//! - [`int_mul`] wraps at 32 bits on both sides; outside the world coordinate
//!   envelope (|x·f| < 32768, i.e. |x| < ~491k voxels at the highest default
//!   frequency 1/15) terrain is garbage but *identical* garbage.
//!
//! There is deliberately no division, sqrt, or transcendental: graph
//! compilation precomputes inverses, and node kinds that would need more
//! (`Power`, `RadialFalloff`) are rejected by v2 validation until they get a
//! mirrored implementation.

/// Q16.16 fixed-point value.
pub type Fx = i32;

pub const ONE: Fx = 1 << 16;
pub const HALF: Fx = 1 << 15;

/// Quantize an f32 parameter to Fx. Runs at graph-compile time only (never in
/// the sampling hot path); `f64::round` of a finite product is exact and
/// platform-independent, so both ends quantize identically.
pub fn from_f32(v: f32) -> Fx {
    ((v as f64) * 65536.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as Fx
}

pub fn to_f32(v: Fx) -> f32 {
    v as f32 / 65536.0
}

/// `(a * b) / 2^16`, truncating toward zero.
#[inline]
pub fn mul(a: Fx, b: Fx) -> Fx {
    ((a as i64 * b as i64) / 65536) as Fx
}

/// `x * f` where `x` is a plain integer (world voxel coordinate) and `f` is
/// Fx: the product is already Q16.16. Wraps at 32 bits (see module docs).
#[inline]
pub fn int_mul(x: i32, f: Fx) -> Fx {
    x.wrapping_mul(f)
}

/// Largest integer <= v, as an integer (arithmetic shift).
#[inline]
pub fn floor(v: Fx) -> i32 {
    v >> 16
}

/// Fractional part in [0, ONE); correct for negatives (`v - (floor(v) << 16)`).
#[inline]
pub fn frac(v: Fx) -> Fx {
    v & 0xffff
}

#[inline]
pub fn abs(v: Fx) -> Fx {
    v.wrapping_abs()
}

#[inline]
pub fn clamp(v: Fx, lo: Fx, hi: Fx) -> Fx {
    v.max(lo).min(hi)
}

/// `a + (b - a) * t`. Wrapping adds so out-of-envelope inputs stay
/// deterministic (WGSL i32 arithmetic wraps; debug Rust must not panic).
#[inline]
pub fn lerp(a: Fx, b: Fx, t: Fx) -> Fx {
    a.wrapping_add(mul(b.wrapping_sub(a), t))
}

/// Round to nearest integer, half away from zero.
#[inline]
pub fn round(v: Fx) -> i32 {
    if v >= 0 { (v + HALF) >> 16 } else { -((-v + HALF) >> 16) }
}

/// Quintic fade `t³(t(6t - 15) + 10)` for `t` in [0, ONE).
#[inline]
pub fn fade(t: Fx) -> Fx {
    // Intermediates stay small: t <= ONE, |6t - 15| <= 15, |t(6t-15)+10| <= 10.
    let t2 = mul(t, t);
    let t3 = mul(t2, t);
    let a = mul(t, 6 * ONE) - 15 * ONE;
    mul(t3, mul(t, a) + 10 * ONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_matches_reference() {
        assert_eq!(mul(ONE, ONE), ONE);
        assert_eq!(mul(3 * ONE, -2 * ONE), -6 * ONE);
        assert_eq!(mul(HALF, HALF), ONE / 4);
        // Truncation toward zero, not toward negative infinity.
        assert_eq!(mul(-1, 1), 0);
        assert_eq!(mul(-3, ONE), -3);
        assert_eq!(mul(-HALF, 1), 0);
    }

    #[test]
    fn int_mul_is_plain_product_in_range() {
        assert_eq!(int_mul(100, from_f32(0.25)), 25 * ONE);
        assert_eq!(int_mul(-15, ONE), -15 * ONE);
    }

    #[test]
    fn floor_frac_round() {
        assert_eq!(floor(from_f32(2.75)), 2);
        assert_eq!(floor(from_f32(-2.25)), -3);
        assert_eq!(frac(from_f32(-2.25)), from_f32(0.75));
        assert_eq!(round(from_f32(2.5)), 3);
        assert_eq!(round(from_f32(-2.5)), -3);
        assert_eq!(round(from_f32(2.4)), 2);
    }

    #[test]
    fn fade_endpoints_midpoint_monotone() {
        assert_eq!(fade(0), 0);
        assert_eq!(fade(ONE), ONE);
        let mid = fade(HALF) as f64 / 65536.0;
        assert!((mid - 0.5).abs() < 0.001, "fade(0.5) = {mid}");
        let mut prev = -1;
        for i in 0..=64 {
            let v = fade(i * (ONE / 64));
            assert!(v >= prev);
            prev = v;
        }
    }

    #[test]
    fn quantization_is_deterministic() {
        assert_eq!(from_f32(1.0), ONE);
        assert_eq!(from_f32(-0.5), -HALF);
        assert_eq!(from_f32(1.0 / 1000.0), 66); // the documented retune drift
        assert_eq!(to_f32(ONE), 1.0);
    }
}
