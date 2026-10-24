//! PIMC advisor: estimate expected points for each legal card.

use crate::types::{MoveTotals, PlayerView};
use std::time::Instant;

/// Samples are processed in fixed-size chunks; chunk `k` uses
/// `seed::child_seed(seed, k)`. Chunks, not threads, own the randomness, so
/// results don't depend on the thread count.
pub const SAMPLE_CHUNK: u32 = 64;

/// Run `samples` determinizations for the player in `view`.
///
/// CONTRACT:
/// - One sample = deal the unseen cards randomly to the other seats,
///   respecting `view.voids`, the known passed cards, and each seat's card
///   count; then, for EVERY legal move, play that move and finish the hand
///   with heuristic bots, recording this player's final hand points
///   (moon-shot adjusted). Using the same deal for every move is
///   deliberate: it reduces noise between moves.
/// - Every returned `MoveStat` has `samples == samples_done`.
/// - `deadline`: checked before each chunk starts; when passed, stop and
///   return what's done (`samples_done < samples`). Never panics on a
///   deadline.
/// - Deterministic for a given `(view, samples, seed)` when no deadline is hit.
/// - Runs on the caller's current rayon pool.
pub fn simulate(
    view: &PlayerView,
    samples: u32,
    seed: u64,
    deadline: Option<Instant>,
) -> MoveTotals {
    let _ = (view, samples, seed, deadline);
    todo!("Agent A: implement simulate")
}
