//! Cluster-wide job locks on PostgreSQL: two replicas (two pools) contend for
//! one job, only one wins, and the lock frees when its holder lets go or dies.
//!
//! ```text
//! MODELROUTER_TEST_POSTGRES_URL=postgres://user:pass@localhost/modelrouter_test \
//!     cargo test --features postgres --test test_job_lock_postgres -- --ignored
//! ```
#![cfg(feature = "postgres")]

use modelrouter::db::postgres::PostgresDb;
use modelrouter::db::repositories::maintenance::{with_job_lock, MaintenanceRepository};

fn pg_url() -> String {
    std::env::var("MODELROUTER_TEST_POSTGRES_URL")
        .expect("MODELROUTER_TEST_POSTGRES_URL must point at a scratch PostgreSQL database")
}

fn unique_job(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_POSTGRES_URL"]
async fn one_replica_holds_a_job_at_a_time() {
    let a = PostgresDb::connect(&pg_url()).await.unwrap();
    let b = PostgresDb::connect(&pg_url()).await.unwrap();
    let job = unique_job("contend");

    let held = a.try_job_lock(&job).await.unwrap().expect("first replica wins");
    assert!(b.try_job_lock(&job).await.unwrap().is_none(), "second replica must not win");
    // Session-level advisory locks are re-entrant for the same session, but the
    // holder's connection left the pool, so even the same replica cannot win twice.
    assert!(a.try_job_lock(&job).await.unwrap().is_none(), "the holder's pool must not win again");

    // A different job is independent.
    let other = b.try_job_lock(&unique_job("other")).await.unwrap();
    assert!(other.is_some());

    drop(held);
    // Closing the detached connection is asynchronous on the server side.
    let mut won = None;
    for _ in 0..50 {
        won = b.try_job_lock(&job).await.unwrap();
        if won.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(won.is_some(), "released when the holder drops the guard");
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_POSTGRES_URL"]
async fn a_dead_holder_releases_the_job() {
    let job = unique_job("crash");
    let survivor = PostgresDb::connect(&pg_url()).await.unwrap();
    {
        let doomed = PostgresDb::connect(&pg_url()).await.unwrap();
        let lock = doomed.try_job_lock(&job).await.unwrap().expect("wins");
        assert!(survivor.try_job_lock(&job).await.unwrap().is_none());
        // A crash: the process goes away without running any release path.
        std::mem::forget(lock);
        doomed.pool.close().await;
    }
    // A forgotten guard leaks its connection inside this test process; a real
    // crash closes the socket. Terminate the leaked backend the way the
    // kernel would, by asking the server to end every session holding it.
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_locks \
         WHERE locktype = 'advisory' AND objid = ($1::bigint & 4294967295)::oid \
           AND pid <> pg_backend_pid()",
    )
    .bind(modelrouter::db::repositories::maintenance::job_lock_key(&job))
    .execute(&survivor.pool)
    .await
    .unwrap();
    let mut won = None;
    for _ in 0..50 {
        won = survivor.try_job_lock(&job).await.unwrap();
        if won.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(won.is_some(), "the server frees a dead session's advisory lock");
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_POSTGRES_URL"]
async fn with_job_lock_runs_the_body_on_one_replica_only() {
    let a = PostgresDb::connect(&pg_url()).await.unwrap();
    let b = PostgresDb::connect(&pg_url()).await.unwrap();
    let job = unique_job("body");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let job_a = job.clone();
    let first = tokio::spawn(async move {
        with_job_lock(&a, &job_a, async move {
            started_tx.send(()).unwrap();
            finish_rx.await.unwrap();
            "a"
        })
        .await
    });
    started_rx.await.unwrap();
    assert_eq!(with_job_lock(&b, &job, async { "b" }).await, None, "skipped while a runs");
    finish_tx.send(()).unwrap();
    assert_eq!(first.await.unwrap(), Some("a"));
}
