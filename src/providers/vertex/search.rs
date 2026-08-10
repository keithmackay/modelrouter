//! Vertex AI web search, via Gemini + Google Search grounding.
//!
//! Ported from Athena's `thesis-validator/src/workers/search/vertex-search.worker.ts`,
//! moved server-side so the mapping is applied once — and so it is cached,
//! priced and metered like every other call — instead of being reimplemented per
//! worker. On this host it is the ONLY usable web search: Tavily needs a key the
//! sandbox must not hold, and the Discovery Engine API is disabled on the
//! project, leaving grounding as the sole path to the live web.
//!
//! One deliberate divergence from the TypeScript, called out because it is a
//! behaviour change and not a port artefact. When Google returns no grounding,
//! Athena's worker falls back to emitting the model's parametric prose as a
//! single result with `url: ''` and a hardcoded score of `0.5`. That is
//! generated text entering the evidence base wearing a citation's clothes — the
//! silent-substitution failure this round exists to eliminate. Here, no
//! grounding is an error.

use anyhow::Context;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::search::{SearchAdapter, SearchRequest, SearchResponse, SearchResultItem};
use crate::providers::vertex::adapter::build_endpoint_url;
use crate::providers::vertex::auth::{GoogleCloudAuthProvider, TokenProvider};
use crate::providers::vertex::dispatch::Publisher;

/// Grounding requires a Gemini model; `gemini-2.5-flash` is the cheapest one
/// verified serving grounded answers from this project on `locations/global`.
/// Overridable via `[providers.vertex].search_model` — this is an operational
/// default in the same spirit as Tavily's `api.tavily.com` base URL, not a
/// judgement value being invented on the caller's behalf.
const DEFAULT_SEARCH_MODEL: &str = "gemini-2.5-flash";

/// How long to spend un-wrapping one Google redirect. Short on purpose: the
/// result is a nicety, the search result is not, and up to `max_results` of
/// these run per uncached query.
const REDIRECT_TIMEOUT_SECS: u64 = 5;

/// Rank-order score decay used when Google reports no confidence — which,
/// measured against live responses from this host, is the common case rather
/// than the exception.
const RANK_SCORE_DECAY: f64 = 0.05;

pub fn build_search_body(req: &SearchRequest) -> serde_json::Value {
    serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": req.query}]}],
        // Without this tool Gemini answers from parametric memory and returns no
        // groundingMetadata at all — i.e. it is the entire mechanism, not a hint.
        "tools": [{"googleSearch": {}}],
    })
}

/// Decide whether an HTTP response redirects us to a usable source URL.
///
/// Pure so the policy is testable without a network: the interesting cases are
/// all about what we REFUSE to follow. Only absolute http(s) targets are
/// accepted — the resolved value is stored as a citation and later rendered to
/// a user, so a `javascript:` or scheme-relative target must not survive.
pub fn redirect_target(status: u16, location: Option<&str>) -> Option<String> {
    if !(300..400).contains(&status) {
        return None;
    }
    let loc = location?;
    if loc.starts_with("https://") || loc.starts_with("http://") {
        Some(loc.to_string())
    } else {
        None
    }
}

/// Map one grounded `:generateContent` response onto search results.
///
/// Errors rather than returning an empty vector when nothing is grounded. An
/// empty result set is indistinguishable from "this target has no web
/// coverage", and acting on that misread is how a research run produces an
/// evidence-less deliverable while reporting success.
pub fn parse_grounded_response(
    v: &serde_json::Value,
    req: &SearchRequest,
) -> anyhow::Result<Vec<SearchResultItem>> {
    let grounding = v["candidates"][0]
        .get("groundingMetadata")
        .ok_or_else(|| {
            // Deliberately does not quote the model's text: the whole point is
            // that ungrounded prose must not travel any further, not even inside
            // an error string a caller might log and later mine.
            anyhow::anyhow!(
                "Vertex returned an answer with no grounding metadata — the googleSearch \
                 tool did not run, so this is generated text and not web evidence. \
                 Refusing to return it as a search result."
            )
        })?;

    let chunks = grounding["groundingChunks"].as_array().map_or(&[][..], |a| a);
    let supports = grounding["groundingSupports"]
        .as_array()
        .map_or(&[][..], |a| a);

    let mut items = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        // A chunk can also be `retrievedContext` (a private datastore hit).
        // Those are not web results; skipping beats emitting a blank URL.
        let Some(web) = chunk.get("web") else { continue };
        let Some(uri) = web["uri"].as_str() else { continue };

        // Google sets `title` to the domain, and sometimes only populates the
        // sibling `domain`. Falling back to the URI keeps the item identifiable
        // rather than blank.
        let title = web["title"]
            .as_str()
            .or_else(|| web["domain"].as_str())
            .unwrap_or(uri);

        let citing: Vec<&serde_json::Value> = supports
            .iter()
            .filter(|s| {
                s["groundingChunkIndices"]
                    .as_array()
                    .is_some_and(|idx| idx.iter().any(|n| n.as_u64() == Some(i as u64)))
            })
            .collect();

        let snippet = citing
            .iter()
            .filter_map(|s| s["segment"]["text"].as_str())
            .collect::<Vec<_>>()
            .join(" … ");

        // `confidenceScores[k]` corresponds to `groundingChunkIndices[k]` — it is
        // indexed by POSITION WITHIN THE SUPPORT, not by chunk index. Reading it
        // as `confidenceScores[i]` attaches the wrong confidence to the wrong
        // source, silently and plausibly.
        let mut confidences: Vec<f64> = Vec::new();
        for s in &citing {
            let Some(indices) = s["groundingChunkIndices"].as_array() else {
                continue;
            };
            let Some(pos) = indices.iter().position(|n| n.as_u64() == Some(i as u64)) else {
                continue;
            };
            if let Some(c) = s["confidenceScores"][pos].as_f64() {
                confidences.push(c);
            }
        }
        let score = confidences
            .iter()
            .copied()
            .fold(None::<f64>, |acc, c| Some(acc.map_or(c, |a: f64| a.max(c))))
            .unwrap_or_else(|| 1.0 - i as f64 * RANK_SCORE_DECAY);

        items.push(SearchResultItem {
            title: title.to_string(),
            url: uri.to_string(),
            snippet,
            score: Some(score),
            // Grounding carries no publication date. Recording `None` rather
            // than inventing one keeps recency filtering honest.
            published_date: None,
        });
    }

    if items.is_empty() {
        anyhow::bail!(
            "Vertex grounding returned no web sources for this query. Reporting the failure \
             rather than an empty result set, which a caller cannot distinguish from a target \
             with no web coverage."
        );
    }

    if let Some(max) = req.max_results {
        items.truncate(max as usize);
    }
    Ok(items)
}

pub struct VertexSearchAdapter {
    project: String,
    region: String,
    model: String,
    token_provider: Arc<dyn TokenProvider>,
    client: reqwest::Client,
    /// Separate client because it must NOT follow redirects: the 302's
    /// `Location` is the answer we want, and following it would fetch the whole
    /// page for nothing.
    redirect_client: reqwest::Client,
}

impl VertexSearchAdapter {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let project = config
            .project
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Vertex search needs `project` under [providers.vertex]"))?;
        let region = config.region.clone().ok_or_else(|| {
            anyhow::anyhow!("Vertex search needs `region` under [providers.vertex]")
        })?;
        let token_provider = Arc::new(GoogleCloudAuthProvider::new(
            config.credentials_path.as_deref(),
        )?) as Arc<dyn TokenProvider>;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build reqwest client for Vertex search")?;
        let redirect_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REDIRECT_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Failed to build redirect-resolving client for Vertex search")?;
        Ok(Self {
            project,
            region,
            model: config
                .search_model
                .clone()
                .unwrap_or_else(|| DEFAULT_SEARCH_MODEL.to_string()),
            token_provider,
            client,
            redirect_client,
        })
    }

    /// Unwrap one `vertexaisearch.cloud.google.com/grounding-api-redirect/…`
    /// URL into the real source.
    ///
    /// Athena's evidence citations and its domain-based credibility scoring are
    /// both worthless against an opaque Google redirect, and this is the last
    /// point at which the real URL is cheaply recoverable. Failure returns the
    /// redirect unchanged — a slightly worse citation beats a dropped result.
    async fn resolve_source_url(&self, url: &str) -> String {
        let Ok(resp) = self.redirect_client.head(url).send().await else {
            return url.to_string();
        };
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok());
        redirect_target(resp.status().as_u16(), location).unwrap_or_else(|| url.to_string())
    }
}

#[async_trait::async_trait]
impl SearchAdapter for VertexSearchAdapter {
    async fn search(&self, req: &SearchRequest) -> anyhow::Result<SearchResponse> {
        let url = build_endpoint_url(
            &self.project,
            &self.region,
            Publisher::Google,
            &self.model,
            false,
        );
        let token = self.token_provider.token().await?;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .json(&build_search_body(req))
            .send()
            .await
            .context("Failed to send search request to Vertex AI")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Search provider returned {}: {}", status, text);
        }

        let v: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Vertex search response")?;
        let mut items = parse_grounded_response(&v, req)?;

        // Concurrent: these are independent HEAD requests bounded by
        // max_results (the route caps it at 20), and doing them in series would
        // add up to 20 round-trips of latency to every uncached query.
        let resolved =
            futures::future::join_all(items.iter().map(|i| self.resolve_source_url(&i.url))).await;
        for (item, url) in items.iter_mut().zip(resolved) {
            item.url = url;
        }

        Ok(SearchResponse {
            results: items,
            engine: "vertex".to_string(),
        })
    }
}
