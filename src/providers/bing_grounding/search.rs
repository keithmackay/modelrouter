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
use crate::providers::azure_credentials::AzureAuth as FoundryAuth;
use crate::providers::azure_entra::{TokenProvider, FOUNDRY_PROJECT_SCOPE};
use crate::providers::search::{
    SearchAdapter, SearchAnswer, SearchRequest, SearchResponse, SearchResultItem,
    MAX_FOLLOW_UP_QUERIES,
};

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

/// Longest answer text returned when the caller asks for it. Bounds what one
/// search can put on the wire; a grounded answer from a 10-50 citation call is
/// normally far shorter.
const MAX_ANSWER_CHARS: usize = 16_000;

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

/// Default `instructions` when the caller asks for the answer but sends no
/// instructions of its own: the generated text is then part of the response,
/// so the model is asked to answer the question.
const ANSWER_INSTRUCTION: &str = "Search the web and answer the question in the input. \
Say plainly where the retrieved pages do not answer part of the question.";

/// Always appended to the answer-mode instructions, whoever wrote them: caller
/// instructions shape the research, they cannot turn it into an ungrounded
/// answer.
const GROUNDING_RULE: &str = "Use only information retrieved from the web tool, never prior \
knowledge, and cite the source of every claim.";

/// Marker of the one line the router parses follow-up queries from. Fixed by
/// the router rather than left to caller instructions, so the parse never
/// depends on how a caller phrased its prompt.
const FOLLOW_UP_MARKER: &str = "FOLLOW_UP_QUERIES:";

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
        // A Foundry PROJECT endpoint (`/api/projects/<project>`), whose
        // documented audience is `https://ai.azure.com/.default` — not the
        // `cognitiveservices` audience the resource-level inference endpoints
        // take. See `providers::azure_entra` for both constants.
        let auth = FoundryAuth::entra_by_default(
            "bing_grounding",
            config,
            FOUNDRY_PROJECT_SCOPE,
            std::time::Duration::from_secs(config.timeout_secs.clamp(1, RECOMMENDED_TIMEOUT_SECS)),
        )?;
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
        let mut body = serde_json::json!({
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
        });
        if req.include_answer {
            let (instructions, input) = answer_mode_prompt(req);
            body["instructions"] = serde_json::json!(instructions);
            body["input"] = serde_json::json!(input);
        }
        body
    }
}

/// `instructions` and `input` for an answer-mode request: the caller's
/// instructions (or the default), the router's grounding rule, and the
/// follow-up line format when follow-ups were asked for.
fn answer_mode_prompt(req: &SearchRequest) -> (String, String) {
    let mut instructions = req
        .instructions
        .as_deref()
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .unwrap_or(ANSWER_INSTRUCTION)
        .to_string();
    instructions.push_str("\n\n");
    instructions.push_str(GROUNDING_RULE);
    let follow_ups = req.max_follow_up_queries.min(MAX_FOLLOW_UP_QUERIES);
    if follow_ups > 0 {
        instructions.push_str(&format!(
            "\n\nEnd your reply with one final line that starts with {FOLLOW_UP_MARKER} followed \
             by a JSON array of at most {follow_ups} web search queries that would find what the \
             retrieved pages did not answer. Write [] if nothing is left to search."
        ));
    }
    let mut input = format!("Question: {}", req.query);
    if let Some(context) = req.context.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        input.push_str("\n\nContext:\n");
        input.push_str(context);
    }
    (instructions, input)
}

/// Split the follow-up line off the answer text. Returns the text without
/// it and the parsed queries, deduped and capped at `max`. A missing or
/// malformed line yields no queries and leaves the text unchanged: follow-ups
/// are guidance, and their absence is not an error.
fn split_follow_ups(text: &str, max: u32) -> (String, Vec<String>) {
    let Some(pos) = text.rfind(FOLLOW_UP_MARKER) else {
        return (text.to_string(), Vec::new());
    };
    let line_start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    if !text[line_start..pos].trim().is_empty() {
        // The marker sits mid-line: prose quoting it, not the format line.
        return (text.to_string(), Vec::new());
    }
    let tail = text[pos + FOLLOW_UP_MARKER.len()..].trim();
    let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(tail) else {
        tracing::warn!(
            tail_chars = tail.chars().count(),
            "bing_grounding: follow-up line is not a JSON array; returning no follow-up queries"
        );
        return (text.to_string(), Vec::new());
    };
    let mut queries: Vec<String> = Vec::new();
    for q in parsed.iter().filter_map(|v| v.as_str()).map(str::trim) {
        if !q.is_empty() && !queries.iter().any(|seen| seen.eq_ignore_ascii_case(q)) {
            queries.push(q.to_string());
        }
    }
    queries.truncate(max.min(MAX_FOLLOW_UP_QUERIES) as usize);
    (text[..line_start].trim_end().to_string(), queries)
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

/// The generated answer and the Bing query URLs from one Responses payload.
///
/// Only called once `parse_responses_payload` has accepted the payload, so the
/// answer is never returned without citations beside it. `None` when the
/// payload carries no message text.
pub fn parse_grounded_answer(v: &serde_json::Value, max_follow_up_queries: u32) -> Option<SearchAnswer> {
    let output = v["output"].as_array().map_or(&[][..], |a| a);
    let mut text = String::new();
    let mut query_urls: Vec<String> = Vec::new();
    for item in output {
        if let Some(content) = item["content"].as_array() {
            for part in content {
                if let Some(t) = part["text"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(t);
                }
            }
        } else if let Some(url) = item["url"].as_str().filter(|u| !u.is_empty()) {
            // Verbatim, like the citation URLs: see the module header.
            if !query_urls.iter().any(|u| u == url) {
                query_urls.push(url.to_string());
            }
        }
    }
    let (text, follow_up_queries) = if max_follow_up_queries > 0 {
        split_follow_ups(&text, max_follow_up_queries)
    } else {
        (text, Vec::new())
    };
    if text.is_empty() && follow_up_queries.is_empty() {
        return None;
    }
    Some(SearchAnswer {
        text: truncate_chars(&text, MAX_ANSWER_CHARS),
        query_urls,
        follow_up_queries,
    })
}

#[async_trait::async_trait]
impl SearchAdapter for BingGroundingAdapter {
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        self.auth.credential_report()
    }

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
        // Foundry accepts a resource key in `api-key` on the same surface.
        request = self.auth.apply(request).await?;

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
        let answer = if req.include_answer {
            parse_grounded_answer(&payload, req.max_follow_up_queries)
        } else {
            None
        };
        tracing::info!(
            citations = items.len(),
            answer_chars = answer.as_ref().map(|a| a.text.chars().count()),
            follow_up_queries = answer.as_ref().map(|a| a.follow_up_queries.len()),
            elapsed_ms = started.elapsed().as_millis() as u64,
            tool = self.tool_type(),
            "bing_grounding: normalised citations into search results"
        );

        Ok(SearchResponse {
            results: items,
            engine: "bing_grounding".to_string(),
            answer,
        })
    }
}
