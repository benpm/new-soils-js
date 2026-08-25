//! Artificial network conditions: latency, gaussian jitter, and packet loss.
//!
//! Used by the multi-client tests and by the real client (`SOILS_NETSIM=...`)
//! so recordings can be made under a bad link. Deterministic given a seed, so
//! a failing run reproduces exactly.
//!
//! Two rules keep this honest rather than merely destructive:
//!
//! * **Delivery is monotonic.** Jitter varies the gap between arrivals but
//!   never reorders, because the transports this models (WebSocket, and
//!   WebTransport's reliable lane) are ordered streams. Reordering them would
//!   test a network that cannot occur.
//! * **Loss applies only to lanes designed for it** — the caller decides, via
//!   [`Lane`]. Dropping a `Manifest` would strand a chunk forever and prove
//!   nothing; dropping an `Inputs` bundle or a `Snapshot` is exactly what the
//!   redundancy and delta-baseline schemes exist to absorb.

use crate::rng::Rng;
use std::time::{Duration, Instant};

/// Which delivery guarantee a message rides on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Loss-tolerant by design: `Inputs` (each bundles the last 3 frames) and
    /// `Snapshot` (delta-coded against acked baselines). Eligible for drops.
    Unreliable,
    /// Everything else — login, manifests, edit acks, warps. Delayed, never
    /// dropped.
    Reliable,
}

/// A simulated link. One instance per direction; a round trip through two of
/// them costs roughly `2 * latency`.
#[derive(Clone, Debug)]
pub struct NetSim {
    /// Mean one-way delay.
    pub latency: Duration,
    /// Standard deviation of the gaussian jitter added to each delay.
    pub jitter: Duration,
    /// Drop probability in `[0, 1]`, applied to [`Lane::Unreliable`] only.
    pub loss: f64,
    seed: u64,
    rng: Rng,
    /// Delivery instant of the last message, so ordering is preserved.
    last: Option<Instant>,
}

impl NetSim {
    pub fn new(latency: Duration, jitter: Duration, loss: f64, seed: u64) -> Self {
        Self {
            latency,
            jitter,
            loss: loss.clamp(0.0, 1.0),
            seed,
            rng: Rng::new(seed),
            last: None,
        }
    }

    /// Two independent links with these parameters, for the two directions of
    /// one connection. Sharing a single instance would make uplink and
    /// downlink jitter move in lockstep, which no real link does.
    pub fn split_direction(&self) -> (Self, Self) {
        (
            Self::new(self.latency, self.jitter, self.loss, self.seed),
            Self::new(self.latency, self.jitter, self.loss, self.seed ^ 0xA5A5_5A5A_C3C3_3C3C),
        )
    }

    /// Parse `"<latency_ms>,<jitter_ms>,<loss_fraction>"`, e.g. `"80,25,0.02"`
    /// for a jittery 80 ms link losing 2% of unreliable traffic. A trailing
    /// `,<seed>` pins the sequence; without it the seed is 1.
    pub fn parse(spec: &str) -> Option<Self> {
        let f: Vec<&str> = spec.split(',').map(str::trim).collect();
        if !(3..=4).contains(&f.len()) {
            return None;
        }
        // `Duration::from_secs_f64` panics on non-finite or overflowing input,
        // and `>= 0.0` admits both `inf` and absurd magnitudes — so bound them
        // here rather than turning a typo in an env var into a crash.
        const MAX_MS: f64 = 60_000.0;
        let ms = |s: &str| {
            s.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0 && *v <= MAX_MS)
        };
        let loss = f[2].parse::<f64>().ok().filter(|v| v.is_finite())?;
        Some(Self::new(
            Duration::from_secs_f64(ms(f[0])? / 1000.0),
            Duration::from_secs_f64(ms(f[1])? / 1000.0),
            loss,
            f.get(3).map_or(Some(1), |s| s.parse::<u64>().ok())?,
        ))
    }

    /// Read a spec from an environment variable. `None` (or an unparseable
    /// value) leaves the link untouched, so this is strictly opt-in.
    pub fn from_env(var: &str) -> Option<Self> {
        let spec = std::env::var(var).ok()?;
        let sim = Self::parse(&spec);
        if sim.is_none() {
            eprintln!("{var}: ignoring unparseable netsim spec {spec:?}");
        }
        sim
    }

    /// True if this message should be dropped. Always false on the reliable
    /// lane.
    pub fn should_drop(&mut self, lane: Lane) -> bool {
        lane == Lane::Unreliable && self.loss > 0.0 && self.rng.next_f64() < self.loss
    }

    /// How long to hold a message sent at `now` before delivering it. Gaussian
    /// around `latency` with `jitter` standard deviation, clamped so delivery
    /// is never before `now` and never before the previous message's.
    pub fn delay(&mut self, now: Instant) -> Duration {
        let jitter = self.jitter.as_secs_f64() * self.rng.next_normal();
        let secs = (self.latency.as_secs_f64() + jitter).max(0.0);
        let mut at = now + Duration::from_secs_f64(secs);
        if let Some(prev) = self.last
            && at < prev
        {
            at = prev;
        }
        self.last = Some(at);
        at.saturating_duration_since(now)
    }

    /// Whether this link does anything at all.
    pub fn is_noop(&self) -> bool {
        self.latency.is_zero() && self.jitter.is_zero() && self.loss == 0.0
    }
}

impl std::fmt::Display for NetSim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.0}ms ±{:.0}ms, {:.1}% loss",
            self.latency.as_secs_f64() * 1000.0,
            self.jitter.as_secs_f64() * 1000.0,
            self.loss * 100.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs_and_rejects_junk() {
        let s = NetSim::parse("80,25,0.02").unwrap();
        assert_eq!(s.latency, Duration::from_millis(80));
        assert_eq!(s.jitter, Duration::from_millis(25));
        assert!((s.loss - 0.02).abs() < 1e-9);
        assert!(NetSim::parse("80,25,0.02,7").is_some(), "explicit seed");
        assert!(NetSim::parse("80,25").is_none(), "too few fields");
        assert!(NetSim::parse("80,25,0.02,7,9").is_none(), "too many fields");
        assert!(NetSim::parse("abc,25,0.02").is_none(), "non-numeric");
        assert!(NetSim::parse("-5,25,0.02").is_none(), "negative latency");
        // `Duration::from_secs_f64` panics on these rather than erroring, so a
        // typo in an env var must be rejected here, not passed through.
        assert!(NetSim::parse("inf,0,0").is_none(), "infinite latency");
        assert!(NetSim::parse("0,inf,0").is_none(), "infinite jitter");
        assert!(NetSim::parse("1e300,0,0").is_none(), "absurd latency");
        assert!(NetSim::parse("0,0,NaN").is_none(), "NaN loss");
    }

    #[test]
    fn delays_are_gaussian_about_the_configured_latency() {
        let mut s = NetSim::new(Duration::from_millis(100), Duration::from_millis(20), 0.0, 42);
        let now = Instant::now();
        // Sample independently of the monotonic clamp, which would truncate
        // the low tail and bias the mean upward.
        let xs: Vec<f64> = (0..20_000)
            .map(|_| {
                s.last = None;
                s.delay(now).as_secs_f64() * 1000.0
            })
            .collect();
        let n = xs.len() as f64;
        let mean = xs.iter().sum::<f64>() / n;
        let sd = (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        assert!((mean - 100.0).abs() < 1.0, "mean {mean}");
        assert!((sd - 20.0).abs() < 1.0, "stddev {sd}");
        // A gaussian puts ~68% within one sigma and ~95% within two.
        let within = |k: f64| xs.iter().filter(|x| (*x - mean).abs() < k * sd).count() as f64 / n;
        assert!((within(1.0) - 0.682).abs() < 0.02, "1σ {}", within(1.0));
        assert!((within(2.0) - 0.954).abs() < 0.02, "2σ {}", within(2.0));
    }

    #[test]
    fn delay_never_goes_negative_under_heavy_jitter() {
        // Jitter far larger than latency: the gaussian tail goes negative, but
        // a message cannot arrive before it was sent.
        let mut s = NetSim::new(Duration::from_millis(5), Duration::from_millis(50), 0.0, 7);
        let now = Instant::now();
        for _ in 0..10_000 {
            s.last = None;
            assert!(s.delay(now) >= Duration::ZERO);
        }
    }

    #[test]
    fn delivery_order_is_preserved() {
        let mut s = NetSim::new(Duration::from_millis(50), Duration::from_millis(40), 0.0, 3);
        let start = Instant::now();
        let mut prev = start;
        for i in 0..5_000 {
            // Messages are sent at a steady 1 ms cadence.
            let sent = start + Duration::from_millis(i);
            let at = sent + s.delay(sent);
            assert!(at >= prev, "delivery went backwards: {at:?} < {prev:?}");
            prev = at;
        }
    }

    #[test]
    fn loss_matches_the_configured_rate_on_the_unreliable_lane_only() {
        let mut s = NetSim::new(Duration::ZERO, Duration::ZERO, 0.1, 11);
        let n = 100_000;
        let dropped =
            (0..n).filter(|_| s.should_drop(Lane::Unreliable)).count() as f64 / n as f64;
        assert!((dropped - 0.1).abs() < 0.01, "drop rate {dropped}");
        assert!(
            (0..n).all(|_| !s.should_drop(Lane::Reliable)),
            "the reliable lane must never drop"
        );
    }

    #[test]
    fn same_seed_gives_the_same_sequence() {
        let sample = |seed| {
            let mut s = NetSim::new(Duration::from_millis(30), Duration::from_millis(10), 0.2, seed);
            let now = Instant::now();
            (0..200)
                .map(|_| (s.delay(now), s.should_drop(Lane::Unreliable)))
                .collect::<Vec<_>>()
        };
        assert_eq!(sample(99), sample(99), "seeded runs must reproduce");
        assert_ne!(sample(99), sample(100), "different seeds must diverge");
    }

    #[test]
    fn directions_are_independent() {
        let (mut up, mut down) = NetSim::parse("50,15,0.05,4").unwrap().split_direction();
        let now = Instant::now();
        let a: Vec<_> = (0..100).map(|_| up.delay(now)).collect();
        let b: Vec<_> = (0..100).map(|_| down.delay(now)).collect();
        assert_ne!(a, b, "the two directions must not share a jitter stream");
        // Same parameters, though.
        assert_eq!((up.latency, up.jitter, up.loss), (down.latency, down.jitter, down.loss));
    }

    #[test]
    fn noop_detection() {
        assert!(NetSim::parse("0,0,0").unwrap().is_noop());
        assert!(!NetSim::parse("1,0,0").unwrap().is_noop());
        assert!(!NetSim::parse("0,0,0.01").unwrap().is_noop());
    }
}
