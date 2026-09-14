//! VertexAdapter — implements `ProviderAdapter` by routing requests to the
//! Gemini or Claude translator based on the model identifier, then posting
//! to Vertex with a Google Cloud OAuth2 Bearer token.

use std::sync::Arc;
use anyhow::Context;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};
use crate::providers::vertex::auth::{GoogleCloudAuthProvider, TokenProvider};
use crate::providers::vertex::dispatch::{parse_model_id, Publisher};
use crate::providers::vertex::{claude, gemini, maas};

/// Build the full Vertex REST URL for a given (project, region, publisher, model).
/// For Gemini streaming, appends `?alt=sse` so the server emits line-framed SSE.
///
/// The `global` location uses the un-prefixed hostname `aiplatform.googleapis.com`
/// while regional locations use `{region}-aiplatform.googleapis.com`. The path
/// segment `locations/{region}` is always included, even for `global`.
pub fn build_endpoint_url(
    project: &str,
    region: &str,
    publisher: Publisher,
    model: &str,
    streaming: bool,
) -> String {
    let host = if region == "global" {
        "aiplatform.googleapis.com".to_string()
    } else {
        format!("{region}-aiplatform.googleapis.com")
    };
    // MaaS publishers (Mistral, Llama, DeepSeek, …) share one OpenAI-compatible
    // endpoint; the publisher travels in the request body's `model` field, and
    // streaming is a body flag (`stream: true`), not a different URL.
    if matches!(publisher, Publisher::Maas) {
        return format!(
            "https://{host}/v1/projects/{project}/locations/{region}/endpoints/openapi/chat/completions"
        );
    }
    let (pub_segment, method) = match (publisher, streaming) {
        (Publisher::Google, false) => ("google", "generateContent"),
        (Publisher::Google, true) => ("google", "streamGenerateContent"),
        (Publisher::Anthropic, false) => ("anthropic", "rawPredict"),
        (Publisher::Anthropic, true) => ("anthropic", "streamRawPredict"),
        (Publisher::Maas, _) => unreachable!("handled above"),
    };
    let mut url = format!(
        "https://{host}/v1/projects/{project}/locations/{region}/publishers/{pub_segment}/models/{model}:{method}"
    );
    if matches!(publisher, Publisher::Google) && streaming {
        url.push_str("?alt=sse");
    }
    url
}

/// Build the Vertex REST URL for a `:predict` call against a Google publisher
/// model (the embedding path — `text-embedding-*` and friends).
///
/// Separate from `build_endpoint_url` because `:predict` is a different verb
/// from the generative `:generateContent` / `:rawPredict`, not a variant of
/// them; the host rule is shared deliberately so a region only has to be
/// reasoned about once. Note that `global` is a legal host here but not a legal
/// embedding location — see `vertex::embed::resolve_embedding_region`.
pub fn build_predict_url(project: &str, region: &str, model: &str) -> String {
    let host = if region == "global" {
        "aiplatform.googleapis.com".to_string()
    } else {
        format!("{region}-aiplatform.googleapis.com")
    };
    format!(
        "https://{host}/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:predict"
    )
}

pub struct VertexAdapter {
    project: String,
    region: String,
    /// Region for Model-as-a-Service publishers (mistralai, meta, …), which
    /// are regional-only: `locations/global` 404s for them. From
    /// `[providers.vertex] maas_region`, falling back to `region` when that is
    /// itself regional. None (global region, no maas_region configured) makes
    /// MaaS dispatch fail loudly with a config fix-hint — region names are
    /// operations data and never live in code (operator ruling 2026-08-20).
    maas_region: Option<String>,
    /// Publisher catalogs to probe for `/admin/api/models/available` — from
    /// `[providers.vertex] catalog_publishers`; empty means the structural
    /// floor in catalog.rs (the two publishers with dedicated dispatch arms).
    catalog_publishers: Vec<String>,
    token_provider: Arc<dyn TokenProvider>,
    client: reqwest::Client,
    /// Scheme+host override for the publisher-models catalog (tests only;
    /// None in production — the host derives from `region`). See catalog.rs.
    catalog_base: Option<String>,
    /// Scheme+host override for generative dispatch — `complete`, `stream` and
    /// the MaaS access probe (tests only; None in production, where the host
    /// derives from `region`). The sibling of `catalog_base`: without it the
    /// only way to exercise request translation, error passthrough and SSE
    /// rewriting is a live Google Cloud project.
    api_base: Option<String>,
}

/// Point a Vertex URL at `base` instead of googleapis.com, preserving the path.
/// `base: None` (always, in production) returns the URL untouched.
fn rebase(url: String, base: Option<&str>) -> String {
    match base {
        None => url,
        // "https://host/v1/projects/…" → ["https:", "", "host", "v1/projects/…"]
        Some(b) => match url.splitn(4, '/').nth(3) {
            Some(path) => format!("{}/{}", b.trim_end_matches('/'), path),
            None => url,
        },
    }
}

impl VertexAdapter {
    /// Build a VertexAdapter from config, using real Google OAuth.
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let project = config
            .project
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Vertex provider requires `project` in config"))?;
        let region = config
            .region
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Vertex provider requires `region` in config"))?;
        let token_provider = Arc::new(
            GoogleCloudAuthProvider::new(config.credentials_path.as_deref())?,
        ) as Arc<dyn TokenProvider>;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("failed to build reqwest client")?;
        let maas_region = config
            .maas_region
            .clone()
            .or_else(|| if region == "global" { None } else { Some(region.clone()) });
        Ok(Self {
            project,
            region,
            maas_region,
            catalog_publishers: config.catalog_publishers.clone().unwrap_or_default(),
            token_provider,
            client,
            catalog_base: None,
            api_base: None,
        })
    }

    /// Test hook: build a VertexAdapter with a caller-supplied token provider
    /// (e.g. `StaticTokenProvider`), bypassing Google OAuth.
    pub fn with_token_provider(
        project: String,
        region: String,
        token_provider: Arc<dyn TokenProvider>,
        timeout_secs: u64,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .context("failed to build reqwest client")?;
        let maas_region = if region == "global" { None } else { Some(region.clone()) };
        Ok(Self {
            project,
            region,
            maas_region,
            catalog_publishers: Vec::new(),
            token_provider,
            client,
            catalog_base: None,
            api_base: None,
        })
    }

    /// Test hook: point catalog discovery at a local mock server.
    pub fn with_catalog_base(mut self, base: String) -> Self {
        self.catalog_base = Some(base);
        self
    }

    /// Test hook: point generative dispatch (`complete`, `stream`, the MaaS
    /// access probe) at a local mock server. Never set in production.
    pub fn with_api_base(mut self, base: String) -> Self {
        self.api_base = Some(base);
        self
    }

    // Catalog accessors (see vertex/catalog.rs) — the catalog impl lives in a
    // sibling module and reuses this adapter's auth and HTTP client.
    pub(crate) fn token_provider(&self) -> &Arc<dyn TokenProvider> {
        &self.token_provider
    }
    pub(crate) fn region(&self) -> &str {
        &self.region
    }
    pub(crate) fn http_client(&self) -> &reqwest::Client {
        &self.client
    }
    pub(crate) fn catalog_base(&self) -> Option<&str> {
        self.catalog_base.as_deref()
    }
    pub(crate) fn catalog_publishers(&self) -> &[String] {
        &self.catalog_publishers
    }
    pub(crate) fn maas_region(&self) -> Option<&str> {
        self.maas_region.as_deref()
    }
    pub(crate) fn project(&self) -> &str {
        &self.project
    }

    /// Access probe for a MaaS model: a real 1-max-token call. An invalid-body
    /// probe does NOT work — Vertex validates the request body before
    /// resolving model access, so a missing-`messages` 400 reads identically
    /// for accessible and inaccessible models (verified live 2026-08-20).
    /// Status semantics: 404/403 = the project cannot call this model (Model
    /// Garden terms not accepted); anything else = access exists. Cost is ~a
    /// token per MaaS model per catalog-cache window — the price of a picker
    /// that never lists an uncallable model (operator ruling). `Err` = the
    /// probe itself failed (network); caller decides the default.
    pub(crate) async fn probe_maas_access(
        &self,
        full_model_id: &str,
        maas_region: &str,
        token: &str,
    ) -> anyhow::Result<bool> {
        let url = rebase(
            build_endpoint_url(
                &self.project,
                maas_region,
                Publisher::Maas,
                full_model_id,
                false,
            ),
            self.api_base.as_deref(),
        );
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "model": full_model_id,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1,
            }))
            .send()
            .await
            .context("MaaS access probe failed")?;
        Ok(!matches!(resp.status().as_u16(), 403 | 404))
    }
    /// Region for a MaaS dispatch, or a config fix-hint when none resolves
    /// (chat region `global` + no `maas_region` set — MaaS is regional-only).
    fn resolve_maas_region(&self) -> anyhow::Result<&str> {
        self.maas_region.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Vertex Model-as-a-Service models need a regional location: `region` is \
                 \"global\" (fine for Claude/Gemini, serves no MaaS models) and no \
                 `maas_region` is set. Add `maas_region = \"<a MaaS region>\"` under \
                 [providers.vertex]."
            )
        })
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for VertexAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let (publisher, model) = parse_model_id(&req.model)?;
        let region = if matches!(publisher, Publisher::Maas) {
            self.resolve_maas_region()?
        } else {
            &self.region
        };
        let url = rebase(
            build_endpoint_url(&self.project, region, publisher, &model, false),
            self.api_base.as_deref(),
        );
        let body = match publisher {
            Publisher::Google => gemini::translate_request(req),
            Publisher::Anthropic => claude::translate_request(req),
            Publisher::Maas => maas::translate_request(req, &model, false),
        };
        let token = self.token_provider.token().await?;
        let dispatched = std::time::Instant::now();
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("failed to send request to Vertex AI")?;
        // Headers are in, body not yet read: time to first token.
        let ttft_ms = dispatched.elapsed().as_millis() as i64;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Vertex AI returned {}: {}", status, text);
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse Vertex response")?;
        let mut result = match publisher {
            Publisher::Google => gemini::parse_response(v),
            Publisher::Anthropic => claude::parse_response(v),
            Publisher::Maas => maas::parse_response(v),
        }?;
        result.ttft_ms = Some(ttft_ms);
        Ok(result)
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let (publisher, model) = parse_model_id(&req.model)?;
        let region = if matches!(publisher, Publisher::Maas) {
            self.resolve_maas_region()?
        } else {
            &self.region
        };
        let url = rebase(
            build_endpoint_url(&self.project, region, publisher, &model, true),
            self.api_base.as_deref(),
        );
        let body = match publisher {
            Publisher::Google => gemini::translate_request(req),
            Publisher::Anthropic => claude::translate_request(req),
            Publisher::Maas => maas::translate_request(req, &model, true),
        };
        let token = self.token_provider.token().await?;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("failed to send streaming request to Vertex AI")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Vertex AI streaming returned {}: {}", status, text);
        }

        let translated = resp
            .bytes_stream()
            .map_err(|e| anyhow::anyhow!("stream error: {}", e))
            .map_ok(move |chunk| {
                let text = String::from_utf8_lossy(&chunk);
                let mut out = String::new();
                for line in text.lines() {
                    let translated = match publisher {
                        Publisher::Google => gemini::translate_sse_line(line),
                        Publisher::Anthropic => claude::translate_sse_line(line),
                        // MaaS streams are already OpenAI-shaped SSE — pass
                        // frames through untouched (including `data: [DONE]`).
                        Publisher::Maas => {
                            if line.is_empty() {
                                None
                            } else {
                                Some(Bytes::from(format!("{line}\n\n")))
                            }
                        }
                    };
                    if let Some(b) = translated {
                        out.push_str(&String::from_utf8_lossy(&b));
                    }
                }
                Bytes::from(out)
            });

        // Gemini's SSE has no terminal event — its final frame carries only
        // `usageMetadata` with no candidates, which `gemini::translate_sse_line`
        // deliberately drops. Append `data: [DONE]\n\n` here so the downstream
        // SSE consumer (log_streaming_request) can detect stream end and commit
        // cost ledger rows, audit entries, and lifecycle hooks. Claude-on-Vertex
        // emits DONE on `message_delta` inside the translator already.
        let stream = if matches!(publisher, Publisher::Google) {
            translated
                .chain(futures::stream::once(async {
                    Ok(Bytes::from_static(b"data: [DONE]\n\n"))
                }))
                .boxed()
        } else {
            translated.boxed()
        };
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    //! Dispatch is exercised against a localhost server speaking the Vertex
    //! wire shape, via `with_api_base`. What that cannot reach is noted where
    //! it applies: real Google OAuth (`GoogleCloudAuthProvider::new`) needs
    //! service-account material or a metadata server, so `VertexAdapter::new`
    //! is covered only up to its config validation.

    use super::*;
    use crate::providers::vertex::auth::StaticTokenProvider;
    use axum::{routing::post, Router};
    use futures::StreamExt;

    async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{addr}"), handle)
    }

    fn adapter(base: &str, region: &str) -> VertexAdapter {
        VertexAdapter::with_token_provider(
            "proj".into(),
            region.into(),
            Arc::new(StaticTokenProvider::new("tok".into())),
            5,
        )
        .unwrap()
        .with_api_base(base.to_string())
    }

    fn req(model: &str) -> NormalizedRequest {
        NormalizedRequest {
            model: model.to_string(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            stream: false,
            temperature: Some(0.2),
            max_tokens: Some(16),
            extra_params: serde_json::Value::Null,
        }
    }

    async fn collect(stream: SseStream) -> String {
        stream
            .map(|c| String::from_utf8_lossy(&c.unwrap()).into_owned())
            .collect::<Vec<_>>()
            .await
            .join("")
    }

    #[test]
    fn rebase_swaps_the_origin_and_keeps_path_and_query() {
        let url = build_endpoint_url("proj", "global", Publisher::Google, "gemini-2.5-pro", true);
        assert_eq!(rebase(url.clone(), None), url, "production leaves the URL alone");
        let rebased = rebase(url, Some("http://127.0.0.1:9/"));
        assert!(rebased.starts_with("http://127.0.0.1:9/v1/projects/proj/"), "{rebased}");
        assert!(rebased.ends_with(":streamGenerateContent?alt=sse"), "{rebased}");
    }

    #[test]
    fn new_rejects_a_config_missing_project_or_region() {
        let mut config = ProviderConfig { timeout_secs: 5, ..Default::default() };
        let err = VertexAdapter::new(&config).err().unwrap().to_string();
        assert!(err.contains("project"), "{err}");

        config.project = Some("proj".into());
        let err = VertexAdapter::new(&config).err().unwrap().to_string();
        assert!(err.contains("region"), "{err}");
    }

    #[test]
    fn accessors_report_what_the_catalog_module_asks_for() {
        let a = adapter("http://127.0.0.1:9", "us-central1");
        assert_eq!(a.project(), "proj");
        assert_eq!(a.region(), "us-central1");
        // A regional chat location doubles as the MaaS location.
        assert_eq!(a.maas_region(), Some("us-central1"));
        assert!(a.catalog_publishers().is_empty());
        assert_eq!(a.catalog_base(), None);
        assert_eq!(
            a.with_catalog_base("http://127.0.0.1:8".into()).catalog_base(),
            Some("http://127.0.0.1:8")
        );

        // `global` serves no MaaS models, so no MaaS region is implied.
        let global = adapter("http://127.0.0.1:9", "global");
        assert_eq!(global.maas_region(), None);
    }

    #[tokio::test]
    async fn maas_dispatch_from_a_global_location_is_a_config_error() {
        let a = adapter("http://127.0.0.1:9", "global");
        for err in [
            a.complete(&req("mistralai/mistral-medium-3")).await.err().unwrap(),
            a.stream(&req("mistralai/mistral-medium-3")).await.err().unwrap(),
        ] {
            let msg = err.to_string();
            assert!(msg.contains("maas_region"), "the error should name the fix: {msg}");
        }
    }

    #[tokio::test]
    async fn gemini_complete_translates_the_request_and_parses_the_response() {
        let seen = Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let sink = seen.clone();
        let router = Router::new().fallback(post(
            move |uri: axum::http::Uri, axum::Json(body): axum::Json<serde_json::Value>| {
                let sink = sink.clone();
                async move {
                    *sink.lock().unwrap() = serde_json::json!({"uri": uri.to_string(), "body": body});
                    axum::Json(serde_json::json!({
                        "candidates": [{
                            "content": {"parts": [{"text": "pong"}]},
                            "finishReason": "STOP"
                        }],
                        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3}
                    }))
                }
            },
        ));
        let (base, _s) = serve(router).await;

        let result = adapter(&base, "global")
            .complete(&req("google/gemini-2.5-pro"))
            .await
            .unwrap();
        assert_eq!(result.content, "pong");
        assert_eq!(result.prompt_tokens, 7);
        assert_eq!(result.completion_tokens, 3);
        assert!(result.ttft_ms.is_some(), "the adapter times the header round trip");

        let seen = seen.lock().unwrap().clone();
        assert!(
            seen["uri"].as_str().unwrap().ends_with("gemini-2.5-pro:generateContent"),
            "{seen}"
        );
        assert_eq!(seen["body"]["contents"][0]["parts"][0]["text"], "hi", "{seen}");
    }

    #[tokio::test]
    async fn claude_complete_translates_the_request_and_parses_the_response() {
        let router = Router::new().fallback(post(|| async {
            axum::Json(serde_json::json!({
                "content": [{"type": "text", "text": "hello"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 4, "output_tokens": 2}
            }))
        }));
        let (base, _s) = serve(router).await;

        let result = adapter(&base, "global")
            .complete(&req("anthropic/claude-sonnet-4-5"))
            .await
            .unwrap();
        assert_eq!(result.content, "hello");
        assert_eq!(result.finish_reason, "end_turn");
        assert_eq!(result.prompt_tokens, 4);
    }

    #[tokio::test]
    async fn maas_complete_uses_the_openai_shaped_endpoint() {
        let seen = Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let sink = seen.clone();
        let router = Router::new().fallback(post(
            move |uri: axum::http::Uri, axum::Json(body): axum::Json<serde_json::Value>| {
                let sink = sink.clone();
                async move {
                    *sink.lock().unwrap() = serde_json::json!({"uri": uri.to_string(), "body": body});
                    axum::Json(serde_json::json!({
                        "choices": [{"message": {"content": "oui"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 2, "completion_tokens": 1}
                    }))
                }
            },
        ));
        let (base, _s) = serve(router).await;

        let result = adapter(&base, "us-central1")
            .complete(&req("mistralai/mistral-medium-3"))
            .await
            .unwrap();
        assert_eq!(result.content, "oui");

        let seen = seen.lock().unwrap().clone();
        assert!(
            seen["uri"].as_str().unwrap().ends_with("/endpoints/openapi/chat/completions"),
            "{seen}"
        );
        // The publisher travels in the body on this endpoint, not the path.
        assert_eq!(seen["body"]["model"], "mistralai/mistral-medium-3", "{seen}");
        assert_eq!(seen["body"]["stream"], false, "{seen}");
    }

    #[tokio::test]
    async fn an_upstream_failure_carries_the_status_and_body_back() {
        let router = Router::new().fallback(post(|| async {
            (axum::http::StatusCode::TOO_MANY_REQUESTS, "quota exhausted")
        }));
        let (base, _s) = serve(router).await;
        let a = adapter(&base, "global");

        let err = a.complete(&req("google/gemini-2.5-pro")).await.unwrap_err().to_string();
        assert!(err.contains("429") && err.contains("quota exhausted"), "{err}");

        let err = a.stream(&req("google/gemini-2.5-pro")).await.err().unwrap().to_string();
        assert!(err.contains("429") && err.contains("quota exhausted"), "{err}");
    }

    #[tokio::test]
    async fn gemini_stream_is_translated_and_terminated_with_done() {
        // Gemini has no stream-end event, so the adapter appends the sentinel.
        let router = Router::new().fallback(post(|| async {
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\n\
             data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"b\"}]}}]}\n\
             data: {\"usageMetadata\":{\"promptTokenCount\":1}}\n"
        }));
        let (base, _s) = serve(router).await;

        let out = collect(
            adapter(&base, "global")
                .stream(&req("google/gemini-2.5-pro"))
                .await
                .unwrap(),
        )
        .await;
        assert!(out.contains("\"content\":\"a\""), "{out}");
        assert!(out.contains("\"content\":\"b\""), "{out}");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
        assert_eq!(out.matches("[DONE]").count(), 1, "exactly one sentinel: {out}");
    }

    #[tokio::test]
    async fn claude_stream_terminates_from_the_translator_not_the_adapter() {
        let router = Router::new().fallback(post(|| async {
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n"
        }));
        let (base, _s) = serve(router).await;

        let out = collect(
            adapter(&base, "global")
                .stream(&req("anthropic/claude-sonnet-4-5"))
                .await
                .unwrap(),
        )
        .await;
        assert!(out.contains("hi"), "{out}");
        assert_eq!(
            out.matches("[DONE]").count(),
            1,
            "the adapter must not append a second sentinel: {out}"
        );
    }

    #[tokio::test]
    async fn maas_stream_frames_pass_through_untouched() {
        let router = Router::new().fallback(post(|| async {
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n"
        }));
        let (base, _s) = serve(router).await;

        let out = collect(
            adapter(&base, "us-central1")
                .stream(&req("mistralai/mistral-medium-3"))
                .await
                .unwrap(),
        )
        .await;
        assert!(out.contains("\"content\":\"x\""), "{out}");
        assert!(out.contains("data: [DONE]"), "MaaS emits its own sentinel: {out}");
    }

    #[tokio::test]
    async fn maas_access_probe_reads_403_and_404_as_no_access() {
        for (status, expected) in [
            (axum::http::StatusCode::OK, true),
            (axum::http::StatusCode::BAD_REQUEST, true),
            (axum::http::StatusCode::FORBIDDEN, false),
            (axum::http::StatusCode::NOT_FOUND, false),
        ] {
            let router = Router::new().fallback(post(move || async move { (status, "") }));
            let (base, _s) = serve(router).await;
            let got = adapter(&base, "us-central1")
                .probe_maas_access("mistralai/mistral-medium-3", "us-central1", "tok")
                .await
                .unwrap();
            assert_eq!(got, expected, "status {status} should mean access={expected}");
        }
    }

    #[tokio::test]
    async fn maas_access_probe_surfaces_a_network_failure() {
        // Port 1 on loopback refuses connections — the probe itself failed, which
        // is distinct from "the project cannot call this model".
        let err = adapter("http://127.0.0.1:1", "us-central1")
            .probe_maas_access("mistralai/mistral-medium-3", "us-central1", "tok")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("probe failed"), "{err}");
    }
}
