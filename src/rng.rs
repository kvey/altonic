//! Deterministic PRNG: Xoshiro256** from `rand_xoshiro`, sampled through
//! `rand`/`rand_distr`.
//!
//! All generator randomness flows through this type, seeded by HMAC digests
//! (see SPEC §5.1). Everything is a pure function of the seed — no OS entropy,
//! no time — which is what makes a masking run reproducible.

use rand::seq::{IndexedRandom, SliceRandom};
use rand::{Rng as _, RngExt as _, SeedableRng};
use rand_distr::StandardNormal;
use rand_xoshiro::Xoshiro256StarStar;

pub struct Rng {
    inner: Xoshiro256StarStar,
}

impl Rng {
    pub fn from_seed(seed: u64) -> Self {
        Rng { inner: Xoshiro256StarStar::seed_from_u64(seed) }
    }

    /// Seed from a 32-byte HMAC digest, using all 256 bits of state.
    pub fn from_digest(d: &[u8; 32]) -> Self {
        Rng { inner: Xoshiro256StarStar::from_seed(*d) }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }

    /// Uniform in [0, bound). bound must be > 0.
    pub fn below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0);
        self.inner.random_range(0..bound)
    }

    /// Uniform integer in [lo, hi] inclusive.
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        self.inner.random_range(lo..=hi)
    }

    /// Uniform float in [0, 1) with 53 bits of precision.
    pub fn f64(&mut self) -> f64 {
        self.inner.random()
    }

    /// Uniform float in [lo, hi). Computed from `f64()` rather than sampled
    /// directly so that an empty range (lo == hi) yields lo instead of panicking.
    pub fn range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f64()
    }

    pub fn bool(&mut self) -> bool {
        self.inner.random()
    }

    /// Standard normal.
    pub fn gaussian(&mut self) -> f64 {
        self.inner.sample(StandardNormal)
    }

    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        debug_assert!(!items.is_empty());
        items.choose(&mut self.inner).expect("choose from a non-empty slice")
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        items.shuffle(&mut self.inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = Rng::from_seed(42);
        let mut b = Rng::from_seed(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = Rng::from_seed(43);
        assert_ne!(Rng::from_seed(42).next_u64(), c.next_u64());
    }

    fn draws(seed: &[u8; 32]) -> Vec<u64> {
        let mut r = Rng::from_digest(seed);
        (0..8).map(|_| r.next_u64()).collect()
    }

    /// Every byte of the digest has to reach the state — a digest differing in
    /// one bit must not produce the same stream. (Xoshiro's first output alone
    /// reflects only one state word, so compare a run of draws.)
    #[test]
    fn digest_seeding_uses_the_whole_digest() {
        let zero = [0u8; 32];
        for byte in 0..32 {
            let mut other = zero;
            other[byte] = 1;
            assert_ne!(draws(&zero), draws(&other), "byte {byte} did not affect the stream");
        }
    }

    /// Xoshiro is stuck at zero if seeded all-zero; the driver remaps that
    /// seed, and this pins the behaviour we rely on.
    #[test]
    fn all_zero_digest_is_not_a_dead_stream() {
        let stream = draws(&[0u8; 32]);
        assert!(stream.iter().any(|&v| v != 0), "all-zero seed produced a dead stream");
    }

    #[test]
    fn bounds_respected() {
        let mut r = Rng::from_seed(7);
        for _ in 0..10_000 {
            let v = r.range_i64(-5, 5);
            assert!((-5..=5).contains(&v));
            let f = r.f64();
            assert!((0.0..1.0).contains(&f));
            let b = r.below(3);
            assert!(b < 3);
        }
    }

    #[test]
    fn degenerate_float_range() {
        let mut r = Rng::from_seed(3);
        assert_eq!(r.range_f64(2.5, 2.5), 2.5);
    }

    #[test]
    fn below_covers_all_values() {
        let mut r = Rng::from_seed(1);
        let mut seen = [false; 10];
        for _ in 0..1000 {
            seen[r.below(10) as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn gaussian_moments() {
        let mut r = Rng::from_seed(9);
        let n = 50_000;
        let samples: Vec<f64> = (0..n).map(|_| r.gaussian()).collect();
        let mean = samples.iter().sum::<f64>() / n as f64;
        let var = samples.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((var - 1.0).abs() < 0.05, "var {var}");
    }
}
