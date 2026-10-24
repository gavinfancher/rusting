//! hearts-cli: run tournaments, ask the advisor, and watch single hands
//! locally, no cluster needed.
//!
//! ```text
//! hearts-cli tournament --hands 100000 --seed 42 --strategies heuristic,heuristic,random,random --threads 8
//! hearts-cli advise --hand "2C 7C 9D QS KS 3H 8H AH 4D 6D JC TS 5S" --samples 2000
//! hearts-cli play --seed 1 --strategies heuristic,heuristic,random,random
//! ```

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use hearts_core::advisor;
use hearts_core::card::{Card, CardSet, Suit};
use hearts_core::game::TrickDone;
use hearts_core::rules::{legal_moves, validate_view};
use hearts_core::seed::hand_seed;
use hearts_core::strategy::Bot;
use hearts_core::tournament::{Observer, play_hand, run_hands};
use hearts_core::types::{
    JobSpec, PassDirection, PlayerView, Rules, Seat, StrategyKind, Trick, seating,
};
use serde::Serialize;
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "hearts-cli",
    version,
    about = "Hearts simulator: tournaments, advisor, single hands"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Simulate many hands and print standings and hands/sec.
    Tournament(TournamentArgs),
    /// Rank the legal cards of a position by expected points (Monte Carlo).
    Advise(AdviseArgs),
    /// Play one hand and print it trick by trick.
    Play(PlayArgs),
}

#[derive(Args)]
struct CommonRules {
    /// Allow hearts / Q-spades on the first trick even when you could avoid it.
    #[arg(long)]
    points_on_first_trick: bool,
}

impl CommonRules {
    fn rules(&self) -> Rules {
        Rules {
            no_points_first_trick: !self.points_on_first_trick,
        }
    }
}

#[derive(Args)]
struct TournamentArgs {
    #[arg(long, default_value_t = 100_000)]
    hands: u64,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// Four comma-separated strategies: random | heuristic | pimc:N
    #[arg(
        long,
        default_value = "heuristic,heuristic,random,random",
        value_delimiter = ','
    )]
    strategies: Vec<StrategyKind>,
    /// Worker threads (0 = all cores).
    #[arg(long, default_value_t = 0)]
    threads: usize,
    /// Print machine-readable JSON instead of a table.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    rules: CommonRules,
}

#[derive(Args)]
struct AdviseArgs {
    /// Your hand, e.g. "2C 5C QS".
    #[arg(long)]
    hand: String,
    /// Cards already on the table this trick, in play order.
    #[arg(long, default_value = "")]
    trick: String,
    /// Seat that led the current trick (default: inferred so that `--seat` is next to play).
    #[arg(long)]
    leader: Option<Seat>,
    /// Your seat (0..3).
    #[arg(long, default_value_t = 0)]
    seat: Seat,
    /// Cards from completed tricks.
    #[arg(long, default_value = "")]
    played: String,
    /// Points taken so far per seat, e.g. "0,13,1,0".
    #[arg(long, value_delimiter = ',')]
    points: Option<Vec<u8>>,
    /// Known voids as SEAT:SUITS, repeatable, e.g. --void 1:CD
    #[arg(long = "void")]
    voids: Vec<String>,
    /// Hearts have been played already (inferred from --played if omitted).
    #[arg(long)]
    hearts_broken: bool,
    /// Cards you passed away and who got them: SEAT:cards, e.g. "1:AS,KS,9H".
    #[arg(long)]
    passed: Option<String>,
    #[arg(long, default_value_t = 2000)]
    samples: u32,
    #[arg(long, default_value_t = 7)]
    seed: u64,
    #[arg(long, default_value_t = 0)]
    threads: usize,
    #[command(flatten)]
    rules: CommonRules,
}

#[derive(Args)]
struct PlayArgs {
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Which hand of the job to play (sets pass direction and seating).
    #[arg(long, default_value_t = 0)]
    hand_index: u64,
    #[arg(
        long,
        default_value = "heuristic,heuristic,random,random",
        value_delimiter = ','
    )]
    strategies: Vec<StrategyKind>,
    #[command(flatten)]
    rules: CommonRules,
}

fn four(v: Vec<StrategyKind>) -> Result<[StrategyKind; 4]> {
    let n = v.len();
    v.try_into()
        .map_err(|_| anyhow::anyhow!("need exactly 4 strategies, got {n}"))
}

fn pool(threads: usize) -> Result<rayon::ThreadPool> {
    // 0 lets rayon pick (all cores / cgroup limit).
    Ok(rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?)
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Tournament(a) => tournament(a),
        Command::Advise(a) => advise(a),
        Command::Play(a) => play(a),
    }
}

#[derive(Serialize)]
struct TournamentOut<'a> {
    spec: &'a JobSpec,
    hands: u64,
    threads: usize,
    elapsed_secs: f64,
    hands_per_sec: f64,
    stats: hearts_core::types::UnitStats,
}

fn tournament(a: TournamentArgs) -> Result<()> {
    let spec = JobSpec {
        seed: a.seed,
        strategies: four(a.strategies)?,
        rules: a.rules.rules(),
    };
    let pool = pool(a.threads)?;
    let threads = pool.current_num_threads();
    let t0 = Instant::now();
    let stats = pool.install(|| run_hands(&spec, 0..a.hands));
    let secs = t0.elapsed().as_secs_f64();
    let hps = a.hands as f64 / secs;

    if a.json {
        let out = TournamentOut {
            spec: &spec,
            hands: a.hands,
            threads,
            elapsed_secs: secs,
            hands_per_sec: hps,
            stats,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!(
        "{} hands, seed {}, {} thread(s)\n",
        stats.hands, spec.seed, threads
    );
    println!(
        "{:<4} {:<12} {:>10} {:>8} {:>8} {:>10} {:>10}",
        "slot", "strategy", "avg pts", "win %", "moons", "Q-spade", "points"
    );
    for (i, s) in stats.slots.iter().enumerate() {
        let h = s.hands.max(1) as f64;
        println!(
            "{:<4} {:<12} {:>10.3} {:>7.2}% {:>8} {:>10} {:>10}",
            i,
            spec.strategies[i].to_string(),
            s.points as f64 / h,
            100.0 * s.wins as f64 / h,
            s.moons,
            s.queens,
            s.points
        );
    }
    println!("\n{secs:.3} s, {hps:.0} hands/sec");
    Ok(())
}

fn parse_cards(s: &str) -> Result<CardSet> {
    CardSet::parse_list(s).map_err(|e| anyhow::anyhow!("bad card list {s:?}: {e}"))
}

fn advise(a: AdviseArgs) -> Result<()> {
    let hand = parse_cards(&a.hand)?;
    let played = parse_cards(&a.played)?;
    let trick_cards: Vec<Card> = a
        .trick
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .map(|t| {
            t.parse::<Card>()
                .map_err(|e| anyhow::anyhow!("bad trick card: {e}"))
        })
        .collect::<Result<_>>()?;
    if a.seat >= 4 {
        bail!("--seat must be 0..3");
    }
    let leader = a
        .leader
        .unwrap_or(((a.seat as usize + 4 - trick_cards.len() % 4) % 4) as Seat);
    if leader >= 4 {
        bail!("--leader must be 0..3");
    }
    let mut trick = Trick::new(leader);
    for c in &trick_cards {
        if trick.is_complete() {
            bail!("a trick has at most 4 cards");
        }
        trick.push(*c);
    }
    let mut voids = [0u8; 4];
    for v in &a.voids {
        let (seat, suits) = v
            .split_once(':')
            .context("--void expects SEAT:SUITS, e.g. 1:CD")?;
        let seat: usize = seat.parse().context("void seat")?;
        if seat >= 4 {
            bail!("void seat must be 0..3");
        }
        for ch in suits.chars() {
            voids[seat] |= Suit::from_letter(ch)
                .with_context(|| format!("unknown suit {ch:?}"))?
                .bit();
        }
    }
    let mut points = [0u8; 4];
    if let Some(p) = &a.points {
        if p.len() != 4 {
            bail!("--points needs 4 values");
        }
        points.copy_from_slice(p);
    }
    let passed = match &a.passed {
        Some(p) => {
            let (to, cards) = p.split_once(':').context("--passed expects SEAT:cards")?;
            Some((
                to.parse::<Seat>().context("passed seat")?,
                parse_cards(cards)?,
            ))
        }
        None => None,
    };
    let view = PlayerView {
        seat: a.seat,
        hand,
        trick,
        played,
        voids,
        hearts_broken: a.hearts_broken || played.has_suit(Suit::Hearts),
        tricks_played: (played.len() / 4) as u8,
        points,
        passed,
        rules: a.rules.rules(),
    };
    validate_view(&view).map_err(|e| anyhow::anyhow!("invalid position: {e}"))?;

    let pool = pool(a.threads)?;
    let t0 = Instant::now();
    let totals = pool.install(|| advisor::simulate(&view, a.samples, a.seed, None));
    let secs = t0.elapsed().as_secs_f64();

    let mut moves = totals.moves.clone();
    moves.sort_by(|x, y| x.mean().total_cmp(&y.mean()).then(x.card.cmp(&y.card)));
    println!(
        "seat {} to play, {} legal card(s), {} samples",
        a.seat,
        legal_moves(&view).len(),
        totals.samples_done
    );
    println!("{:<6} {:>12}", "card", "avg points");
    for m in &moves {
        println!("{:<6} {:>12.3}", m.card.to_string(), m.mean());
    }
    if let Some(b) = totals.best() {
        println!("\nbest: {b}");
    }
    println!(
        "{secs:.3} s, {:.0} samples/sec",
        totals.samples_done as f64 / secs
    );
    Ok(())
}

/// Prints the hand as it happens.
struct Printer {
    names: [String; 4],
    trick_no: u32,
    cards: Vec<(Seat, Card)>,
}

impl Observer for Printer {
    fn dealt(&mut self, hands: &[CardSet; 4]) {
        println!("Deal:");
        for (s, (name, hand)) in self.names.iter().zip(hands).enumerate() {
            println!("  seat {s} {name:<18} {hand}");
        }
    }
    fn passed(&mut self, dir: PassDirection, passes: &[CardSet; 4]) {
        println!("\nPass {dir:?}:");
        for s in 0..4u8 {
            if let Some(t) = dir.target(s) {
                println!("  seat {s} -> seat {t}: {}", passes[s as usize]);
            }
        }
        println!();
    }
    fn played(&mut self, seat: Seat, card: Card, done: Option<TrickDone>) {
        self.cards.push((seat, card));
        if let Some(d) = done {
            self.trick_no += 1;
            let plays: Vec<String> = self
                .cards
                .iter()
                .map(|(s, c)| format!("s{s}:{c}"))
                .collect();
            println!(
                "trick {:>2}: {}  -> seat {} takes {} pt(s)",
                self.trick_no,
                plays.join(" "),
                d.winner,
                d.points
            );
            self.cards.clear();
        }
    }
}

fn play(a: PlayArgs) -> Result<()> {
    let kinds = four(a.strategies)?;
    let slots = seating(a.hand_index);
    let bots: [Bot; 4] = std::array::from_fn(|s| Bot::new(kinds[slots[s] as usize]));
    let mut p = Printer {
        names: std::array::from_fn(|s| {
            format!("(slot {}: {})", slots[s], kinds[slots[s] as usize])
        }),
        trick_no: 0,
        cards: Vec::new(),
    };
    let r = play_hand(
        &bots,
        a.rules.rules(),
        a.hand_index,
        hand_seed(a.seed, a.hand_index),
        &mut p,
    );
    println!("\nFinal scores by seat: {:?}", r.points);
    match r.moon {
        Some(m) => println!("Seat {m} shot the moon!"),
        None => println!("Seat {} took the Q-spade", r.queen_taker),
    }
    Ok(())
}
