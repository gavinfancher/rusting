//! # hearts-core
//!
//! The pure game engine: cards, rules, bots, tournament runner, and the
//! Monte Carlo (PIMC) advisor. No I/O, no async, no networking, so it can be
//! tested and benchmarked on its own and reused by the CLI, the worker and
//! the coordinator.
//!
//! Parallelism: `tournament::run_hands` and `advisor::simulate` use rayon and
//! run on whatever rayon thread pool the caller installs (`ThreadPool::install`).
//! That is how the worker's `WORKER_THREADS` setting controls vertical scaling.

pub mod advisor;
pub mod card;
pub mod game;
pub mod learn;
pub mod linear;
pub mod rng;
pub mod rules;
pub mod seed;
pub mod strategy;
pub mod tournament;
pub mod types;

pub use card::{Card, CardSet, ParseCardError, Suit};
pub use types::*;
