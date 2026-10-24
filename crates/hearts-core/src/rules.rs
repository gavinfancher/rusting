//! Hearts rules: dealing, legal moves, trick resolution, scoring, and view
//! validation. All functions are allocation-free `CardSet` bit twiddling.
//!
//! Rules implemented (standard Hearts):
//! - 2♣ leads the first trick; players must follow suit if they can.
//! - First trick: if you cannot follow, hearts and Q♠ are barred when
//!   `Rules::no_points_first_trick` is set, unless your hand has nothing else.
//! - Hearts cannot be led until broken (any heart played, including a
//!   discard) unless the leader holds only hearts. Q♠ does not break hearts.
//! - The highest card of the led suit wins the trick and leads the next.
//! - Hearts are 1 point, Q♠ is 13. Taking all 26 ("shooting the moon")
//!   scores 0 for the shooter and 26 for each opponent.

use crate::card::{Card, CardSet, Suit};
use crate::rng::Rng;
use crate::types::{PlayerView, Seat, Trick};

/// Shuffle a fresh deck (Fisher-Yates) and deal 13 cards to each seat.
pub fn deal(rng: &mut Rng) -> [CardSet; 4] {
    let mut deck: [u8; 52] = std::array::from_fn(|i| i as u8);
    rng.shuffle(&mut deck);
    std::array::from_fn(|s| {
        deck[s * 13..s * 13 + 13]
            .iter()
            .map(|&i| Card::from_index(i))
            .collect()
    })
}

/// Seat that wins a (complete or in-progress) trick: highest card of the led suit.
pub fn trick_winner(trick: &Trick) -> Seat {
    let Some(led) = trick.led_suit() else {
        return trick.leader;
    };
    let mut best_seat = trick.leader;
    let mut best_rank = 0u8;
    for (seat, c) in trick.iter() {
        if c.suit() == led && c.rank() >= best_rank {
            best_rank = c.rank();
            best_seat = seat;
        }
    }
    best_seat
}

/// The seat that took all 26 points, if any.
pub fn moon_shooter(points: [u8; 4]) -> Option<Seat> {
    points.iter().position(|&p| p == 26).map(|s| s as Seat)
}

/// Hand scores after the moon-shot adjustment.
pub fn final_points(points: [u8; 4]) -> [u8; 4] {
    match moon_shooter(points) {
        Some(m) => std::array::from_fn(|s| if s as Seat == m { 0 } else { 26 }),
        None => points,
    }
}

/// Legal cards for the player in `view`:
/// - first trick, leading: must lead 2♣
/// - following: must follow the led suit if possible
/// - first trick, can't follow: no hearts or Q♠ (if `rules.no_points_first_trick`)
///   unless the hand has nothing else
/// - leading: no hearts until broken, unless the hand is only hearts
pub fn legal_moves(view: &PlayerView) -> CardSet {
    let hand = view.hand;
    match view.trick.led_suit() {
        None => {
            // Leading.
            if view.tricks_played == 0 && hand.contains(Card::TWO_OF_CLUBS) {
                return Card::TWO_OF_CLUBS.bit();
            }
            if !view.hearts_broken {
                let non_hearts = hand - Suit::Hearts.mask();
                if !non_hearts.is_empty() {
                    return non_hearts;
                }
            }
            hand
        }
        Some(led) => {
            let following = hand.of_suit(led);
            if !following.is_empty() {
                return following;
            }
            // Void in the led suit: anything goes, except the first-trick rule.
            if view.tricks_played == 0 && view.rules.no_points_first_trick {
                let safe = hand - CardSet::POINT_CARDS;
                if !safe.is_empty() {
                    return safe;
                }
            }
            hand
        }
    }
}

/// Check that a view (e.g. from an advisor HTTP request) is internally
/// consistent: correct card counts for the trick position, no card in two
/// places, voids not contradicted by the player's own hand, seat values < 4.
/// Returns a human-readable reason on failure (sent back as HTTP 400).
///
/// Deliberately lenient about things a hand-typed request may get wrong
/// harmlessly (e.g. `hearts_broken`, or `points` that under-count): only
/// impossible values are rejected.
pub fn validate_view(view: &PlayerView) -> Result<(), String> {
    let err = |m: &str| Err(m.to_string());
    if view.seat >= 4 {
        return err("seat must be 0..=3");
    }
    if view.trick.leader >= 4 {
        return err("trick_leader must be 0..=3");
    }
    if view.trick.is_complete() {
        return err("the current trick already has 4 cards");
    }
    if view.tricks_played > 12 {
        return err("tricks_played must be 0..=12");
    }
    if view.voids.iter().any(|&v| v & !0b1111 != 0) {
        return err("voids must only contain suits C, D, S, H");
    }
    if view.trick.to_play() != view.seat {
        return Err(format!(
            "it is seat {} to play in this trick (leader {}, {} cards played), not seat {}",
            view.trick.to_play(),
            view.trick.leader,
            view.trick.len(),
            view.seat
        ));
    }
    let tp = view.tricks_played as u32;
    if view.hand.len() != 13 - tp {
        return Err(format!(
            "hand has {} cards but {} are expected after {} completed tricks",
            view.hand.len(),
            13 - tp,
            tp
        ));
    }
    if view.played.len() != 4 * tp {
        return Err(format!(
            "played has {} cards but {} are expected after {} completed tricks",
            view.played.len(),
            4 * tp,
            tp
        ));
    }
    let on_table = view.trick.cards();
    if on_table.len() as usize != view.trick.len() {
        return err("the same card appears twice in the current trick");
    }
    if !(view.hand & view.played).is_empty()
        || !(view.hand & on_table).is_empty()
        || !(view.played & on_table).is_empty()
    {
        return err("a card appears in more than one of hand / played / trick");
    }
    if tp == 0 {
        match view.trick.get(0) {
            Some(c) if c != Card::TWO_OF_CLUBS => {
                return err("the first trick must be led with 2C");
            }
            None if !view.hand.contains(Card::TWO_OF_CLUBS) => {
                return err("on the first trick the leader must hold 2C");
            }
            _ => {}
        }
    }
    for suit in Suit::ALL {
        if view.voids[view.seat as usize] & suit.bit() != 0 && view.hand.has_suit(suit) {
            return Err(format!(
                "the player is marked void in {} but holds cards of that suit",
                suit.letter()
            ));
        }
    }
    if let Some((to, cards)) = view.passed {
        if to >= 4 || to == view.seat {
            return err("passed_to must be a seat 0..=3 other than the player");
        }
        if cards.len() != 3 {
            return err("exactly 3 passed cards are required");
        }
        if !(cards & view.hand).is_empty() {
            return err("a passed card is still in the player's hand");
        }
        // Passed cards still unplayed are in the receiver's hand, so the
        // receiver cannot be void in their suit.
        let live = cards - view.played - on_table;
        for suit in Suit::ALL {
            if live.has_suit(suit) && view.voids[to as usize] & suit.bit() != 0 {
                return Err(format!(
                    "seat {to} is marked void in {} but was passed a card of that suit",
                    suit.letter()
                ));
            }
        }
    }
    let total: u32 = view.points.iter().map(|&p| p as u32).sum();
    if view.points.iter().any(|&p| p > 26) || total > 26 {
        return err("points must total at most 26");
    }
    if total > view.played.points() {
        return err("points taken exceed the point cards in completed tricks");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Card {
        s.parse().unwrap()
    }

    fn view(
        hand: &str,
        trick: &[&str],
        leader: Seat,
        tricks_played: u8,
        broken: bool,
    ) -> PlayerView {
        let mut t = Trick::new(leader);
        for s in trick {
            t.push(c(s));
        }
        PlayerView {
            seat: t.to_play(),
            hand: CardSet::parse_list(hand).unwrap(),
            trick: t,
            played: CardSet::EMPTY,
            voids: [0; 4],
            hearts_broken: broken,
            tricks_played,
            points: [0; 4],
            passed: None,
            rules: Default::default(),
        }
    }

    #[test]
    fn two_of_clubs_must_lead() {
        let v = view("2C 5D QS AH", &[], 0, 0, false);
        assert_eq!(legal_moves(&v), c("2C").bit());
    }

    #[test]
    fn must_follow_suit() {
        let v = view("5C 9C 3D QS", &["2C"], 0, 0, false);
        assert_eq!(legal_moves(&v), CardSet::parse_list("5C 9C").unwrap());
    }

    #[test]
    fn no_hearts_lead_until_broken() {
        let v = view("3D 5H 9H", &[], 2, 3, false);
        assert_eq!(legal_moves(&v), c("3D").bit());
        let v = view("3D 5H 9H", &[], 2, 3, true);
        assert_eq!(legal_moves(&v).len(), 3);
        let v = view("5H 9H", &[], 2, 3, false);
        assert_eq!(legal_moves(&v).len(), 2, "only hearts: allowed");
    }

    #[test]
    fn queen_of_spades_lead_is_fine_and_does_not_break() {
        let v = view("QS 5H", &[], 0, 3, false);
        assert_eq!(legal_moves(&v), c("QS").bit());
    }

    #[test]
    fn first_trick_point_rule() {
        let v = view("3D QS AH", &["2C"], 0, 0, false);
        assert_eq!(legal_moves(&v), c("3D").bit());
        let v = view("QS AH 2H", &["2C"], 0, 0, false);
        assert_eq!(legal_moves(&v).len(), 3);
        let mut v = view("3D QS AH", &["2C"], 0, 0, false);
        v.rules.no_points_first_trick = false;
        assert_eq!(legal_moves(&v).len(), 3);
        let v = view("3D QS AH", &["2C"], 0, 1, false);
        assert_eq!(legal_moves(&v).len(), 3);
    }

    #[test]
    fn trick_winner_is_highest_of_led_suit() {
        let mut t = Trick::new(3);
        for s in ["5H", "9H", "AS", "TH"] {
            t.push(c(s));
        }
        // seats: 3:5H 0:9H 1:AS 2:TH -> TH wins -> seat 2
        assert_eq!(trick_winner(&t), 2);
    }

    #[test]
    fn scoring_and_moon() {
        assert_eq!(c("QS").points(), 13);
        assert_eq!(CardSet::FULL_DECK.points(), 26);
        assert_eq!(final_points([5, 8, 13, 0]), [5, 8, 13, 0]);
        assert_eq!(final_points([0, 26, 0, 0]), [26, 0, 26, 26]);
        assert_eq!(moon_shooter([0, 26, 0, 0]), Some(1));
    }

    #[test]
    fn validate_accepts_good_and_rejects_bad() {
        let v = view("2C 5D QS AH 3C 4C 5C 6C 7C 8C 9C TC JC", &[], 0, 0, false);
        assert_eq!(validate_view(&v), Ok(()));
        let mut bad = v;
        bad.seat = 4;
        assert!(validate_view(&bad).is_err());
        let mut bad = v;
        bad.hand = CardSet::parse_list("2C 3C").unwrap();
        assert!(validate_view(&bad).unwrap_err().contains("hand has"));
        let mut bad = v;
        bad.voids[0] = Suit::Clubs.bit();
        assert!(validate_view(&bad).unwrap_err().contains("void"));
        let mut bad = v;
        bad.seat = 2;
        assert!(validate_view(&bad).is_err());
        let mut bad = v;
        bad.passed = Some((1, CardSet::parse_list("2C 3D 4D").unwrap()));
        assert!(validate_view(&bad).is_err(), "passed card still in hand");
        let mut bad = v;
        bad.played = c("3D").bit();
        assert!(validate_view(&bad).is_err());
    }
}
