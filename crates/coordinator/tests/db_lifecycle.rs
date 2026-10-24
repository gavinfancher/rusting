//! Data-layer tests against a real Postgres 18.4: the work-unit lifecycle.

mod common;

use common::*;
use std::collections::HashSet;
use std::time::Duration;

#[tokio::test]
async fn create_job_makes_correct_unit_boundaries() {
    let t = setup(30, 3).await;
    // 10_500 hands in units of 2000: five full units and a last one of 500.
    let id = t.db.create_job(None, &spec(1), 10_500, 2000).await.unwrap();
    let rows: Vec<(i64, i64)> =
        sqlx::query_as("SELECT hand_start, hand_end FROM work_units WHERE job_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(&t.pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            (0, 2000),
            (2000, 4000),
            (4000, 6000),
            (6000, 8000),
            (8000, 10000),
            (10000, 10500)
        ]
    );
    let j = job(&t, id).await;
    assert_eq!(j.status, "pending");
    assert_eq!(j.progress.units_total, 6);
    assert_eq!(j.progress.pending, 6);
    assert_eq!(j.standings.len(), 4);
    assert!(j.standings.iter().all(|s| s.hands == 0));
    assert!(j.started_at.is_none() && j.hands_per_sec.is_none());

    // Exact multiple: no extra empty unit.
    let id2 = t.db.create_job(None, &spec(1), 4000, 2000).await.unwrap();
    assert_eq!(job(&t, id2).await.progress.units_total, 2);
}

#[tokio::test]
async fn concurrent_leases_hand_out_each_unit_exactly_once() {
    let t = setup(30, 3).await;
    let job_id = make_job(&t, 100).await;

    // 8 workers hammer the lease query at once, taking 3 units at a time.
    let mut tasks = vec![];
    for w in 0..8 {
        let db = t.db.clone();
        tasks.push(tokio::spawn(async move {
            let mut mine = vec![];
            loop {
                let got = db.lease(&format!("worker-{w}"), 3).await.unwrap();
                if got.is_empty() {
                    break;
                }
                mine.extend(got.into_iter().map(|u| u.unit_id));
            }
            mine
        }));
    }
    let mut all = vec![];
    for task in tasks {
        all.extend(task.await.unwrap());
    }
    assert_eq!(all.len(), 100, "every unit leased");
    assert_eq!(
        all.iter().collect::<HashSet<_>>().len(),
        100,
        "no unit leased twice"
    );

    let j = job(&t, job_id).await;
    assert_eq!(j.status, "running");
    assert!(j.started_at.is_some());
    assert_eq!(j.progress.leased, 100);
    assert_eq!(j.progress.reclaims, 0);
}

#[tokio::test]
async fn expired_lease_is_reclaimed_by_another_worker() {
    let t = setup(1, 3).await; // 1-second leases
    let job_id = make_job(&t, 1).await;

    let first = t.db.lease("w1", 1).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].attempt, 1);
    assert!(!first[0].reclaimed);

    // Not yet expired: nothing for w2.
    assert!(t.db.lease("w2", 1).await.unwrap().is_empty());

    tokio::time::sleep(Duration::from_millis(1500)).await;

    let second = t.db.lease("w2", 1).await.unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].unit_id, first[0].unit_id);
    assert_eq!(second[0].attempt, 2);
    assert!(second[0].reclaimed);
    assert_eq!(t.metrics.units_reclaimed.get(), 1);
    assert_eq!(job(&t, job_id).await.progress.reclaims, 1);

    // w1 has lost the unit; w2 holds it.
    let (held, lost) = t.db.heartbeat("w1", &[first[0].unit_id]).await.unwrap();
    assert!(held.is_empty());
    assert_eq!(lost, vec![first[0].unit_id]);
    let (held, lost) = t.db.heartbeat("w2", &[first[0].unit_id]).await.unwrap();
    assert_eq!(held.len(), 1);
    assert!(lost.is_empty());

    // Unit stays unique in the DB.
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM work_units")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn completing_twice_is_idempotent() {
    let t = setup(30, 3).await;
    let job_id = t.db.create_job(None, &spec(7), 10, 10).await.unwrap(); // one unit of 10 hands
    let unit = t.db.lease("w1", 1).await.unwrap().remove(0);

    let first =
        t.db.complete_ok("w1", unit.unit_id, &stats(10), 5)
            .await
            .unwrap();
    assert!(first.accepted);
    assert_eq!(first.job_finished.as_deref(), Some("done"));

    // A second report, even from a different worker, is discarded.
    let second =
        t.db.complete_ok("w2", unit.unit_id, &stats(10), 5)
            .await
            .unwrap();
    assert!(!second.accepted);
    assert!(second.job_finished.is_none());
    assert_eq!(t.metrics.units_duplicate.get(), 1);
    assert_eq!(t.metrics.units_completed.get(), 1);
    assert_eq!(t.metrics.hands_completed.get(), 10);

    let j = job(&t, job_id).await;
    assert_eq!(j.progress.hands_done, 10);
    // Counted once: slot i has (i+1) points per hand over 10 hands.
    for (i, s) in j.standings.iter().enumerate() {
        assert_eq!(s.hands, 10);
        assert_eq!(s.points, 10 * (i as i64 + 1));
        assert!((s.avg_points - (i as f64 + 1.0)).abs() < 1e-9);
    }
    assert_eq!(j.standings[0].wins, 10);
    assert!((j.standings[0].win_pct - 100.0).abs() < 1e-9);
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM unit_results")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(rows, 4);
}

#[tokio::test]
async fn last_completion_finishes_the_job() {
    let t = setup(30, 3).await;
    let job_id = make_job(&t, 3).await;
    let units = t.db.lease("w1", 3).await.unwrap();
    assert_eq!(units.len(), 3);

    for u in &units[..2] {
        let o =
            t.db.complete_ok("w1", u.unit_id, &stats(1), 1)
                .await
                .unwrap();
        assert!(o.accepted && o.job_finished.is_none());
    }
    assert_eq!(job(&t, job_id).await.status, "running");

    let o =
        t.db.complete_ok("w1", units[2].unit_id, &stats(1), 1)
            .await
            .unwrap();
    assert_eq!(o.job_finished.as_deref(), Some("done"));
    let j = job(&t, job_id).await;
    assert_eq!(j.status, "done");
    assert!(j.finished_at.is_some());
    assert!(j.elapsed_ms.is_some());
    assert!(j.is_terminal());
    assert_eq!(
        t.metrics.jobs_finished.with_label_values(&["done"]).get(),
        1
    );
}

/// Two workers finish the last two units at the same instant. Without the
/// job-row lock in `finish_job_if_complete`, each transaction could see the
/// other's unit as still leased and the job would never finish.
#[tokio::test]
async fn simultaneous_last_completions_still_finish_the_job() {
    let t = setup(30, 3).await;
    for round in 0..10 {
        let job_id = make_job(&t, 2).await;
        let units = t.db.lease(&format!("w{round}"), 2).await.unwrap();
        let (a, b) = (units[0].unit_id, units[1].unit_id);
        let (db1, db2) = (t.db.clone(), t.db.clone());
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { db1.complete_ok("a", a, &stats(1), 1).await.unwrap() }),
            tokio::spawn(async move { db2.complete_ok("b", b, &stats(1), 1).await.unwrap() }),
        );
        let (r1, r2) = (r1.unwrap(), r2.unwrap());
        assert!(r1.accepted && r2.accepted);
        // Exactly one of them reports the job as finished.
        assert_eq!(
            [r1.job_finished.is_some(), r2.job_finished.is_some()]
                .iter()
                .filter(|x| **x)
                .count(),
            1,
            "round {round}"
        );
        assert_eq!(job(&t, job_id).await.status, "done");
    }
}

#[tokio::test]
async fn errors_retry_then_fail_the_job_at_max_attempts() {
    let t = setup(30, 2).await; // MAX_ATTEMPTS = 2
    let job_id = make_job(&t, 1).await;

    let u = t.db.lease("w1", 1).await.unwrap().remove(0);
    let o = t.db.complete_err("w1", u.unit_id, "boom").await.unwrap();
    assert!(o.accepted);
    assert!(o.job_finished.is_none(), "attempt 1 of 2: back to pending");
    assert_eq!(job(&t, job_id).await.progress.pending, 1);

    // An error from a worker that doesn't hold the lease is ignored.
    let u = t.db.lease("w2", 1).await.unwrap().remove(0);
    assert_eq!(u.attempt, 2);
    assert!(
        !t.db
            .complete_err("w1", u.unit_id, "stale")
            .await
            .unwrap()
            .accepted
    );

    let o =
        t.db.complete_err("w2", u.unit_id, "boom again")
            .await
            .unwrap();
    assert!(o.accepted);
    assert_eq!(o.job_finished.as_deref(), Some("failed"));
    let j = job(&t, job_id).await;
    assert_eq!(j.status, "failed");
    assert_eq!(j.progress.failed, 1);
    assert!(j.finished_at.is_some());
    assert!(
        t.db.lease("w3", 1).await.unwrap().is_empty(),
        "failed units are not leased"
    );
    assert_eq!(t.metrics.units_failed.get(), 1);
    // Counts every error report received, including the ignored stale one.
    assert_eq!(t.metrics.units_errored.get(), 3);
}

#[tokio::test]
async fn sweeper_fails_exhausted_expired_leases_and_finishes_job() {
    let t = setup(1, 1).await; // 1s lease, a single attempt allowed
    let job_id = make_job(&t, 1).await;
    let u = t.db.lease("w1", 1).await.unwrap().remove(0);
    assert_eq!(u.attempt, 1);

    // Not expired yet: sweeper leaves it alone.
    assert_eq!(t.db.sweep_expired().await.unwrap(), 0);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // Expired, but no attempts left, so it can't be re-leased...
    assert!(t.db.lease("w2", 1).await.unwrap().is_empty());
    // ...and the sweeper marks it failed and finishes the job.
    assert_eq!(t.db.sweep_expired().await.unwrap(), 1);
    assert_eq!(t.db.sweep_expired().await.unwrap(), 0, "idempotent");
    let j = job(&t, job_id).await;
    assert_eq!(j.status, "failed");
    assert_eq!(j.progress.failed, 1);
}

#[tokio::test]
async fn cancel_stops_further_leasing() {
    let t = setup(30, 3).await;
    let job_id = make_job(&t, 3).await;
    let u = t.db.lease("w1", 1).await.unwrap().remove(0);

    let j = t.db.cancel_job(job_id).await.unwrap().unwrap();
    assert_eq!(j.status, "cancelled");
    assert!(j.finished_at.is_some());
    assert_eq!(j.progress.cancelled, 3);

    assert!(t.db.lease("w2", 5).await.unwrap().is_empty());
    // The worker computing the leased unit finds out via heartbeat, and its
    // late result is discarded.
    let (_, lost) = t.db.heartbeat("w1", &[u.unit_id]).await.unwrap();
    assert_eq!(lost, vec![u.unit_id]);
    assert!(
        !t.db
            .complete_ok("w1", u.unit_id, &stats(1), 1)
            .await
            .unwrap()
            .accepted
    );

    // Cancelling again is harmless; unknown jobs give None.
    assert_eq!(
        t.db.cancel_job(job_id).await.unwrap().unwrap().status,
        "cancelled"
    );
    assert!(t.db.cancel_job(99_999).await.unwrap().is_none());
}

#[tokio::test]
async fn workers_are_registered_and_listed() {
    let t = setup(30, 3).await;
    make_job(&t, 2).await;
    t.db.upsert_worker("w1", "pod-1", "node-a", 4)
        .await
        .unwrap();
    t.db.lease("w1", 2).await.unwrap();
    t.db.upsert_worker("w1", "pod-1", "node-a", 4)
        .await
        .unwrap(); // upsert again
    let ws = t.db.list_workers().await.unwrap();
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0].worker_id, "w1");
    assert_eq!(ws[0].threads, 4);
    assert!(ws[0].active);
    assert_eq!(ws[0].leased_units, 2);
    assert_eq!(t.db.count_active_workers().await.unwrap(), 1);
}
