//! Advisor fan-out tests with mock `AdvisorShard` servers. No database and
//! no engine code needed: we build the `PlayerView` by hand and the mocks
//! return canned statistics.

use coordinator::{
    advisor_fanout::{Fanout, FanoutError},
    metrics::Metrics,
};
use hearts_core::{Card, CardSet, PlayerView, Rules, Trick};
use hearts_proto::pb::{
    self,
    advisor_shard_server::{AdvisorShard, AdvisorShardServer},
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

/// A fake worker. For every call it (optionally) sleeps, then reports that
/// playing 2C costs 1 point per sample and QS costs 10.
struct MockShard {
    delay: Duration,
    fail: bool,
}

#[tonic::async_trait]
impl AdvisorShard for MockShard {
    async fn simulate(
        &self,
        request: Request<pb::SimulateRequest>,
    ) -> Result<Response<pb::SimulateResponse>, Status> {
        tokio::time::sleep(self.delay).await;
        if self.fail {
            return Err(Status::internal("mock failure"));
        }
        let n = request.into_inner().samples;
        Ok(Response::new(pb::SimulateResponse {
            moves: vec![
                pb::MoveStat {
                    card: "2C".into(),
                    total_points: u64::from(n),
                    samples: n,
                },
                pb::MoveStat {
                    card: "QS".into(),
                    total_points: 10 * u64::from(n),
                    samples: n,
                },
            ],
            samples_done: n,
        }))
    }
}

async fn start_mock(delay_ms: u64, fail: bool) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(AdvisorShardServer::new(MockShard {
                delay: Duration::from_millis(delay_ms),
                fail,
            }))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    addr
}

fn view() -> PlayerView {
    PlayerView {
        seat: 0,
        hand: CardSet::parse_list("2C QS AH").unwrap(),
        trick: Trick::new(0),
        played: CardSet::EMPTY,
        voids: [0; 4],
        hearts_broken: false,
        tricks_played: 0,
        points: [0; 4],
        passed: None,
        rules: Rules::default(),
    }
}

#[tokio::test]
async fn merges_all_shards() {
    let metrics = Arc::new(Metrics::new());
    let a = start_mock(0, false).await;
    let b = start_mock(0, false).await;
    let c = start_mock(0, false).await;
    let fan = Fanout::with_static_addrs(vec![a, b, c], metrics.clone());

    let r = fan.advise(&view(), 1000, 7, 2000).await.unwrap();
    assert_eq!(r.shards, 3);
    assert_eq!(r.shards_failed, 0);
    assert_eq!(r.totals.samples_done, 1000, "334 + 333 + 333");
    assert_eq!(r.totals.best(), Some("2C".parse::<Card>().unwrap()));
    let two_c = r
        .totals
        .moves
        .iter()
        .find(|m| m.card.to_string() == "2C")
        .unwrap();
    assert_eq!(two_c.samples, 1000);
    assert!((two_c.mean() - 1.0).abs() < 1e-9);
}

#[tokio::test]
async fn slow_shard_gives_partial_answer() {
    let metrics = Arc::new(Metrics::new());
    let fast = start_mock(0, false).await;
    let slow = start_mock(5_000, false).await; // far past the deadline
    let fan = Fanout::with_static_addrs(vec![fast, slow], metrics.clone());

    let started = std::time::Instant::now();
    // deadline 300ms -> per-call timeout 800ms
    let r = fan.advise(&view(), 200, 1, 300).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "must not wait for the slow shard"
    );
    assert_eq!(r.shards, 2);
    assert_eq!(r.shards_failed, 1);
    assert_eq!(r.totals.samples_done, 100, "only the fast shard's half");
    assert!(r.totals.samples_done < r.samples_requested, "=> partial");
    assert_eq!(
        metrics
            .advise_shard_errors
            .with_label_values(&["timeout"])
            .get(),
        1
    );
}

#[tokio::test]
async fn errors_and_dead_shards_are_counted() {
    let metrics = Arc::new(Metrics::new());
    let good = start_mock(0, false).await;
    let bad = start_mock(0, true).await;
    // A port nobody listens on: bind then drop.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let fan = Fanout::with_static_addrs(vec![good, bad, dead], metrics.clone());
    let r = fan.advise(&view(), 300, 1, 1000).await.unwrap();
    assert_eq!(r.shards_failed, 2);
    assert_eq!(r.totals.samples_done, 100);
    assert_eq!(
        metrics
            .advise_shard_errors
            .with_label_values(&["error"])
            .get(),
        1
    );
    assert_eq!(
        metrics
            .advise_shard_errors
            .with_label_values(&["connect"])
            .get(),
        1
    );
}

#[tokio::test]
async fn no_answer_is_an_error() {
    let metrics = Arc::new(Metrics::new());
    let bad = start_mock(0, true).await;
    let fan = Fanout::with_static_addrs(vec![bad], metrics.clone());
    assert_eq!(
        fan.advise(&view(), 10, 1, 500).await.unwrap_err(),
        FanoutError::AllFailed
    );

    let empty = Fanout::with_static_addrs(vec![], metrics.clone());
    assert_eq!(
        empty.advise(&view(), 10, 1, 500).await.unwrap_err(),
        FanoutError::NoShards
    );

    let disabled = Fanout::new(String::new(), metrics);
    assert!(!disabled.enabled());
    assert_eq!(
        disabled.advise(&view(), 10, 1, 500).await.unwrap_err(),
        FanoutError::Disabled
    );
}

#[tokio::test]
async fn resolves_dns_name_on_each_request() {
    let metrics = Arc::new(Metrics::new());
    let a = start_mock(0, false).await;
    // "host:port" via the system resolver, the same path production uses.
    let fan = Fanout::new(format!("localhost:{}", a.port()), metrics);
    let r = fan.advise(&view(), 50, 1, 1000).await;
    // `localhost` may resolve to ::1 as well as 127.0.0.1; the IPv4 one answers.
    let r = r.unwrap();
    assert!(r.totals.samples_done > 0);
}
