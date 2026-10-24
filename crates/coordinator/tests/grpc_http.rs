//! End-to-end tests of the two network front doors: the tonic gRPC server
//! (driven with the generated client over a real socket) and the axum router
//! (driven in-process with `tower::ServiceExt::oneshot`).

mod common;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use common::*;
use coordinator::{grpc::WorkerSvc, http};
use hearts_proto::pb::{self, worker_service_client::WorkerServiceClient};
use serde_json::{Value, json};
use tokio_stream::wrappers::TcpListenerStream;
use tower::ServiceExt;

fn worker(id: &str) -> Option<pb::WorkerInfo> {
    Some(pb::WorkerInfo {
        worker_id: id.into(),
        pod_name: format!("{id}-pod"),
        node_name: "node-1".into(),
        threads: 2,
    })
}

fn wire_stats(hands: u64) -> pb::UnitStats {
    pb::UnitStats::from(&stats(hands))
}

#[tokio::test]
async fn grpc_lease_heartbeat_complete_end_to_end() {
    let t = setup(30, 3).await;
    let job_id =
        t.db.create_job(Some("grpc"), &spec(5), 25, 10)
            .await
            .unwrap(); // units: 10, 10, 5
    let svc = WorkerSvc::new(t.db.clone(), t.metrics.clone(), 750).into_server();

    // Real server on a random local port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                let _ = stop_rx.wait_for(|s| *s).await;
            })
            .await
    });
    let mut client = WorkerServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    // Lease two units.
    let resp = client
        .lease_work(pb::LeaseWorkRequest {
            worker: worker("w1"),
            max_units: 2,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.units.len(), 2);
    assert_eq!(resp.retry_after_ms, 0);
    let u0 = &resp.units[0];
    assert_eq!((u0.hand_start, u0.hand_end), (0, 10));
    assert_eq!(u0.attempt, 1);
    assert_eq!(u0.job_id, job_id);
    let spec_back = u0.spec.as_ref().unwrap();
    assert_eq!(spec_back.seed, 5);
    assert_eq!(
        spec_back.strategies,
        vec!["heuristic", "heuristic", "random", "random"]
    );
    assert!(u0.lease_expires_at_unix_ms > 0);

    // Heartbeat: both held, plus a bogus id reported lost.
    let ids: Vec<i64> = resp.units.iter().map(|u| u.unit_id).collect();
    let mut hb_ids = ids.clone();
    hb_ids.push(987_654);
    let hb = client
        .heartbeat(pb::HeartbeatRequest {
            worker_id: "w1".into(),
            unit_ids: hb_ids,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(hb.lost_unit_ids, vec![987_654]);
    assert!(hb.lease_expires_at_unix_ms > 0);

    // Complete both units; the third is leased by another worker and finishes the job.
    for (i, u) in resp.units.iter().enumerate() {
        let r = client
            .complete_work(pb::CompleteWorkRequest {
                worker_id: "w1".into(),
                unit_id: u.unit_id,
                outcome: Some(pb::complete_work_request::Outcome::Stats(wire_stats(10))),
                compute_ms: 12,
            })
            .await
            .unwrap()
            .into_inner();
        assert!(r.accepted, "unit {i}");
        assert!(!r.job_finished);
    }
    let last = client
        .lease_work(pb::LeaseWorkRequest {
            worker: worker("w2"),
            max_units: 0,
        }) // 0 -> 1
        .await
        .unwrap()
        .into_inner();
    assert_eq!(last.units.len(), 1);
    assert_eq!((last.units[0].hand_start, last.units[0].hand_end), (20, 25));
    let done = client
        .complete_work(pb::CompleteWorkRequest {
            worker_id: "w2".into(),
            unit_id: last.units[0].unit_id,
            outcome: Some(pb::complete_work_request::Outcome::Stats(wire_stats(5))),
            compute_ms: 3,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(done.accepted && done.job_finished);

    // Duplicate report -> accepted=false.
    let dup = client
        .complete_work(pb::CompleteWorkRequest {
            worker_id: "w2".into(),
            unit_id: last.units[0].unit_id,
            outcome: Some(pb::complete_work_request::Outcome::Stats(wire_stats(5))),
            compute_ms: 3,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!dup.accepted);

    // No more work: empty list and the configured retry hint.
    let none = client
        .lease_work(pb::LeaseWorkRequest {
            worker: worker("w1"),
            max_units: 4,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(none.units.is_empty());
    assert_eq!(none.retry_after_ms, 750);

    // Input validation maps to INVALID_ARGUMENT.
    let bad = client
        .complete_work(pb::CompleteWorkRequest {
            worker_id: "w1".into(),
            unit_id: 1,
            outcome: Some(pb::complete_work_request::Outcome::Stats(pb::UnitStats {
                hands: 1,
                slots: vec![], // must be 4
            })),
            compute_ms: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(bad.code(), tonic::Code::InvalidArgument);
    let bad = client
        .lease_work(pb::LeaseWorkRequest {
            worker: None,
            max_units: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(bad.code(), tonic::Code::InvalidArgument);

    let j = job(&t, job_id).await;
    assert_eq!(j.status, "done");
    assert_eq!(j.progress.hands_done, 25);
    assert_eq!(j.standings[3].hands, 25);

    // Metrics saw the traffic.
    let text = t.metrics.render();
    assert!(text.contains("hearts_grpc_request_duration_seconds_count{method=\"LeaseWork\"}"));
    assert_eq!(t.metrics.units_leased.get(), 3);
    assert_eq!(t.metrics.units_completed.get(), 3);

    // Graceful shutdown.
    stop_tx.send(true).unwrap();
    server.await.unwrap().unwrap();
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

#[tokio::test]
async fn http_jobs_api_shapes_and_validation() {
    let t = setup(30, 3).await;
    let (state, _stop) = t.app_state(|_| {});
    let app = http::router(state);

    // --- create ---
    let (status, job) = send(
        &app,
        "POST",
        "/api/jobs",
        Some(json!({
            "name": "heuristic vs random",
            "strategies": ["heuristic", "heuristic", "random", "pimc:200"],
            "num_hands": 10500, "seed": 42, "unit_size": 2000,
            "rules": {"no_points_first_trick": false}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    let id = job["id"].as_i64().unwrap();
    assert_eq!(job["status"], "pending");
    assert_eq!(job["name"], "heuristic vs random");
    assert_eq!(job["spec"]["seed"], 42);
    assert_eq!(
        job["spec"]["strategies"],
        json!(["heuristic", "heuristic", "random", "pimc:200"])
    );
    assert_eq!(job["spec"]["rules"]["no_points_first_trick"], false);
    assert_eq!(job["num_hands"], 10500);
    assert_eq!(job["unit_size"], 2000);
    assert!(job["created_at"].as_str().unwrap().contains('T'));
    assert!(job["started_at"].is_null() && job["finished_at"].is_null());
    assert_eq!(job["progress"]["units_total"], 6);
    assert_eq!(job["progress"]["pending"], 6);
    assert_eq!(job["progress"]["hands_done"], 0);
    assert_eq!(job["progress"]["reclaims"], 0);
    let standings = job["standings"].as_array().unwrap();
    assert_eq!(standings.len(), 4);
    assert_eq!(standings[3]["strategy"], "pimc:200");
    assert_eq!(standings[0]["slot"], 0);
    assert_eq!(standings[0]["hands"], 0);
    assert!(job["hands_per_sec"].is_null());

    // --- defaults: no name / seed / unit_size / rules ---
    let (status, j2) = send(
        &app,
        "POST",
        "/api/jobs",
        Some(json!({"strategies": ["random","random","random","random"], "num_hands": 5000})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(j2["unit_size"], 2000);
    assert_eq!(j2["progress"]["units_total"], 3);
    assert!(j2["spec"]["seed"].as_u64().unwrap() < (1 << 53));
    assert_eq!(j2["spec"]["rules"]["no_points_first_trick"], true);

    // --- get / list ---
    let (status, got) = send(&app, "GET", &format!("/api/jobs/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["id"], id);
    let (status, list) = send(&app, "GET", "/api/jobs?limit=1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], j2["id"], "newest first");
    let (_, list) = send(&app, "GET", "/api/jobs", None).await;
    assert_eq!(list.as_array().unwrap().len(), 2);

    // --- errors are JSON {"error": ..} ---
    for (body, why) in [
        (
            json!({"strategies": ["random"], "num_hands": 10}),
            "needs 4 strategies",
        ),
        (
            json!({"strategies": ["a","b","c","d"], "num_hands": 10}),
            "unknown strategy",
        ),
        (
            json!({"strategies": ["random","random","random","random"], "num_hands": 0}),
            "num_hands 0",
        ),
        (
            json!({"strategies": ["random","random","random","random"], "num_hands": 2000000000}),
            "too many hands",
        ),
        (
            json!({"strategies": ["random","random","random","random"], "num_hands": 10, "unit_size": 0}),
            "unit 0",
        ),
        (
            json!({"strategies": ["random","random","random","random"], "num_hands": 10, "seed": 9007199254740993u64}),
            "seed too big",
        ),
        (
            json!({"strategies": ["random","random","random","random"], "num_hands": 1000000000, "unit_size": 100}),
            "too many units",
        ),
    ] {
        let (status, body) = send(&app, "POST", "/api/jobs", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
        assert!(body["error"].is_string(), "{why}");
    }
    let (status, body) = send(&app, "GET", "/api/jobs/999999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
    let (status, _) = send(&app, "GET", "/api/jobs/abc", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/jobs")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // --- cancel ---
    let (status, cancelled) = send(&app, "POST", &format!("/api/jobs/{id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["progress"]["cancelled"], 6);
    let (status, _) = send(&app, "POST", "/api/jobs/999999/cancel", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_health_metrics_ui_workers_benchmarks_and_advise_disabled() {
    let t = setup(30, 3).await;
    let (state, _stop) = t.app_state(|_| {});
    let app = http::router(state.clone());

    let (status, _) = send(&app, "GET", "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, "GET", "/readyz", None).await;
    assert_eq!(status, StatusCode::OK);

    // UI is embedded.
    let resp = app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let (status, _) = send(&app, "GET", "/no-such-file.js", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Advisor disabled (ADVISOR_SHARD_DNS empty) -> 503 before touching the engine.
    let (status, body) = send(&app, "POST", "/api/advise", Some(json!({"hand": ["2C"]}))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body["error"].is_string());

    // Workers.
    t.db.upsert_worker("w-http", "pod-x", "node-y", 3)
        .await
        .unwrap();
    let (status, workers) = send(&app, "GET", "/api/workers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(workers[0]["worker_id"], "w-http");
    assert_eq!(workers[0]["active"], true);
    assert_eq!(workers[0]["leased_units"], 0);
    assert_eq!(workers[0]["threads"], 3);

    // Benchmarks: fill missing fields from the job.
    let job_id = t.db.create_job(None, &spec(1), 4000, 2000).await.unwrap();
    for u in t.db.lease("w-http", 2).await.unwrap() {
        t.db.complete_ok("w-http", u.unit_id, &stats(2000), 100)
            .await
            .unwrap();
    }
    let (status, b) = send(
        &app,
        "POST",
        "/api/benchmarks",
        Some(json!({"experiment": "E2-horizontal", "job_id": job_id, "worker_replicas": 3, "notes": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    assert_eq!(b["hands"], 4000);
    assert_eq!(b["unit_size"], 2000);
    assert!(b["wall_ms"].is_i64() || b["wall_ms"].is_u64());
    assert_eq!(b["worker_replicas"], 3);
    let (_, _) = send(
        &app,
        "POST",
        "/api/benchmarks",
        Some(json!({"experiment": "other"})),
    )
    .await;
    let (_, all) = send(&app, "GET", "/api/benchmarks", None).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    let (_, some) = send(
        &app,
        "GET",
        "/api/benchmarks?experiment=E2-horizontal",
        None,
    )
    .await;
    assert_eq!(some.as_array().unwrap().len(), 1);
    let (status, _) = send(
        &app,
        "POST",
        "/api/benchmarks",
        Some(json!({"experiment": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Background tick refreshes the polled gauges.
    coordinator::background::tick(&state).await;

    // Metrics endpoint.
    let resp = app
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let text = String::from_utf8(
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for name in [
        "hearts_units_leased_total 2",
        "hearts_units_completed_total 2",
        "hearts_hands_completed_total 4000",
        "hearts_jobs_created_total 1",
        "hearts_jobs_finished_total{status=\"done\"} 1",
        "hearts_units{state=\"done\"} 2",
        "hearts_workers_active 1",
        "hearts_db_query_duration_seconds_count{query=\"lease\"}",
        "hearts_db_pool_connections{state=\"idle\"}",
        "hearts_http_requests_total{route=\"/healthz\",status=\"200\"} 1",
    ] {
        assert!(text.contains(name), "missing {name} in:\n{text}");
    }
}

#[tokio::test]
async fn http_route_label_uses_pattern_not_concrete_url() {
    let t = setup(30, 3).await;
    let (state, _stop) = t.app_state(|_| {});
    let app = http::router(state);
    send(&app, "GET", "/api/jobs/12345", None).await;
    let text = t.metrics.render();
    assert!(
        text.contains("hearts_http_requests_total{route=\"/api/jobs/:id\",status=\"404\"} 1"),
        "{text}"
    );
    assert!(!text.contains("12345"));
}

#[tokio::test]
async fn sse_sends_done_for_finished_job_and_closes() {
    let t = setup(30, 3).await;
    let (state, _stop) = t.app_state(|_| {});
    let app = http::router(state);
    let job_id = t.db.create_job(None, &spec(1), 1, 1).await.unwrap();
    let u = t.db.lease("w", 1).await.unwrap().remove(0);
    t.db.complete_ok("w", u.unit_id, &stats(1), 1)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/jobs/{job_id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    // The stream ends by itself after the `done` event, so reading the whole body terminates.
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        to_bytes(resp.into_body(), usize::MAX),
    )
    .await
    .expect("stream should close")
    .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: done"), "{text}");
    let data_line = text.lines().find(|l| l.starts_with("data: ")).unwrap();
    let data: Value = serde_json::from_str(&data_line[6..]).unwrap();
    assert_eq!(data["status"], "done");
    assert_eq!(data["id"], job_id);

    let (status, _) = send(&app, "GET", "/api/jobs/424242/events", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sse_emits_progress_while_running() {
    let t = setup(30, 3).await;
    let (state, stop) = t.app_state(|_| {});
    let app = http::router(state);
    let job_id = t.db.create_job(None, &spec(1), 2, 1).await.unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/jobs/{job_id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Shutting the server down ends the (otherwise endless) stream.
    let reader = tokio::spawn(async move { to_bytes(resp.into_body(), usize::MAX).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    stop.send(true).unwrap();
    let body = tokio::time::timeout(std::time::Duration::from_secs(5), reader)
        .await
        .expect("stream closes on shutdown")
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("event: progress"), "{text}");
    assert!(!text.contains("event: done"));
}
