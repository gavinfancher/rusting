//! Integration tests: the real lease loop against an in-process mock
//! coordinator (a genuine tonic server on a loopback port) and a fake engine.
//!
//! None of these touch `hearts_core`'s real engine (still `todo!()` while
//! another agent implements it); the `Engine` trait is what makes that possible.

use hearts_core::{JobSpec, MoveStat, MoveTotals, PlayerView, SlotStats, UnitStats};
use hearts_proto::pb::{
    self,
    advisor_shard_server::AdvisorShard,
    complete_work_request::Outcome,
    worker_service_client::WorkerServiceClient,
    worker_service_server::{WorkerService, WorkerServiceServer},
};
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use worker::advisor_shard::AdvisorService;
use worker::engine::Engine;
use worker::http::Health;
use worker::lease::{self, LeaseCtx, LeaseSettings};
use worker::metrics::Metrics;
use worker::runner::{Faults, Runner};

// ---------------------------------------------------------------------------
// Fake engine
// ---------------------------------------------------------------------------

/// Deterministic, cheap stand-in for the real engine.
/// `slots[0].points` is the sum of the hand indices, which is additive over
/// sub-ranges, so tests can check that chunking + merging is exact.
#[derive(Default)]
struct FakeEngine {
    /// Sleep per `run_hands` call (i.e. per chunk) to make units take time.
    chunk_delay: Duration,
    /// Panic (once) when a chunk contains this hand index.
    panic_once_at: Option<u64>,
    panicked: AtomicBool,
    /// Number of `run_hands` calls so far.
    calls: AtomicU64,
    /// Whether the last `simulate` got a deadline.
    saw_deadline: AtomicBool,
}

impl Engine for FakeEngine {
    fn run_hands(&self, _spec: &JobSpec, range: Range<u64>) -> UnitStats {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.chunk_delay);
        if let Some(at) = self.panic_once_at
            && range.contains(&at)
            && !self.panicked.swap(true, Ordering::SeqCst)
        {
            panic!("fake engine exploded at hand {at}");
        }
        let n = range.end - range.start;
        let mut s = UnitStats {
            hands: n,
            slots: [SlotStats::default(); 4],
        };
        s.slots[0].points = range.clone().sum();
        s.slots[0].hands = n;
        s
    }

    fn simulate(
        &self,
        _view: &PlayerView,
        samples: u32,
        seed: u64,
        deadline: Option<Instant>,
    ) -> MoveTotals {
        self.saw_deadline
            .store(deadline.is_some(), Ordering::SeqCst);
        MoveTotals {
            moves: vec![MoveStat {
                card: "2C".parse().unwrap(),
                total_points: u64::from(samples) * 2 + seed,
                samples,
            }],
            samples_done: samples,
        }
    }
}

// ---------------------------------------------------------------------------
// Mock coordinator
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockState {
    pending: VecDeque<pb::WorkUnit>,
    /// (unit_id, outcome) in arrival order.
    completions: Vec<(i64, Outcome)>,
    done: HashSet<i64>,
    /// Heartbeats naming these ids answer "lost".
    lost: HashSet<i64>,
    lease_calls: u32,
}

#[derive(Clone, Default)]
struct Mock(Arc<Mutex<MockState>>);

impl Mock {
    fn with_units(n: i64, hands_each: u64) -> Mock {
        let m = Mock::default();
        {
            let mut st = m.0.lock().unwrap();
            for i in 0..n {
                st.pending.push_back(pb::WorkUnit {
                    unit_id: i + 1,
                    job_id: 1,
                    hand_start: i as u64 * hands_each,
                    hand_end: (i as u64 + 1) * hands_each,
                    spec: Some(pb::JobSpec {
                        seed: 42,
                        strategies: vec!["random".into(); 4],
                        no_points_first_trick: true,
                    }),
                    lease_expires_at_unix_ms: 0,
                    attempt: 1,
                });
            }
        }
        m
    }
    fn stats_completions(&self) -> Vec<(i64, pb::UnitStats)> {
        self.0
            .lock()
            .unwrap()
            .completions
            .iter()
            .filter_map(|(id, o)| match o {
                Outcome::Stats(s) => Some((*id, s.clone())),
                Outcome::Error(_) => None,
            })
            .collect()
    }
    fn error_completions(&self) -> Vec<(i64, String)> {
        self.0
            .lock()
            .unwrap()
            .completions
            .iter()
            .filter_map(|(id, o)| match o {
                Outcome::Error(e) => Some((*id, e.clone())),
                Outcome::Stats(_) => None,
            })
            .collect()
    }
    fn lease_calls(&self) -> u32 {
        self.0.lock().unwrap().lease_calls
    }
}

#[tonic::async_trait]
impl WorkerService for Mock {
    async fn lease_work(
        &self,
        req: Request<pb::LeaseWorkRequest>,
    ) -> Result<Response<pb::LeaseWorkResponse>, Status> {
        let req = req.into_inner();
        let mut st = self.0.lock().unwrap();
        st.lease_calls += 1;
        let mut units = Vec::new();
        while units.len() < req.max_units as usize {
            match st.pending.pop_front() {
                Some(u) => units.push(u),
                None => break,
            }
        }
        Ok(Response::new(pb::LeaseWorkResponse {
            units,
            retry_after_ms: 50,
        }))
    }

    async fn heartbeat(
        &self,
        req: Request<pb::HeartbeatRequest>,
    ) -> Result<Response<pb::HeartbeatResponse>, Status> {
        let st = self.0.lock().unwrap();
        let lost = req
            .into_inner()
            .unit_ids
            .into_iter()
            .filter(|id| st.lost.contains(id))
            .collect();
        Ok(Response::new(pb::HeartbeatResponse {
            lost_unit_ids: lost,
            lease_expires_at_unix_ms: 0,
        }))
    }

    async fn complete_work(
        &self,
        req: Request<pb::CompleteWorkRequest>,
    ) -> Result<Response<pb::CompleteWorkResponse>, Status> {
        let req = req.into_inner();
        let mut st = self.0.lock().unwrap();
        let outcome = req
            .outcome
            .ok_or_else(|| Status::invalid_argument("no outcome"))?;
        if st.done.contains(&req.unit_id) {
            return Ok(Response::new(pb::CompleteWorkResponse {
                accepted: false,
                job_finished: false,
            }));
        }
        match &outcome {
            Outcome::Stats(_) => {
                st.done.insert(req.unit_id);
            }
            Outcome::Error(_) => {
                // Like the real coordinator: back to pending for a retry.
                // (Reconstruct a unit with the same id/range from the id.)
                // The tests that use errors use 1-hand-per-id layouts below.
                let u = st.requeue_template(req.unit_id);
                st.pending.push_back(u);
            }
        }
        st.completions.push((req.unit_id, outcome));
        Ok(Response::new(pb::CompleteWorkResponse {
            accepted: true,
            job_finished: false,
        }))
    }
}

impl MockState {
    /// Rebuild the unit so an errored one can be re-leased. Units in these
    /// tests all have `HANDS_EACH` hands starting at `(id-1)*HANDS_EACH`.
    fn requeue_template(&self, unit_id: i64) -> pb::WorkUnit {
        pb::WorkUnit {
            unit_id,
            job_id: 1,
            hand_start: (unit_id as u64 - 1) * HANDS_EACH,
            hand_end: unit_id as u64 * HANDS_EACH,
            spec: Some(pb::JobSpec {
                seed: 42,
                strategies: vec!["random".into(); 4],
                no_points_first_trick: true,
            }),
            lease_expires_at_unix_ms: 0,
            attempt: 2,
        }
    }
}

const HANDS_EACH: u64 = 64;

/// Serve `mock` on `listener` until the returned handle is aborted.
fn serve(mock: Mock, listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let _ = tonic::transport::Server::builder()
            .add_service(WorkerServiceServer::new(mock))
            .serve_with_incoming(incoming)
            .await;
    })
}

async fn bind_local() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn settings() -> LeaseSettings {
    LeaseSettings {
        max_units: 1,
        heartbeat: Duration::from_millis(100),
        prefetch: false,
        shutdown_grace: Duration::from_secs(10),
        default_idle_retry: Duration::from_millis(50),
    }
}

struct Harness {
    ctx: Arc<LeaseCtx>,
    metrics: Arc<Metrics>,
    shutdown: CancellationToken,
    run: JoinHandle<()>,
}

fn start_worker(addr: SocketAddr, engine: Arc<FakeEngine>, settings: LeaseSettings) -> Harness {
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap(),
    );
    let metrics = Arc::new(Metrics::new());
    let runner = Arc::new(Runner::new(
        engine,
        pool,
        metrics.clone(),
        Faults::default(),
    ));
    let channel = lease::connect_lazy(&format!("http://{addr}")).unwrap();
    let ctx = Arc::new(LeaseCtx::new(
        WorkerServiceClient::new(channel),
        pb::WorkerInfo {
            worker_id: "test-worker".into(),
            pod_name: "test-worker".into(),
            node_name: "local".into(),
            threads: 2,
        },
        runner,
        metrics.clone(),
        Arc::new(Health::new(Duration::from_secs(30))),
        settings,
    ));
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(lease::run(ctx.clone(), shutdown.clone()));
    Harness {
        ctx,
        metrics,
        shutdown,
        run,
    }
}

impl Harness {
    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), self.run)
            .await
            .expect("worker should stop")
            .unwrap();
    }
    fn counter(&self, result: &str) -> u64 {
        self.metrics
            .units_completed
            .with_label_values(&[result])
            .get()
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn expected_points(unit_id: i64) -> u64 {
    let start = (unit_id as u64 - 1) * HANDS_EACH;
    (start..start + HANDS_EACH).sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completes_every_unit_exactly_once() {
    let mock = Mock::with_units(6, HANDS_EACH);
    let (l, addr) = bind_local().await;
    let _srv = serve(mock.clone(), l);
    let h = start_worker(addr, Arc::new(FakeEngine::default()), settings());

    wait_until("6 completions", || mock.stats_completions().len() == 6).await;
    let done = mock.stats_completions();
    let ids: HashSet<i64> = done.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        ids,
        (1..=6).collect::<HashSet<_>>(),
        "each unit exactly once"
    );
    for (id, stats) in &done {
        assert_eq!(stats.hands, HANDS_EACH);
        // Chunked + merged result must equal the whole-range result.
        assert_eq!(stats.slots[0].points, expected_points(*id));
    }
    assert_eq!(h.counter("accepted"), 6);
    assert_eq!(h.metrics.hands_simulated.get(), 6 * HANDS_EACH);
    assert_eq!(h.metrics.wasted_units.get(), 0);
    assert_eq!(h.ctx.inflight_count(), 0);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetch_still_completes_every_unit_once() {
    let mock = Mock::with_units(5, HANDS_EACH);
    let (l, addr) = bind_local().await;
    let _srv = serve(mock.clone(), l);
    let mut s = settings();
    s.prefetch = true;
    let engine = Arc::new(FakeEngine {
        chunk_delay: Duration::from_millis(2),
        ..Default::default()
    });
    let h = start_worker(addr, engine, s);

    wait_until("5 completions", || mock.stats_completions().len() == 5).await;
    let ids: HashSet<i64> = mock.stats_completions().iter().map(|(id, _)| *id).collect();
    assert_eq!(ids.len(), 5);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_lease_stops_unit_and_counts_as_wasted() {
    // Unit 1 is "lost" as soon as the first heartbeat (~100ms) reports it.
    // 64 chunks * 25ms = 1.6s if it ran to the end.
    let mock = Mock::with_units(2, HANDS_EACH);
    mock.0.lock().unwrap().lost.insert(1);
    let (l, addr) = bind_local().await;
    let _srv = serve(mock.clone(), l);
    let engine = Arc::new(FakeEngine {
        chunk_delay: Duration::from_millis(25),
        ..Default::default()
    });
    let h = start_worker(addr, engine, settings());

    wait_until("unit 2 done", || mock.stats_completions().len() == 1).await;
    let ids: Vec<i64> = mock.stats_completions().iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![2], "the lost unit must not be reported");
    assert_eq!(h.metrics.wasted_units.get(), 1);
    // Unit 1 was abandoned part-way: fewer hands simulated than both units.
    assert!(
        h.metrics.hands_simulated.get() < 2 * HANDS_EACH,
        "unit 1 should have been cut short, simulated {}",
        h.metrics.hands_simulated.get()
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_becomes_error_completion_and_worker_continues() {
    let mock = Mock::with_units(3, HANDS_EACH);
    let (l, addr) = bind_local().await;
    let _srv = serve(mock.clone(), l);
    // Hand 70 belongs to unit 2; it panics the first time only.
    let engine = Arc::new(FakeEngine {
        panic_once_at: Some(70),
        ..Default::default()
    });
    let h = start_worker(addr, engine, settings());

    // The mock re-queues the errored unit; the retry then succeeds.
    wait_until("3 successful completions", || {
        mock.stats_completions().len() == 3
    })
    .await;
    let errors = mock.error_completions();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 2);
    assert!(
        errors[0].1.contains("fake engine exploded"),
        "{}",
        errors[0].1
    );
    assert_eq!(h.counter("error"), 1);
    assert_eq!(h.counter("accepted"), 3);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fault_injection_panics_every_nth_unit() {
    let mock = Mock::with_units(4, HANDS_EACH);
    let (l, addr) = bind_local().await;
    let _srv = serve(mock.clone(), l);

    // Build the worker by hand to set Faults.
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap(),
    );
    let metrics = Arc::new(Metrics::new());
    let runner = Arc::new(Runner::new(
        Arc::new(FakeEngine::default()),
        pool,
        metrics.clone(),
        Faults {
            panic_every_n: 2,
            slow_ms: 0,
        },
    ));
    let ctx = Arc::new(LeaseCtx::new(
        WorkerServiceClient::new(lease::connect_lazy(&format!("http://{addr}")).unwrap()),
        pb::WorkerInfo::default(),
        runner,
        metrics.clone(),
        Arc::new(Health::new(Duration::from_secs(30))),
        settings(),
    ));
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(lease::run(ctx, shutdown.clone()));

    // Every 2nd attempt panics, so errors are re-queued and all 4 finish.
    wait_until("4 completions", || mock.stats_completions().len() == 4).await;
    assert!(!mock.error_completions().is_empty());
    shutdown.cancel();
    let _ = run.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovers_when_coordinator_starts_late() {
    // Reserve a port, release it, and start the worker pointing at it with
    // nothing listening.
    let (l, addr) = bind_local().await;
    drop(l);
    let h = start_worker(addr, Arc::new(FakeEngine::default()), settings());

    wait_until("lease errors while coordinator is down", || {
        h.metrics.lease_errors.with_label_values(&["lease"]).get() >= 2
    })
    .await;
    assert!(
        !h.run.is_finished(),
        "worker must survive a coordinator outage"
    );

    // Now bring the coordinator up on the same port.
    let mock = Mock::with_units(3, HANDS_EACH);
    let l = TcpListener::bind(addr).await.expect("rebind port");
    let _srv = serve(mock.clone(), l);

    wait_until("3 completions after recovery", || {
        mock.stats_completions().len() == 3
    })
    .await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_shutdown_finishes_current_unit_and_leases_no_more() {
    let mock = Mock::with_units(5, HANDS_EACH);
    let (l, addr) = bind_local().await;
    let _srv = serve(mock.clone(), l);
    let engine = Arc::new(FakeEngine {
        chunk_delay: Duration::from_millis(15), // ~1s per unit
        ..Default::default()
    });
    let h = start_worker(addr, engine.clone(), settings());

    wait_until("unit 1 started", || engine.calls.load(Ordering::SeqCst) > 0).await;
    h.shutdown.cancel(); // "SIGTERM" mid-unit
    tokio::time::timeout(Duration::from_secs(10), h.run)
        .await
        .expect("worker should exit after finishing the unit")
        .unwrap();

    assert_eq!(
        mock.stats_completions().len(),
        1,
        "the in-flight unit finishes and is reported"
    );
    let calls = mock.lease_calls();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        mock.lease_calls(),
        calls,
        "no lease requests after shutdown"
    );
    assert_eq!(h.metrics.wasted_units.get(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_grace_expiry_abandons_unit() {
    let mock = Mock::with_units(2, HANDS_EACH);
    let (l, addr) = bind_local().await;
    let _srv = serve(mock.clone(), l);
    let engine = Arc::new(FakeEngine {
        chunk_delay: Duration::from_millis(50), // ~3s per unit
        ..Default::default()
    });
    let mut s = settings();
    s.shutdown_grace = Duration::from_millis(200);
    let h = start_worker(addr, engine.clone(), s);

    wait_until("unit started", || engine.calls.load(Ordering::SeqCst) > 0).await;
    h.shutdown.cancel();
    let t = Instant::now();
    tokio::time::timeout(Duration::from_secs(2), h.run)
        .await
        .expect("must exit once the grace period ends")
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(2));
    assert!(
        mock.stats_completions().is_empty(),
        "abandoned, not reported"
    );
    assert_eq!(h.metrics.wasted_units.get(), 1);
}

// ---------------------------------------------------------------------------
// AdvisorShard
// ---------------------------------------------------------------------------

fn advisor_service(engine: Arc<FakeEngine>) -> (AdvisorService, Arc<Metrics>) {
    let metrics = Arc::new(Metrics::new());
    let svc = AdvisorService {
        engine,
        pool: Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build()
                .unwrap(),
        ),
        metrics: metrics.clone(),
    };
    (svc, metrics)
}

fn view_pb(hand: &[&str]) -> pb::PlayerView {
    pb::PlayerView {
        seat: 0,
        hand: hand.iter().map(|s| s.to_string()).collect(),
        trick_leader: 0,
        trick: vec![],
        played: vec![],
        voids: vec![0; 4],
        hearts_broken: false,
        tricks_played: 0,
        points: vec![0; 4],
        passed_to: None,
        passed_cards: vec![],
        no_points_first_trick: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advisor_simulate_maps_request_and_response() {
    let engine = Arc::new(FakeEngine::default());
    let (svc, metrics) = advisor_service(engine.clone());
    let resp = svc
        .simulate(Request::new(pb::SimulateRequest {
            view: Some(view_pb(&["2C", "QS", "AH"])),
            samples: 100,
            seed: 7,
            deadline_ms: 500,
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.samples_done, 100);
    assert_eq!(resp.moves.len(), 1);
    assert_eq!(resp.moves[0].card, "2C");
    assert_eq!(resp.moves[0].samples, 100);
    assert_eq!(resp.moves[0].total_points, 100 * 2 + 7); // fake: samples*2 + seed
    assert!(engine.saw_deadline.load(Ordering::SeqCst));
    assert_eq!(metrics.advise_samples.get(), 100);
    assert_eq!(metrics.advise_duration.get_sample_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advisor_zero_deadline_means_none_and_bad_view_is_rejected() {
    let engine = Arc::new(FakeEngine::default());
    let (svc, _m) = advisor_service(engine.clone());
    svc.simulate(Request::new(pb::SimulateRequest {
        view: Some(view_pb(&["2C"])),
        samples: 1,
        seed: 0,
        deadline_ms: 0,
    }))
    .await
    .unwrap();
    assert!(!engine.saw_deadline.load(Ordering::SeqCst));

    let err = svc
        .simulate(Request::new(pb::SimulateRequest {
            view: Some(view_pb(&["ZZ"])),
            samples: 1,
            seed: 0,
            deadline_ms: 0,
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
