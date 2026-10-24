//! Shared data types: the contract between the engine, the coordinator, the
//! worker, and the wire formats (gRPC + JSON). Changing anything here is a
//! contract change; see docs/CONTRACTS.md.

use crate::card::{Card, CardSet, Suit};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Seat at the table, 0..4. Play goes clockwise: seat `s` is followed by
/// `(s + 1) % 4`. "Left" of seat `s` is `s + 1`.
pub type Seat = u8;

pub const fn next_seat(s: Seat) -> Seat {
    (s + 1) % 4
}

/// Which way cards are passed before a hand. Cycles by hand index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PassDirection {
    Left,
    Right,
    Across,
    Hold,
}

impl PassDirection {
    /// CONTRACT: hand `i` passes in direction `i % 4` = Left, Right, Across, Hold.
    pub const fn for_hand(hand_index: u64) -> PassDirection {
        match hand_index % 4 {
            0 => PassDirection::Left,
            1 => PassDirection::Right,
            2 => PassDirection::Across,
            _ => PassDirection::Hold,
        }
    }

    /// Seat that receives the cards passed by `from`; `None` on a hold hand.
    pub const fn target(self, from: Seat) -> Option<Seat> {
        match self {
            PassDirection::Left => Some((from + 1) % 4),
            PassDirection::Right => Some((from + 3) % 4),
            PassDirection::Across => Some((from + 2) % 4),
            PassDirection::Hold => None,
        }
    }
}

/// Every ordering of four slots. Used to rotate who sits where.
const SEATINGS: [[u8; 4]; 24] = [
    [0, 1, 2, 3],
    [0, 1, 3, 2],
    [0, 2, 1, 3],
    [0, 2, 3, 1],
    [0, 3, 1, 2],
    [0, 3, 2, 1],
    [1, 0, 2, 3],
    [1, 0, 3, 2],
    [1, 2, 0, 3],
    [1, 2, 3, 0],
    [1, 3, 0, 2],
    [1, 3, 2, 0],
    [2, 0, 1, 3],
    [2, 0, 3, 1],
    [2, 1, 0, 3],
    [2, 1, 3, 0],
    [2, 3, 0, 1],
    [2, 3, 1, 0],
    [3, 0, 1, 2],
    [3, 0, 2, 1],
    [3, 1, 0, 2],
    [3, 1, 2, 0],
    [3, 2, 0, 1],
    [3, 2, 1, 0],
];

/// CONTRACT: for hand `hand_index`, returns `seat -> strategy slot`.
/// The pass direction cycles every hand and the seating permutation changes
/// every 4 hands, so after 96 hands every slot has sat in every position
/// relative to every other slot with every pass direction. This cancels seat
/// bias in tournament results.
pub const fn seating(hand_index: u64) -> [u8; 4] {
    SEATINGS[((hand_index / 4) % 24) as usize]
}

/// Rule toggles. Defaults are standard Hearts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// No hearts or Q♠ may be played on the first trick (unless the player
    /// has nothing else).
    pub no_points_first_trick: bool,
}

impl Default for Rules {
    fn default() -> Rules {
        Rules {
            no_points_first_trick: true,
        }
    }
}

/// Which bot plays a slot. Text form: `"random"`, `"heuristic"`, `"pimc:200"`
/// (PIMC with 200 samples per decision).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StrategyKind {
    Random,
    Heuristic,
    /// Monte Carlo search on every decision. `samples` is the CPU dial.
    Pimc {
        samples: u32,
    },
}

impl fmt::Display for StrategyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StrategyKind::Random => f.write_str("random"),
            StrategyKind::Heuristic => f.write_str("heuristic"),
            StrategyKind::Pimc { samples } => write!(f, "pimc:{samples}"),
        }
    }
}

impl FromStr for StrategyKind {
    type Err = String;

    fn from_str(s: &str) -> Result<StrategyKind, String> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "random" => Ok(StrategyKind::Random),
            "heuristic" => Ok(StrategyKind::Heuristic),
            _ => {
                let n = s
                    .strip_prefix("pimc:")
                    .and_then(|n| n.parse::<u32>().ok())
                    .filter(|&n| n > 0)
                    .ok_or_else(|| {
                        format!("unknown strategy {s:?} (expected random | heuristic | pimc:N)")
                    })?;
                Ok(StrategyKind::Pimc { samples: n })
            }
        }
    }
}

impl Serialize for StrategyKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for StrategyKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<StrategyKind, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Everything needed to simulate any hand of a tournament job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub seed: u64,
    /// The four competitors. Index = "slot"; results are reported per slot.
    pub strategies: [StrategyKind; 4],
    #[serde(default)]
    pub rules: Rules,
}

/// Results for one strategy slot, summed over many hands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SlotStats {
    /// Total penalty points taken (after moon-shot adjustment). Lower is better.
    pub points: u64,
    pub hands: u64,
    /// Hands where this slot shot the moon.
    pub moons: u64,
    /// Hands where this slot took Q♠.
    pub queens: u64,
    /// Hands where this slot had the lowest score (ties: every tied slot wins).
    pub wins: u64,
}

impl SlotStats {
    pub fn merge(&mut self, o: &SlotStats) {
        self.points += o.points;
        self.hands += o.hands;
        self.moons += o.moons;
        self.queens += o.queens;
        self.wins += o.wins;
    }
}

/// Output of simulating a range of hands. Merging is commutative and
/// associative, so results don't depend on the order hands finish in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UnitStats {
    pub hands: u64,
    /// Indexed by strategy slot (`JobSpec::strategies`), not by seat.
    pub slots: [SlotStats; 4],
}

impl UnitStats {
    pub fn merge(&mut self, o: &UnitStats) {
        self.hands += o.hands;
        for (a, b) in self.slots.iter_mut().zip(o.slots.iter()) {
            a.merge(b);
        }
    }
}

/// The cards played so far in the current trick, in play order starting
/// with `leader`. Fixed-size and `Copy` so it costs nothing to pass around.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Trick {
    pub leader: Seat,
    len: u8,
    cards: [Card; 4],
}

impl Trick {
    pub const fn new(leader: Seat) -> Trick {
        // The filler card is never read: `get` and `iter` stop at `len`.
        Trick {
            leader,
            len: 0,
            cards: [Card::TWO_OF_CLUBS; 4],
        }
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn is_complete(&self) -> bool {
        self.len == 4
    }

    /// Panics if the trick already has 4 cards.
    pub fn push(&mut self, c: Card) {
        assert!(self.len < 4, "trick already complete");
        self.cards[self.len as usize] = c;
        self.len += 1;
    }

    /// `i`-th card played (0 = the lead).
    pub fn get(&self, i: usize) -> Option<Card> {
        (i < self.len()).then(|| self.cards[i])
    }

    /// Seat that played (or will play) the `i`-th card.
    pub const fn seat_of(&self, i: usize) -> Seat {
        (self.leader + i as u8) % 4
    }

    /// Seat whose turn it is (meaningless once complete).
    pub const fn to_play(&self) -> Seat {
        self.seat_of(self.len as usize)
    }

    pub fn led_suit(&self) -> Option<Suit> {
        self.get(0).map(Card::suit)
    }

    /// `(seat, card)` pairs in play order.
    pub fn iter(&self) -> impl Iterator<Item = (Seat, Card)> + '_ {
        self.cards[..self.len()]
            .iter()
            .enumerate()
            .map(|(i, &c)| (self.seat_of(i), c))
    }

    pub fn cards(&self) -> CardSet {
        self.cards[..self.len()].iter().copied().collect()
    }
}

/// What one player legitimately knows when it is their turn. Bots and the
/// advisor receive ONLY this, never the other hands, so they cannot cheat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlayerView {
    pub seat: Seat,
    pub hand: CardSet,
    pub trick: Trick,
    /// Cards in completed tricks this hand (not including `trick`).
    pub played: CardSet,
    /// `voids[s]`: bitmask of `Suit::bit()` for suits seat `s` is known to be
    /// out of (they failed to follow suit earlier).
    pub voids: [u8; 4],
    pub hearts_broken: bool,
    /// Completed tricks this hand, 0..13. 0 means this is the first trick.
    pub tricks_played: u8,
    /// Penalty points taken so far this hand, per seat.
    pub points: [u8; 4],
    /// Cards this player passed and who received them. Those cards are known
    /// to be in that seat's hand unless already played.
    pub passed: Option<(Seat, CardSet)>,
    pub rules: Rules,
}

impl PlayerView {
    /// Cards whose location this player doesn't know: not in hand, not
    /// played, not on the table.
    pub fn unseen(&self) -> CardSet {
        CardSet::FULL_DECK - self.hand - self.played - self.trick.cards()
    }
}

/// Advisor statistics for one candidate card.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MoveStat {
    pub card: Card,
    /// Sum over samples of the points this player ended the hand with.
    pub total_points: u64,
    pub samples: u32,
}

impl MoveStat {
    /// Expected points if this card is played. Lower is better.
    pub fn mean(&self) -> f64 {
        if self.samples == 0 {
            f64::NAN
        } else {
            self.total_points as f64 / self.samples as f64
        }
    }
}

/// Advisor output. Shard results from different workers are combined with
/// [`MoveTotals::merge`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveTotals {
    /// One entry per legal move, sorted by card.
    pub moves: Vec<MoveStat>,
    /// Number of determinizations completed (may be < requested if a
    /// deadline was hit).
    pub samples_done: u32,
}

impl MoveTotals {
    pub fn merge(&mut self, other: &MoveTotals) {
        for m in &other.moves {
            match self.moves.iter_mut().find(|x| x.card == m.card) {
                Some(x) => {
                    x.total_points += m.total_points;
                    x.samples += m.samples;
                }
                None => self.moves.push(*m),
            }
        }
        self.moves.sort_by_key(|m| m.card);
        self.samples_done += other.samples_done;
    }

    /// Card with the lowest expected points.
    pub fn best(&self) -> Option<Card> {
        self.moves
            .iter()
            .filter(|m| m.samples > 0)
            .min_by(|a, b| a.mean().total_cmp(&b.mean()))
            .map(|m| m.card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_text_round_trips() {
        for s in ["random", "heuristic", "pimc:200"] {
            assert_eq!(s.parse::<StrategyKind>().unwrap().to_string(), s);
        }
        assert!("pimc:0".parse::<StrategyKind>().is_err());
        assert!("greedy".parse::<StrategyKind>().is_err());
    }

    #[test]
    fn job_spec_json() {
        let json = r#"{"seed":42,"strategies":["heuristic","heuristic","random","pimc:50"]}"#;
        let spec: JobSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.strategies[3], StrategyKind::Pimc { samples: 50 });
        assert!(spec.rules.no_points_first_trick);
    }

    #[test]
    fn seating_covers_everyone() {
        for i in 0..96 {
            let mut s = seating(i);
            s.sort();
            assert_eq!(s, [0, 1, 2, 3]);
        }
    }

    #[test]
    fn trick_order() {
        let mut t = Trick::new(2);
        t.push("5H".parse().unwrap());
        t.push("9H".parse().unwrap());
        assert_eq!(t.to_play(), 0);
        assert_eq!(t.led_suit(), Some(Suit::Hearts));
        let seats: Vec<_> = t.iter().map(|(s, _)| s).collect();
        assert_eq!(seats, vec![2, 3]);
    }

    #[test]
    fn move_totals_merge() {
        let c1: Card = "2C".parse().unwrap();
        let c2: Card = "3C".parse().unwrap();
        let mut a = MoveTotals {
            moves: vec![MoveStat {
                card: c1,
                total_points: 10,
                samples: 5,
            }],
            samples_done: 5,
        };
        let b = MoveTotals {
            moves: vec![
                MoveStat {
                    card: c1,
                    total_points: 0,
                    samples: 5,
                },
                MoveStat {
                    card: c2,
                    total_points: 50,
                    samples: 5,
                },
            ],
            samples_done: 5,
        };
        a.merge(&b);
        assert_eq!(a.samples_done, 10);
        assert_eq!(a.moves[0].mean(), 1.0);
        assert_eq!(a.best(), Some(c1));
    }
}
