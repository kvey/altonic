//! Deterministic PRNG: SplitMix64 seed expansion feeding Xoshiro256**.
//!
//! All generator randomness flows through this type, seeded by HMAC digests
//! (see SPEC §5.1). Uniform integer sampling uses Lemire's rejection method to
//! avoid modulo bias; Gaussian uses Box–Muller.

pub struct Rng {
    s: [u64; 4],
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

impl Rng {
    pub fn from_seed(seed: u64) -> Self {
        let mut sm = seed;
        let s = [
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
        ];
        Rng { s }
    }

    /// Seed from a 32-byte HMAC digest, using all 256 bits of state.
    pub fn from_digest(d: &[u8; 32]) -> Self {
        let mut s = [0u64; 4];
        for i in 0..4 {
            s[i] = u64::from_be_bytes(d[i * 8..i * 8 + 8].try_into().unwrap());
        }
        // Xoshiro must not be seeded all-zero; SplitMix a fallback (unreachable
        // for HMAC output in practice, but cheap to guard).
        if s == [0, 0, 0, 0] {
            return Rng::from_seed(0);
        }
        Rng { s }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, bound) without modulo bias (Lemire). bound must be > 0.
    pub fn below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0);
        loop {
            let x = self.next_u64();
            let m = (x as u128) * (bound as u128);
            let lo = m as u64;
            if lo >= bound || lo >= (u64::MAX - bound + 1) % bound {
                return (m >> 64) as u64;
            }
        }
    }

    /// Uniform integer in [lo, hi] inclusive.
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        let span = (hi as i128 - lo as i128 + 1) as u128;
        if span > u64::MAX as u128 {
            // Full-ish i64 span: take a raw u64.
            return self.next_u64() as i64;
        }
        lo.wrapping_add(self.below(span as u64) as i64)
    }

    /// Uniform float in [0, 1) with 53 bits of precision.
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    pub fn range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f64()
    }

    pub fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// Standard normal via Box–Muller.
    pub fn gaussian(&mut self) -> f64 {
        let u1 = loop {
            let u = self.f64();
            if u > 0.0 {
                break u;
            }
        };
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        debug_assert!(!items.is_empty());
        &items[self.below(items.len() as u64) as usize]
    }

    /// Fisher–Yates shuffle.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
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
