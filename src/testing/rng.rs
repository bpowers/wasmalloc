//! Deterministic entropy for the model tester.
//!
//! Every decision the model makes (operation kind, size, alignment, victim block) is drawn from an
//! [`Entropy`] source. A seeded [`Rng`] makes a run reproducible from `(seed, op count)`; a
//! [`ByteSource`] over a fuzzer's input makes each mutation of the input a mutation of the
//! operation sequence. Both are a few lines so the crate keeps zero dependencies.

/// A stream of 32-bit values that may run dry.
pub trait Entropy {
    /// Next value, or `None` once the source is exhausted (a PRNG never is).
    fn next_u32(&mut self) -> Option<u32>;

    /// Uniform in `0..n` for non-zero `n`. Lemire's multiply-shift; its bias is far below
    /// anything a test could observe and it needs no rejection loop that could consume an
    /// unbounded amount of a fuzzer's input.
    #[inline]
    fn below(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n > 0);
        self.next_u32()
            .map(|x| ((x as u64 * n as u64) >> 32) as u32)
    }

    /// Uniform in `lo..=hi`; `lo <= hi` and the range must be below `2^32` wide.
    #[inline]
    fn range(&mut self, lo: usize, hi: usize) -> Option<usize> {
        debug_assert!(lo <= hi);
        let width = hi - lo + 1;
        debug_assert!(width <= u32::MAX as usize);
        self.below(width as u32).map(|x| lo + x as usize)
    }

    /// Pick an index with probability proportional to `weights[i]`. The weights must not all be
    /// zero. Entries with weight zero are never chosen.
    #[inline]
    fn weighted(&mut self, weights: &[u32]) -> Option<usize> {
        let total: u32 = weights.iter().sum();
        debug_assert!(total > 0, "all weights are zero");
        let mut x = self.below(total)?;
        for (i, &w) in weights.iter().enumerate() {
            if x < w {
                return Some(i);
            }
            x -= w;
        }
        // Unreachable when `total` is the sum of the weights; kept total so a bad table cannot
        // index out of bounds.
        Some(weights.len() - 1)
    }
}

/// splitmix64 output mixer (Steele, Lea and Flood, "Fast splittable pseudorandom number
/// generators", 2014). A bijection on `u64` with good avalanche, so distinct inputs give
/// distinct, uncorrelated outputs; the block fill patterns are built from it as well.
#[inline]
pub const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// splitmix64: a Weyl sequence through [`mix64`]. Full period, no bad seeds (unlike xorshift,
/// whose zero state is a fixed point), and cheap on wasm32.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Start from `seed`; every seed is valid.
    pub const fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    /// Next 64 random bits.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix64(self.state)
    }
}

impl Entropy for Rng {
    #[inline]
    fn next_u32(&mut self) -> Option<u32> {
        Some((self.next_u64() >> 32) as u32)
    }
}

/// Little-endian `u32`s read off a byte slice; runs dry when fewer than four bytes remain.
#[derive(Clone, Debug)]
pub struct ByteSource<'a> {
    data: &'a [u8],
}

impl<'a> ByteSource<'a> {
    /// Read from `data`.
    pub const fn new(data: &'a [u8]) -> Self {
        ByteSource { data }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.data.len()
    }
}

impl Entropy for ByteSource<'_> {
    #[inline]
    fn next_u32(&mut self) -> Option<u32> {
        let (head, tail) = self.data.split_first_chunk::<4>()?;
        self.data = tail;
        Some(u32::from_le_bytes(*head))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_seed_sensitive() {
        let a: std::vec::Vec<u64> = (0..8).map(|_| 0).collect();
        let mut r1 = Rng::new(42);
        let mut r2 = Rng::new(42);
        let mut r3 = Rng::new(43);
        let v1: std::vec::Vec<u64> = a.iter().map(|_| r1.next_u64()).collect();
        let v2: std::vec::Vec<u64> = a.iter().map(|_| r2.next_u64()).collect();
        let v3: std::vec::Vec<u64> = a.iter().map(|_| r3.next_u64()).collect();
        assert_eq!(v1, v2);
        assert_ne!(v1, v3);
        // The zero seed is not degenerate.
        let mut z = Rng::new(0);
        assert_ne!(z.next_u64(), z.next_u64());
    }

    #[test]
    fn below_and_range_stay_in_bounds() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            assert!(r.below(3).unwrap() < 3);
            let x = r.range(10, 12).unwrap();
            assert!((10..=12).contains(&x));
            assert_eq!(r.range(5, 5).unwrap(), 5);
        }
    }

    #[test]
    fn weighted_never_picks_zero_weight_and_hits_every_positive_weight() {
        let mut r = Rng::new(1);
        let mut hits = [0usize; 4];
        for _ in 0..20_000 {
            hits[r.weighted(&[1, 0, 3, 4]).unwrap()] += 1;
        }
        assert_eq!(hits[1], 0);
        assert!(hits[0] > 0 && hits[2] > hits[0] && hits[3] > hits[2]);
    }

    #[test]
    fn byte_source_runs_dry_on_short_tail() {
        let mut s = ByteSource::new(&[1, 0, 0, 0, 2, 0, 0, 0, 9, 9]);
        assert_eq!(s.next_u32(), Some(1));
        assert_eq!(s.next_u32(), Some(2));
        assert_eq!(s.remaining(), 2);
        assert_eq!(s.next_u32(), None);
        assert_eq!(s.below(10), None);
    }
}
