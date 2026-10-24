//! Advisor fan-out: spread one "which card should I play?" question across
//! every worker pod and merge the answers.
//!
//! How it works
//! 1. **Discover shards.** `ADVISOR_SHARD_DNS` points at a Kubernetes
//!    *headless* Service, whose DNS name resolves to one A record per ready
//!    worker pod. We resolve it on **every request** because pods come and go.
//! 2. **Split.** `samples` is divided evenly (the remainder goes to the first
//!    shards). Shard `s` gets seed `child_seed(seed, s)` so shards explore
//!    different random deals, yet the whole answer is reproducible for a fixed
//!    seed and shard count.
//! 3. **Call concurrently** with a per-call timeout of `deadline + 500ms`
//!    (the shard itself stops starting new chunks at `deadline`; the extra
//!    half second covers network and finishing the chunk in flight).
//! 4. **Merge** the `MoveTotals` of the shards that answered. A slow or dead
//!    shard just contributes fewer samples (a "partial" answer) instead of
//!    failing the whole request.
//!
//! gRPC channels are cached per address so we don't redo TCP/HTTP2 setup per
//! request. They're created *lazily* (`connect_lazy`): creating one never
//! blocks, and a connection failure shows up as an error on the call, where
//! we drop the cached channel so the next request reconnects from scratch.

use crate::metrics::Metrics;
use hearts_core::{MoveTotals, PlayerView, seed::child_seed};
use hearts_proto::pb::{self, advisor_shard_client::AdvisorShardClient};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tonic::transport::{Channel, Endpoint};

/// Where shard addresses come from.
enum Source {
    /// Resolve this `host:port` on every request (production).
    Dns(String),
    /// A fixed list (tests, local experiments).
    Static(Vec<SocketAddr>),
}

pub struct Fanout {
    source: Source,
    metrics: Arc<Metrics>,
    /// Cached lazy channels by shard address. A plain `std` mutex is fine:
    /// it is only held for quick map operations, never across an `.await`.
    channels: Mutex<HashMap<SocketAddr, Channel>>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FanoutError {
    #[error("advisor is disabled (ADVISOR_SHARD_DNS is empty)")]
    Disabled,
    #[error("no advisor shards found")]
    NoShards,
    #[error("no advisor shard answered")]
    AllFailed,
}

#[derive(Debug)]
pub struct FanoutResult {
    pub totals: MoveTotals,
    pub samples_requested: u32,
    /// Shards we asked (shards with a zero sample share are not asked).
    pub shards: u32,
    pub shards_failed: u32,
}

/// Split `samples` into `shards` near-equal shares (first shards get the
/// remainder). Shares of zero are dropped, so asking for 2 samples across 5
/// shards uses only 2 shards.
pub fn split_samples(samples: u32, shards: u32) -> Vec<u32> {
    if shards == 0 {
        return vec![];
    }
    let base = samples / shards;
    let extra = samples % shards;
    (0..shards)
        .map(|i| base + u32::from(i < extra))
        .filter(|&n| n > 0)
        .collect()
}

impl Fanout {
    pub fn new(dns: String, metrics: Arc<Metrics>) -> Fanout {
        let source = Source::Dns(dns);
        Fanout::build(source, metrics)
    }

    pub fn with_static_addrs(addrs: Vec<SocketAddr>, metrics: Arc<Metrics>) -> Fanout {
        Fanout::build(Source::Static(addrs), metrics)
    }

    fn build(source: Source, metrics: Arc<Metrics>) -> Fanout {
        Fanout {
            source,
            metrics,
            channels: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        match &self.source {
            Source::Dns(d) => !d.trim().is_empty(),
            Source::Static(_) => true,
        }
    }

    async fn resolve(&self) -> Vec<SocketAddr> {
        let mut addrs = match &self.source {
            Source::Static(a) => a.clone(),
            Source::Dns(name) => match tokio::net::lookup_host(name.as_str()).await {
                Ok(it) => it.collect(),
                Err(e) => {
                    tracing::warn!(dns = %name, error = %e, "advisor shard DNS lookup failed");
                    vec![]
                }
            },
        };
        // Stable order, so shard index -> address (and thus seed) is
        // deterministic while the pod set is unchanged.
        addrs.sort();
        addrs.dedup();
        addrs
    }

    /// Get (or lazily create) the channel for `addr`, and forget channels
    /// for addresses that are no longer in the resolved set.
    fn channel_for(&self, addr: SocketAddr, live: &[SocketAddr]) -> Option<Channel> {
        let mut map = self.channels.lock().ok()?;
        map.retain(|a, _| live.contains(a));
        if let Some(c) = map.get(&addr) {
            return Some(c.clone());
        }
        let endpoint = Endpoint::from_shared(format!("http://{addr}"))
            .ok()?
            .connect_timeout(Duration::from_millis(500));
        let channel = endpoint.connect_lazy();
        map.insert(addr, channel.clone());
        Some(channel)
    }

    fn drop_channel(&self, addr: SocketAddr) {
        if let Ok(mut map) = self.channels.lock() {
            map.remove(&addr);
        }
    }

    /// Run the advisor across all shards. The caller must already have
    /// validated `view`.
    pub async fn advise(
        &self,
        view: &PlayerView,
        samples: u32,
        seed: u64,
        deadline_ms: u32,
    ) -> Result<FanoutResult, FanoutError> {
        if !self.enabled() {
            return Err(FanoutError::Disabled);
        }
        let addrs = self.resolve().await;
        if addrs.is_empty() {
            return Err(FanoutError::NoShards);
        }
        let shares = split_samples(samples, addrs.len() as u32);
        let wire_view = pb::PlayerView::from(view);
        let call_timeout = Duration::from_millis(u64::from(deadline_ms) + 500);

        // One future per shard; `join_all` polls them concurrently on this
        // task, so total latency is the slowest shard, not the sum.
        let calls = shares.iter().enumerate().map(|(i, &share)| {
            let addr = addrs[i];
            let req = pb::SimulateRequest {
                view: Some(wire_view.clone()),
                samples: share,
                seed: child_seed(seed, i as u64),
                deadline_ms,
            };
            let channel = self.channel_for(addr, &addrs);
            async move {
                (
                    addr,
                    self.call_shard(addr, channel, req, call_timeout).await,
                )
            }
        });
        let results = futures_util::future::join_all(calls).await;

        let mut totals = MoveTotals::default();
        let mut answered = 0u32;
        let mut failed = 0u32;
        for (_, r) in results {
            match r {
                Some(t) => {
                    totals.merge(&t);
                    answered += 1;
                }
                None => failed += 1,
            }
        }
        if answered == 0 {
            return Err(FanoutError::AllFailed);
        }
        Ok(FanoutResult {
            totals,
            samples_requested: samples,
            shards: shares.len() as u32,
            shards_failed: failed,
        })
    }

    /// Call one shard, recording latency and classifying failures. Returns
    /// `None` on any failure (already counted and logged).
    async fn call_shard(
        &self,
        addr: SocketAddr,
        channel: Option<Channel>,
        req: pb::SimulateRequest,
        call_timeout: Duration,
    ) -> Option<MoveTotals> {
        let Some(channel) = channel else {
            self.shard_error("connect");
            return None;
        };
        let started = Instant::now();
        let mut client = AdvisorShardClient::new(channel);
        let outcome = tokio::time::timeout(call_timeout, client.simulate(req)).await;
        self.metrics
            .advise_shard_duration
            .observe(started.elapsed().as_secs_f64());
        match outcome {
            Err(_elapsed) => {
                tracing::warn!(%addr, "advisor shard timed out");
                self.shard_error("timeout");
                None
            }
            Ok(Err(status)) => {
                tracing::warn!(%addr, code = ?status.code(), "advisor shard call failed");
                // UNAVAILABLE = could not connect / connection dropped.
                let reason = if status.code() == tonic::Code::Unavailable {
                    "connect"
                } else {
                    "error"
                };
                self.shard_error(reason);
                // Forget the channel so the next request starts fresh.
                self.drop_channel(addr);
                None
            }
            Ok(Ok(resp)) => match MoveTotals::try_from(resp.get_ref()) {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "advisor shard sent a bad response");
                    self.shard_error("error");
                    None
                }
            },
        }
    }

    fn shard_error(&self, reason: &str) {
        self.metrics
            .advise_shard_errors
            .with_label_values(&[reason])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_evenly_with_remainder_first() {
        assert_eq!(split_samples(10, 3), vec![4, 3, 3]);
        assert_eq!(split_samples(9, 3), vec![3, 3, 3]);
        assert_eq!(split_samples(2, 5), vec![1, 1]);
        assert_eq!(split_samples(5, 0), Vec::<u32>::new());
        assert_eq!(split_samples(2000, 1), vec![2000]);
    }
}
