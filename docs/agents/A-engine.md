# Agent A: Game engine

**Owns:** `crates/hearts-core/src/{rules,strategy,tournament,advisor}.rs` plus any new files under `crates/hearts-core/` (`benches/`, `tests/`, new modules) and `crates/hearts-cli/`, and `docs/OPTIMIZATION_LOG.md`.
**Must not edit:** `card.rs`, `types.rs`, `seed.rs` (contracts; you may *add* new modules and `impl` blocks in your own files).

## Goal
A fast, correct, deterministic Hearts engine. It's the CPU workload that every scaling experiment measures, so **performance is a feature**: no heap allocation in the per-trick play loop, `CardSet` bit operations everywhere, and `Copy` state structs.

## Build
1. **Game state** (private to the engine): four hands as `CardSet`, the current `Trick`, the played set, points per seat, `hearts_broken`, voids, and pass info. A method that produces the `PlayerView` for the seat to play (this is the only thing bots see).
2. **Rules** (`rules.rs`): deal from a seed (Fisher-Yates over the 52 cards), passing (3 cards; `PassDirection::for_hand`), 2♣ leads trick 1, follow suit, the first-trick point rule (`Rules::no_points_first_trick`, falling back to point cards only if nothing else is available), hearts can't be led until broken (unless only hearts are held; Q♠ does *not* break hearts), trick winner = highest card of the led suit, scoring with shooting the moon (the shooter gets 0 and the others get +26). Implement the stubbed `legal_moves` and `validate_view` exactly as documented.
3. **RNG:** pick a fast seedable PRNG (e.g. `rand_xoshiro::Xoshiro256PlusPlus` via `cargo add`, or a hand-written xoshiro/wyrand to avoid `rand` API churn). Everything derives from `seed::hand_seed` / `seed::child_seed`. Document the seed derivation per decision in comments.
4. **Strategies** (`strategy.rs`): a trait with `choose_pass` and `choose_play`, both taking only player-visible info plus an `&mut` RNG. Implementations:
   - `Random`: uniform over legal moves and passes.
   - `Heuristic`: passing gets rid of Q♠/K♠/A♠ (unless guarded by many spades), high hearts, and tries to void a short suit. Playing: when following, play the highest card that still loses the trick; if forced to win, win high (unless points are at stake later in the trick); when void, dump Q♠, then the highest heart, then the highest card. When leading, lead low from safe suits and avoid leading spades above Q when Q♠ is unseen. Keep it simple, readable, and fast; it is the rollout policy for PIMC.
   - `Pimc { samples }`: calls `advisor::simulate` on the current view (seed from the hand seed plus decision index) and plays `best()`. For passing, use the heuristic pass.
   - `fn make_strategy(kind: StrategyKind) -> Box<dyn Strategy>` (or an enum dispatch; enum dispatch is faster, so measure it if you're curious).
5. **Advisor** (`advisor.rs`): implement `simulate` per its doc contract. Determinization: deal `view.unseen()` to the other three seats matching their remaining card counts, never giving a seat a card of a suit it's void in, and placing passed cards with their receiver (if not yet played). Use rejection-free dealing: assign the most constrained seats first. Chunks of `SAMPLE_CHUNK` go through `rayon` `par_iter` and are reduced in chunk order, so the result is deterministic. Deadline: check it before each chunk.
6. **Tournament** (`tournament.rs`): implement `run_hands` per its contract: `into_par_iter` over hands, map each to a per-hand `UnitStats`, then reduce with `merge`. Per hand: seat → slot from `seating(i)`, record per-slot points/moons/queens/wins.
7. **CLI** (`hearts-cli`, using `clap` derive):
   - `hearts-cli tournament --hands 100000 --seed 42 --strategies heuristic,heuristic,random,pimc:50 --threads 6 [--json]` prints standings (avg points per hand, win %, moons, Q♠ taken) and **hands/sec**, and runs inside `rayon::ThreadPoolBuilder::new().num_threads(n).build()?.install(..)`.
   - `hearts-cli advise --hand "2C 5C ..." [--trick "5H"] [--leader 3] [--played ...] [--samples 2000] [--seed 7] [--threads N]` prints each legal card's expected points, sorted.
   - `hearts-cli play --seed 1 --strategies ...` prints one hand trick by trick (great for debugging and for the owner to *see* the game).

## Tests (in `crates/hearts-core/tests/` or as unit tests)
- Rules: 2♣ must lead; must follow suit; no hearts leads before broken; the first-trick point rule; moon-shot scoring; Q♠ = 13.
- `proptest`: for random seeds, a full hand with random strategies plays all 52 cards exactly once, all moves are legal, and the total points are 26 (or 78 = 3×26 on a moon shot).
- Determinism: `run_hands` gives the same stats for 1 thread and 8 threads (build two pools), and `run_hands(0..1000) == merge(run_hands(0..500), run_hands(500..1000))`.
- Advisor: deterministic for a fixed seed; respects voids (sample deals never violate them, so test the dealer directly); returns `samples_done < samples` when the deadline is already passed; with one legal move, returns it.
- Heuristic beats Random over 20k hands (avg points clearly lower) and PIMC(100) beats Heuristic over a smaller sample. This is a sanity test, so keep it fast with a modest hand count, or mark it `#[ignore]` with a note if it's slow.

## Benchmarks (`criterion`, in `crates/hearts-core/benches/engine.rs`)
`hand/random`, `hand/heuristic`, `run_hands/heuristic_1000`, `advisor/simulate_1000_samples`. Then write `docs/OPTIMIZATION_LOG.md` with a short intro ("each optimization records before/after numbers"), a **baseline table** (machine: Apple Silicon Mac, 14 cores; record single-thread hands/sec for random and heuristic and advisor samples/sec), and a "next ideas" list.

## Acceptance
```
cargo test -p hearts-core
cargo bench -p hearts-core -- --quick        # or a short measurement time
cargo run --release -p hearts-cli -- tournament --hands 100000 --seed 42 --strategies heuristic,heuristic,random,random --threads 1
cargo run --release -p hearts-cli -- tournament --hands 100000 --seed 42 --strategies heuristic,heuristic,random,random --threads 8   # identical standings, ~linear speedup
cargo run --release -p hearts-cli -- advise --hand "2C 7C 9D QS KS 3H 8H AH 4D 6D JC TS 5S" --samples 2000
```
