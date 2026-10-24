//! Tournament runner: simulate a range of hands of a job.

use crate::types::{JobSpec, UnitStats};
use std::ops::Range;

/// Simulate hands `range` (hand indices, e.g. `0..2000`) of `spec` and
/// return per-slot totals.
///
/// CONTRACT (other crates depend on this):
/// - Hand `i` is dealt from `seed::hand_seed(spec.seed, i)`; all randomness
///   for that hand (including bot decisions) derives from that seed.
/// - Seating comes from `types::seating(i)` (seat -> slot) and passing from
///   `PassDirection::for_hand(i)`.
/// - Deterministic: the same `(spec, range)` always returns identical stats,
///   regardless of rayon thread count, and splitting a range in two and
///   merging the results gives exactly the same stats as the whole range.
/// - Runs in parallel on the caller's current rayon pool.
/// - Pure CPU; never blocks on I/O. May take seconds to minutes (PIMC).
pub fn run_hands(spec: &JobSpec, range: Range<u64>) -> UnitStats {
    let _ = (spec, range);
    todo!("Agent A: implement run_hands")
}
