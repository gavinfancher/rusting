# Optimization log

The engine (`hearts-core`) is the CPU workload for every scaling experiment, so
its speed matters. Each optimization below records **before/after numbers**
from the same machine, with the command used, so the effect is visible and
reproducible. Optimizations must not change results unless stated: the
tournament standings for `--seed 42 --strategies heuristic,heuristic,random,random
--hands 100000` (slot 0 = 304999 points, slot 1 = 308327, slot 2 = 1076125,
slot 3 = 1069617) are the regression check.

Benchmarks: `cargo bench -p hearts-core -- --quick` (criterion, single thread
unless noted). CLI: `cargo run --release -p hearts-cli -- tournament ...`.

## Baseline

Machine: Apple Silicon Mac (M4 Pro, 14 cores = 10 performance + 4 efficiency),
Rust 1.99, release profile with fat LTO, `codegen-units = 1`.

| Benchmark | Time | Throughput |
|---|---|---|
| `hand/random` (deal + pass + 52 plays) | 1.06 us | ~945k hands/s |
| `hand/heuristic` | 1.15 us | ~871k hands/s |
| `run_hands/heuristic_1000` (1 thread) | 1.04 ms | ~965k hands/s |
| `advisor/simulate_1000_samples` (1 thread, 3 legal moves) | 2.16 ms | ~463k samples/s |
| CLI `tournament --threads 1`, 100k hands (heuristic x2, random x2) | 0.129 s | ~773k hands/s |
| CLI `tournament --threads 8`, 100k hands | 0.016 s | ~6.2M hands/s |

PIMC tournament cost (`pimc:100` x2 vs `heuristic` x2, 1 thread, 2000 hands):
7.2 s, about 277 hands/s. That is roughly 3000x more expensive than the
heuristic hand, which is what makes PIMC jobs the interesting scaling workload.

Thread scaling, 2M hands, heuristic x2 + random x2, identical standings at
every thread count:

| Threads | Time | Hands/s | Speedup |
|---|---|---|---|
| 1 | 2.43 s | 824k | 1.0x |
| 8 | 0.315 s | 6.36M | 7.7x |
| 14 | 0.206 s | 9.70M | 11.8x (4 of the 14 cores are efficiency cores) |

## Why it is already fast (design, not an optimization pass)

- `CardSet` is a `u64`; legal moves, voids, suit membership are single bit
  operations. `Game`, `Trick`, `PlayerView` are `Copy` stack structs.
- No heap allocation in the per-trick loop. Passing sorts a 13-element stack
  array. The advisor copies the whole `Game` (a memcpy) to try each move.
- Enum dispatch (`Bot`) instead of `Box<dyn Strategy>` in the tournament loop.
- `run_hands` uses rayon `fold` + `reduce`: each task owns a private
  `UnitStats`, so there are no locks or atomics on the hot path.

## Optimizations

### 1. Per-suit candidate scan in `lead` / `dump` (heuristic bot)
Instead of iterating all legal cards to find the lowest-scoring lead or the
highest dump, look only at each suit's lowest/highest card (at most 4
candidates; the rest cannot win the comparison). Results are bit-identical
(standings unchanged).

| | before | after |
|---|---|---|
| `hand/heuristic` | 1.148 us | 1.164 us (within noise) |
| `run_hands/heuristic_1000` | 1.037 ms | 1.012 ms (-2%, within noise) |

Verdict: no measurable gain. The legal sets are small, so the iterator version
was not a bottleneck. Kept because it is not slower and is still simple.
Lesson: measure before assuming a loop is hot.

### 2. Drop the shuffle in the advisor's dealer
`determinize` shuffled the unseen cards and then placed each one into a
random eligible seat weighted by remaining need. The placement is already
random per card (and with no voids it is exactly a uniform deal regardless of
order), so the shuffle was redundant. This changes the advisor's random stream
(advisor numbers shift within sampling noise; tournament standings of
heuristic/random bots are unaffected since they do not use the dealer).

| | before | after |
|---|---|---|
| `advisor/determinize_1000` | 392 us | 352 us (-10%) |
| `advisor/simulate_1000_samples` | 2.12 ms | 2.06 ms (-2%) |

The dealer is about 17% of a sample; the other 83% is the heuristic playouts
(about 11 ns per card played).

## Next ideas

1. **Playout without `PlayerView`**: a `heuristic_play_game(&Game)` that reads
   the `Game` fields directly. LTO probably already inlines most of the copy,
   so profile first (`cargo flamegraph` / samply; line tables are enabled in
   the release profile).
2. **Cheaper trick resolution**: `Game::play` rebuilds a `CardSet` from the
   trick (`Trick::cards`) and `trick_winner` re-walks it. Track the trick's
   card set and current winner incrementally as cards are pushed.
3. **Early playout cut-off in the advisor**: once every remaining point card
   is gone (or all remaining points are decided), the rest of the hand cannot
   change the score, so stop rolling out.
4. **Share rollouts between moves**: moves that lead to the same position
   (e.g. two adjacent ranks in a suit when the card between them is already
   played) are equivalent; collapse them.
5. **Smarter sampling**: importance-sample deals that are consistent with
   inferred bot behaviour, or use more samples only for close decisions.
6. **SIMD / branchless suit logic** for the heuristic: precompute per-suit
   masks once per decision.
7. **Larger `SAMPLE_CHUNK`-aware scheduling** for the advisor on many cores
   (64-sample chunks are ~140 us; on 14 threads with only a few hundred samples
   some threads idle). Contract fixes the chunk size, so this would be a
   proposal.
8. **PGO / `target-cpu=native` builds** for the worker image (measure; PGO
   needs a representative workload such as the PIMC tournament above).
