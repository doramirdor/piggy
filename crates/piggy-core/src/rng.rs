//! A tiny deterministic pseudo-random generator (xorshift64*) used by the
//! attribution bootstrap.
//!
//! We hand-roll this deliberately: the honesty story depends on the confidence
//! interval being *reproducible* (a fixed seed in tests must give a byte-stable
//! CI), and pulling in `rand` would add a dependency and a non-deterministic
//! default seed for no benefit. The generator is not cryptographic — it only
//! needs a well-distributed stream of indices for resampling.

/// xorshift64* generator. Seed with any non-zero `u64`.
#[derive(Debug, Clone)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// Create a generator from `seed`. A zero seed is remapped to a fixed
    /// non-zero constant (xorshift is degenerate at 0).
    pub fn new(seed: u64) -> Self {
        XorShift64 {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// Next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A uniformly-distributed index in `0..n` (Lemire's multiply-shift; `n` must
    /// be non-zero). Bias is negligible for the small `n` used here.
    pub fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0, "below(0) is undefined");
        let product = (self.next_u64() as u128) * (n as u128);
        (product >> 64) as usize
    }
}
