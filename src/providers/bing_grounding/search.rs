//! Web search served by "Grounding with Bing Search" on Azure AI Foundry.
//!
//! Bing is no longer reachable as a bare search REST API — the classic Bing Web
//! Search API is retired. What replaces it is a TOOL: you provision a
//! "Grounding with Bing Search" resource, connect it to a Foundry project, and
//! attach it to a model call. So this adapter, like the Vertex one, is a
//! generation call wearing a search interface: a minimal Responses request with
//! a fixed extraction instruction, whose URL citations are normalised into the
//! router's search-result schema.
//!
//! Request shape follows the REST sample in
//! <https://learn.microsoft.com/azure/ai-foundry/agents/how-to/tools/bing-tools>:
//!
//! ```text
//! POST {foundry_project_endpoint}/openai/v1/responses
//! Authorization: Bearer <Entra token for https://ai.azure.com/.default>
//! {
//!   "model": "<deployment name>",
//!   "input": "<instruction + query>",
//!   "tool_choice": "required",
//!   "tools": [{"type": "bing_grounding",
//!              "bing_grounding": {"search_configurations": [
//!                 {"project_connection_id": "...", "count": 10}]}}]
//! }
//! ```
//!
//! ## Citations are returned verbatim
//!
//! Grounding with Bing Search is governed by Microsoft's Use and Display
//! Requirements (<https://www.microsoft.com/bing/apis/grounding-legal>): the
//! website URLs and the Bing search query URL must be retained and displayed in
//! the exact form Microsoft returned them. This adapter therefore performs NO
//! rewriting of citation URLs — no unescaping, no tracking-parameter stripping,
//! no redirect unwrapping (contrast the Vertex adapter, which must unwrap
//! Google's redirector). Callers rendering these results carry the display
//! obligation; see `docs/local-setup.md`.
//!
//! ## No network in this environment
//!
//! This code was written against the published API spec — the development host
//! has no route to Azure. Everything below is therefore covered by mocked-HTTP
//! tests (`tests/test_bing_grounding.rs`) and instrumented with tracing at the
//! points where a live deployment would first diverge from the spec: the
//! resolved URL, the credential source, the tool type, and the citation count.

use anyhow::Context;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::bing_grounding::auth::{EntraTokenProvider, TokenProvider};
use crate::providers::search::{SearchAdapter, SearchRequest, SearchResponse, SearchResultItem};

/// Path of the GA ("v1") Responses surface on a Foundry project endpoint.
///
/// The v1 path is api-version-independent — the documented sample sends no
/// `api-version` at all — so unlike the Azure OpenAI adapter there is no
/// default version constant here. `api_version` in config appends the query
/// parameter anyway, which is how an operator reaches a preview surface without
/// waiting for a router release.
const RESPONSES_PATH: &str = "/openai/v1/responses";

/// General web grounding tool type.
const TOOL_GENERAL: &str = "bing_grounding";

/// Domain-restricted variant (preview). Same request surface, different type
/// name and an extra `instance_name` in the search configuration.
const TOOL_CUSTOM: &str = "bing_custom_search_preview";

/// Bing caps `count` at 50 per the tool's optional-parameter table. The search
/// route already caps `max_results` at 20; this is belt-and-braces so a direct
/// caller of the adapter cannot send a value the tool will reject.
const MAX_COUNT: u32 = 50;

/// Longest snippet kept when a citation carries no span indices to slice with.
const MAX_SNIPPET_CHARS: usize = 400;

/// Below this, the timeout is likely a copy of the router-wide default rather
/// than a considered value: this call embeds a model generation *and* a Bing
/// round trip, so it behaves like a completion, not like a search API.
const RECOMMENDED_TIMEOUT_SECS: u64 = 300;

/// Prefixed to the caller's query. Fixed, and deliberately narrow: the model
/// here is plumbing for the search tool, not an analyst. Telling it to answer
/// only from retrieved pages and to cite everything is what makes the
/// annotations — the only part of the response this adapter keeps — dense
/// enough to be useful.
const EXTRACTION_INSTRUCTION: &str = "Search the web and report what you find for the query below. \
Use only information retrieved from the web tool, never prior knowledge, and cite every source. \
For each relevant source, state in one or two sentences what it says about the query.\n\nQuery: ";

/// How the request authenticates to the Foundry data plane.
enum FoundryAuth {
    /// Intended mode: no secret on disk.
    Entra(Arc<dyn TokenProvider>),
    /// Explicit opt-in, selected by setting a non-empty `api_key`.
    ApiKey(String),
}

pub struct BingGroundingAdapter {
    endpoint: String,
    api_version: Option<String>,
    model: String,
    project_connection_id: String,
    custom_search: bool,
    custom_search_instance: Option<String>,
    auth: FoundryAuth,
    client: reqwest::Client,
}

impl BingGroundingAdapter {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let auth = if config.api_key.trim().is_empty() {
            let provider = EntraTokenProvider::from_env(std::time::Duration::from_secs(
                config.timeout_secs.min(RECOMMENDED_TIMEOUT_SECS),
            ))?;
            tracing::debug!(
                credential_source = provider.source_label(),
                "bing_grounding: authenticating with Entra"
            );
            FoundryAuth::Entra(Arc::new(provider) as Arc<dyn TokenProvider>)
        } else {
            tracing::info!(
                "bing_grounding: `api_key` is set, so key auth is used instead of Entra. \
                 Entra (managed identity or an app registration in the environment) is the \
                 intended mode — a key in config.toml is a secret at rest."
            );
            FoundryAuth::ApiKey(config.api_key.trim().to_string())
        };
        Self::build(config, auth)
    }

    /// Construct with a caller-supplied token source. The seam the tests use to
    /// exercise the Foundry request without an Entra round trip.
    pub fn with_token_provider(
        config: &ProviderConfig,
        token_provider: Arc<dyn TokenProvider>,
    ) -> anyhow::Result<Self> {
        Self::build(config, FoundryAuth::Entra(token_provider))
    }

    fn build(config: &ProviderConfig, auth: FoundryAuth) -> anyhow::Result<Self> {
        let endpoint = config
            .foundry_project_endpoint
            .clone()
            .or_else(|| config.api_base.clone())
            .map(|e| e.trim().trim_end_matches('/').to_string())
            .filter(|e| !e.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "bing_grounding needs `foundry_project_endpoint` (or `api_base`) under \
                     [providers.bing_grounding] — the Foundry project endpoint from the Azure \
                     portal, e.g. https://<resource>.services.ai.azure.com/api/projects/<project>"
                )
            })?;

        let project_connection_id = config
            .project_connection_id
            .clone()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "bing_grounding needs `project_connection_id` under \
                     [providers.bing_grounding] — the connection id of the Grounding with Bing \
                     Search resource attached to the Foundry project. Without it the tool call \
                     has no resource to bill or query."
                )
            })?;

        // No default model: the value is a deployment name that exists only in
        // the operator's project, and the documented exclusions (gpt-4o-mini
        // 2024-07-18, the gpt-5 family) mean a guessed default would fail at
        // request time with an error that points at the wrong thing.
        let model = config
            .search_model
            .clone()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "bing_grounding needs `search_model` under [providers.bing_grounding] — the \
                     name of a model deployment in the Foundry project that the grounding tool \
                     will run on. Note that gpt-4o-mini (2024-07-18) and the gpt-5 family are \
                     documented as unsupported by this tool."
                )
            })?;

        if config.custom_search && config.custom_search_instance.is_none() {
            anyhow::bail!(
                "bing_grounding has `custom_search = true` but no `custom_search_instance` — the \
                 Bing Custom Search (preview) tool requires the instance (configuration) name of \
                 the Custom Search resource."
            );
        }

        if config.timeout_secs < RECOMMENDED_TIMEOUT_SECS {
            tracing::warn!(
                timeout_secs = config.timeout_secs,
                recommended = RECOMMENDED_TIMEOUT_SECS,
                "bing_grounding: timeout_secs is below the recommended floor. This call runs a \
                 Bing search AND a model generation, so it is a completion-length operation; a \
                 search-length timeout will cut off answers that would have succeeded."
            );
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build reqwest client for bing_grounding")?;

        Ok(Self {
            endpoint,
            api_version: config
                .api_version
                .clone()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            model,
            project_connection_id,
            custom_search: config.custom_search,
            custom_search_instance: config.custom_search_instance.clone(),
            auth,
            client,
        })
    }

    fn tool_type(&self) -> &'static str {
        if self.custom_search {
            TOOL_CUSTOM
        } else {
            TOOL_GENERAL
        }
    }

    pub fn responses_url(&self) -> String {
        match &self.api_version {
            Some(v) => format!("{}{}?api-version={}", self.endpoint, RESPONSES_PATH, v),
            None => format!("{}{}", self.endpoint, RESPONSES_PATH),
        }
    }

    pub fn build_body(&self, req: &SearchRequest) -> serde_json::Value {
        let mut search_config = serde_json::json!({
            "project_connection_id": self.project_connection_id,
        });
        if let Some(instance) = self.custom_search_instance.as_deref() {
            search_config["instance_name"] = serde_json::json!(instance);
        }
        if let Some(max) = req.max_results {
            search_config["count"] = serde_json::json!(max.min(MAX_COUNT));
        }

        let tool_type = self.tool_type();
        serde_json::json!({
            "model": self.model,
            "input": format!("{EXTRACTION_INSTRUCTION}{}", req.query),
            // Without this the model is free to answer from parametric memory
            // and return no annotations at all — i.e. to return generated prose
            // where the caller asked for web evidence.
            "tool_choice": "required",
            "tools": [{
                "type": tool_type,
                tool_type: {"search_configurations": [search_config]},
            }],
        })
    }
}

/// Take `chars[from..to]` with every index clamped to the string's length.
///
/// The Responses annotations carry `start_index`/`end_index` into the message
/// text. Whether those are character or byte offsets is not nailed down by the
/// docs, and this host cannot check against a live response, so they are
/// treated as character offsets and clamped: a payload that meant bytes yields
/// a slightly ragged snippet, never a panic and never a dropped result.
fn char_slice(text: &str, from: usize, to: usize) -> String {
    if to <= from {
        return String::new();
    }
    text.chars().skip(from).take(to - from).collect()
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// Normalise one Responses payload into search results.
///
/// Errors rather than returning an empty vector when the response carries no
/// citations. An empty result set is indistinguishable from "the web has
/// nothing on this", and the failure mode being guarded against is the model
/// answering from memory: ungrounded prose must not reach a caller wearing a
/// search result's clothes. Same ruling as the Vertex grounding adapter.
pub fn parse_responses_payload(
    v: &serde_json::Value,
    req: &SearchRequest,
) -> anyhow::Result<Vec<SearchResultItem>> {
    let output = v["output"].as_array().map_or(&[][..], |a| a);

    let mut items: Vec<SearchResultItem> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for item in output {
        let Some(content) = item["content"].as_array() else {
            // Tool-call items (`bing_grounding_call` and friends) carry the Bing
            // query URL rather than citations. Logged, not returned: it is not
            // a search result, though a UI displaying these results must show
            // it to satisfy the Use and Display Requirements.
            if let Some(url) = item["url"].as_str() {
                tracing::debug!(item_type = item["type"].as_str(), bing_query_url = url,
                    "bing_grounding: tool-call item");
            }
            continue;
        };

        for part in content {
            let text = part["text"].as_str().unwrap_or("");
            let Some(annotations) = part["annotations"].as_array() else {
                continue;
            };

            // Cursor into `text`: each citation's snippet is the span since the
            // previous citation on this part, which is the sentence (or few)
            // that the citation supports.
            let mut cursor = 0usize;
            for ann in annotations {
                if ann["type"].as_str() != Some("url_citation") {
                    continue;
                }
                let Some(url) = ann["url"].as_str().filter(|u| !u.is_empty()) else {
                    continue;
                };

                let end = ann["end_index"].as_u64().map(|n| n as usize);
                let snippet = match end {
                    Some(end) => {
                        let s = char_slice(text, cursor, end);
                        cursor = end;
                        let s = s.trim().to_string();
                        if s.is_empty() {
                            truncate_chars(text.trim(), MAX_SNIPPET_CHARS)
                        } else {
                            truncate_chars(&s, MAX_SNIPPET_CHARS)
                        }
                    }
                    None => truncate_chars(text.trim(), MAX_SNIPPET_CHARS),
                };

                // URL is the dedupe key and is stored EXACTLY as Microsoft sent
                // it — see the module header on display requirements.
                if !seen.insert(url.to_string()) {
                    continue;
                }

                let title = ann["title"]
                    .as_str()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .unwrap_or(url)
                    .to_string();

                items.push(SearchResultItem {
                    title,
                    url: url.to_string(),
                    snippet,
                    // Bing grounding annotations carry no relevance score.
                    // Synthesising one from citation order would be inventing
                    // confidence the provider never expressed; order already
                    // carries whatever ranking exists.
                    score: None,
                    // No publication date in the annotation schema.
                    published_date: None,
                });
            }
        }
    }

    if items.is_empty() {
        let status = v["status"].as_str().unwrap_or("unknown");
        let detail = v["error"]["message"]
            .as_str()
            .or_else(|| v["incomplete_details"]["reason"].as_str())
            .unwrap_or("no url_citation annotations in the response");
        // Deliberately does not quote the model's text: ungrounded prose must
        // not travel further, not even inside an error a caller might log.
        anyhow::bail!(
            "bing_grounding returned no web citations (response status: {status}; {detail}). \
             The grounding tool either did not run or found nothing, so any text in the response \
             is generated rather than retrieved. Refusing to return it as a search result."
        );
    }

    if let Some(max) = req.max_results {
        items.truncate(max as usize);
    }
    Ok(items)
}

#[async_trait::async_trait]
impl SearchAdapter for BingGroundingAdapter {
    async fn search(&self, req: &SearchRequest) -> anyhow::Result<SearchResponse> {
        let url = self.responses_url();
        let body = self.build_body(req);

        tracing::debug!(
            url = %url,
            model = %self.model,
            tool = self.tool_type(),
            max_results = ?req.max_results,
            query_chars = req.query.chars().count(),
            "bing_grounding: dispatching Foundry Responses request"
        );

        let mut request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json");
        request = match &self.auth {
            FoundryAuth::Entra(provider) => {
                let token = provider.token().await?;
                request.bearer_auth(token)
            }
            // Foundry accepts a resource key in `api-key` on the same surface.
            FoundryAuth::ApiKey(key) => request.header("api-key", key),
        };

        let started = std::time::Instant::now();
        let resp = request.json(&body).send().await.with_context(|| {
            format!("Failed to send bing_grounding request to Foundry endpoint {url}")
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                status = status.as_u16(),
                url = %url,
                body = %text,
                "bing_grounding: Foundry returned an error"
            );
            // Message keeps the "returned {status}" shape the search route's
            // retry classifier reads to decide 4xx-surface vs 5xx-failover.
            anyhow::bail!("Search provider returned {}: {}", status, text);
        }

        let payload: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse bing_grounding Responses payload")?;

        let items = parse_responses_payload(&payload, req)?;
        tracing::info!(
            citations = items.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            tool = self.tool_type(),
            "bing_grounding: normalised citations into search results"
        );

        Ok(SearchResponse {
            results: items,
            engine: "bing_grounding".to_string(),
        })
    }
}
