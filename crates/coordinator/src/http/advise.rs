//! `POST /api/advise`: ask the worker fleet which card to play.
//!
//! This handler only translates JSON into a `PlayerView`, validates it with
//! the engine, and delegates to [`crate::advisor_fanout::Fanout`].

use super::{ApiError, parse_json};
use crate::{advisor_fanout::FanoutError, state::AppState};
use axum::{Json, body::Bytes, extract::State, http::StatusCode};
use hearts_core::{Card, CardSet, PlayerView, Rules, Suit, Trick};
use serde::{Deserialize, Serialize};
use std::time::Instant;

const MAX_SAMPLES: u32 = 1_000_000;
const MAX_DEADLINE_MS: u32 = 60_000;

#[derive(Debug, Deserialize)]
pub struct AdviseRequest {
    #[serde(default)]
    seat: u8,
    hand: Vec<String>,
    trick_leader: Option<u8>,
    #[serde(default)]
    trick: Vec<String>,
    #[serde(default)]
    played: Vec<String>,
    /// Per seat, the suits known to be void: `[[], ["C"], [], []]`.
    voids: Option<Vec<Vec<String>>>,
    #[serde(default)]
    hearts_broken: bool,
    tricks_played: Option<u8>,
    points: Option<Vec<u8>>,
    passed_to: Option<u8>,
    #[serde(default)]
    passed_cards: Vec<String>,
    rules: Option<Rules>,
    samples: Option<u32>,
    deadline_ms: Option<u32>,
    seed: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MoveOut {
    card: String,
    avg_points: f64,
    samples: u32,
}

#[derive(Debug, Serialize)]
pub struct AdviseResponse {
    best: String,
    moves: Vec<MoveOut>,
    samples_requested: u32,
    samples_achieved: u32,
    partial: bool,
    shards: u32,
    shards_failed: u32,
    latency_ms: u64,
}

fn parse_cards(list: &[String], what: &str) -> Result<CardSet, ApiError> {
    let mut set = CardSet::EMPTY;
    for s in list {
        let card: Card = s.parse().map_err(|e: hearts_core::ParseCardError| {
            ApiError::bad_request(format!("{what}: {e}"))
        })?;
        if set.contains(card) {
            return Err(ApiError::bad_request(format!(
                "{what}: duplicate card {card}"
            )));
        }
        set.insert(card);
    }
    Ok(set)
}

fn check_seat(s: u8, what: &str) -> Result<u8, ApiError> {
    if s < 4 {
        Ok(s)
    } else {
        Err(ApiError::bad_request(format!("{what} must be 0..3")))
    }
}

/// Turn the request into an engine `PlayerView`, applying the documented
/// defaults. Only *shape* problems are rejected here (bad card text, wrong
/// array lengths); whether the situation is a legal Hearts position is up to
/// `hearts_core::rules::validate_view`.
pub fn build_view(req: &AdviseRequest) -> Result<PlayerView, ApiError> {
    let seat = check_seat(req.seat, "seat")?;
    let leader = check_seat(req.trick_leader.unwrap_or(seat), "trick_leader")?;
    if req.trick.len() > 3 {
        return Err(ApiError::bad_request(
            "trick: at most 3 cards can be on the table on your turn",
        ));
    }
    let mut trick = Trick::new(leader);
    let trick_cards: Vec<Card> = req
        .trick
        .iter()
        .map(|s| {
            s.parse::<Card>()
                .map_err(|e| ApiError::bad_request(format!("trick: {e}")))
        })
        .collect::<Result<_, _>>()?;
    for c in trick_cards {
        trick.push(c);
    }

    let mut voids = [0u8; 4];
    if let Some(v) = &req.voids {
        if v.len() != 4 {
            return Err(ApiError::bad_request("voids must have exactly 4 entries"));
        }
        for (seat_idx, suits) in v.iter().enumerate() {
            for s in suits {
                let suit = s
                    .chars()
                    .next()
                    .filter(|_| s.chars().count() == 1)
                    .and_then(Suit::from_letter)
                    .ok_or_else(|| ApiError::bad_request(format!("voids: bad suit {s:?}")))?;
                voids[seat_idx] |= suit.bit();
            }
        }
    }

    let played = parse_cards(&req.played, "played")?;
    let tricks_played = req.tricks_played.unwrap_or((req.played.len() / 4) as u8);
    if tricks_played >= 13 {
        return Err(ApiError::bad_request("tricks_played must be 0..12"));
    }
    let points = match &req.points {
        None => [0u8; 4],
        Some(p) => {
            let arr: [u8; 4] = p
                .as_slice()
                .try_into()
                .map_err(|_| ApiError::bad_request("points must have exactly 4 entries"))?;
            if arr.iter().any(|&x| x > 26) {
                return Err(ApiError::bad_request("points entries must be 0..=26"));
            }
            arr
        }
    };
    let passed = match req.passed_to {
        Some(to) => Some((
            check_seat(to, "passed_to")?,
            parse_cards(&req.passed_cards, "passed_cards")?,
        )),
        None => None,
    };

    Ok(PlayerView {
        seat,
        hand: parse_cards(&req.hand, "hand")?,
        trick,
        played,
        voids,
        hearts_broken: req.hearts_broken,
        tricks_played,
        points,
        passed,
        rules: req.rules.unwrap_or_default(),
    })
}

pub async fn advise(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<AdviseResponse>, ApiError> {
    // Cheapest checks first: no point validating if there is nobody to ask.
    if !state.fanout.enabled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "advisor is disabled (ADVISOR_SHARD_DNS is not set)",
        ));
    }
    let started = Instant::now();
    let req: AdviseRequest = parse_json(&body)?;
    let view = build_view(&req)?;
    hearts_core::rules::validate_view(&view).map_err(ApiError::bad_request)?;

    let samples = req.samples.unwrap_or(1000);
    if !(1..=MAX_SAMPLES).contains(&samples) {
        return Err(ApiError::bad_request(format!(
            "samples must be between 1 and {MAX_SAMPLES}"
        )));
    }
    let deadline_ms = req
        .deadline_ms
        .unwrap_or(state.config.advisor_default_deadline_ms);
    if !(1..=MAX_DEADLINE_MS).contains(&deadline_ms) {
        return Err(ApiError::bad_request(format!(
            "deadline_ms must be between 1 and {MAX_DEADLINE_MS}"
        )));
    }
    let seed = req.seed.unwrap_or_else(rand::random::<u64>);

    let result = state
        .fanout
        .advise(&view, samples, seed, deadline_ms)
        .await
        .map_err(|e| match e {
            FanoutError::Disabled | FanoutError::NoShards | FanoutError::AllFailed => {
                ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e.to_string())
            }
        })?;

    let achieved = result.totals.samples_done;
    let partial = achieved < result.samples_requested;
    if partial {
        state.metrics.advise_partial.inc();
    }
    let best = result.totals.best().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no samples completed before the deadline",
        )
    })?;
    let mut moves: Vec<MoveOut> = result
        .totals
        .moves
        .iter()
        .map(|m| MoveOut {
            card: m.card.to_string(),
            avg_points: m.mean(),
            samples: m.samples,
        })
        .collect();
    // Best (lowest expected points) first; moves with no samples (NaN) last.
    moves.sort_by(|a, b| a.avg_points.total_cmp(&b.avg_points));

    let elapsed = started.elapsed();
    state.metrics.advise_duration.observe(elapsed.as_secs_f64());
    Ok(Json(AdviseResponse {
        best: best.to_string(),
        moves,
        samples_requested: result.samples_requested,
        samples_achieved: achieved,
        partial,
        shards: result.shards,
        shards_failed: result.shards_failed,
        latency_ms: elapsed.as_millis() as u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(json: &str) -> AdviseRequest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn applies_defaults() {
        let v = build_view(&req(r#"{"hand":["2C","QS","AH"]}"#)).unwrap();
        assert_eq!(v.seat, 0);
        assert_eq!(v.trick.leader, 0);
        assert_eq!(v.tricks_played, 0);
        assert_eq!(v.points, [0; 4]);
        assert!(v.passed.is_none());
        assert_eq!(v.hand.len(), 3);
    }

    #[test]
    fn parses_full_example() {
        let v = build_view(&req(
            r#"{"seat":0,"hand":["2C","QS","AH"],"trick_leader":3,"trick":["5H"],
                "played":[],"voids":[[],["C"],[],[]],"hearts_broken":false,
                "points":[0,0,0,0],"passed_to":1,"passed_cards":["AS","KS","9H"]}"#,
        ))
        .unwrap();
        assert_eq!(v.trick.leader, 3);
        assert_eq!(v.trick.len(), 1);
        assert_eq!(v.voids, [0, Suit::Clubs.bit(), 0, 0]);
        assert_eq!(v.passed.unwrap().0, 1);
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            r#"{"hand":["ZZ"]}"#,
            r#"{"hand":["2C","2C"]}"#,
            r#"{"hand":[],"seat":4}"#,
            r#"{"hand":[],"trick":["2C","3C","4C","5C"]}"#,
            r#"{"hand":[],"voids":[[],[]]}"#,
            r#"{"hand":[],"voids":[["X"],[],[],[]]}"#,
            r#"{"hand":[],"points":[1,2,3]}"#,
        ] {
            assert!(build_view(&req(bad)).is_err(), "{bad}");
        }
    }
}
