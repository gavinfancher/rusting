//! The learnable bot: a linear scoring function over hand-crafted features.
//!
//! OWNER: Agent G (learning engine). For every candidate card, compute the
//! features listed in `types::FEATURE_NAMES` (as small integers, documented
//! here), score = Σ weight × feature, and pick the highest (ties: lowest
//! card index, so it's deterministic). Must be allocation-free and fast:
//! it's in the hot loop of every training tournament.

use crate::card::{Card, CardSet};
use crate::types::{PassDirection, PlayerView, Weights};

/// Pick a card to play from `legal_moves(view)`.
pub fn linear_play(w: &Weights, view: &PlayerView) -> Card {
    let _ = (w, view);
    todo!("Agent G: implement linear_play")
}

/// Pick 3 cards from `hand` to pass.
pub fn linear_pass(w: &Weights, hand: CardSet, dir: PassDirection) -> CardSet {
    let _ = (w, hand, dir);
    todo!("Agent G: implement linear_pass")
}

/// Hand-set starting weights that play roughly like `Heuristic` (within
/// ~0.5 avg points per hand of it against 3 heuristic opponents).
/// Training's `init: "heuristic"` starts here.
pub fn default_weights() -> Weights {
    todo!("Agent G: implement default_weights")
}
