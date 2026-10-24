//! Deterministic seed derivation.
//!
//! Every random decision in the system is derived from a job's seed, so the
//! same job always produces exactly the same results no matter which worker
//! runs which hands, how many threads it uses, or how many times a unit is
//! retried after a crash. That property is what lets a game day check
//! "standings identical to the control run".

/// SplitMix64: a tiny, high-quality mixing function. Turns related inputs
/// (1, 2, 3, ...) into unrelated-looking outputs.
pub const fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Seed for one indexed child of `parent`: hand `i` of a job, sample chunk
/// `k` of an advisor call, shard `s` of an advisor fan-out, and so on.
pub const fn child_seed(parent: u64, index: u64) -> u64 {
    splitmix64(parent ^ splitmix64(index))
}

/// Seed for hand `hand_index` of a tournament job. CONTRACT: do not change,
/// or old benchmark and game-day results stop being reproducible.
pub const fn hand_seed(job_seed: u64, hand_index: u64) -> u64 {
    child_seed(job_seed, hand_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_and_stable() {
        assert_ne!(hand_seed(42, 0), hand_seed(42, 1));
        assert_ne!(hand_seed(42, 0), hand_seed(43, 0));
        // Pinned value: fails if someone changes the derivation.
        assert_eq!(splitmix64(0), 0xE220_A839_7B1D_CDAF);
    }
}
