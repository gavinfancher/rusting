# Learning (Level 1): evolving a Hearts bot

## What is being learned
The `linear` bot doesn't follow hand-written rules. For each card it could play, it measures 16 facts (**features**) about that card in the current situation, multiplies each by a **weight**, adds them up, and plays the card with the highest score. Passing works the same way with 8 more features. **Learning means finding the 24 weights that lose the fewest points.**

```
score(card) = w0·leading + w1·rank + w2·is_heart + … + w15·lead_low_spade_fishing
```
A positive weight means "I like cards with this property"; a negative one means "avoid them". The exact feature definitions and scales are documented in a table at the top of `crates/hearts-core/src/linear.rs`. Names and order are a contract (`FEATURE_NAMES` in `types.rs`).

| Play feature | Plain English |
|---|---|
| `rank` | how high the card is |
| `is_heart`, `is_queen_spades` | is it a point card? |
| `high_spade_queen_unseen` | is it K♠/A♠ while the Q♠ is still out there? (dangerous) |
| `wins_now` | would it currently take the trick? |
| `wins_x_points_on_table` | …and how many points would that cost? |
| `wins_x_last_to_play` | …and is the win certain because we're last? |
| `wins_x_my_points` | …and have I already taken points (moon-shot attempt)? |
| `dump_x_points`, `dump_x_rank` | I can't follow suit, so how good a dump is this card? |
| `makes_void`, `suit_len_after` | does playing it run me out of a suit (so I can dump later)? |
| `higher_unseen`, `lower_unseen` | how many cards of this suit are still out above or below it? (safety) |
| `lead_low_spade_fishing` | leading a low spade to flush out the Q♠ |

## How it learns: the cross-entropy method (CEM)
There are no gradients and no neural network. Just "try variations, keep what works":

```
belief: for each weight, a mean and a spread (sigma)
        ┌───────────────────────────────────────────────┐
        ▼                                               │
  sample 24 weight vectors around the mean              │
  (candidate 0 = the mean itself)                       │
        ▼                                               │
  each plays 40,000 hands vs 3 heuristic bots           │   one
  fitness = average points per hand (lower = better)    │ generation
        ▼                                               │
  take the best 6 ("elites")                            │
  move mean toward them, shrink sigma toward their spread
        └───────────────────────────────────────────────┘
```
As sigma shrinks, the search narrows in on good weights. Code: `crates/hearts-core/src/learn.rs` (`sample`, `update`).

### Why every candidate plays the same hands
In a generation, all candidates play exactly the same deals (seed `generation_seed(run_seed, g)`). Hearts is very noisy: one bad deal costs 13+ points. Comparing candidates on identical deals ("common random numbers") cancels that luck, so the difference in scores reflects the weights, not the cards. Each generation uses fresh deals, so the bot can't overfit one set of hands.

### Why it's deterministic, and why that matters
Sampling uses a seeded RNG, fitness comes from the deterministic tournament engine, and ties are broken by candidate index. So **the same config and seed always produce the same champion**, whether you run it on your laptop with 3 threads or on the cluster with 9 pods. That makes distributed training testable: drill 11 kills the coordinator mid-training and checks that the champion still matches `hearts-cli train`.

## Measured results (M4 Pro, 12 threads)
**From scratch** (`--init zero`, 16 candidates, 10 generations, 20k hands): from near-random play to decent in **1.5 s**.
```
gen   0: 11.80  ███████████▊
gen   4:  9.66  █████████▋
gen   8:  7.65  ███████▋
gen   9:  7.76  ███████▊      (heuristic ≈ 6.7, not reached yet: needs more generations)
```

**From the heuristic-like start** (defaults: `--init heuristic`, 24 candidates, 30 generations, 40k hands): **6.4 s**.
```
gen   0: 6.47   gen  10: 6.31   gen  20: 6.10   gen  29: 6.05
validation on 200,000 fresh hands vs 3 heuristic bots:
  champion  6.087 avg points
  heuristic 6.756 avg points
  difference −0.670 ± 0.039 (1 SE)   ← the learned bot is clearly better
```
The curve in the first generations goes *up* before it goes down, because sigma starts wide and most samples are worse than the start. Once sigma shrinks (20 → 2), steady improvement takes over.

### Reading the learned weights
Champion of the run above:
`linear:0,-11,-10,-47,-25,-29,-9,4,-8,143,42,8,-45,8,33,19,-8,10,13,22,-8,20,-1,13`
- `dump_x_points` **+143** (the biggest weight): when you can't follow suit, dump point cards. That's the most important habit in Hearts.
- `is_queen_spades` **−47**, `high_spade_queen_unseen` **−25**, `wins_now` **−29**: don't play the queen yourself, don't expose K♠/A♠, and avoid winning tricks in general.
- `suit_len_after` **−45**: prefer playing from short suits, to get void sooner and dump later.
- `lower_unseen` **+33**: play cards with many lower cards still out (they're likely to be beaten).
- Passing: `is_high_spade` **+22**, `short_side_suit` **+20**: pass away K♠/A♠ and empty short suits.

These match expert Hearts advice, learned from nothing but "lose fewer points".

## Try it
**Locally:**
```
cargo run --release -p hearts-cli -- train --init zero --generations 40 --seed 7
cargo run --release -p hearts-cli -- features --strategy linear:...    # pretty-print a champion
cargo run --release -p hearts-cli -- tournament --hands 100000 --strategies linear:...,heuristic,heuristic,pimc:20
```
**On the cluster:** the Training page in the UI, or:
```
curl -XPOST https://hearts.k8s.ad.gavinf.com/api/training -H 'content-type: application/json' \
  -d '{"name":"first","init":"heuristic","generations":30,"seed":1}'
```
Each generation becomes 24 tournament jobs that the workers chew through. The **Hearts Training** dashboard shows fitness per generation. Then use the champion anywhere with `"champion:<run_id>"` as a strategy. Same seed means the same champion as the CLI command above.

**Scaling angle:** one generation is ~1M hands, so training is a throughput workload, and more worker pods mean more generations per hour. With `opponents: ["pimc:10", ...]` each generation becomes vastly heavier, which is a good stress test.

## Ideas for Level 2
- Train against a mix of opponents (including `pimc`, or earlier champions) so the bot doesn't just exploit the heuristic.
- Richer features (card counting, per-seat void info, score context for moon defence).
- Replace the linear score with a small neural net trained to imitate `pimc:200` (distillation), still on CPU.
