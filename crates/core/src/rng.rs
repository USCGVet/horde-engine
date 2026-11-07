//! Stateless randomness: every random number is a hash of what it's for, so parallel tasks never share
//! generator state and results don't depend on the order work runs in.

/// SplitMix64's output function: a fast, well-mixed 64-bit hash.
#[inline]
pub fn hash(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A uniform value in `[0, 1)` for `entity` at `tick`, independent for each `salt`.
#[inline]
pub fn unit(seed: u64, tick: u64, entity: usize, salt: u64) -> f32 {
    let h = hash(seed ^ hash(tick ^ ((entity as u64) << 24) ^ (salt << 56)));
    (h >> 40) as f32 * (1.0 / (1u64 << 24) as f32)
}
