//! Hearts rules: dealing, passing, legal moves, trick resolution, scoring.
//!
//! OWNER: Agent A (engine). Everything here except the public signatures
//! below is up to the engine implementation.

use crate::card::CardSet;
use crate::types::PlayerView;

/// Legal cards for the player in `view`:
/// - first trick, leading: must lead 2♣
/// - following: must follow the led suit if possible
/// - first trick, can't follow: no hearts or Q♠ (if `rules.no_points_first_trick`)
///   unless the hand has nothing else
/// - leading: no hearts until broken, unless the hand is only hearts
pub fn legal_moves(view: &PlayerView) -> CardSet {
    let _ = view;
    todo!("Agent A: implement legal_moves")
}

/// Check that a view (e.g. from an advisor HTTP request) is internally
/// consistent: correct card counts for the trick position, no card in two
/// places, voids not contradicted by the player's own hand, seat values < 4.
/// Returns a human-readable reason on failure (sent back as HTTP 400).
pub fn validate_view(view: &PlayerView) -> Result<(), String> {
    let _ = view;
    todo!("Agent A: implement validate_view")
}
