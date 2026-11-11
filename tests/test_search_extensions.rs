//! Request extensions on `/v1/search`: a hook can rewrite what is sent,
//! refuse the request, or fail; the provider and the cache see only what the
//! hooks allow.

mod common;

use std::sync::{Arc, Mutex};

use common::search_app::{search_app, RecordingSearchAdapter};
use modelrouter::config::schema::CacheConfig;
use modelrouter::db::repositories::failures::FailureRepository;
use modelrouter::extensions::{
    Extensions, HookOutcome, Refusal, RequestContext, RequestExtension, RuleHit, SearchText,
};
use modelrouter::providers::search_registry::SearchRegistry;
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum Behaviour {
    /// Replace "secret" with "public" in every field.
    Rewrite,
    /// Refuse any query containing "secret".
    Refuse,
    /// Return an error.
    Fail,
    /// Rewrite the whole query to blank.
    Blank,
}

struct TestExtension {
    behaviour: Behaviour,
    seen: Arc<Mutex<Vec<RequestContext>>>,
}

#[async_trait::async_trait]
impl RequestExtension for TestExtension {
    fn name(&self) -> &str {
        "test-ext"
    }

    async fn on_search(&self, ctx: &RequestContext, text: &mut SearchText) -> anyhow::Result<HookOutcome> {
        self.seen.lock().unwrap().push(ctx.clone());
        let hit = || vec![RuleHit { rule: "word".into(), category: "test".into(), action: "redact".into() }];
        match self.behaviour {
            Behaviour::Rewrite => {
                let swap = |s: &str| s.replace("secret", "public");
                text.query = swap(&text.query);
                text.instructions = text.instructions.as_deref().map(swap);
                text.context = text.context.as_deref().map(swap);
                let mut response_fields = serde_json::Map::new();
                response_fields.insert("policy".into(), json!({ "sent_query": text.query }));
                Ok(HookOutcome { refusal: None, hits: hit(), response_fields })
            }
            Behaviour::Refuse if text.query.contains("secret") => Ok(HookOutcome {
                refusal: Some(Refusal {
                    status: 422,
                    body: json!({ "error": { "code": "refused_by_test", "rule": "word" } }),
                    reason: "refused by rule word".into(),
                }),
                hits: hit(),
                response_fields: serde_json::Map::new(),
            }),
            Behaviour::Refuse => Ok(HookOutcome::proceed()),
            Behaviour::Fail => anyhow::bail!("matcher unavailable"),
            Behaviour::Blank => {
                text.query = "   ".into();
                Ok(HookOutcome::proceed())
            }
        }
    }

    fn status(&self) -> Value {
        json!({ "policy_hash": "abc123" })
    }
}

struct App {
    server: axum_test::TestServer,
    db: Arc<dyn modelrouter::api::app::DatabaseProvider>,
    provider: RecordingSearchAdapter,
    seen: Arc<Mutex<Vec<RequestContext>>>,
}

async fn app(behaviour: Option<Behaviour>, cache: CacheConfig) -> App {
    let provider = RecordingSearchAdapter::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let extensions = match behaviour {
        Some(behaviour) => Extensions::new(vec![Arc::new(TestExtension { behaviour, seen: seen.clone() })]),
        None => Extensions::default(),
    };
    let (server, db) =
        search_app(vec![], cache, SearchRegistry::new_with_mock(provider.clone()), None, extensions).await;
    App { server, db, provider, seen }
}

async fn post(app: &App, body: Value) -> axum_test::TestResponse {
    app.server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .add_header(
            axum::http::HeaderName::from_static("x-project"),
            axum::http::HeaderValue::from_static("proj-7"),
        )
        .add_header(
            axum::http::HeaderName::from_static("x-attribution-correlation-id"),
            axum::http::HeaderValue::from_static("corr-1"),
        )
        .json(&body)
        .await
}

fn sent(app: &App) -> Vec<modelrouter::providers::search::SearchRequest> {
    app.provider.requests.lock().unwrap().clone()
}

fn cache_on() -> CacheConfig {
    CacheConfig { enabled: true, max_entries: 10, ttl_seconds: 60, ..Default::default() }
}

#[tokio::test]
async fn a_rewrite_is_what_the_provider_receives_in_every_text_field() {
    let app = app(Some(Behaviour::Rewrite), CacheConfig::default()).await;
    let resp = post(&app, json!({
        "query": "secret topic",
        "include_answer": true,
        "instructions": "answer about the secret",
        "context": "secret background",
    }))
    .await;
    assert_eq!(resp.status_code(), 200);
    let [req] = sent(&app).try_into().expect("exactly one provider call");
    assert_eq!(req.query, "public topic");
    assert_eq!(req.instructions.as_deref(), Some("answer about the public"));
    assert_eq!(req.context.as_deref(), Some("public background"));
}

#[tokio::test]
async fn the_hook_sees_the_project_and_correlation_id() {
    let app = app(Some(Behaviour::Rewrite), CacheConfig::default()).await;
    post(&app, json!({ "query": "anything" })).await;
    let [ctx] = app.seen.lock().unwrap().clone().try_into().expect("one hook call");
    assert_eq!(ctx.endpoint, "/v1/search");
    assert_eq!(ctx.project.as_deref(), Some("proj-7"));
    assert_eq!(ctx.correlation_id.as_deref(), Some("corr-1"));
}

#[tokio::test]
async fn a_refusal_returns_the_extension_body_and_never_reaches_the_provider() {
    let app = app(Some(Behaviour::Refuse), CacheConfig::default()).await;
    let resp = post(&app, json!({ "query": "a secret query" })).await;
    assert_eq!(resp.status_code(), 422);
    assert_eq!(resp.json::<Value>(), json!({ "error": { "code": "refused_by_test", "rule": "word" } }));
    assert!(sent(&app).is_empty(), "a refused query must not be sent");

    let failures = FailureRepository::list(&*app.db, 10, 0).await.unwrap();
    let [failure] = failures.try_into().expect("one failure row");
    assert_eq!(failure.stage, "policy");
    assert_eq!(failure.status_code, Some(422));
    assert_eq!(failure.provider.as_deref(), Some("extension:test-ext"));
    assert_eq!(failure.error_message, "refused by rule word");
}

#[tokio::test]
async fn a_failing_hook_refuses_the_request_rather_than_sending_it_unchecked() {
    let app = app(Some(Behaviour::Fail), CacheConfig::default()).await;
    let resp = post(&app, json!({ "query": "anything" })).await;
    assert_eq!(resp.status_code(), 503);
    assert_eq!(resp.json::<Value>()["error"]["code"], "extension_error");
    assert!(sent(&app).is_empty());
}

#[tokio::test]
async fn a_query_rewritten_to_nothing_is_a_400_not_an_empty_search() {
    let app = app(Some(Behaviour::Blank), CacheConfig::default()).await;
    let resp = post(&app, json!({ "query": "anything" })).await;
    assert_eq!(resp.status_code(), 400);
    assert!(sent(&app).is_empty());
}

#[tokio::test]
async fn the_cache_is_keyed_and_filled_after_the_rewrite() {
    let app = app(Some(Behaviour::Rewrite), cache_on()).await;
    let first = post(&app, json!({ "query": "secret topic" })).await;
    assert_eq!(first.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    // The stored entry is under the rewritten text, so the rewritten query hits it.
    let second = post(&app, json!({ "query": "public topic" })).await;
    assert_eq!(second.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    assert_eq!(sent(&app).len(), 1);
}

#[tokio::test]
async fn a_refused_query_is_never_cached_or_served_from_cache() {
    let app = app(Some(Behaviour::Refuse), cache_on()).await;
    assert_eq!(post(&app, json!({ "query": "a secret query" })).await.status_code(), 422);
    assert_eq!(post(&app, json!({ "query": "a secret query" })).await.status_code(), 422);
    assert!(sent(&app).is_empty());
}

#[tokio::test]
async fn health_names_the_loaded_extensions_and_omits_the_block_without_them() {
    let with = app(Some(Behaviour::Rewrite), CacheConfig::default()).await;
    let body: Value = with.server.get("/health").await.json();
    assert_eq!(body["extensions"], json!([{ "name": "test-ext", "policy_hash": "abc123" }]));

    let without = app(None, CacheConfig::default()).await;
    let body: Value = without.server.get("/health").await.json();
    assert!(body.get("extensions").is_none());
}

#[tokio::test]
async fn without_extensions_the_query_goes_out_untouched() {
    let app = app(None, CacheConfig::default()).await;
    assert_eq!(post(&app, json!({ "query": "secret topic" })).await.status_code(), 200);
    assert_eq!(sent(&app)[0].query, "secret topic");
}

#[cfg(feature = "prometheus")]
#[tokio::test]
async fn each_rule_hit_is_counted_on_the_metrics_endpoint_with_its_labels() {
    let provider = RecordingSearchAdapter::default();
    let extensions = Extensions::new(vec![Arc::new(TestExtension {
        behaviour: Behaviour::Rewrite,
        seen: Arc::new(Mutex::new(Vec::new())),
    })]);
    let (server, _db) = common::search_app::search_app_with(
        vec![],
        CacheConfig::default(),
        SearchRegistry::new_with_mock(provider.clone()),
        None,
        extensions,
        |state| state.app_metrics = Some(Arc::new(modelrouter::metrics::AppMetrics::new().unwrap())),
    )
    .await;
    let app = App { server, db: _db, provider, seen: Arc::new(Mutex::new(Vec::new())) };
    post(&app, json!({ "query": "secret one" })).await;
    post(&app, json!({ "query": "secret two" })).await;

    let metrics = app.server.get("/metrics").await.text();
    let line = metrics
        .lines()
        .find(|l| l.starts_with("modelrouter_policy_rejected_total{"))
        .expect("the counter is exported");
    for label in [r#"extension="test-ext""#, r#"rule="word""#, r#"category="test""#, r#"action="redact""#, r#"project="proj-7""#] {
        assert!(line.contains(label), "missing {label} in {line}");
    }
    assert!(line.ends_with(" 2"), "two searches, two hits: {line}");
}

#[tokio::test]
async fn hook_response_fields_reach_a_fresh_answer_and_a_cache_hit_but_not_the_cache() {
    let app = app(Some(Behaviour::Rewrite), cache_on()).await;
    let fresh: Value = post(&app, json!({ "query": "secret topic" })).await.json();
    assert_eq!(fresh["policy"], json!({ "sent_query": "public topic" }));
    // A hit carries the fields of the request it answers, not those stored.
    let hit = post(&app, json!({ "query": "public topic" })).await;
    assert_eq!(hit.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    assert_eq!(hit.json::<Value>()["policy"], json!({ "sent_query": "public topic" }));
}

#[tokio::test]
async fn without_extensions_there_is_no_policy_field() {
    let app = app(None, CacheConfig::default()).await;
    let body: Value = post(&app, json!({ "query": "topic" })).await.json();
    assert!(body.get("policy").is_none());
}
