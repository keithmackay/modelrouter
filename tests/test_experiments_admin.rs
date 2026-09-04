//! Experiment administration (spec §7a): create / list / get / close through
//! `/admin/api/experiments`, the pricing gate, audit rows, admin auth, the
//! no-restart effect on the live registry, and the results document served
//! by `GET /admin/api/experiments/:id/results`.

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
    /// The same database as `db`, kept concrete so a test can pin a row's
    /// `created_at` (the repositories always stamp "now").
    sqlite: modelrouter::db::sqlite::SqliteDb,
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
    let sqlite = db.clone();
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
        sqlite,
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

// ── Results ───────────────────────────────────────────────────────────────────

mod seed {
    use super::*;
    use modelrouter::db::models::{
        FailureStage, NewCostLedgerEntry, NewPrompt, NewRequestFailure, NewRunOutcome, NewUser,
    };
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::repositories::failures::FailureRepository;
    use modelrouter::db::repositories::outcomes::OutcomeRepository;
    use modelrouter::db::repositories::prompts::PromptRepository;
    use modelrouter::db::repositories::users::UserRepository;

    pub const ALICE: i64 = 1;

    /// A second user, so a correlation id can be shared across users.
    pub async fn bob(h: &Harness) -> i64 {
        UserRepository::create(&*h.db, NewUser { name: "bob".to_string(), email: None })
            .await
            .unwrap()
            .id
    }

    pub struct Row<'a> {
        pub user: i64,
        pub run: &'a str,
        /// `None` is a turn sent without the experiment header.
        pub variant: Option<&'a str>,
        pub model: &'a str,
        pub cost: f64,
        pub tokens: (i64, i64),
        pub estimated: bool,
        /// RFC3339 `created_at` to pin the row to.
        pub at: &'a str,
    }

    impl<'a> Row<'a> {
        pub fn new(user: i64, run: &'a str, variant: Option<&'a str>, at: &'a str) -> Self {
            Row {
                user,
                run,
                variant,
                model: "openai/gpt-4o-mini",
                cost: 0.01,
                tokens: (100, 50),
                estimated: false,
                at,
            }
        }
    }

    /// One ledger row, pinned to `row.at`. Returns its id.
    pub async fn ledger(h: &Harness, experiment: i64, row: Row<'_>) -> i64 {
        let entry = CostRepository::create(
            &*h.db,
            NewCostLedgerEntry {
                user_id: row.user,
                prompt_id: None,
                model: row.model.to_string(),
                provider: "openai".to_string(),
                project: None,
                tokens_in: row.tokens.0,
                tokens_out: row.tokens.1,
                cost_usd: row.cost,
                api_key_id: None,
                attribution_correlation_id: Some(row.run.to_string()),
                attribution_tags: "{}".to_string(),
                experiment_id: row.variant.map(|_| experiment),
                experiment_variant: row.variant.map(str::to_string),
                tokens_estimated: row.estimated,
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE cost_ledger SET created_at = ? WHERE id = ?")
            .bind(row.at)
            .bind(entry.id)
            .execute(&h.sqlite.pool)
            .await
            .unwrap();
        entry.id
    }

    /// One prompt row with a latency measurement.
    pub async fn prompt(h: &Harness, experiment: i64, user: i64, run: &str, variant: &str, latency_ms: i64) {
        PromptRepository::create(
            &*h.db,
            NewPrompt {
                user_id: user,
                session_id: None,
                request_model: "fast".to_string(),
                routed_model: "openai/gpt-4o-mini".to_string(),
                provider: "openai".to_string(),
                messages: "[{\"role\":\"user\",\"content\":\"hi\"}]".to_string(),
                response: Some("hello".to_string()),
                finish_reason: Some("stop".to_string()),
                prompt_tokens: 100,
                completion_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.01,
                latency_ms: Some(latency_ms),
                tags: "{}".to_string(),
                project: None,
                attribution_correlation_id: Some(run.to_string()),
                attribution_tags: "{}".to_string(),
                experiment_id: Some(experiment),
                experiment_variant: Some(variant.to_string()),
            },
        )
        .await
        .unwrap();
    }

    /// One failed request of a run, pinned to `at`.
    pub async fn failure(h: &Harness, experiment: i64, user: i64, run: &str, variant: &str, at: &str) {
        let row = FailureRepository::create(
            &*h.db,
            NewRequestFailure {
                user_id: Some(user),
                api_key_id: None,
                endpoint: "/v1/chat/completions".to_string(),
                request_model: "fast".to_string(),
                routed_model: Some("openai/gpt-4o-mini".to_string()),
                provider: Some("openai".to_string()),
                stage: FailureStage::Provider,
                status_code: Some(502),
                error_message: "upstream".to_string(),
                attempts: 1,
                latency_ms: None,
                project: None,
                attribution_correlation_id: Some(run.to_string()),
                attribution_tags: "{}".to_string(),
                experiment_id: Some(experiment),
                experiment_variant: Some(variant.to_string()),
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE request_failures SET created_at = ? WHERE id = ?")
            .bind(at)
            .bind(row.id)
            .execute(&h.sqlite.pool)
            .await
            .unwrap();
    }

    /// Report `(outcome, score, rating)` for a run of `variant`.
    pub async fn outcome(
        h: &Harness,
        experiment: i64,
        user: i64,
        run: &str,
        variant: &str,
        report: (&str, Option<f64>, Option<i64>),
    ) {
        OutcomeRepository::upsert(
            &*h.db,
            NewRunOutcome {
                user_id: user,
                attribution_correlation_id: run.to_string(),
                outcome: report.0.to_string(),
                score: report.1,
                rating: report.2,
                note: None,
                experiment_id: Some(experiment),
                experiment_variant: Some(variant.to_string()),
            },
        )
        .await
        .unwrap();
    }
}

/// GET the results of experiment `id` (any id text) with `query` pairs.
async fn results_response(h: &Harness, id: &str, query: &[(&str, &str)]) -> axum_test::TestResponse {
    let (hk, hv) = bearer(&jwt(&h.settings, "admin"));
    let mut req = h
        .server
        .get(&format!("/admin/api/experiments/{id}/results"))
        .add_header(hk, hv);
    for (k, v) in query {
        req = req.add_query_param(k, v);
    }
    req.await
}

async fn results(h: &Harness, experiment: i64) -> Value {
    let res = results_response(h, &experiment.to_string(), &[]).await;
    res.assert_status_ok();
    res.json::<Value>()
}

fn variant<'a>(doc: &'a Value, label: &str) -> &'a Value {
    doc["variants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["label"] == label)
        .unwrap_or_else(|| panic!("no variant {label} in {doc}"))
}

fn run<'a>(doc: &'a Value, user: i64, correlation_id: &str) -> &'a Value {
    doc["runs"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["user_id"] == user && r["correlation_id"] == correlation_id)
        .unwrap_or_else(|| panic!("no run {user}/{correlation_id} in {doc}"))
}

fn approx(v: &Value, expected: f64) -> bool {
    (v.as_f64().unwrap() - expected).abs() < 1e-9
}

const T0: &str = "2026-09-01T10:00:00Z";
const T1: &str = "2026-09-01T10:00:30Z";
const T2: &str = "2026-09-01T10:02:00Z";
const T3: &str = "2026-09-01T11:00:00Z";
const T4: &str = "2026-09-01T11:00:10Z";

/// Two variants, two runs each: the whole document from the ledger up.
#[tokio::test]
async fn results_aggregate_two_variants_over_two_runs_each() {
    let h = build_server().await;
    let id = create(&h, &body("r")).await.json::<Value>()["id"].as_i64().unwrap();
    use seed::{Row, ALICE};

    // control/c1: two turns 30s apart; control/c2: one turn.
    seed::ledger(&h, id, Row { cost: 0.01, tokens: (100, 50), ..Row::new(ALICE, "c1", Some("control"), T0) }).await;
    seed::ledger(&h, id, Row { cost: 0.02, tokens: (200, 100), ..Row::new(ALICE, "c1", Some("control"), T1) }).await;
    seed::ledger(&h, id, Row { cost: 0.03, tokens: (300, 150), ..Row::new(ALICE, "c2", Some("control"), T2) }).await;
    // candidate/k1: one turn on a different model; candidate/k2: two turns 10s apart.
    let haiku = |run, at| Row {
        model: "anthropic/claude-haiku-4-5",
        cost: 0.005,
        tokens: (100, 20),
        ..Row::new(ALICE, run, Some("candidate"), at)
    };
    seed::ledger(&h, id, haiku("k1", T3)).await;
    seed::ledger(&h, id, haiku("k2", T3)).await;
    seed::ledger(&h, id, haiku("k2", T4)).await;
    // Prompt rows only for control (candidate has none).
    seed::prompt(&h, id, ALICE, "c1", "control", 100).await;
    seed::prompt(&h, id, ALICE, "c1", "control", 300).await;
    seed::prompt(&h, id, ALICE, "c2", "control", 500).await;
    // Outcomes: control 1/2 succeeded, candidate 2/2; ratings on three runs.
    seed::outcome(&h, id, ALICE, "c1", "control", ("success", Some(0.9), Some(4))).await;
    seed::outcome(&h, id, ALICE, "c2", "control", ("failure", None, Some(2))).await;
    seed::outcome(&h, id, ALICE, "k1", "candidate", ("success", Some(0.7), None)).await;
    seed::outcome(&h, id, ALICE, "k2", "candidate", ("success", Some(0.5), Some(5))).await;

    let doc = results(&h, id).await;
    assert_eq!(doc["experiment"]["id"], id);
    assert_eq!(doc["experiment"]["name"], "r");
    assert!(chrono::DateTime::parse_from_rfc3339(doc["computed_at"].as_str().unwrap()).is_ok());
    assert!(doc.get("retained_content_bytes").is_none(), "not retaining content");

    let control = variant(&doc, "control");
    assert_eq!(control["runs"], 2);
    assert_eq!(control["requests"], 3);
    assert_eq!(control["turns"], 3);
    assert_eq!(control["mixed_runs"], 0);
    assert!(approx(&control["cost_usd"], 0.06));
    assert_eq!(control["tokens"]["prompt"], 600);
    assert_eq!(control["tokens"]["completion"], 300);
    assert_eq!(control["tokens"]["total"], 900);
    assert!(approx(&control["per_run"]["turns"], 1.5));
    assert!(approx(&control["per_run"]["cost_usd"], 0.03));
    assert!(approx(&control["per_run"]["span_secs"], 15.0), "{}", control["per_run"]);
    assert!(approx(&control["per_request"]["cost_usd"], 0.02));
    assert_eq!(control["latency_samples"], 3);
    assert!(approx(&control["latency"]["mean_ms"], 300.0), "{}", control["latency"]);
    assert_eq!(control["models"].as_array().unwrap().len(), 1);
    assert_eq!(control["models"][0]["model"], "openai/gpt-4o-mini");
    assert_eq!(control["models"][0]["requests"], 3);
    assert_eq!(control["unpriced"], false);
    assert_eq!(control["outcomes"]["reported"], 2);
    assert!(approx(&control["outcomes"]["success_rate"], 0.5));
    assert!(approx(&control["outcomes"]["mean_rating"], 3.0));
    assert_eq!(control["outcomes"]["rating_samples"], 2);
    assert!(approx(&control["outcomes"]["mean_score"], 0.9));

    let candidate = variant(&doc, "candidate");
    assert_eq!(candidate["runs"], 2);
    assert_eq!(candidate["requests"], 3);
    assert!(approx(&candidate["cost_usd"], 0.015));
    assert_eq!(candidate["tokens"]["total"], 360);
    assert!(approx(&candidate["per_run"]["span_secs"], 5.0));
    assert_eq!(candidate["latency"], Value::Null);
    assert_eq!(candidate["latency_samples"], 0);
    assert_eq!(candidate["models"][0]["model"], "anthropic/claude-haiku-4-5");
    assert!(approx(&candidate["outcomes"]["success_rate"], 1.0));
    assert!(approx(&candidate["outcomes"]["mean_rating"], 5.0));
    assert!(approx(&candidate["outcomes"]["mean_score"], 0.6));

    let totals = &doc["totals"];
    assert_eq!(totals["runs"], 4);
    assert_eq!(totals["requests"], 6);
    assert_eq!(totals["turns"], 6);
    assert!(approx(&totals["cost_usd"], 0.075));
    assert_eq!(totals["tokens"]["total"], 1260);
    assert_eq!(totals["latency_samples"], 3);
    assert_eq!(totals["outcomes"]["reported"], 4);
    assert!(approx(&totals["outcomes"]["success_rate"], 0.75));

    // Runs: newest activity first, every figure on the row.
    let runs = &doc["runs"];
    assert_eq!(runs["total"], 4);
    assert_eq!(runs["limit"], 200);
    assert_eq!(runs["offset"], 0);
    let order: Vec<&str> = runs["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["k2", "k1", "c2", "c1"]);
    let c1 = run(&doc, ALICE, "c1");
    assert_eq!(c1["variant"], "control");
    assert_eq!(c1["mixed"], false);
    assert_eq!(c1["requests"], 2);
    assert_eq!(c1["turns"], 2);
    assert!(approx(&c1["cost_usd"], 0.03));
    assert_eq!(c1["tokens"]["total"], 450);
    assert!(approx(&c1["span_secs"], 30.0));
    assert_eq!(c1["first_at"], T0);
    assert_eq!(c1["last_at"], T1);
    assert_eq!(c1["latency_samples"], 2);
    assert!(approx(&c1["latency"]["mean_ms"], 200.0));
    assert_eq!(c1["outcome"]["outcome"], "success");
    assert_eq!(c1["outcome"]["rating"], 4);
    assert_eq!(c1["failures"], 0);
    let k1 = run(&doc, ALICE, "k1");
    assert_eq!(k1["latency"], Value::Null);
    assert_eq!(k1["latency_samples"], 0);
    assert!(approx(&k1["span_secs"], 0.0));
}

/// A run seen under two variants is flagged, counted once, attributed at run
/// level to its earliest variant, and split at request level.
#[tokio::test]
async fn a_mixed_run_is_flagged_and_split_by_level() {
    let h = build_server().await;
    let id = create(&h, &body("mixed")).await.json::<Value>()["id"].as_i64().unwrap();
    use seed::{Row, ALICE};
    seed::ledger(&h, id, Row { cost: 0.01, ..Row::new(ALICE, "m", Some("control"), T0) }).await;
    seed::ledger(&h, id, Row { cost: 0.02, ..Row::new(ALICE, "m", Some("candidate"), T1) }).await;
    seed::ledger(&h, id, Row { cost: 0.04, ..Row::new(ALICE, "m", Some("candidate"), T2) }).await;
    seed::outcome(&h, id, ALICE, "m", "control", ("success", None, None)).await;

    let doc = results(&h, id).await;
    let m = run(&doc, ALICE, "m");
    assert_eq!(m["mixed"], true);
    assert_eq!(m["variant"], "control");
    assert_eq!(m["requests"], 3);
    assert!(approx(&m["cost_usd"], 0.07));

    let control = variant(&doc, "control");
    assert_eq!(control["runs"], 1);
    assert_eq!(control["mixed_runs"], 1);
    assert_eq!(control["turns"], 3, "turns follow the run");
    assert_eq!(control["requests"], 1, "requests follow the row");
    assert!(approx(&control["cost_usd"], 0.01));
    assert!(approx(&control["per_run"]["span_secs"], 120.0));
    assert_eq!(control["outcomes"]["reported"], 1);

    let candidate = variant(&doc, "candidate");
    assert_eq!(candidate["runs"], 0);
    assert_eq!(candidate["mixed_runs"], 0);
    assert_eq!(candidate["turns"], 0);
    assert_eq!(candidate["requests"], 2);
    assert!(approx(&candidate["cost_usd"], 0.06));
    assert_eq!(candidate["per_run"], Value::Null);
    assert_eq!(candidate["outcomes"]["reported"], 0);
    assert_eq!(candidate["outcomes"]["success_rate"], Value::Null);

    assert_eq!(doc["totals"]["runs"], 1);
    assert_eq!(doc["totals"]["mixed_runs"], 1);
    assert_eq!(doc["totals"]["requests"], 3);
    assert_eq!(doc["runs"]["total"], 1);
}

/// The run key is `(user, correlation id)`: the same id under two users is
/// two runs.
#[tokio::test]
async fn the_same_correlation_id_under_two_users_is_two_runs() {
    let h = build_server().await;
    let id = create(&h, &body("users")).await.json::<Value>()["id"].as_i64().unwrap();
    let bob = seed::bob(&h).await;
    use seed::{Row, ALICE};
    seed::ledger(&h, id, Row::new(ALICE, "shared", Some("control"), T0)).await;
    seed::ledger(&h, id, Row::new(bob, "shared", Some("control"), T1)).await;

    let doc = results(&h, id).await;
    assert_eq!(doc["runs"]["total"], 2);
    assert_eq!(variant(&doc, "control")["runs"], 2);
    assert_eq!(run(&doc, ALICE, "shared")["turns"], 1);
    assert_eq!(run(&doc, bob, "shared")["turns"], 1);
}

/// A turn sent without the header shares the run's key but carries no
/// experiment id: it raises `unbound_requests` and nothing else.
#[tokio::test]
async fn a_header_less_turn_is_counted_as_unbound_not_merged() {
    let h = build_server().await;
    let id = create(&h, &body("unbound")).await.json::<Value>()["id"].as_i64().unwrap();
    use seed::{Row, ALICE};
    seed::ledger(&h, id, Row { cost: 0.01, ..Row::new(ALICE, "u", Some("control"), T0) }).await;
    seed::ledger(&h, id, Row { cost: 0.50, tokens: (9000, 9000), ..Row::new(ALICE, "u", None, T1) }).await;
    seed::ledger(&h, id, Row { cost: 0.50, tokens: (9000, 9000), ..Row::new(ALICE, "u", None, T2) }).await;
    // A header-less run with no bound rows is not this experiment's at all.
    seed::ledger(&h, id, Row::new(ALICE, "other", None, T2)).await;

    let doc = results(&h, id).await;
    let u = run(&doc, ALICE, "u");
    assert_eq!(u["unbound_requests"], 2);
    assert_eq!(u["requests"], 1);
    assert_eq!(u["turns"], 1);
    assert!(approx(&u["cost_usd"], 0.01));
    assert_eq!(u["tokens"]["total"], 150);
    assert_eq!(u["last_at"], T0, "unbound rows do not extend the span");
    let control = variant(&doc, "control");
    assert_eq!(control["unbound_requests"], 2);
    assert_eq!(control["requests"], 1);
    assert_eq!(control["turns"], 1);
    assert!(approx(&control["cost_usd"], 0.01));
    assert_eq!(doc["totals"]["unbound_requests"], 2);
    assert_eq!(doc["totals"]["requests"], 1);
    assert_eq!(doc["runs"]["total"], 1);
}

/// A row whose tokens were estimated locally raises `estimated_rows` at
/// every level.
#[tokio::test]
async fn an_estimated_row_is_counted() {
    let h = build_server().await;
    let id = create(&h, &body("est")).await.json::<Value>()["id"].as_i64().unwrap();
    use seed::{Row, ALICE};
    seed::ledger(&h, id, Row::new(ALICE, "e", Some("control"), T0)).await;
    seed::ledger(&h, id, Row { estimated: true, ..Row::new(ALICE, "e", Some("control"), T1) }).await;
    seed::ledger(&h, id, Row::new(ALICE, "f", Some("candidate"), T2)).await;

    let doc = results(&h, id).await;
    assert_eq!(run(&doc, ALICE, "e")["estimated_rows"], 1);
    assert_eq!(run(&doc, ALICE, "f")["estimated_rows"], 0);
    let control = variant(&doc, "control");
    assert_eq!(control["estimated_rows"], 1);
    assert_eq!(control["models"][0]["estimated_rows"], 1);
    assert_eq!(variant(&doc, "candidate")["estimated_rows"], 0);
    assert_eq!(doc["totals"]["estimated_rows"], 1);
}

/// A run whose every request failed has no ledger rows; it still appears,
/// with its failures and no turns. Failures on a run with ledger rows are
/// counted beside them.
#[tokio::test]
async fn a_failure_only_run_appears_with_no_turns() {
    let h = build_server().await;
    let id = create(&h, &body("fail")).await.json::<Value>()["id"].as_i64().unwrap();
    use seed::{Row, ALICE};
    seed::ledger(&h, id, Row::new(ALICE, "ok", Some("control"), T0)).await;
    seed::failure(&h, id, ALICE, "ok", "control", T1).await;
    seed::failure(&h, id, ALICE, "dead", "candidate", T2).await;
    seed::failure(&h, id, ALICE, "dead", "candidate", T3).await;
    seed::outcome(&h, id, ALICE, "dead", "candidate", ("failure", None, Some(1))).await;

    let doc = results(&h, id).await;
    assert_eq!(doc["runs"]["total"], 2);
    let order: Vec<&str> = doc["runs"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["ok", "dead"], "ledger runs first, then failure-only runs");
    let dead = run(&doc, ALICE, "dead");
    assert_eq!(dead["turns"], 0);
    assert_eq!(dead["requests"], 0);
    assert_eq!(dead["failures"], 2);
    assert_eq!(dead["variant"], "candidate");
    assert!(approx(&dead["cost_usd"], 0.0));
    assert!(approx(&dead["span_secs"], 3480.0));
    assert_eq!(dead["first_at"], T2);
    assert_eq!(dead["last_at"], T3);
    assert_eq!(dead["outcome"]["outcome"], "failure");
    let ok = run(&doc, ALICE, "ok");
    assert_eq!(ok["turns"], 1);
    assert_eq!(ok["failures"], 1);

    let candidate = variant(&doc, "candidate");
    assert_eq!(candidate["runs"], 1);
    assert_eq!(candidate["turns"], 0);
    assert_eq!(candidate["failures"], 2);
    assert_eq!(candidate["outcomes"]["failure"], 1);
    assert!(approx(&candidate["outcomes"]["success_rate"], 0.0));
    let control = variant(&doc, "control");
    assert_eq!(control["runs"], 1);
    assert_eq!(control["failures"], 1);
    assert_eq!(doc["totals"]["runs"], 2);
    assert_eq!(doc["totals"]["failures"], 3);
}

/// `retain_content` adds the stored bytes; declared variants with no traffic
/// still appear, empty.
#[tokio::test]
async fn retained_content_bytes_appear_only_when_retaining() {
    let h = build_server().await;
    let mut retaining = body("keep");
    retaining["retain_content"] = json!(true);
    retaining["expires_at"] = json!("2999-01-01T00:00:00Z");
    retaining["content_retention_days"] = json!(30);
    let id = create(&h, &retaining).await.json::<Value>()["id"].as_i64().unwrap();
    use seed::ALICE;
    seed::prompt(&h, id, ALICE, "p", "control", 10).await;

    let doc = results(&h, id).await;
    let messages = "[{\"role\":\"user\",\"content\":\"hi\"}]".len() + "hello".len();
    assert_eq!(doc["retained_content_bytes"], messages as i64);
    assert_eq!(doc["totals"]["runs"], 0, "prompt rows alone make no run");
    assert_eq!(variant(&doc, "control")["latency_samples"], 1);
    let candidate = variant(&doc, "candidate");
    assert_eq!(candidate["runs"], 0);
    assert_eq!(candidate["requests"], 0);
    assert_eq!(candidate["models"], json!([]));
}

/// `limit` and `offset` page the run list; `total` is the whole count.
#[tokio::test]
async fn runs_are_paged_with_limit_and_offset() {
    let h = build_server().await;
    let id = create(&h, &body("page")).await.json::<Value>()["id"].as_i64().unwrap();
    use seed::{Row, ALICE};
    seed::ledger(&h, id, Row::new(ALICE, "first", Some("control"), T0)).await;
    seed::ledger(&h, id, Row::new(ALICE, "second", Some("control"), T1)).await;
    seed::ledger(&h, id, Row::new(ALICE, "third", Some("candidate"), T2)).await;
    seed::failure(&h, id, ALICE, "dead", "candidate", T3).await;

    let h = &h;
    let page = move |q: &'static [(&'static str, &'static str)]| async move {
        let res = results_response(h, &id.to_string(), q).await;
        res.assert_status_ok();
        res.json::<Value>()
    };

    let doc = page(&[("limit", "1"), ("offset", "1")]).await;
    assert_eq!(doc["runs"]["total"], 4);
    assert_eq!(doc["runs"]["limit"], 1);
    assert_eq!(doc["runs"]["offset"], 1);
    let items = doc["runs"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["correlation_id"], "second");
    // Aggregates are not paged.
    assert_eq!(doc["totals"]["runs"], 4);
    assert_eq!(variant(&doc, "control")["runs"], 2);

    // The page straddles the ledger runs and the failure-only tail.
    let doc = page(&[("limit", "2"), ("offset", "2")]).await;
    let ids: Vec<&str> = doc["runs"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["correlation_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["first", "dead"]);

    let doc = page(&[("offset", "3")]).await;
    assert_eq!(doc["runs"]["limit"], 200);
    assert_eq!(doc["runs"]["items"][0]["correlation_id"], "dead");
    assert_eq!(doc["runs"]["items"].as_array().unwrap().len(), 1);

    let doc = page(&[("offset", "4")]).await;
    assert_eq!(doc["runs"]["items"], json!([]));
    assert_eq!(doc["runs"]["total"], 4);
}

#[tokio::test]
async fn results_errors_name_the_offending_field() {
    let h = build_server().await;
    let id = create(&h, &body("bad")).await.json::<Value>()["id"].as_i64().unwrap();

    let id = id.to_string();
    assert_bad_request(&results_response(&h, "999", &[]).await, &["999"]);
    assert_bad_request(&results_response(&h, "abc", &[]).await, &["id"]);
    for bad in ["0", "1001", "-1", "ten"] {
        let res = results_response(&h, &id, &[("limit", bad)]).await;
        assert_bad_request(&res, &["limit"]);
    }
    for bad in ["-1", "x"] {
        let res = results_response(&h, &id, &[("offset", bad)]).await;
        assert_bad_request(&res, &["offset"]);
    }
    // Any admin session reads; no session does not.
    h.server
        .get(&format!("/admin/api/experiments/{id}/results"))
        .await
        .assert_status_unauthorized();
}
