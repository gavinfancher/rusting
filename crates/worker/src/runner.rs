//! Running one work unit on the CPU pool.
//!
//! ## Why `spawn_blocking` + `pool.install`
//!
//! `run_hands` is a long, CPU-bound call. If we called it directly inside an
//! `async fn` it would occupy a tokio worker thread for seconds or minutes
//! and starve everything else scheduled on that thread (heartbeats, health
//! checks, the gRPC server). `tokio::task::spawn_blocking` runs a closure on
//! a *separate* thread pool intended for exactly this: it may block as long
//! as it likes and the async runtime keeps going.
//!
//! Inside that blocking thread we call `rayon_pool.install(...)`. rayon's
//! parallel iterators (used by the engine) run on "the current pool"; `install`
//! makes our explicitly-sized pool the current one, so the engine uses
//! exactly `WORKER_THREADS` threads rather than rayon's global default.
//!
//! ## Chunking and cancellation (design choice)
//!
//! `run_hands` cannot be interrupted mid-call. Rather than letting a unit
//! whose lease was lost run to completion (wasted CPU), we split the hand
//! range into up to [`MAX_CHUNKS`] sub-ranges and check a cancel flag between
//! them. This is safe because the engine contract guarantees that
//! splitting a range and merging the stats gives identical results to the
//! whole range, so chunking never changes the answer.
//!
//! ## Panics
//!
//! A bug (or the `FAULT_PANIC_EVERY_N` game-day switch) can panic inside the
//! engine. rayon forwards panics from its worker threads to the caller of
//! `install`, so a single `catch_unwind` around the whole computation
//! catches them all, and the unit is reported as an error instead of
//! killing the process.

use crate::engine::Engine;
use crate::metrics::Metrics;
use hearts_core::{JobSpec, UnitStats};
use hearts_proto::pb;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Upper bound on sub-ranges per unit (and thus on cancel-check granularity).
pub const MAX_CHUNKS: u64 = 64;

/// Game-day fault switches (see `FAULT_*` env vars).
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    pub panic_every_n: u64,
    pub slow_ms: u64,
}

/// What happened to a unit.
#[derive(Debug)]
pub enum UnitOutcome {
    Done(UnitStats),
    /// The cancel flag was raised (lost lease / shutdown). Partial work is dropped.
    Cancelled,
    /// Panic, bad spec, or other failure; the message goes to the coordinator.
    Failed(String),
}

pub struct Runner {
    engine: Arc<dyn Engine>,
    pool: Arc<rayon::ThreadPool>,
    metrics: Arc<Metrics>,
    faults: Faults,
    /// Units started so far; drives `FAULT_PANIC_EVERY_N`.
    started: AtomicU64,
}

impl Runner {
    pub fn new(
        engine: Arc<dyn Engine>,
        pool: Arc<rayon::ThreadPool>,
        metrics: Arc<Metrics>,
        faults: Faults,
    ) -> Runner {
        Runner {
            engine,
            pool,
            metrics,
            faults,
            started: AtomicU64::new(0),
        }
    }

    /// Compute one unit without blocking the async runtime.
    pub async fn run_unit(&self, unit: &pb::WorkUnit, cancel: Arc<AtomicBool>) -> UnitOutcome {
        let spec = match unit.spec.as_ref().map(JobSpec::try_from) {
            Some(Ok(s)) => s,
            Some(Err(e)) => return UnitOutcome::Failed(format!("bad job spec: {e}")),
            None => return UnitOutcome::Failed("work unit has no spec".into()),
        };
        let range = unit.hand_start..unit.hand_end;
        let nth = self.started.fetch_add(1, Ordering::Relaxed) + 1;

        // Everything the blocking closure needs must be owned ('static):
        // the closure may outlive this function call if the future is dropped.
        let engine = self.engine.clone();
        let pool = self.pool.clone();
        let metrics = self.metrics.clone();
        let faults = self.faults;

        self.metrics.busy.set(1);
        let handle = tokio::task::spawn_blocking(move || {
            // AssertUnwindSafe: we promise not to observe broken state after
            // a panic; all we do is turn it into an error message.
            catch_unwind(AssertUnwindSafe(|| {
                pool.install(|| compute(&*engine, &spec, range, &cancel, faults, nth, &metrics))
            }))
        });
        let joined = handle.await;
        self.metrics.busy.set(0);

        match joined {
            Ok(Ok(Some(stats))) => UnitOutcome::Done(stats),
            Ok(Ok(None)) => UnitOutcome::Cancelled,
            Ok(Err(payload)) => UnitOutcome::Failed(format!("panic: {}", panic_message(&payload))),
            // The blocking task itself was cancelled/panicked outside catch_unwind.
            Err(e) => UnitOutcome::Failed(format!("blocking task failed: {e}")),
        }
    }
}

/// The CPU-bound part. Runs on a blocking thread, inside the rayon pool.
/// Returns `None` if cancelled.
fn compute(
    engine: &dyn Engine,
    spec: &JobSpec,
    range: Range<u64>,
    cancel: &AtomicBool,
    faults: Faults,
    nth: u64,
    metrics: &Metrics,
) -> Option<UnitStats> {
    if faults.panic_every_n > 0 && nth.is_multiple_of(faults.panic_every_n) {
        panic!(
            "injected fault: FAULT_PANIC_EVERY_N={} (unit #{nth})",
            faults.panic_every_n
        );
    }
    if faults.slow_ms > 0 {
        // Sleep in slices so a cancel (shutdown / lost lease) is noticed.
        let mut left = faults.slow_ms;
        while left > 0 {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let step = left.min(50);
            std::thread::sleep(Duration::from_millis(step));
            left -= step;
        }
    }

    let mut total = UnitStats::default();
    for chunk in chunk_ranges(range, MAX_CHUNKS) {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let n = chunk.end - chunk.start;
        let stats = engine.run_hands(spec, chunk);
        // Counted even if the unit is later discarded: the metric is
        // "hands simulated, including wasted work".
        metrics.hands_simulated.inc_by(n);
        total.merge(&stats);
    }
    Some(total)
}

/// Split `range` into at most `max_chunks` contiguous, near-equal sub-ranges
/// that together cover it exactly. An empty range yields no chunks.
pub fn chunk_ranges(range: Range<u64>, max_chunks: u64) -> Vec<Range<u64>> {
    if range.start >= range.end {
        return Vec::new();
    }
    let len = range.end - range.start;
    let chunks = len.min(max_chunks.max(1));
    let base = len / chunks;
    let extra = len % chunks; // the first `extra` chunks get one more hand
    let mut out = Vec::with_capacity(chunks as usize);
    let mut start = range.start;
    for i in 0..chunks {
        let size = base + u64::from(i < extra);
        out.push(start..start + size);
        start += size;
    }
    out
}

/// Panic payloads are `Box<dyn Any>`; `panic!("..")` yields `&str` or `String`.
pub fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_range_exactly() {
        for (start, end) in [(0, 1), (0, 10), (5, 2005), (100, 100), (0, 64), (0, 65)] {
            let chunks = chunk_ranges(start..end, 64);
            assert!(chunks.len() <= 64);
            let mut at = start;
            for c in &chunks {
                assert_eq!(c.start, at);
                assert!(c.end > c.start);
                at = c.end;
            }
            assert_eq!(at, if end > start { end } else { start });
        }
    }

    #[test]
    fn chunk_sizes_differ_by_at_most_one() {
        let chunks = chunk_ranges(0..1000, 64);
        let min = chunks.iter().map(|c| c.end - c.start).min();
        let max = chunks.iter().map(|c| c.end - c.start).max();
        assert!(max.unwrap() - min.unwrap() <= 1);
    }
}
