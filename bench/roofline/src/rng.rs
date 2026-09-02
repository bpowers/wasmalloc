//! Tiny deterministic PRNG (xorshift64*). Deliberately cheap on wasm32: three
//! shift/xor pairs and one i64 multiply, no 128-bit arithmetic.

pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        // A zero state would be a fixed point; the constant is arbitrary.
        Rng(seed | 0x9E37_79B9_7F4A_7C15)
    }

    #[inline(always)]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    /// Uniform in 0..n (Lemire's multiply-shift; the tiny bias is irrelevant here).
    #[inline(always)]
    pub fn below(&mut self, n: u32) -> u32 {
        ((self.next_u32() as u64 * n as u64) >> 32) as u32
    }
}
