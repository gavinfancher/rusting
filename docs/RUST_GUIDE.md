# A Rust tour of the Hearts codebase

This is not a Rust textbook (read [The Rust Book](https://doc.rust-lang.org/book/) alongside it). It is a guided tour of the Rust concepts **as they are used in this repository**:
for each idea, a short excerpt or a pointer to the real file and function, why the code is written that way, and what to try. Paths are relative to the repo root.

Setup: `export PATH=/opt/homebrew/opt/rustup/bin:$PATH` (the Homebrew rustup shims are not on `PATH` by default). Use `cargo doc --open --workspace --no-deps` to browse the module docs: every file starts with `//!` comments that explain what and why.

A suggested reading order for the code: `crates/hearts-core/src/card.rs` → `types.rs` → `rules.rs` → `game.rs` → `strategy.rs` → `tournament.rs` (the whole engine, no I/O) → `crates/worker/src/{config,engine,runner,lease,app}.rs` (async + threads) → `crates/coordinator/src/{db,grpc,http/}` (database and API).

- [1 Ownership and borrowing](#1-ownership-and-borrowing)
- [2 Enums and pattern matching](#2-enums-and-pattern-matching)
- [3 Traits: static vs dynamic dispatch](#3-traits-static-vs-dynamic-dispatch)
- [4 Iterators](#4-iterators)
- [5 Error handling](#5-error-handling)
- [6 Modules, crates, workspace](#6-modules-crates-and-the-workspace)
- [7 rayon: data parallelism](#7-rayon-data-parallelism)
- [8 async/tokio vs CPU threads](#8-asynctokio-vs-cpu-threads-and-spawn_blocking)
- [9 Arc, Mutex, atomics](#9-arc-mutex-and-shared-state)
- [10 catch_unwind](#10-catch_unwind-surviving-a-panic)
- [11 tonic and build.rs](#11-tonic-code-generation-with-buildrs)
- [12 sqlx](#12-sqlx-queries)
- [13 Tests](#13-tests)
- [14 criterion benchmarks](#14-criterion-benchmarks)
- [15 Cargo cheat sheet](#15-cargo-cheat-sheet)
- [16 Exercises](#16-exercises)

---

## 1 Ownership and borrowing

Every value has exactly one owner; when the owner goes out of scope the value is dropped. You can **move** a value (the old name becomes unusable), **borrow** it immutably (`&T`, any number at once), or borrow it mutably (`&mut T`, exactly one and no other borrows meanwhile). The compiler checks this, which is how Rust avoids data races and use-after-free without a garbage collector.

**Types that are `Copy` are copied instead of moved**, so none of this ceremony applies to them. That is why the hot-path types in the engine are tiny `Copy` values (`crates/hearts-core/src/card.rs`):

```rust
/// A set of cards as a 52-bit mask. `Copy` and 8 bytes, so pass it by value.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CardSet(pub u64);

impl CardSet {
    pub const fn contains(self, c: Card) -> bool {   // `self` by value: it is just a u64
        self.0 & (1 << c.0) != 0
    }
```
A `CardSet` is one `u64` bitmask, so copying it costs the same as copying an integer, and you never fight the borrow checker over it. `Game`, `Trick` and `PlayerView` are `Copy` too. In `crates/hearts-core/src/advisor.rs` the PIMC advisor tries each legal move on a fresh copy of the game:

```rust
let mut g = base; // `Game` is Copy: this is a memcpy
g.play(card);
```
Compare with a type that is *not* `Copy`, such as `Vec<String>`: `fn strings_to_cards(v: &[String]) -> Result<CardSet, ConvertError>` (`crates/hearts-proto/src/lib.rs`) takes a **borrowed slice** (`&[String]`): it reads the caller's data without taking ownership or copying it.

Mutable borrow: `UnitStats::merge(&mut self, o: &UnitStats)` (`types.rs`) mutates the receiver and only reads `o`. Because `self` is exclusively borrowed, no one can read it half-updated.

**Moves into closures.** In `crates/worker/src/runner.rs`, `run_unit` clones the `Arc`s it needs and then `move`s them into the closure given to `spawn_blocking`: "Everything the blocking closure needs must be owned ('static): the closure may outlive this function call".
A closure running on another thread cannot borrow locals from a function that may return before it finishes, so the compiler forces you to give it ownership.

Try: in `Strategy::choose_play(&self, view: &PlayerView, rng: &mut Rng)` (`strategy.rs`), why `&self` but `&mut Rng`? (Bots are stateless; all randomness is owned by the caller, which also makes results reproducible.)

Reading compiler errors: `cannot borrow ... as mutable because it is also borrowed as immutable` means two live references conflict; `value used after move` means you handed it to someone else; `... may not live long enough` means a reference outlives its owner. Fix by cloning, borrowing for less time, or restructuring.

## 2 Enums and pattern matching

Rust enums are *sum types*: each variant can carry data, and `match` must cover every variant (checked at compile time). `crates/hearts-core/src/types.rs`:

```rust
pub enum StrategyKind {
    Random,
    Heuristic,
    /// Monte Carlo search on every decision. `samples` is the CPU dial.
    Pimc { samples: u32 },
}

impl FromStr for StrategyKind {
    type Err = String;
    fn from_str(s: &str) -> Result<StrategyKind, String> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "random" => Ok(StrategyKind::Random),
            "heuristic" => Ok(StrategyKind::Heuristic),
            _ => {
                let n = s.strip_prefix("pimc:")
                    .and_then(|n| n.parse::<u32>().ok())
                    .filter(|&n| n > 0)
                    .ok_or_else(|| format!("unknown strategy {s:?} (expected random | heuristic | pimc:N)"))?;
                Ok(StrategyKind::Pimc { samples: n })
            }
        }
    }
}
```
`"pimc:200"` becomes `StrategyKind::Pimc { samples: 200 }`. Note the chain on `Option`: `strip_prefix` returns `Option<&str>`, `and_then` parses, `filter` rejects zero, `ok_or_else` turns `None` into an `Err`, and `?` returns early on error.

`PassDirection::for_hand` (same file) is a `const fn` with a `match` on an integer; the hearts rule "pass left, right, across, hold" is four lines. Adding a fifth variant makes every non-exhaustive `match` fail to compile: **let the compiler be your checklist** when you change an enum (Exercise 2).

Other places: `UnitOutcome { Done(UnitStats), Cancelled, Failed(String) }` in `crates/worker/src/runner.rs` and its `match joined { Ok(Ok(Some(stats))) => ..., Ok(Err(payload)) => ..., Err(e) => ... }` (nested patterns destructure `Result<Result<Option<_>, _>, _>` in one go). `let Some(x) = opt else { return ... };` (let-else) is used in `Db::complete_ok` and `process_unit`.

## 3 Traits: static vs dynamic dispatch

A **trait** is a set of methods a type can implement. You can use traits two ways:

- **Static dispatch** (generics / enums): the compiler generates specialised code; calls can be inlined. No runtime cost.
- **Dynamic dispatch** (`dyn Trait` behind a pointer, e.g. `Box<dyn Trait>`, `Arc<dyn Trait>`): one compiled function, calls go through a vtable pointer. Flexible, a small cost per call.

`crates/hearts-core/src/strategy.rs` shows both, and why:

```rust
pub trait Strategy: Send + Sync {
    fn choose_pass(&self, hand: CardSet, dir: PassDirection, rng: &mut Rng) -> CardSet;
    fn choose_play(&self, view: &PlayerView, rng: &mut Rng) -> Card;
}

/// Enum dispatch over the built-in bots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bot { Random, Heuristic, Pimc { samples: u32 } }

impl Strategy for Bot {
    fn choose_play(&self, view: &PlayerView, rng: &mut Rng) -> Card {
        match self {
            Bot::Random => random_play(view, rng),
            Bot::Heuristic => heuristic_play(view),
            Bot::Pimc { samples } => pimc_play(view, *samples, rng),
        }
    }
    // ...
}

pub fn make_strategy(kind: StrategyKind) -> Box<dyn Strategy> { Box::new(Bot::new(kind)) }
```
The tournament loop plays tens of millions of moves, so it uses the `Bot` **enum**: a `match`, no vtable, no heap, and `Bot` is `Copy`. `make_strategy` gives you a `Box<dyn Strategy>` for callers that want to treat bots uniformly or add their own. Both satisfy the same trait.

The opposite choice is right elsewhere: `crates/worker/src/engine.rs` defines `trait Engine` (`run_hands`, `simulate`), and the worker holds an `Arc<dyn Engine>`. Production passes `CoreEngine`; the tests in `crates/worker/tests/integration.rs` pass a `FakeEngine` that is fast and can panic or sleep on demand. Here the call happens once per work unit (milliseconds to minutes), so the vtable cost is irrelevant and substitutability is valuable. **Rule of thumb: dyn at the seams, generics/enums in the hot loops.**

A third pattern: `trait Observer` in `tournament.rs` has default no-op methods and `impl Observer for () {}`. `play_hand<O: Observer>` is generic, so for `()` the compiler removes the callbacks entirely (monomorphisation: zero cost when unused), while `hearts-cli play` passes a printing observer.

`Send + Sync` bounds say a value can move to / be shared between threads. The compiler checks them, so "can I put this in a rayon or tokio task?" is answered at compile time.

## 4 Iterators

Iterators are lazy chains (`map`, `filter`, `fold`, ...) that compile to loops. To make your own type iterable, implement `Iterator::next`. `CardSet` is a bitmask, and iterating it pops the lowest set bit (`crates/hearts-core/src/card.rs`):

```rust
/// Iterator that repeatedly pops the lowest set bit.
pub struct CardSetIter(u64);

impl Iterator for CardSetIter {
    type Item = Card;
    fn next(&mut self) -> Option<Card> {
        if self.0 == 0 { return None; }
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
```
With `impl IntoIterator for CardSet`, you can write `for c in hand { ... }` and `hand.iter().map(|c| c.to_string())` (see `cards_to_strings` in hearts-proto). Iterating up to 13 cards costs a few instructions per card, with no allocation. `size_hint` and `ExactSizeIterator` let `collect()` allocate exactly once and make `.len()` work.

Elsewhere: `std::array::from_fn(|seat| ...)` builds a fixed-size array without a loop (`run_hands`); `slot_bots`/`bots` arrays are `[Bot; 4]`, on the stack. `.zip`, `.iter_mut` in `UnitStats::merge`. Parallel iterators (section 7) have the same shape.

## 5 Error handling

No exceptions. A function that can fail returns `Result<T, E>`; the `?` operator returns early with the error (after converting it with `From`).

- **Libraries use `thiserror`** to define precise error types. `crates/coordinator/src/db.rs`:
  ```rust
  #[derive(Debug, thiserror::Error)]
  pub enum DbError {
      #[error("database error: {0}")]
      Sql(#[from] sqlx::Error),         // #[from] generates `impl From<sqlx::Error> for DbError`
      #[error("stored job spec is invalid: {0}")]
      BadSpec(String),
  }
  pub type Result<T> = std::result::Result<T, DbError>;
  ```
  Any `sqlx::Error` is automatically converted by `?`. Also `FanoutError` in `advisor_fanout.rs`.
- **Binaries use `anyhow`**: `anyhow::Result` holds any error plus context. `crates/coordinator/src/main.rs`: `db::connect_with_retry(...).await.context("connecting to the database")?;` produces "connecting to the database: could not connect..." chains.
- **Conversions between layers with `TryFrom`.** A gRPC message can contain garbage (a 3-card hand, a bad strategy string), so converting `pb::JobSpec` into the engine's `JobSpec` can fail. `crates/hearts-proto/src/lib.rs`:
  ```rust
  impl TryFrom<&pb::JobSpec> for JobSpec {
      type Error = ConvertError;
      fn try_from(p: &pb::JobSpec) -> Result<JobSpec, ConvertError> { ... s.parse::<StrategyKind>().map_err(err) ... }
  }
  impl From<ConvertError> for tonic::Status {          // so a handler can just use `?`
      fn from(e: ConvertError) -> tonic::Status { tonic::Status::invalid_argument(e.to_string()) }
  }
  ```
  In `crates/worker/src/advisor_shard.rs`: `let view = PlayerView::try_from(view_pb)?;`, where an invalid message becomes a gRPC `INVALID_ARGUMENT` automatically. Convention: `From` for conversions that cannot fail (core → wire), `TryFrom` for those that can (wire → core), so untrusted data is validated at the boundary and the engine can assume well-formed input.
- **HTTP errors**: `ApiError` implements `IntoResponse` (`crates/coordinator/src/http/mod.rs`), so handlers return `Result<Json<T>, ApiError>` and every error becomes `{"error": "..."}` with the right status; `impl From<DbError> for ApiError` logs the detail and returns a generic 500.
- **No `unwrap()` on runtime failures in binaries** (project rule). `expect("metric registration")` is accepted only for programmer errors that fail on the first run (`worker/src/metrics.rs`).

Try: find every `?` in `Db::complete_ok` and say which error type it converts from and to.

## 6 Modules, crates, and the workspace

A **crate** is a compilation unit (a library or a binary); a **module** (`mod`) is a namespace inside it; a **workspace** is several crates built together with one `Cargo.lock` and one `target/` directory. Root `Cargo.toml`:
```toml
[workspace]
members = ["crates/*"]
[workspace.dependencies]
hearts-core = { path = "crates/hearts-core" }
rayon = "1.12.0"      # crates opt in with `rayon = { workspace = true }`
```
The crates and who depends on whom:

| Crate | Kind | Role |
|---|---|---|
| `hearts-core` | lib | engine; depends only on `serde` and `rayon`; no I/O, no async |
| `hearts-proto` | lib | `build.rs` generates gRPC code; converts wire types ↔ core types |
| `hearts-cli` | bin | run tournaments locally (`cargo run -p hearts-cli -- tournament ...`) |
| `worker` | lib + bin | compute node |
| `coordinator` | lib + bin | API, queue, UI |

Why `worker` and `coordinator` are **lib + bin**: `src/lib.rs` declares the modules and `src/main.rs` is a thin `main`. Integration tests in `tests/` can then `use coordinator::...` / `use worker::...` (a bin crate cannot be imported). Look at `crates/hearts-core/src/lib.rs`: `pub mod card;`, ..., `pub use types::*;` re-exports so callers write `hearts_core::JobSpec` instead of `hearts_core::types::JobSpec`.
Visibility: items are private by default; `pub` exposes them; `pub(crate)` only within the crate. Keeping `hearts-core` I/O-free means it builds and tests in a second and benchmarks cleanly.

## 7 rayon: data parallelism

rayon turns a sequential iterator into a parallel one by changing `into_iter()` to `into_par_iter()`; a work-stealing thread pool does the rest. The heart of the project, `run_hands` in `crates/hearts-core/src/tournament.rs`:

```rust
range
    .into_par_iter()
    // `fold` gives each rayon task a private accumulator (no sharing, no
    // locks); `reduce` then merges the per-task accumulators.
    .fold(UnitStats::default, |mut acc, i| {
        let slots = seating(i);
        let bots: [Bot; 4] = std::array::from_fn(|seat| slot_bots[slots[seat] as usize]);
        let r = play_hand(&bots, rules, i, hand_seed(seed, i), &mut ());
        accumulate(&mut acc, &r, slots);
        acc
    })
    .reduce(UnitStats::default, |mut a, b| { a.merge(&b); a })
```
- `fold` = each task builds a private partial result: **no locks, no atomics, no contention**. `reduce` combines partials with `merge`.
- Because the result of hand *i* depends only on `(seed, i)`, and `merge` is integer addition (commutative and associative), the final answer is identical for 1 thread or 100: **determinism under parallelism**, which is what makes game days checkable.
- The Rust type system prevents sharing mutable state between tasks by accident: a closure that captured `&mut` data used from two threads would not compile.

**Which pool runs it?** Parallel iterators run on "the current pool". In `crates/worker/src/app.rs` the worker builds ONE `rayon::ThreadPoolBuilder::new().num_threads(threads)` (threads = `WORKER_THREADS`, or `available_parallelism()`, which on Linux honours the container's cgroup CPU quota, so threads follow the Kubernetes CPU limit), and `crates/worker/src/runner.rs` calls `pool.install(|| compute(...))`, which makes that pool current for everything inside. This is the lever for the vertical-scaling experiments (E1, E4). The advisor (`advisor.rs`, `simulate`) uses the same mechanism and, on a worker, the same pool, so advisor requests and tournament units compete for the same threads.

## 8 async/tokio vs CPU threads, and `spawn_blocking`

`async fn` code runs on a small pool of **tokio** worker threads; an `.await` yields the thread so thousands of tasks (network I/O, timers, gRPC) share a few threads. That only works if tasks **yield quickly**. A CPU-bound call that runs for seconds inside an async task occupies a tokio thread and starves everything scheduled on it (heartbeats, health checks, the gRPC server).

So the worker separates concerns (`crates/worker/src/runner.rs`, the module docs explain it):

```rust
self.metrics.busy.set(1);
let handle = tokio::task::spawn_blocking(move || {
    catch_unwind(AssertUnwindSafe(|| {
        pool.install(|| compute(&*engine, &spec, range, &cancel, faults, nth, &metrics))
    }))
});
let joined = handle.await;
```
Three kinds of threads cooperate:
1. tokio async threads: the lease loop, heartbeat task, gRPC and HTTP servers (cheap, mostly waiting);
2. tokio's **blocking pool**: `spawn_blocking` hands the closure to a dedicated thread that may block as long as it likes;
3. the **rayon pool**: the compute threads, sized to the CPU limit.

`handle.await` suspends only the async task, not a thread. The same pattern is in `advisor_shard.rs`.

Other async things worth reading: `tokio::select!` racing a unit against shutdown (`process_unit` in `lease.rs`), `tokio::spawn` for the heartbeat task, `CancellationToken` for shutdown, `#[tonic::async_trait]` (async methods in traits), and exponential backoff with jitter (`Backoff` in `lease.rs`). `crates/worker/src/main.rs` builds the runtime by hand (`tokio::runtime::Builder::new_multi_thread()`), and installs a SIGTERM handler so Kubernetes can drain a pod gracefully (game day 1).
Heartbeat note: the heartbeat task is `async`, so it keeps working while the compute threads are busy; that is the whole point of the separation. (And if the container is CPU-throttled, async threads are throttled too: game day 5.)

## 9 Arc, Mutex, and shared state

Threads and tasks sharing data need two things: shared **ownership** (`Arc<T>`, an atomically reference-counted pointer; cloning copies the pointer, not the data) and, if it is mutable, **synchronisation** (`Mutex`, atomics, channels).

- `Arc<Metrics>`, `Arc<Config>`, `Arc<rayon::ThreadPool>`, `Arc<dyn Engine>`: read-only (or internally synchronised) shared handles, cloned freely into tasks. `AppState` (`crates/coordinator/src/state.rs`) is `#[derive(Clone)]` and made only of cheap-to-clone handles, because axum clones it per request.
- The **in-flight table** in `crates/worker/src/lease.rs` is `std::sync::Mutex<HashMap<i64, Arc<AtomicBool>>>`: unit id → cancel flag. The lease loop inserts, the heartbeat task flags lost units, and the compute thread polls the flag between chunks. An `AtomicBool` is the right tool for a flag one thread sets and another reads: no lock needed. The module docs explain why a **std** mutex, not tokio's: the lock is held for a few instructions and never across an `.await`.
  ```rust
  fn table(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Arc<AtomicBool>>> {
      self.inflight.lock().unwrap_or_else(|e| e.into_inner())   // recover from a "poisoned" lock
  }
  ```
  A mutex is *poisoned* if a thread panicked while holding it; here the map is still valid, so the code takes the guard anyway.
- Prometheus counters (`IntCounter`, `IntGauge`) are internally `Arc` + atomics; updating from any thread is lock-free.
- The advisor fan-out keeps a `Mutex<HashMap<SocketAddr, Channel>>` of gRPC channels (`advisor_fanout.rs`).

The compiler enforces the rules: sharing a plain `Rc<RefCell<_>>` across threads does not compile (`Rc` is not `Send`); that is "fearless concurrency". What it does *not* prevent is deadlocks and holding a lock across an `.await`, which is why those comments exist.

## 10 `catch_unwind`: surviving a panic

A panic unwinds the stack and, by default, ends the thread (and with it, the process if it is the main thread; here a panic inside the rayon pool is forwarded to the caller of `install`). A worker must not die because one unit hit a bug, and the game-day fault `FAULT_PANIC_EVERY_N` forces exactly that. `std::panic::catch_unwind` runs a closure and turns a panic into an `Err` (`runner.rs`):

```rust
catch_unwind(AssertUnwindSafe(|| {
    pool.install(|| compute(&*engine, &spec, range, &cancel, faults, nth, &metrics))
}))
// ...
Ok(Err(payload)) => UnitOutcome::Failed(format!("panic: {}", panic_message(&payload))),
```
- `AssertUnwindSafe` is a promise: "if this panics, I will not use data that might be half-modified". True here: all we do afterwards is report an error.
- The payload is `Box<dyn Any + Send>`; `panic_message` downcasts it to `&str` or `String` to get the message.
- Do not use panics for ordinary errors; `Result` is for that. `catch_unwind` is a firewall at a process boundary (a bug, an injected fault), turning "crash" into "this unit failed, report it, take the next" (game day 8: poison unit).
- It only works with `panic = "unwind"` (the default); with `panic = "abort"` in the release profile it would not.

## 11 tonic code generation with `build.rs`

gRPC services are defined once in `proto/hearts.proto` (messages and `service WorkerService`, `service AdvisorShard`). A **build script** (`crates/hearts-proto/build.rs`) runs before the crate compiles and generates Rust code with `protoc`:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/hearts.proto");   // tell cargo when to re-run
    tonic_prost_build::configure()
        .compile_protos(&["../../proto/hearts.proto"], &["../../proto"])?;
    Ok(())
}
```
The generated code lands in `target/` and is pulled in by a macro in `crates/hearts-proto/src/lib.rs`:
```rust
pub mod pb { tonic::include_proto!("hearts.v1"); }
```
You get message structs (`pb::LeaseWorkRequest`), a **client** (`pb::worker_service_client::WorkerServiceClient`, used in `worker/src/lease.rs`) and a **server trait** to implement (`pb::advisor_shard_server::AdvisorShard`, implemented by `AdvisorService` in `worker/src/advisor_shard.rs`; `WorkerSvc` in `coordinator/src/grpc.rs` implements `WorkerService`). If you change the `.proto` file and a method signature no longer matches, the compiler shows you every call site and impl to fix. Requires `protoc` (installed with Homebrew).
Why separate hearts-proto: the wire types (strings for cards) differ from the engine types (bitmasks), and the conversion layer is the one place that validates. Look at `rpc LeaseWork` in the proto, then `lease_once` (client side) and `LeaseWork` in `grpc.rs` (server side).

## 12 sqlx queries

`sqlx` is an async, pure-Rust Postgres client. This project uses **runtime-checked** queries (`sqlx::query` / `query_as` with SQL strings) instead of the compile-time `query!` macro, so building does not require a live database.

```rust
let updated: Option<(i64,)> = sqlx::query_as(
    "UPDATE work_units SET state = 'done', completed_by = $2, ...
      WHERE id = $1 AND state NOT IN ('done', 'cancelled')
  RETURNING job_id",
)
.bind(unit_id)
.bind(worker_id)
.fetch_optional(&mut *tx)
.await?;
```
- `$1`, `$2` are **bind parameters**: values are sent separately from the SQL, so there is no SQL injection and the plan can be reused.
- `query_as` maps result rows into a Rust type: a tuple `(i64,)` or a struct with `#[derive(sqlx::FromRow)]` (`JobRow`, `LeaseRow`, `WorkerRow`). `Json<JobSpec>` maps a `jsonb` column straight to a Rust type via serde.
- `fetch_one`/`fetch_optional`/`fetch_all`/`execute`: one row, zero-or-one, a Vec, or no rows.
- **Transactions**: `let mut tx = self.pool.begin().await?; ... tx.commit().await?;` If `tx` is dropped without `commit`, it rolls back (see the duplicate-completion path in `complete_ok`: "Nothing changed; dropping `tx` rolls back").
- **Migrations**: `sqlx::migrate!("../../migrations").run(pool)` embeds `migrations/0001_init.sql` into the binary at compile time and applies new files at startup.
- **The pool** (`PgPool`, size `DB_POOL_SIZE`) hands connections to concurrent tasks; `acquire_timeout` bounds the wait. Dashboard: "DB pool connections".

The most instructive query is the lease in `Db::lease` (`crates/coordinator/src/db.rs`): a CTE with `FOR UPDATE SKIP LOCKED` that atomically picks the next unit (pending or expired lease), marks it leased, bumps `attempts`, and starts the job, in one statement. Read the doc comment above it, then `docs/CONTRACTS.md` ("Work unit lifecycle"). `finish_job_if_complete` explains why a second statement is needed under READ COMMITTED (E3's serial bottleneck lives there).
Use `psql` (`make psql`) to run those SQL statements by hand and `EXPLAIN` them.

## 13 Tests

Rust has four kinds of tests in this repo.

1. **Unit tests**: next to the code in `#[cfg(test)] mod tests { ... }`, with `#[test]` functions. They can see private items. Examples: `chunk_ranges` in `crates/worker/src/runner.rs` (ranges must cover the input exactly and differ in size by at most one), `seed.rs` (a pinned constant: "fails if someone changes the derivation"), `Backoff` in `lease.rs`, the `StrategyKind` parse round trip in `types.rs`.
2. **Integration tests**: files in `crates/<name>/tests/`; each is a separate crate that can only use the public API. `crates/worker/tests/integration.rs` starts a **real tonic server** as a mock coordinator on a loopback port and runs the real lease loop against it with a `FakeEngine` (this is what the `Engine` trait is for): tests like `completes_every_unit_exactly_once`, `lost_lease_stops_unit_and_counts_as_wasted`, `panic_becomes_error_completion_and_worker_continues`, `recovers_when_coordinator_starts_late`. These are the executable versions of game days 1, 2 and 8.
3. **Property tests** (`proptest`): instead of examples, state a property and let the library generate hundreds of random inputs, then shrink a failing one to a minimal case. `crates/hearts-core/tests/engine.rs`:
   ```rust
   proptest! {
       #[test]
       fn full_hand_plays_every_card_once_and_scores_26_or_78(seed in any::<u64>(), idx in 0u64..96) { ... }
   }
   ```
   "For any seed and hand index, a full game plays each card exactly once and scores 26 (or 78 when someone shoots the moon)." Properties find edge cases you would not think of. Failing seeds are saved under `proptest-regressions/`.
4. **Container tests** (`testcontainers`): `crates/coordinator/tests/common/mod.rs` starts a throwaway `postgres:18.4` container per test (random host port), applies migrations, and runs the real `Db`/HTTP/gRPC code against it (`db_lifecycle.rs`, `grpc_http.rs`, `advisor_fanout.rs`). **Docker must be running.** This is how the lease SQL is proven to work, including the SKIP LOCKED and reclaim semantics.

```bash
cargo test --workspace                         # everything (needs Docker for the coordinator tests)
cargo test -p hearts-core                      # fast, no Docker
cargo test -p worker panic_becomes_error       # name filter
cargo test -p coordinator --test db_lifecycle -- --nocapture   # one file, show println!/tracing output
make test                                      # fmt --check + clippy -D warnings + tests (the CI gate)
```

## 14 criterion benchmarks

`cargo bench` with the `criterion` crate measures small pieces statistically (warm-up, many iterations, outlier detection, comparison to the previous run). `crates/hearts-core/benches/engine.rs`:
```rust
let mut g = c.benchmark_group("hand");
g.throughput(Throughput::Elements(1));            // report hands/s, not just time
g.bench_function(name, |b| b.iter(|| { i += 1; black_box(play_hand(&bots, Rules::default(), i, hand_seed(42, i), &mut ())) }));
```
`black_box` stops the optimiser from deleting work whose result is unused. `bench = false` in the `[lib]` section avoids running the library's own tests as benchmarks, and `harness = false` on `[[bench]]` hands `main` to criterion. Run `cargo bench -p hearts-core -- --quick` and compare to the table in `docs/OPTIMIZATION_LOG.md` (about 870k heuristic hands/s per thread on the author's M4 Pro). Benchmarks are how you check an optimisation really helped (the log records one that did not). Release builds use `lto = "fat"`, `codegen-units = 1`, and keep line tables so profilers show function names (`[profile.release]` in the root `Cargo.toml`).

## 15 Cargo cheat sheet

```bash
export PATH=/opt/homebrew/opt/rustup/bin:$PATH

cargo check --workspace                 # type-check only: the fastest feedback loop
cargo build --release                   # optimised build into target/release/
cargo run --release -p hearts-cli -- tournament --hands 100000 --seed 42 --strategies heuristic,heuristic,random,pimc:20
cargo run --release -p hearts-cli -- play --seed 1 --strategies heuristic,heuristic,random,random
cargo run -p coordinator                # needs DATABASE_URL; or `make dev` for the whole stack in Docker
cargo test --workspace                  # see section 13
cargo bench -p hearts-core -- --quick   # section 14
cargo fmt --all                         # format;  `cargo fmt --all --check` in CI
cargo clippy --workspace --all-targets -- -D warnings   # lints; a great teacher: read the explanations
cargo doc --open --workspace --no-deps  # browse the //! docs
cargo add -p worker some-crate          # add a dependency to one crate (edits that crate's Cargo.toml)
cargo add -p worker proptest --dev      # a dev-dependency (tests only)
cargo tree -p worker                    # dependency tree; `-i tokio` shows who pulls tokio in
cargo update -p tokio                   # bump one dependency within semver rules
cargo clean -p hearts-proto             # force regeneration of the gRPC code
RUST_LOG=debug LOG_FORMAT=pretty cargo run -p worker     # tracing output; RUST_LOG=worker=debug,info for one crate
cargo test -- --nocapture --test-threads=1               # show output, run serially
```
Directory map: `Cargo.toml` (workspace), `Cargo.lock` (exact versions, committed), `target/` (build output, git-ignored), `crates/*/Cargo.toml` (per-crate deps), `edition = "2024"` and `rust-version = "1.99"` in `[workspace.package]`.

---

## 16 Exercises

Do them in order; each is a small change that touches a different concept. After each: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.

1. **Trace a hand.** Run `cargo run -p hearts-cli -- play --seed 1 --strategies heuristic,heuristic,random,random`. Open `tournament.rs::play_hand` and follow one trick through `Game::play` (`game.rs`). Where is the trick winner computed? Which types are `Copy`? Draw the data flow.
2. **A new strategy: `lowest`.** A bot that always plays its lowest legal card. Steps:
   (a) in `types.rs` add `StrategyKind::Lowest` and extend `Display` and `FromStr` (`"lowest"`);
   (b) in `strategy.rs` add `Bot::Lowest`, map it in `Bot::new`, implement `lowest_play(view)` using `legal_moves(view)` and `CardSet::iter().next()` (CardSet iterates in ascending order; `first_card` in the same file does that), pass with `heuristic_pass`;
   (c) fix every non-exhaustive `match` the compiler reports (that is your to-do list);
   (d) add a unit test for the parse round trip and a proptest that `lowest_play` is always in `legal_moves`;
   (e) run `hearts-cli tournament --strategies lowest,heuristic,random,random` and explain the standings. Because the wire format is a string parsed by `StrategyKind::from_str`, the coordinator and worker accept `"lowest"` with no further change; submit a job through the UI. (Note: `types.rs` is the shared contract file; update `docs/CONTRACTS.md` if you keep it.)
3. **A new metric.** Add `hearts_worker_units_started_total` (counter) to `crates/worker/src/metrics.rs`: a field in `Metrics`, create it with `IntCounter::new`, register it (`reg(Box::new(...))`), increment it in `Runner::run_unit` next to `started.fetch_add`. Run a local worker (`make dev`), then `curl localhost:9090/metrics | grep started` (compose publishes the worker port? otherwise `docker compose exec`). Add a Grafana panel to `deploy/monitoring/dashboards/hearts-workers.json` with `sum(rate(hearts_worker_units_started_total[1m]))`. Extra: what is the difference between this and `hearts_worker_units_completed_total`, and when are they different (game day 8)?
4. **A new REST field.** Add `hands_remaining` (= `num_hands - progress.hands_done`) to the Job JSON. Edit `Job` in `crates/coordinator/src/model.rs` and wherever `Job` is built in `Db::load_jobs` (`db.rs`); `Serialize` derives the JSON for free. Update the assertion in `crates/coordinator/tests/grpc_http.rs` and the contract in `docs/CONTRACTS.md`; show it in `web/` if you like. Concept: serde derives, `i64` arithmetic, tests with testcontainers. Extra: make it `null` for terminal jobs using `Option<i64>`.
5. **A property test for chunking.** In `crates/worker/src/runner.rs` the unit tests check hand-picked ranges. `cargo add -p worker proptest --dev`, then write `proptest!` that for random `start`, `len`, and `max_chunks` the chunks are contiguous, non-empty, cover the range exactly, and differ in size by at most one. (If you find a counterexample, congratulations: you found a bug.)
6. **Benchmark the PIMC cost curve locally.** In `crates/hearts-core/benches/engine.rs` add a group that benchmarks `run_hands` with `[Heuristic, Heuristic, Random, Pimc{samples}]` for 10/50/200 over 20 hands on a single-thread pool, and compare with experiment E5's cluster numbers (`docs/EXPERIMENTS.md`). Why are they close, and what is the difference between them?
7. **A stats endpoint.** Add `GET /api/stats` returning `{ "jobs": {"done": N, ...}, "units": {...} }`. New file `crates/coordinator/src/http/stats.rs`, a `Db::job_counts` method with `sqlx::query_as::<_, (String, i64)>("SELECT status::text, count(*) FROM jobs GROUP BY status")`, and a route in `http/mod.rs::router`. Covers modules, error conversion (`DbError` → `ApiError` via `?`), and routing. Test it with the testcontainers helper `common::setup`.
8. **Confidence intervals.** In `hearts-cli`'s tournament output, print the standard error of the average points per hand per slot. You need the sum of squares per slot, so extend `SlotStats` (a contract type: you would have to extend `UnitStats::merge`, the proto, the `unit_results` table...). First do the quick version: compute it in the CLI by running 20 seeds. Then consider what the "right" distributed version needs (this is a lesson in how a small change ripples through the layers).
9. **Break the compiler on purpose.** In `lease.rs`, try returning `&self.inflight.lock().unwrap()` from `table()`; try holding the guard across an `.await`; try putting an `Rc` in `LeaseCtx`. Read each error message and note what rule it enforces.
10. **Read a flame graph.** `cargo install flamegraph` (or use `samply`) and profile `hearts-cli tournament --threads 1` on the PIMC workload. Where does the time go? Compare with the "Next ideas" in `docs/OPTIMIZATION_LOG.md` and try one.
