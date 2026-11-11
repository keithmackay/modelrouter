//! `[subscription]`: chat traffic for Anthropic and OpenAI models answered by
//! the vendor CLIs (here, fake ones), resumed per conversation, and metered
//! under the CLI's provider name.
#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{Settings, SubscriptionConfig};
use modelrouter::db::models::CostLedgerEntry;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use serde_json::{json, Value};

const TOKEN: &str = "sub-token";

/// A fake `claude`: answers `claude:<prompt>`, keeps a resumed session id,
/// logs one line per call.
fn fake_claude(dir: &Path) -> PathBuf {
    script(
        dir,
        "claude",
        r#"session="c-$$"; prev=""
for a in "$@"; do [ "$prev" = "--resume" ] && session="$a"; prev="$a"; done
prompt="$(cat | tr '\n' ' ')"
echo "claude|$session|$*" >> "$(dirname "$0")/calls.log"
printf '{"type":"result","subtype":"success","is_error":false,"result":"claude:%s","session_id":"%s","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}' "$prompt" "$session"
"#,
    )
}

/// A fake `codex exec --json`.
fn fake_codex(dir: &Path) -> PathBuf {
    script(
        dir,
        "codex",
        r#"prompt="$(cat | tr '\n' ' ')"
echo "codex|$*" >> "$(dirname "$0")/calls.log"
echo '{"type":"thread.started","thread_id":"x-1"}'
printf '{"type":"item.completed","item":{"type":"agent_message","text":"codex:%s"}}\n' "$prompt"
echo '{"type":"turn.completed","usage":{"input_tokens":300,"cached_input_tokens":100,"output_tokens":50}}'
"#,
    )
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

async fn app(dir: &Path, enabled: bool) -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    common::create_user(&db, "alice", TOKEN).await;

    let mut subscription = SubscriptionConfig { enabled, ..Default::default() };
    subscription.claude.command = fake_claude(dir).display().to_string();
    subscription.codex.command = fake_codex(dir).display().to_string();
    subscription.work_dir = dir.join("sessions").display().to_string();
    let settings = Arc::new(Settings { subscription: subscription.clone(), ..Default::default() });

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(
            ProviderRegistry::new_with_tier_timeouts(HashMap::new(), Default::default())
                .with_subscription(subscription),
        ),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default(),
        )),
        embedding_registry: Arc::new(modelrouter::providers::embed_registry::EmbeddingRegistry::new(HashMap::new())),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(HashMap::new())),
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
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    (TestServer::new(build_router(state)).unwrap(), db)
}

/// Ledger rows, once the request's background write has landed.
async fn ledger(db: &Arc<dyn DatabaseProvider>, want: usize) -> Vec<CostLedgerEntry> {
    for _ in 0..100 {
        let rows = db.list_cost_entries_before(common::FOREVER).await.unwrap();
        if rows.len() >= want {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    db.list_cost_entries_before(common::FOREVER).await.unwrap()
}

async fn chat(server: &TestServer, body: Value) -> axum_test::TestResponse {
    server
        .post("/v1/chat/completions")
        .add_header(axum::http::header::AUTHORIZATION, axum::http::HeaderValue::from_static("Bearer sub-token"))
        .json(&body)
        .await
}

#[tokio::test]
async fn chat_completions_run_through_the_cli_and_resume_the_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, db) = app(tmp.path(), true).await;

    let first = chat(&server, json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "q1"}],
    }))
    .await;
    first.assert_status_ok();
    let answer = first.json::<Value>()["choices"][0]["message"]["content"].as_str().unwrap().to_string();
    assert_eq!(answer, "claude:q1");

    let second = chat(&server, json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "q1"},
            {"role": "assistant", "content": answer},
            {"role": "user", "content": "q2"},
        ],
    }))
    .await;
    second.assert_status_ok();
    assert_eq!(second.json::<Value>()["choices"][0]["message"]["content"], "claude:q2");

    let log = calls(tmp.path());
    assert_eq!(log.len(), 2);
    let first_session = log[0].split('|').nth(1).unwrap();
    assert!(log[1].contains(&format!("--resume {first_session}")), "{log:?}");
    assert!(log[0].contains("--model claude-sonnet-4-6"));

    // Metered under the CLI, priced at the model's API rate.
    let rows = ledger(&db, 2).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.provider == "claude-cli" && r.model == "claude-sonnet-4-6"));
    assert!(rows.iter().all(|r| r.tokens_in == 1000 && r.tokens_out == 500));
    let expected = CostCalculator::new().calculate("claude-sonnet-4-6", 1000, 500);
    assert!(expected > 0.0);
    assert!(rows.iter().all(|r| (r.cost_usd - expected).abs() < 1e-12));
}

#[tokio::test]
async fn openai_models_go_through_codex_and_streams_are_metered() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, db) = app(tmp.path(), true).await;

    let resp = chat(&server, json!({
        "model": "openai/gpt-5",
        "stream": true,
        "messages": [{"role": "system", "content": "be brief"}, {"role": "user", "content": "hi"}],
    }))
    .await;
    resp.assert_status_ok();
    let sse = resp.text();
    assert!(sse.contains("codex:<system> be brief </system>  hi"), "{sse}");
    assert!(sse.contains("[DONE]"));

    let rows = ledger(&db, 1).await;
    assert_eq!(rows[0].provider, "codex-cli");
    assert_eq!(rows[0].model, "gpt-5");
    // Whole prompt as reported: 200 uncached + 100 cached.
    assert_eq!((rows[0].tokens_in, rows[0].tokens_out), (300, 50));
    assert!(calls(tmp.path())[0].contains("exec --json --skip-git-repo-check --sandbox read-only -m gpt-5 -"));
}

#[tokio::test]
async fn messages_api_runs_through_claude_streamed_or_not() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, db) = app(tmp.path(), true).await;
    let post = |body: Value| {
        server
            .post("/v1/messages")
            .add_header(axum::http::header::AUTHORIZATION, axum::http::HeaderValue::from_static("Bearer sub-token"))
            .json(&body)
    };

    let plain = post(json!({
        "model": "anthropic/claude-sonnet-4-6",
        "max_tokens": 100,
        "system": [{"type": "text", "text": "sys"}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
    }))
    .await;
    plain.assert_status_ok();
    let message = plain.json::<Value>();
    assert_eq!(message["type"], "message");
    assert_eq!(message["content"][0]["text"], "claude:hello");
    assert_eq!(message["usage"]["input_tokens"], 1000);
    assert!(calls(tmp.path())[0].contains("--system-prompt sys"));

    let streamed = post(json!({
        "model": "anthropic/claude-sonnet-4-6",
        "max_tokens": 100,
        "stream": true,
        "messages": [{"role": "user", "content": "again"}],
    }))
    .await;
    streamed.assert_status_ok();
    let sse = streamed.text();
    assert!(sse.contains("event: message_start") && sse.contains("claude:again"), "{sse}");
    assert!(sse.contains("event: message_stop"));

    let rows = ledger(&db, 2).await;
    assert_eq!(rows.len(), 2, "the streamed call is metered exactly, not estimated");
    assert!(rows.iter().all(|r| r.provider == "claude-cli" && r.tokens_in == 1000));

    // Tools cannot go through a CLI session.
    let with_tools = post(json!({
        "model": "anthropic/claude-sonnet-4-6",
        "max_tokens": 100,
        "tools": [{"name": "t", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "x"}],
    }))
    .await;
    with_tools.assert_status_bad_request();
    let images = post(json!({
        "model": "anthropic/claude-sonnet-4-6",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": [{"type": "image", "source": {}}]}],
    }))
    .await;
    images.assert_status_bad_request();
    assert_eq!(calls(tmp.path()).len(), 2, "refused requests never start the CLI");
}

#[tokio::test]
async fn chat_tool_requests_are_refused_in_subscription_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _db) = app(tmp.path(), true).await;
    let resp = chat(&server, json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "q"}],
        "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}],
    }))
    .await;
    resp.assert_status_bad_request();
    assert!(calls(tmp.path()).is_empty());
}

#[tokio::test]
async fn cli_providers_do_not_exist_unless_the_mode_is_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _db) = app(tmp.path(), false).await;
    let resp = chat(&server, json!({
        "model": "claude-cli/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "q"}],
    }))
    .await;
    assert!(resp.status_code().is_client_error() || resp.status_code().is_server_error());
    assert!(calls(tmp.path()).is_empty(), "no CLI ran");
}
