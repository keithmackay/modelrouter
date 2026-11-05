//! Cross-replica state in Redis, proven with two limiter instances that
//! share one Redis the way two replicas would. Needs a live Redis:
//!
//! ```text
//! MODELROUTER_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!     cargo test --test test_cluster_redis -- --ignored
//! ```
//!
//! Every test uses its own namespace, so runs never see each other's keys.

use std::sync::Arc;

use modelrouter::api::admin::oidc::OidcStateStore;
use modelrouter::api::middleware::ip_rate_limit::IpRateLimiter;
use modelrouter::cluster::RedisLink;
use modelrouter::config::schema::ClusterConfig;
use modelrouter::router::concurrency::{ConcurrencyLimiter, ConcurrencyPermit};
use modelrouter::router::session_affinity::SessionAffinityMap;
use modelrouter::router::session_limits::SessionLimiter;

fn redis_url() -> String {
    std::env::var("MODELROUTER_TEST_REDIS_URL")
        .expect("MODELROUTER_TEST_REDIS_URL must point at a scratch Redis")
}

fn namespace(test: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("mr-test:{test}:{}:{nanos}", std::process::id())
}

fn link(test: &str) -> Arc<RedisLink> {
    RedisLink::open(&redis_url(), &namespace(test)).expect("open redis link")
}

fn cluster(replicas: u32) -> ClusterConfig {
    ClusterConfig {
        replicas,
        backend: "redis".into(),
        lease_secs: 2,
        ..ClusterConfig::default()
    }
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_REDIS_URL"]
async fn concurrency_permits_are_counted_across_instances_and_released_on_drop() {
    let link = link("cc");
    let a = ConcurrencyLimiter::for_cluster(cluster(2)).with_redis(Some(link.clone()));
    let b = ConcurrencyLimiter::for_cluster(cluster(2)).with_redis(Some(link.clone()));

    let p1 = a.try_acquire(9, 2).await.expect("first slot");
    assert!(
        matches!(p1, ConcurrencyPermit::Shared(_)),
        "counted in redis, not locally"
    );
    let p2 = b
        .try_acquire(9, 2)
        .await
        .expect("second slot, from the other instance");
    assert!(
        a.try_acquire(9, 2).await.is_none(),
        "the full limit of 2 is shared, not 2 per instance"
    );
    assert!(b.try_acquire(9, 2).await.is_none());

    drop(p1);
    // Release is a spawned ZREM; give it a moment.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        b.try_acquire(9, 2).await.is_some(),
        "a slot freed on one instance is visible on the other"
    );
    drop(p2);
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_REDIS_URL"]
async fn a_permit_whose_holder_vanished_expires_after_the_lease() {
    // A crashed replica neither releases its permit nor refreshes it. Model
    // that with a runtime of its own that is torn down while the permit is
    // still "held": the refresh task dies with the runtime, the permit is
    // never dropped, and only the lease can bring the slot back. Each side
    // opens its own link, as two processes would.
    let ns = namespace("cc-lease");
    let crashed = RedisLink::open(&redis_url(), &ns).unwrap();
    let link = RedisLink::open(&redis_url(), &ns).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let a = ConcurrencyLimiter::for_cluster(cluster(1)).with_redis(Some(crashed));
            let p = a.try_acquire(1, 1).await.expect("slot");
            std::mem::forget(p);
        });
    })
    .join()
    .unwrap();

    let b = ConcurrencyLimiter::for_cluster(cluster(1)).with_redis(Some(link));
    assert!(
        b.try_acquire(1, 1).await.is_none(),
        "still held right after the crash"
    );
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert!(
        b.try_acquire(1, 1).await.is_some(),
        "the lease expired and the slot came back"
    );
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_REDIS_URL"]
async fn session_buckets_are_shared_across_instances() {
    let link = link("sess");
    let a = SessionLimiter::new(100, 2)
        .for_cluster(cluster(2))
        .with_redis(Some(link.clone()));
    let b = SessionLimiter::new(100, 2)
        .for_cluster(cluster(2))
        .with_redis(Some(link.clone()));
    assert!(a.check_and_record("s1", 10).await);
    assert!(b.check_and_record("s1", 10).await);
    assert!(
        !a.check_and_record("s1", 10).await,
        "rpm 2 is shared: the third request is refused"
    );
    assert!(!b.check_and_record("s1", 10).await);
    assert!(
        a.check_and_record("s2", 10).await,
        "another session is unaffected"
    );

    // Token budget: 100 shared; 60 then 50 must not both pass.
    let c = SessionLimiter::new(100, 0)
        .for_cluster(cluster(2))
        .with_redis(Some(link.clone()));
    let d = SessionLimiter::new(100, 0)
        .for_cluster(cluster(2))
        .with_redis(Some(link.clone()));
    assert!(c.check_and_record("t", 60).await);
    assert!(!d.check_and_record("t", 50).await);
    assert!(d.check_and_record("t", 40).await);
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_REDIS_URL"]
async fn ip_counters_are_shared_across_instances() {
    let link = link("ip");
    let a = IpRateLimiter::new(3)
        .for_cluster(cluster(3))
        .with_redis(Some(link.clone()));
    let b = IpRateLimiter::new(3)
        .for_cluster(cluster(3))
        .with_redis(Some(link.clone()));
    assert!(a.check_and_increment("10.0.0.1").await);
    assert!(b.check_and_increment("10.0.0.1").await);
    assert!(a.check_and_increment("10.0.0.1").await);
    assert!(
        !b.check_and_increment("10.0.0.1").await,
        "3 per minute across both, not 3 each"
    );
    assert!(b.check_and_increment("10.0.0.2").await);
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_REDIS_URL"]
async fn a_pin_set_on_one_instance_is_honoured_on_another() {
    let link = link("pin");
    let a = SessionAffinityMap::new(60).with_redis(Some(link.clone()));
    let b = SessionAffinityMap::new(60).with_redis(Some(link.clone()));
    assert!(b.get("sess").await.is_none());
    a.set("sess", "prov", "model-x").await;
    let pin = b
        .get("sess")
        .await
        .expect("pin visible from the other instance");
    assert_eq!(
        (pin.provider.as_str(), pin.model.as_str()),
        ("prov", "model-x")
    );
    assert_eq!(a.len(), 0, "nothing was kept locally");
}

#[tokio::test]
#[ignore = "needs MODELROUTER_TEST_REDIS_URL"]
async fn a_login_started_on_one_instance_finishes_once_on_another() {
    let link = link("oidc");
    let a = OidcStateStore::new().with_redis(Some(link.clone()));
    let b = OidcStateStore::new().with_redis(Some(link.clone()));
    a.insert("st".into(), "verifier".into()).await.unwrap();
    assert_eq!(b.take("st").await.as_deref(), Some("verifier"));
    assert!(a.take("st").await.is_none(), "single use");
    assert!(b.take("st").await.is_none());
}

#[tokio::test]
async fn unreachable_redis_falls_back_to_the_local_share_for_limits_and_fails_logins() {
    let dead = RedisLink::open("redis://127.0.0.1:1", "dead").unwrap();
    let ip = IpRateLimiter::new(4)
        .for_cluster(cluster(2))
        .with_redis(Some(dead.clone()));
    assert!(ip.check_and_increment("1.1.1.1").await);
    assert!(ip.check_and_increment("1.1.1.1").await);
    assert!(
        !ip.check_and_increment("1.1.1.1").await,
        "local share of 4 across 2 replicas is 2"
    );

    let sess = SessionLimiter::new(0, 2)
        .for_cluster(cluster(2))
        .with_redis(Some(dead.clone()));
    assert!(sess.check_and_record("s", 1).await);
    assert!(
        !sess.check_and_record("s", 1).await,
        "local share of rpm 2 across 2 replicas is 1"
    );

    let pins = SessionAffinityMap::new(60).with_redis(Some(dead.clone()));
    pins.set("s", "p", "m").await;
    assert!(
        pins.get("s").await.is_some(),
        "pins fall back to the local map"
    );

    let logins = OidcStateStore::new().with_redis(Some(dead));
    let err = logins
        .insert("st".into(), "v".into())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unreachable"), "{err}");
}
