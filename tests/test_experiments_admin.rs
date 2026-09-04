//! Experiment administration (spec §7a): create / list / get / close through
//! `/admin/api/experiments`, the pricing gate, audit rows, admin auth, and the
//! no-restart effect on the live registry.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{CacheConfig, LbPoolEntry, LoadBalancerConfig, Settings};
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::cache::ResponseCache;
use modelrouter::router::experiments::ExperimentRegistry;
use modelrouter::router::{
    complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

struct Harness {
    server: TestServer,
    settings: Arc<Settings>,
    db: Arc<dyn DatabaseProvider>,
    experiments: Arc<ExperimentRegistry>,
}

/// Build a server with a config alias (`fast -> openai/gpt-4o-mini`), a
/// config alias to an unpriced model (`mystery -> openai/gpt-unpriced`), a
/// load balancer pool named `pool`, and a seeded user so allow lists can be
/// checked.
async fn build_server() -> Harness {
    let db = common::in_memory_db().await;
    // Audit rows reference admin_users(id); seed the actor the test JWTs claim.
    {
        use modelrouter::db::models::NewAdminUser;
        use modelrouter::db::repositories::admin_users::AdminUserRepository;
        AdminUserRepository::create(
            &db,
            NewAdminUser {
                name: "superadmin-user".to_string(),
                password_hash: "x".to_string(),
                role: "superadmin".to_string(),
            },
        )
        .await
        .unwrap();
    }
    {
        use modelrouter::db::models::NewUser;
        use modelrouter::db::repositories::users::UserRepository;
        UserRepository::create(&db, NewUser { name: "alice".to_string(), email: None })
            .await
            .unwrap();
    }
    let mut base = Settings::default();
    base.routing
        .model_aliases
        .insert("fast".to_string(), "openai/gpt-4o-mini".to_string());
    base.routing
        .model_aliases
        .insert("mystery".to_string(), "openai/gpt-unpriced".to_string());
    let settings = Arc::new(base);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let experiments = Arc::new(ExperimentRegistry::default());

    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        LoadBalancerConfig {
            strategy: Default::default(),
            pool: vec![LbPoolEntry {
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
                weight: 1,
            }],
        },
    );

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router,
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "ok".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
        embedding_registry: Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
            ),
        ),
        search_registry: Arc::new(
            modelrouter::providers::search_registry::SearchRegistry::new_with_mock(
                common::MockSearchAdapter { results: vec![] },
            ),
        ),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(pools)),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: experiments.clone(),
    };
    Harness {
        server: TestServer::new(build_router(state)).unwrap(),
        settings,
        db,
        experiments,
    }
}

fn jwt(settings: &Settings, role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    issue_jwt(
        &AdminClaims {
            sub: 1,
            name: format!("{}-user", role),
            role: role.to_string(),
            exp,
        },
        &settings.auth.jwt_secret,
    )
    .unwrap()
}

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    )
}

/// A valid create body: two variants, never expires, no retention.
fn body(name: &str) -> Value {
    json!({
        "name": name,
        "variants": {
            "control": { "fast": "fast" },
            "candidate": { "fast": "anthropic/claude-haiku-4-5" }
        },
        "expires_at": 0,
        "content_retention_days": 0,
        "retain_content": false
    })
}

async fn create(h: &Harness, body: &Value) -> axum_test::TestResponse {
    let (hk, hv) = bearer(&jwt(&h.settings, "superadmin"));
    h.server
        .post("/admin/api/experiments")
        .add_header(hk, hv)
        .json(body)
        .await
}

fn error_message(res: &axum_test::TestResponse) -> String {
    res.json::<Value>()["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Assert a 400 whose message mentions every one of `needles`.
fn assert_bad_request(res: &axum_test::TestResponse, needles: &[&str]) {
    res.assert_status_bad_request();
    let msg = error_message(res);
    for needle in needles {
        assert!(msg.contains(needle), "expected '{needle}' in: {msg}");
    }
}

#[tokio::test]
async fn create_pins_each_target_and_returns_the_row() {
    let h = build_server().await;
    let res = create(&h, &body("pinning")).await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let row = res.json::<Value>();

    assert!(row["id"].as_i64().unwrap() > 0);
    assert_eq!(row["name"], "pinning");
    assert_eq!(row["status"], "active");
    assert_eq!(row["expires_at"], 0);
    assert_eq!(row["content_retention_days"], 0);
    assert_eq!(row["retain_content"], false);
    assert_eq!(row["feed_learning"], false);
    assert_eq!(row["allowed_user_ids"], json!([]));
    assert!(row["closed_at"].is_null());
    assert!(chrono::DateTime::parse_from_rfc3339(row["created_at"].as_str().unwrap()).is_ok());

    // The alias is stored as written and pinned to what it resolved to.
    let control = &row["variants"]["control"]["fast"];
    assert_eq!(control["target"], "fast");
    assert_eq!(control["provider"], "openai");
    assert_eq!(control["model"], "gpt-4o-mini");
    let candidate = &row["variants"]["candidate"]["fast"];
    assert_eq!(candidate["target"], "anthropic/claude-haiku-4-5");
    assert_eq!(candidate["provider"], "anthropic");
    assert_eq!(candidate["model"], "claude-haiku-4-5");

    // GET returns the same row.
    let (hk, hv) = bearer(&jwt(&h.settings, "admin"));
    let got = h
        .server
        .get(&format!("/admin/api/experiments/{}", row["id"]))
        .add_header(hk, hv)
        .await;
    got.assert_status_ok();
    assert_eq!(got.json::<Value>(), row);
}

#[tokio::test]
async fn list_defaults_to_active_and_all_includes_closed() {
    let h = build_server().await;
    let a = create(&h, &body("a")).await.json::<Value>()["id"].as_i64().unwrap();
    let b = create(&h, &body("b")).await.json::<Value>()["id"].as_i64().unwrap();

    let (sk, sv) = bearer(&jwt(&h.settings, "superadmin"));
    h.server
        .post(&format!("/admin/api/experiments/{a}/close"))
        .add_header(sk, sv)
        .await
        .assert_status_ok();

    let (hk, hv) = bearer(&jwt(&h.settings, "admin"));
    let ids = |res: axum_test::TestResponse| -> Vec<i64> {
        res.json::<Value>()["experiments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_i64().unwrap())
            .collect()
    };

    let active = h.server.get("/admin/api/experiments").add_header(hk.clone(), hv.clone()).await;
    active.assert_status_ok();
    assert_eq!(ids(active), vec![b]);

    let closed = h
        .server
        .get("/admin/api/experiments")
        .add_query_param("status", "closed")
        .add_header(hk.clone(), hv.clone())
        .await;
    assert_eq!(ids(closed), vec![a]);

    let all = h
        .server
        .get("/admin/api/experiments")
        .add_query_param("status", "all")
        .add_header(hk.clone(), hv.clone())
        .await;
    let mut all = ids(all);
    all.sort();
    assert_eq!(all, vec![a, b]);

    let bad = h
        .server
        .get("/admin/api/experiments")
        .add_query_param("status", "paused")
        .add_header(hk, hv)
        .await;
    assert_bad_request(&bad, &["status"]);
}

#[tokio::test]
async fn close_returns_the_row_with_closed_at_and_refuses_a_second_close() {
    let h = build_server().await;
    let id = create(&h, &body("closing")).await.json::<Value>()["id"].as_i64().unwrap();

    let (hk, hv) = bearer(&jwt(&h.settings, "superadmin"));
    let res = h
        .server
        .post(&format!("/admin/api/experiments/{id}/close"))
        .add_header(hk.clone(), hv.clone())
        .await;
    res.assert_status_ok();
    let row = res.json::<Value>();
    assert_eq!(row["id"], id);
    assert_eq!(row["status"], "closed");
    assert!(chrono::DateTime::parse_from_rfc3339(row["closed_at"].as_str().unwrap()).is_ok());

    let again = h
        .server
        .post(&format!("/admin/api/experiments/{id}/close"))
        .add_header(hk.clone(), hv.clone())
        .await;
    assert_bad_request(&again, &["already closed"]);

    let unknown = h
        .server
        .post("/admin/api/experiments/9999/close")
        .add_header(hk.clone(), hv.clone())
        .await;
    assert_bad_request(&unknown, &["9999"]);

    let unknown_get = h
        .server
        .get("/admin/api/experiments/9999")
        .add_header(hk, hv)
        .await;
    assert_bad_request(&unknown_get, &["9999"]);

    use modelrouter::db::repositories::audit::AuditRepository;
    let entries = AuditRepository::list(&*h.db, 50, 0).await.unwrap();
    let close = entries.iter().find(|e| e.action == "experiment.close").unwrap();
    assert_eq!(close.target.as_deref(), Some(&*format!("experiment:{id}")));
    assert!(close.before_json.as_ref().unwrap().contains("active"));
    assert!(close.after_json.as_ref().unwrap().contains("closed"));
}

#[tokio::test]
async fn never_expiring_forever_retained_is_accepted_and_audited_as_never() {
    let h = build_server().await;
    let res = create(&h, &body("forever")).await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let row = res.json::<Value>();
    assert_eq!(row["expires_at"], 0);
    assert_eq!(row["content_retention_days"], 0);

    use modelrouter::db::repositories::audit::AuditRepository;
    let entries = AuditRepository::list(&*h.db, 50, 0).await.unwrap();
    let created = entries.iter().find(|e| e.action == "experiment.create").unwrap();
    assert_eq!(created.target.as_deref(), Some(&*format!("experiment:{}", row["id"])));
    assert!(created.actor_name.contains("superadmin"));
    assert!(created.before_json.is_none());
    let after: Value = serde_json::from_str(created.after_json.as_ref().unwrap()).unwrap();
    assert_eq!(after["expires_at"], "never");
    assert_eq!(after["content_retention_days"], "never");
    assert_eq!(after["name"], "forever");
    assert_eq!(after["variants"]["control"]["fast"]["model"], "gpt-4o-mini");
}

#[tokio::test]
async fn dated_expiry_is_stored_as_epoch_seconds() {
    let h = build_server().await;
    let mut b = body("dated");
    b["expires_at"] = json!("2999-01-01T00:00:00Z");
    b["retain_content"] = json!(true);
    b["content_retention_days"] = json!(30);
    let res = create(&h, &b).await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let row = res.json::<Value>();
    let expected = chrono::DateTime::parse_from_rfc3339("2999-01-01T00:00:00Z")
        .unwrap()
        .timestamp();
    assert_eq!(row["expires_at"], expected);
    assert_eq!(row["retain_content"], true);
    assert_eq!(row["content_retention_days"], 30);
}

#[tokio::test]
async fn retain_content_without_expiry_names_both_fields() {
    let h = build_server().await;
    let mut b = body("retain");
    b["retain_content"] = json!(true);
    let res = create(&h, &b).await;
    assert_bad_request(&res, &["retain_content", "expires_at"]);
}

#[tokio::test]
async fn allowed_user_ids_must_exist() {
    let h = build_server().await;
    let mut b = body("allow");
    b["allowed_user_ids"] = json!([1, 4242]);
    let res = create(&h, &b).await;
    assert_bad_request(&res, &["allowed_user_ids", "4242"]);

    b["allowed_user_ids"] = json!([1]);
    let res = create(&h, &b).await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    assert_eq!(res.json::<Value>()["allowed_user_ids"], json!([1]));
}

#[tokio::test]
async fn variant_shape_errors_name_the_offender() {
    let h = build_server().await;

    // One variant.
    let mut b = body("one");
    b["variants"] = json!({ "only": { "fast": "fast" } });
    assert_bad_request(&create(&h, &b).await, &["variants", "2-16"]);

    // Seventeen variants.
    let mut many = serde_json::Map::new();
    for i in 0..17 {
        many.insert(format!("v{i}"), json!({ "fast": "fast" }));
    }
    b["variants"] = Value::Object(many);
    assert_bad_request(&create(&h, &b).await, &["variants", "17"]);

    // Bad label.
    b["variants"] = json!({ "has space": { "fast": "fast" }, "ok": { "fast": "fast" } });
    assert_bad_request(&create(&h, &b).await, &["has space"]);

    // Duplicate name.
    create(&h, &body("dup")).await.assert_status(axum::http::StatusCode::CREATED);
    assert_bad_request(&create(&h, &body("dup")).await, &["name", "dup"]);

    // Missing expires_at.
    let mut b = body("missing");
    b.as_object_mut().unwrap().remove("expires_at");
    assert_bad_request(&create(&h, &b).await, &["expires_at"]);
}

#[tokio::test]
async fn duplicate_label_is_refused() {
    // A JSON object cannot carry two identical keys, so the duplicate has to
    // be smuggled in as raw text: serde_json keeps the last one and the
    // request then fails the variant-count check, naming `variants`.
    let h = build_server().await;
    let (hk, hv) = bearer(&jwt(&h.settings, "superadmin"));
    let raw = r#"{"name":"duplabel","variants":{"same":{"fast":"fast"},"same":{"fast":"fast"}},"expires_at":0,"content_retention_days":0,"retain_content":false}"#;
    let res = h
        .server
        .post("/admin/api/experiments")
        .add_header(hk, hv)
        .bytes(bytes::Bytes::from_static(raw.as_bytes()))
        .content_type("application/json")
        .await;
    assert_bad_request(&res, &["variants"]);
}

#[tokio::test]
async fn pricing_gate_refuses_pools_substitutions_and_unpriced_models() {
    let h = build_server().await;

    // A load balancer pool.
    let mut b = body("pool");
    b["variants"]["candidate"]["fast"] = json!("pool");
    assert_bad_request(
        &create(&h, &b).await,
        &["variant 'candidate'", "key 'fast'", "target 'pool'", "pool"],
    );

    // A name that resolves nowhere and would be substituted with the default.
    let mut b = body("subst");
    b["variants"]["candidate"]["fast"] = json!("no-such-model");
    assert_bad_request(
        &create(&h, &b).await,
        &["variant 'candidate'", "key 'fast'", "target 'no-such-model'", "substituted"],
    );

    // Resolves fine but has no pricing entry, directly and via an alias.
    let mut b = body("unpriced");
    b["variants"]["candidate"]["fast"] = json!("openai/gpt-unpriced");
    assert_bad_request(
        &create(&h, &b).await,
        &["variant 'candidate'", "key 'fast'", "target 'openai/gpt-unpriced'", "pricing"],
    );
    b["variants"]["candidate"]["fast"] = json!("mystery");
    assert_bad_request(
        &create(&h, &b).await,
        &["target 'mystery'", "openai/gpt-unpriced", "pricing"],
    );

    // Nothing was stored by the refused requests.
    let (hk, hv) = bearer(&jwt(&h.settings, "admin"));
    let all = h
        .server
        .get("/admin/api/experiments")
        .add_query_param("status", "all")
        .add_header(hk, hv)
        .await
        .json::<Value>();
    assert!(all["experiments"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn writes_need_a_superadmin_and_reads_need_a_session() {
    let h = build_server().await;

    // No token at all.
    h.server.get("/admin/api/experiments").await.assert_status_unauthorized();
    h.server
        .post("/admin/api/experiments")
        .json(&body("anon"))
        .await
        .assert_status_unauthorized();

    // Plain admin may read but not write.
    let (hk, hv) = bearer(&jwt(&h.settings, "admin"));
    h.server
        .get("/admin/api/experiments")
        .add_header(hk.clone(), hv.clone())
        .await
        .assert_status_ok();
    h.server
        .post("/admin/api/experiments")
        .add_header(hk.clone(), hv.clone())
        .json(&body("admin"))
        .await
        .assert_status_forbidden();
    h.server
        .post("/admin/api/experiments/1/close")
        .add_header(hk, hv)
        .await
        .assert_status_forbidden();
}

#[tokio::test]
async fn a_created_experiment_binds_without_a_restart() {
    let h = build_server().await;
    assert!(h.experiments.is_empty());

    let id = create(&h, &body("live")).await.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(h.experiments.len(), 1);

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        modelrouter::router::experiments::EXPERIMENT_HEADER,
        axum::http::HeaderValue::from_str(&format!("{id}:candidate")).unwrap(),
    );
    let binding = h
        .experiments
        .bind(&headers, &json!({}), Some("run-1"), 1, chrono::Utc::now().timestamp())
        .unwrap()
        .expect("header present");
    assert_eq!(binding.experiment_id, id);
    assert_eq!(binding.variant, "candidate");
    assert_eq!(binding.overlay["fast"], "anthropic/claude-haiku-4-5");

    // Closing is live too: the same header is now refused as closed.
    let (hk, hv) = bearer(&jwt(&h.settings, "superadmin"));
    h.server
        .post(&format!("/admin/api/experiments/{id}/close"))
        .add_header(hk, hv)
        .await
        .assert_status_ok();
    let err = h
        .experiments
        .bind(&headers, &json!({}), Some("run-1"), 1, chrono::Utc::now().timestamp())
        .unwrap_err();
    assert_eq!(err, modelrouter::router::experiments::BindError::Closed(id));
}
