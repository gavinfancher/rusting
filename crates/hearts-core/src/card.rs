//! Cards and sets of cards.
//!
//! A `Card` is a single byte (0..52) and a `CardSet` is a `u64` bitmask where
//! bit `i` set means "card `i` is in the set". This makes the hot operations in
//! the simulator (does this hand contain a heart? remove a card, count cards)
//! single CPU instructions instead of searching a `Vec`.
//!
//! Index layout: `index = suit * 13 + rank`, with suits ordered
//! Clubs(0) Diamonds(1) Spades(2) Hearts(3) and ranks 0 = Two … 12 = Ace.
//!
//! Text notation (used in JSON and gRPC): rank char + suit char, e.g. `"QS"`,
//! `"TH"` (ten of hearts), `"2C"`. Parsing also accepts lowercase and `"10H"`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::ops::{BitAnd, BitOr, Not, Sub};
use std::str::FromStr;

/// The four suits. `#[repr(u8)]` pins each variant to the given number so we
/// can do arithmetic with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Suit {
    Clubs = 0,
    Diamonds = 1,
    Spades = 2,
    Hearts = 3,
}

impl Suit {
    pub const ALL: [Suit; 4] = [Suit::Clubs, Suit::Diamonds, Suit::Spades, Suit::Hearts];

    pub const fn index(self) -> usize {
        self as usize
    }

    /// Inverse of [`Suit::index`]. Only the low two bits are used.
    pub const fn from_index(i: usize) -> Suit {
        match i & 3 {
            0 => Suit::Clubs,
            1 => Suit::Diamonds,
            2 => Suit::Spades,
            _ => Suit::Hearts,
        }
    }

    /// All 13 cards of this suit as a set.
    pub const fn mask(self) -> CardSet {
        CardSet(0x1FFF_u64 << (13 * self as u64))
    }

    /// One bit per suit, used for "known void" bitmasks (`PlayerView::voids`).
    pub const fn bit(self) -> u8 {
        1 << self as u8
    }

    pub const fn letter(self) -> char {
        match self {
            Suit::Clubs => 'C',
            Suit::Diamonds => 'D',
            Suit::Spades => 'S',
            Suit::Hearts => 'H',
        }
    }

    pub fn from_letter(c: char) -> Option<Suit> {
        match c.to_ascii_uppercase() {
            'C' => Some(Suit::Clubs),
            'D' => Some(Suit::Diamonds),
            'S' => Some(Suit::Spades),
            'H' => Some(Suit::Hearts),
            _ => None,
        }
    }
}

const RANK_CHARS: [char; 13] = [
    '2', '3', '4', '5', '6', '7', '8', '9', 'T', 'J', 'Q', 'K', 'A',
];

/// A single playing card. The inner byte is private so every `Card` is
/// guaranteed valid (0..52).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Card(u8);

impl Card {
    pub const TWO_OF_CLUBS: Card = Card::new(Suit::Clubs, 0);
    pub const QUEEN_OF_SPADES: Card = Card::new(Suit::Spades, 10);

    /// `rank`: 0 = Two … 12 = Ace. Panics (at compile time in const contexts)
    /// if `rank > 12`.
    pub const fn new(suit: Suit, rank: u8) -> Card {
        assert!(rank < 13, "rank must be 0..13");
        Card(suit as u8 * 13 + rank)
    }

    /// Panics if `i >= 52`.
    pub const fn from_index(i: u8) -> Card {
        assert!(i < 52, "card index must be 0..52");
        Card(i)
    }

    pub const fn index(self) -> u8 {
        self.0
    }

    pub const fn suit(self) -> Suit {
        Suit::from_index((self.0 / 13) as usize)
    }

    /// 0 = Two … 12 = Ace. Higher rank wins a trick within the led suit.
    pub const fn rank(self) -> u8 {
        self.0 % 13
    }

    /// Penalty points for taking this card: hearts are 1, Q♠ is 13.
    pub const fn points(self) -> u8 {
        if self.0 == Card::QUEEN_OF_SPADES.0 {
            13
        } else if self.0 / 13 == Suit::Hearts as u8 {
            1
        } else {
            0
        }
    }

    /// The set containing only this card.
    pub const fn bit(self) -> CardSet {
        CardSet(1 << self.0)
    }
}

impl fmt::Display for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}",
            RANK_CHARS[self.rank() as usize],
            self.suit().letter()
        )
    }
}

// Debug prints the same short form so test failures read like "QS" not "Card(36)".
impl fmt::Debug for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Returned when a string isn't valid card notation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCardError(pub String);

impl fmt::Display for ParseCardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid card {:?} (expected e.g. \"QS\", \"TH\", \"2C\")",
            self.0
        )
    }
}

impl std::error::Error for ParseCardError {}

impl FromStr for Card {
    type Err = ParseCardError;

    fn from_str(s: &str) -> Result<Card, ParseCardError> {
        let err = || ParseCardError(s.to_string());
        let t = s.trim();
        // Split off the last char as the suit; everything before it is the rank.
        let suit_char = t.chars().last().ok_or_else(err)?;
        let suit = Suit::from_letter(suit_char).ok_or_else(err)?;
        let rank_str = &t[..t.len() - suit_char.len_utf8()];
        let rank = match rank_str.to_ascii_uppercase().as_str() {
            "10" | "T" => 8,
            r if r.len() == 1 => {
                let c = r.chars().next().unwrap();
                RANK_CHARS.iter().position(|&rc| rc == c).ok_or_else(err)? as u8
            }
            _ => return Err(err()),
        };
        Ok(Card::new(suit, rank))
    }
}

impl Serialize for Card {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Card {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Card, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A set of cards as a 52-bit mask. `Copy` and 8 bytes, so pass it by value.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CardSet(pub u64);

impl CardSet {
    pub const EMPTY: CardSet = CardSet(0);
    /// All 52 cards.
    pub const FULL_DECK: CardSet = CardSet((1u64 << 52) - 1);
    /// Every heart plus Q♠: the cards worth points.
    pub const POINT_CARDS: CardSet = CardSet(Suit::Hearts.mask().0 | Card::QUEEN_OF_SPADES.bit().0);

    pub const fn contains(self, c: Card) -> bool {
        self.0 & (1 << c.0) != 0
    }

    pub fn insert(&mut self, c: Card) {
        self.0 |= 1 << c.0;
    }

    pub fn remove(&mut self, c: Card) {
        self.0 &= !(1 << c.0);
    }

    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Cards of one suit within this set.
    pub const fn of_suit(self, suit: Suit) -> CardSet {
        CardSet(self.0 & suit.mask().0)
    }

    pub const fn has_suit(self, suit: Suit) -> bool {
        self.0 & suit.mask().0 != 0
    }

    /// Lowest card by index (lowest suit first, then lowest rank). Within a
    /// single suit this is the lowest rank, so combine with `of_suit`.
    pub const fn lowest(self) -> Option<Card> {
        if self.0 == 0 {
            None
        } else {
            Some(Card(self.0.trailing_zeros() as u8))
        }
    }

    /// Highest card by index; see [`CardSet::lowest`].
    pub const fn highest(self) -> Option<Card> {
        if self.0 == 0 {
            None
        } else {
            Some(Card(63 - self.0.leading_zeros() as u8))
        }
    }

    /// Total penalty points of the cards in this set.
    pub const fn points(self) -> u32 {
        let hearts = (self.0 & Suit::Hearts.mask().0).count_ones();
        let queen = if self.contains(Card::QUEEN_OF_SPADES) {
            13
        } else {
            0
        };
        hearts + queen
    }

    /// Iterate cards in ascending index order. Allocation-free.
    pub fn iter(self) -> CardSetIter {
        CardSetIter(self.0)
    }

    /// Parse whitespace- or comma-separated cards, e.g. `"QS 2C, TH"`.
    pub fn parse_list(s: &str) -> Result<CardSet, ParseCardError> {
        s.split(|c: char| c.is_whitespace() || c == ',')
            .filter(|t| !t.is_empty())
            .map(str::parse::<Card>)
            .collect()
    }
}

impl BitOr for CardSet {
    type Output = CardSet;
    fn bitor(self, rhs: CardSet) -> CardSet {
        CardSet(self.0 | rhs.0)
    }
}

impl BitAnd for CardSet {
    type Output = CardSet;
    fn bitand(self, rhs: CardSet) -> CardSet {
        CardSet(self.0 & rhs.0)
    }
}

/// Set difference: `a - b` is the cards in `a` that are not in `b`.
impl Sub for CardSet {
    type Output = CardSet;
    fn sub(self, rhs: CardSet) -> CardSet {
        CardSet(self.0 & !rhs.0)
    }
}

/// Complement within the 52-card deck.
impl Not for CardSet {
    type Output = CardSet;
    fn not(self) -> CardSet {
        CardSet(!self.0 & CardSet::FULL_DECK.0)
    }
}

impl FromIterator<Card> for CardSet {
    fn from_iter<I: IntoIterator<Item = Card>>(iter: I) -> CardSet {
        let mut set = CardSet::EMPTY;
        for c in iter {
            set.insert(c);
        }
        set
    }
}

impl IntoIterator for CardSet {
    type Item = Card;
    type IntoIter = CardSetIter;
    fn into_iter(self) -> CardSetIter {
        self.iter()
    }
}

/// Iterator that repeatedly pops the lowest set bit.
pub struct CardSetIter(u64);

impl Iterator for CardSetIter {
    type Item = Card;

    fn next(&mut self) -> Option<Card> {
        if self.0 == 0 {
            return None;
        }
        let i = self.0.trailing_zeros() as u8;
        self.0 &= self.0 - 1; // clear lowest set bit
        Some(Card(i))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.0.count_ones() as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for CardSetIter {}

impl fmt::Display for CardSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for CardSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{{self}}}")
    }
}

/// Serialized as a JSON array of card strings, e.g. `["2C","QS"]`.
impl Serialize for CardSet {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for CardSet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<CardSet, D::Error> {
        let cards = Vec::<Card>::deserialize(d)?;
        Ok(cards.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notation_round_trips_for_every_card() {
        for i in 0..52 {
            let c = Card::from_index(i);
            assert_eq!(c.to_string().parse::<Card>().unwrap(), c);
        }
    }

    #[test]
    fn parses_aliases() {
        assert_eq!("10h".parse::<Card>().unwrap(), "TH".parse().unwrap());
        assert_eq!("qs".parse::<Card>().unwrap(), Card::QUEEN_OF_SPADES);
        assert!("1X".parse::<Card>().is_err());
        assert!("".parse::<Card>().is_err());
    }

    #[test]
    fn points() {
        assert_eq!(Card::QUEEN_OF_SPADES.points(), 13);
        assert_eq!("2H".parse::<Card>().unwrap().points(), 1);
        assert_eq!(CardSet::FULL_DECK.points(), 26);
        assert_eq!(CardSet::POINT_CARDS.len(), 14);
    }

    #[test]
    fn set_ops() {
        let s = CardSet::parse_list("2C QS, TH").unwrap();
        assert_eq!(s.len(), 3);
        assert!(s.has_suit(Suit::Hearts));
        assert!(!s.has_suit(Suit::Diamonds));
        assert_eq!(s.lowest(), Some(Card::TWO_OF_CLUBS));
        assert_eq!(
            s.of_suit(Suit::Spades).highest(),
            Some(Card::QUEEN_OF_SPADES)
        );
        assert_eq!((!s).len(), 49);
        assert_eq!((CardSet::FULL_DECK - s).len(), 49);
        assert_eq!(s.to_string(), "2C QS TH");
    }

    #[test]
    fn serde_as_strings() {
        let s = CardSet::parse_list("QS 2C").unwrap();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"["2C","QS"]"#);
        assert_eq!(serde_json::from_str::<CardSet>(&json).unwrap(), s);
    }
}
