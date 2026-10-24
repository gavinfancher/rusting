//! The engine abstraction.
//!
//! The worker never calls `hearts_core::tournament::run_hands` directly.
//! It talks to the [`Engine`] trait instead. Production uses [`CoreEngine`]
//! (a thin wrapper over `hearts_core`); tests plug in a fake that is fast,
//! deterministic and can be told to panic or sleep. This is the standard
//! Rust way to make code testable: depend on a trait, inject the impl.
//!
//! Both methods are *synchronous and CPU-bound*. Callers are responsible for
//! running them on a blocking thread inside the rayon pool (see `runner`).
//! The `Send + Sync + 'static` bounds let one engine be shared (`Arc<dyn
//! Engine>`) between the lease loop's blocking tasks and the gRPC server.

use hearts_core::{JobSpec, MoveTotals, PlayerView, UnitStats};
use std::ops::Range;
use std::time::Instant;

/// Everything CPU-heavy the worker can be asked to do.
pub trait Engine: Send + Sync + 'static {
    /// Simulate hand indices `range` of `spec`. Must be deterministic, and
    /// splitting a range then merging must equal the whole (see the
    /// `hearts_core::tournament::run_hands` contract); the runner relies on
    /// that to chunk a unit so it can be cancelled between chunks.
    fn run_hands(&self, spec: &JobSpec, range: Range<u64>) -> UnitStats;

    /// PIMC advisor samples. See `hearts_core::advisor::simulate`.
    fn simulate(
        &self,
        view: &PlayerView,
        samples: u32,
        seed: u64,
        deadline: Option<Instant>,
    ) -> MoveTotals;
}

/// The real engine: delegates to `hearts_core`.
pub struct CoreEngine;

impl Engine for CoreEngine {
    fn run_hands(&self, spec: &JobSpec, range: Range<u64>) -> UnitStats {
        hearts_core::tournament::run_hands(spec, range)
    }

    fn simulate(
        &self,
        view: &PlayerView,
        samples: u32,
        seed: u64,
        deadline: Option<Instant>,
    ) -> MoveTotals {
        hearts_core::advisor::simulate(view, samples, seed, deadline)
    }
}
