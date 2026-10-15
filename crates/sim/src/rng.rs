// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! A small seeded random number generator.
//!
//! Written here rather than taken from a crate so that a seed replays the same
//! run on every platform and with any version of the dependencies.

/// xoshiro256**, with its state expanded from the seed by SplitMix64.
#[derive(Clone, Debug)]
pub struct Rng {
    state: [u64; 4],
}

impl Rng {
    /// A generator whose whole output follows from `seed`.
    pub fn new(seed: u64) -> Self {
        let mut mix = seed;
        let mut next = || {
            mix = mix.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = mix;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        Rng {
            state: [next(), next(), next(), next()],
        }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let [s0, s1, s2, s3] = &mut self.state;
        let result = s1.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = *s1 << 17;
        *s2 ^= *s0;
        *s3 ^= *s1;
        *s1 ^= *s2;
        *s0 ^= *s3;
        *s2 ^= t;
        *s3 = s3.rotate_left(45);
        result
    }

    /// A number in `0..n`, without bias. `n` must not be 0.
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0) has no answer");
        let n = n as u64;
        // Lemire's multiply-and-shift, rejecting the few values that would
        // favour small results.
        let threshold = n.wrapping_neg() % n;
        loop {
            let product = u128::from(self.next_u64()) * u128::from(n);
            if (product as u64) >= threshold {
                return (product >> 64) as usize;
            }
        }
    }

    /// A number in `low..high`, which must not be empty.
    pub fn range(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low)
    }

    /// True with the given probability.
    pub fn chance(&mut self, probability: f64) -> bool {
        const SCALE: f64 = 1.0 / (1u64 << 53) as f64;
        ((self.next_u64() >> 11) as f64) * SCALE < probability
    }

    /// One of `items`, which must not be empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_numbers() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let mut c = Rng::new(8);
        let first: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let second: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        let other: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
        assert_eq!(first, second);
        assert_ne!(first, other);
    }

    #[test]
    fn known_output() {
        // Pinned so that a change to the generator, which would change every
        // run, does not go unnoticed.
        let mut rng = Rng::new(0);
        assert_eq!(rng.next_u64(), 0x99ec_5f36_cb75_f2b4);
    }

    #[test]
    fn ranges_stay_in_bounds_and_cover_them() {
        let mut rng = Rng::new(1);
        let mut seen = [0u32; 7];
        for _ in 0..7000 {
            seen[rng.below(7)] += 1;
            let x = rng.range(3, 5);
            assert!((3..5).contains(&x));
        }
        assert!(seen.iter().all(|count| *count > 800), "{seen:?}");
        assert!(!rng.chance(0.0));
        assert!(rng.chance(1.0));
        assert_eq!(*rng.pick(&[42]), 42);
    }
}
