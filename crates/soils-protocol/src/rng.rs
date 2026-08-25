//! Deterministic pseudo-randomness.
//!
//! Seeded, reproducible, and dependency-free — the same seed gives the same
//! sequence on every platform and every run, which is what tests, the netsim
//! link and the bot's wander all actually need. It is **not** cryptographic;
//! for anything that guards a secret, use a real CSPRNG.
//!
//! Kept here in `soils-protocol` because it is the crate every other one
//! already depends on, and because a handful of ad-hoc xorshifts and LCGs had
//! accumulated in its absence.

/// Deterministic xorshift64\*, with a cached Box-Muller normal.
///
/// ```
/// # use soils_protocol::Rng;
/// let mut a = Rng::new(7);
/// let mut b = Rng::new(7);
/// assert_eq!(a.next_u64(), b.next_u64());
/// ```
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
    spare: Option<f64>,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // 0 is a fixed point of xorshift; substitute an arbitrary nonzero.
        Self { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed }, spare: None }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`, from the top 53 bits — the exactly-representable
    /// range of an f64 mantissa.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[0, n)`. Lemire's multiply-shift without the rejection
    /// step: the bias is on the order of `n / 2^64` and no caller here is
    /// drawing enough samples for it to be observable.
    pub fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform in `[lo, hi)`.
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next_f64() as f32
    }

    /// Standard normal via polar Box-Muller. Generates two at a time; the
    /// second is cached so the cost amortizes.
    pub fn next_normal(&mut self) -> f64 {
        if let Some(z) = self.spare.take() {
            return z;
        }
        loop {
            let u = self.next_f64() * 2.0 - 1.0;
            let v = self.next_f64() * 2.0 - 1.0;
            let s = u * u + v * v;
            if s > 0.0 && s < 1.0 {
                let f = (-2.0 * s.ln() / s).sqrt();
                self.spare = Some(v * f);
                return u * f;
            }
        }
    }
}

/// A stateless, well-mixed `u64 -> u64` — splitmix64's step function.
///
/// For values that must be *recomputed* from their inputs rather than drawn
/// from a stream — a bot's heading at tick `t`, one body's jitter in a pile —
/// where holding a generator would mean holding order-dependent state.
///
/// The golden-ratio increment is not decoration: splitmix64's finalizer alone
/// maps 0 to 0, and a hash that fixes the most common input in the codebase
/// (index 0, tick 0) is a trap.
pub fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_reproduces_and_different_seeds_diverge() {
        let take = |seed| Rng::new(seed).next_u64();
        assert_eq!(take(42), take(42));
        assert_ne!(take(42), take(43));
        // Zero is remapped rather than sticking at zero forever.
        let mut z = Rng::new(0);
        assert_ne!(z.next_u64(), 0);
        assert_ne!(z.next_u64(), 0);
    }

    #[test]
    fn uniforms_stay_in_range_and_look_flat() {
        let mut r = Rng::new(1);
        let n = 100_000;
        let mut bins = [0u32; 10];
        for _ in 0..n {
            let x = r.next_f64();
            assert!((0.0..1.0).contains(&x), "{x}");
            bins[(x * 10.0) as usize] += 1;
        }
        // Each decile should hold a tenth; ±10% of that is a loose bound that
        // still fails on anything actually skewed.
        for (i, c) in bins.iter().enumerate() {
            assert!((*c as f64 - n as f64 / 10.0).abs() < n as f64 / 100.0, "bin {i} = {c}");
        }
    }

    #[test]
    fn below_respects_its_bound() {
        let mut r = Rng::new(9);
        let mut seen = [false; 7];
        for _ in 0..10_000 {
            let v = r.below(7);
            assert!(v < 7);
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|s| *s), "every value in 0..7 should appear");
        assert_eq!(r.below(1), 0, "a bound of 1 admits only 0");
        assert_eq!(r.below(0), 0, "an empty range must not panic");
    }

    #[test]
    fn range_spans_its_interval() {
        let mut r = Rng::new(5);
        let xs: Vec<f32> = (0..10_000).map(|_| r.range(-2.0, 3.0)).collect();
        assert!(xs.iter().all(|x| (-2.0..3.0).contains(x)));
        let mean = xs.iter().sum::<f32>() / xs.len() as f32;
        assert!((mean - 0.5).abs() < 0.1, "mean {mean}");
    }

    #[test]
    fn normals_are_standard() {
        let mut r = Rng::new(3);
        let xs: Vec<f64> = (0..50_000).map(|_| r.next_normal()).collect();
        let n = xs.len() as f64;
        let mean = xs.iter().sum::<f64>() / n;
        let sd = (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((sd - 1.0).abs() < 0.02, "stddev {sd}");
        let within = |k: f64| xs.iter().filter(|x| x.abs() < k).count() as f64 / n;
        assert!((within(1.0) - 0.682).abs() < 0.02, "1σ {}", within(1.0));
        assert!((within(2.0) - 0.954).abs() < 0.02, "2σ {}", within(2.0));
    }

    #[test]
    fn mix_is_stateless_and_avalanches() {
        assert_eq!(mix(12345), mix(12345));
        assert_ne!(mix(0), 0, "the finalizer must not fix zero");
        // Adjacent inputs must not produce adjacent outputs: flipping one bit
        // should change about half of them.
        let flipped: Vec<u32> =
            (0..64).map(|b| (mix(0xDEAD_BEEF) ^ mix(0xDEAD_BEEF ^ (1 << b))).count_ones()).collect();
        let mean = flipped.iter().sum::<u32>() as f64 / 64.0;
        assert!((mean - 32.0).abs() < 3.0, "mean avalanche {mean}");
        assert!(flipped.iter().all(|&c| (16..48).contains(&c)), "{flipped:?}");
    }
}
