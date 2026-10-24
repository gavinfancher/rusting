//! # hearts-proto
//!
//! `pb` holds the Rust code generated from `proto/hearts.proto` by `build.rs`
//! (message structs plus tonic client and server traits). The rest of this
//! file converts between those wire types and the `hearts-core` types, so
//! the coordinator and worker never hand-parse card strings themselves.
//!
//! Direction convention: `From<core> for pb` can't fail; `TryFrom<pb> for
//! core` validates (card strings, array lengths) and returns `ConvertError`.

use hearts_core::{
    Card, CardSet, JobSpec, MoveStat, MoveTotals, PlayerView, Rules, SlotStats, StrategyKind,
    Trick, UnitStats,
};
use std::fmt;

/// Generated protobuf/gRPC code (package `hearts.v1`).
pub mod pb {
    tonic::include_proto!("hearts.v1");
}

/// A wire message didn't hold valid data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError(pub String);

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid message: {}", self.0)
    }
}

impl std::error::Error for ConvertError {}

/// Lets tonic handlers write `let spec: JobSpec = req.spec.try_into()?;`
/// and return a gRPC INVALID_ARGUMENT status.
impl From<ConvertError> for tonic::Status {
    fn from(e: ConvertError) -> tonic::Status {
        tonic::Status::invalid_argument(e.to_string())
    }
}

fn err(msg: impl Into<String>) -> ConvertError {
    ConvertError(msg.into())
}

fn cards_to_strings(set: CardSet) -> Vec<String> {
    set.iter().map(|c| c.to_string()).collect()
}

fn strings_to_cards(v: &[String]) -> Result<CardSet, ConvertError> {
    v.iter()
        .map(|s| s.parse::<Card>().map_err(|e| err(e.to_string())))
        .collect()
}

fn four<T: Copy, U>(
    v: &[U],
    what: &str,
    f: impl Fn(&U) -> Result<T, ConvertError>,
) -> Result<[T; 4], ConvertError> {
    if v.len() != 4 {
        return Err(err(format!("{what}: expected 4 entries, got {}", v.len())));
    }
    Ok([f(&v[0])?, f(&v[1])?, f(&v[2])?, f(&v[3])?])
}

// ----- JobSpec --------------------------------------------------------------

impl From<&JobSpec> for pb::JobSpec {
    fn from(s: &JobSpec) -> pb::JobSpec {
        pb::JobSpec {
            seed: s.seed,
            strategies: s.strategies.iter().map(|k| k.to_string()).collect(),
            no_points_first_trick: s.rules.no_points_first_trick,
        }
    }
}

impl TryFrom<&pb::JobSpec> for JobSpec {
    type Error = ConvertError;
    fn try_from(p: &pb::JobSpec) -> Result<JobSpec, ConvertError> {
        Ok(JobSpec {
            seed: p.seed,
            strategies: four(&p.strategies, "strategies", |s| {
                s.parse::<StrategyKind>().map_err(err)
            })?,
            rules: Rules {
                no_points_first_trick: p.no_points_first_trick,
            },
        })
    }
}

// ----- UnitStats ------------------------------------------------------------

impl From<&SlotStats> for pb::SlotStats {
    fn from(s: &SlotStats) -> pb::SlotStats {
        pb::SlotStats {
            points: s.points,
            hands: s.hands,
            moons: s.moons,
            queens: s.queens,
            wins: s.wins,
        }
    }
}

impl From<&pb::SlotStats> for SlotStats {
    fn from(s: &pb::SlotStats) -> SlotStats {
        SlotStats {
            points: s.points,
            hands: s.hands,
            moons: s.moons,
            queens: s.queens,
            wins: s.wins,
        }
    }
}

impl From<&UnitStats> for pb::UnitStats {
    fn from(u: &UnitStats) -> pb::UnitStats {
        pb::UnitStats {
            hands: u.hands,
            slots: u.slots.iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<&pb::UnitStats> for UnitStats {
    type Error = ConvertError;
    fn try_from(p: &pb::UnitStats) -> Result<UnitStats, ConvertError> {
        Ok(UnitStats {
            hands: p.hands,
            slots: four(&p.slots, "slots", |s| Ok(SlotStats::from(s)))?,
        })
    }
}

// ----- PlayerView -----------------------------------------------------------

impl From<&PlayerView> for pb::PlayerView {
    fn from(v: &PlayerView) -> pb::PlayerView {
        pb::PlayerView {
            seat: v.seat as u32,
            hand: cards_to_strings(v.hand),
            trick_leader: v.trick.leader as u32,
            trick: v.trick.iter().map(|(_, c)| c.to_string()).collect(),
            played: cards_to_strings(v.played),
            voids: v.voids.iter().map(|&b| b as u32).collect(),
            hearts_broken: v.hearts_broken,
            tricks_played: v.tricks_played as u32,
            points: v.points.iter().map(|&p| p as u32).collect(),
            passed_to: v.passed.map(|(s, _)| s as u32),
            passed_cards: v
                .passed
                .map(|(_, c)| cards_to_strings(c))
                .unwrap_or_default(),
            no_points_first_trick: v.rules.no_points_first_trick,
        }
    }
}

impl TryFrom<&pb::PlayerView> for PlayerView {
    type Error = ConvertError;
    fn try_from(p: &pb::PlayerView) -> Result<PlayerView, ConvertError> {
        let seat = |s: u32, what: &str| -> Result<u8, ConvertError> {
            if s < 4 {
                Ok(s as u8)
            } else {
                Err(err(format!("{what}: seat {s} out of range")))
            }
        };
        if p.trick.len() > 3 {
            return Err(err(
                "trick: at most 3 cards can be on the table on your turn",
            ));
        }
        let mut trick = Trick::new(seat(p.trick_leader, "trick_leader")?);
        for s in &p.trick {
            trick.push(
                s.parse()
                    .map_err(|e: hearts_core::ParseCardError| err(e.to_string()))?,
            );
        }
        let passed = match p.passed_to {
            Some(to) => Some((seat(to, "passed_to")?, strings_to_cards(&p.passed_cards)?)),
            None => None,
        };
        Ok(PlayerView {
            seat: seat(p.seat, "seat")?,
            hand: strings_to_cards(&p.hand)?,
            trick,
            played: strings_to_cards(&p.played)?,
            voids: four(&p.voids, "voids", |&b| {
                u8::try_from(b & 0xF).map_err(|_| err("voids"))
            })?,
            hearts_broken: p.hearts_broken,
            tricks_played: u8::try_from(p.tricks_played)
                .ok()
                .filter(|&t| t < 13)
                .ok_or_else(|| err("tricks_played must be 0..13"))?,
            points: four(&p.points, "points", |&x| {
                u8::try_from(x)
                    .ok()
                    .filter(|&x| x <= 26)
                    .ok_or_else(|| err("points must be 0..=26"))
            })?,
            passed,
            rules: Rules {
                no_points_first_trick: p.no_points_first_trick,
            },
        })
    }
}

// ----- MoveTotals -----------------------------------------------------------

impl From<&MoveTotals> for pb::SimulateResponse {
    fn from(m: &MoveTotals) -> pb::SimulateResponse {
        pb::SimulateResponse {
            moves: m
                .moves
                .iter()
                .map(|s| pb::MoveStat {
                    card: s.card.to_string(),
                    total_points: s.total_points,
                    samples: s.samples,
                })
                .collect(),
            samples_done: m.samples_done,
        }
    }
}

impl TryFrom<&pb::SimulateResponse> for MoveTotals {
    type Error = ConvertError;
    fn try_from(p: &pb::SimulateResponse) -> Result<MoveTotals, ConvertError> {
        let moves = p
            .moves
            .iter()
            .map(|m| {
                Ok(MoveStat {
                    card: m
                        .card
                        .parse()
                        .map_err(|e: hearts_core::ParseCardError| err(e.to_string()))?,
                    total_points: m.total_points,
                    samples: m.samples,
                })
            })
            .collect::<Result<Vec<_>, ConvertError>>()?;
        Ok(MoveTotals {
            moves,
            samples_done: p.samples_done,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_spec_round_trip() {
        let spec = JobSpec {
            seed: 7,
            strategies: ["heuristic", "random", "pimc:10", "heuristic"].map(|s| s.parse().unwrap()),
            rules: Rules::default(),
        };
        let wire = pb::JobSpec::from(&spec);
        assert_eq!(JobSpec::try_from(&wire).unwrap(), spec);
    }

    #[test]
    fn player_view_round_trip() {
        let mut trick = Trick::new(3);
        trick.push("5H".parse().unwrap());
        let view = PlayerView {
            seat: 0,
            hand: CardSet::parse_list("2C 3C QS AH").unwrap(),
            trick,
            played: CardSet::parse_list("4D 5D 6D 7D").unwrap(),
            voids: [0, 1, 0, 0],
            hearts_broken: true,
            tricks_played: 1,
            points: [0, 0, 0, 1],
            passed: Some((1, CardSet::parse_list("AS KS 9H").unwrap())),
            rules: Rules::default(),
        };
        let wire = pb::PlayerView::from(&view);
        assert_eq!(PlayerView::try_from(&wire).unwrap(), view);
    }

    #[test]
    fn rejects_bad_input() {
        let bad = pb::JobSpec {
            seed: 1,
            strategies: vec!["random".into()],
            no_points_first_trick: true,
        };
        assert!(JobSpec::try_from(&bad).is_err());
    }
}
