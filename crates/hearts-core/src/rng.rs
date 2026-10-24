//! A tiny, fast, seedable PRNG (xoshiro256++), hand-written so the engine has
//! no dependency on the `rand` crate's changing API and so the exact random
//! stream is pinned forever (reproducibility is a project contract).
//!
//! Seed derivation, per decision (see also `seed.rs`). With `hs =
//! seed::hand_seed(job_seed, hand_index)`:
//! - deal / shuffle:      `Rng::new(child_seed(hs, 0))`
//! - seat `s`'s bot:      `Rng::new(child_seed(hs, 1 + s))` (one stream per
//!   seat, so one bot's RNG use never perturbs another bot's decisions)
//! - a PIMC decision:     one `next_u64()` drawn from that seat's stream is
//!   the seed of the advisor call (so "hand seed + decision index" is implied
//!   by the stream position).
//! - advisor chunk `k`:   `Rng::new(child_seed(advisor_seed, k))`

use crate::seed::splitmix64;

/// xoshiro256++ generator. 32 bytes of state, `Clone` to fork a stream.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// Expand a 64-bit seed into the 256-bit state with SplitMix64 (the
    /// initialisation recommended by the xoshiro authors).
    pub fn new(seed: u64) -> Rng {
        const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut st = seed;
        let mut s = [0u64; 4];
        for w in &mut s {
            *w = splitmix64(st);
            st = st.wrapping_add(GAMMA);
        }
        Rng { s }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[0].wrapping_add(s[3]).rotate_left(23).wrapping_add(s[0]);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform-ish integer in `0..n` (`n` must be `1..2^32`). Uses the
    /// multiply-shift trick instead of `%` (no division); the bias is below
    /// 2^-32 which is irrelevant for card games.
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0 && n < (1usize << 32));
        (((self.next_u64() >> 32) * n as u64) >> 32) as usize
    }

    /// In-place Fisher-Yates shuffle.
    #[inline]
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::new(0);
        let mut b = Rng::new(0);
        let va: Vec<u64> = (0..3).map(|_| a.next_u64()).collect();
        let vb: Vec<u64> = (0..3).map(|_| b.next_u64()).collect();
        assert_eq!(va, vb);
        assert_ne!(va[0], va[1]);
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    }

    #[test]
    fn below_in_range_and_shuffle_permutes() {
        let mut r = Rng::new(7);
        for n in 1..60 {
            assert!(r.below(n) < n);
        }
        let mut a: Vec<u32> = (0..52).collect();
        r.shuffle(&mut a);
        let mut b = a.clone();
        b.sort();
        assert_eq!(b, (0..52).collect::<Vec<_>>());
        assert_ne!(a, b);
    }
}
