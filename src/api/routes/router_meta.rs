//! `x_router`: the router's own account of one priced call, returned beside
//! the OpenAI-shaped `usage` object on every priced response (chat
//! completions, the terminal chunk of a completion stream, embeddings,
//! search). See README, "Per-call cost and routing metadata in responses".
//!
//! Every figure here is the one the router wrote to its ledger for the same
//! request (`cost_ledger` / `prompts` rows), so a caller can record the
//! router's model, tokens, price and timing verbatim instead of keeping a
//! private price list that drifts from it.

use serde::Serialize;
use serde_json::Value;

/// What one call cost the router, exactly as written to its `cost_ledger` row.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CallCost {
    /// USD charged for this call — the ledger row's `cost_usd`.
    pub cost_usd: f64,
    /// Served from the router's response cache: `cost_usd` is 0 and
    /// `saved_usd` carries the avoided cost (the ledger row's `saved_usd`).
    pub cache_hit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_usd: Option<f64>,
}

impl CallCost {
    /// A call that reached a provider and was charged `cost_usd`.
    pub fn spent(cost_usd: f64) -> Self {
        Self {
            cost_usd,
            cache_hit: false,
            saved_usd: None,
        }
    }

    /// A response-cache hit: nothing spent, `avoided` saved.
    pub fn cache_hit(avoided: f64) -> Self {
        Self {
            cost_usd: 0.0,
            cache_hit: true,
            saved_usd: Some(avoided),
        }
    }

    /// Write the cost fields into an OpenAI-shaped `usage` object
    /// (`cost_usd` always; `cache_hit`/`saved_usd` only on a hit).
    pub fn write_into(&self, usage: &mut Value) {
        usage["cost_usd"] = serde_json::json!(self.cost_usd);
        if self.cache_hit {
            usage["cache_hit"] = Value::Bool(true);
        }
        if let Some(saved) = self.saved_usd {
            usage["saved_usd"] = serde_json::json!(saved);
        }
    }
}

/// Token accounting for one call, as recorded on its ledger/prompt rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct TokenMeta {
    /// Whole prompt, including any provider-cache-served share.
    pub prompt: u32,
    pub completion: u32,
    pub total: u32,
    /// Prompt tokens served from the provider's prompt cache.
    pub cache_read: u32,
    /// Prompt tokens written to the provider's prompt cache.
    pub cache_write: u32,
    /// Reasoning/thinking tokens, when the provider reported a figure.
    pub reasoning: Option<u32>,
    /// True when the provider reported no usage and the counts are the
    /// router's character-count estimate (the ledger's `tokens_estimated`).
    pub estimated: bool,
}

impl TokenMeta {
    pub fn new(prompt: u32, completion: u32) -> Self {
        Self {
            prompt,
            completion,
            total: prompt + completion,
            ..Default::default()
        }
    }
}

/// Where the time went. `latency_ms`/`ttft_ms`/`attempts` are the prompt
/// row's values; `total_ms` additionally covers the router's own work before
/// dispatch (auth, policy, guardrails, routing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct TimingMeta {
    /// Request received → response (or, for a stream, terminal chunk) ready.
    pub total_ms: i64,
    /// Dispatch → completion, including retries and fallback hops — the
    /// latency the router records for the call. 0 on a cache hit.
    pub latency_ms: i64,
    /// Duration of the provider call that produced the answer. `None` on a
    /// cache hit (no provider was called).
    pub provider_ms: Option<i64>,
    /// Time to first token (first byte for a stream, headers for a
    /// non-streamed call); `None` where unmeasurable or on a cache hit.
    pub ttft_ms: Option<i64>,
    /// Provider calls made, retries and fallback hops included. 0 on a hit.
    pub attempts: i64,
    /// Hops to a different model/engine along the fallback chain.
    pub fallbacks: i64,
}

/// The `x_router` object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouterMeta {
    /// The model/engine name the caller sent.
    pub requested_model: String,
    /// The concrete model that answered — the ledger row's `model`.
    pub model: String,
    /// The provider that answered — the ledger row's `provider`.
    pub provider: String,
    /// Settings actually sent to the provider after router resolution and
    /// adapter defaults (endpoint-specific keys; see README).
    pub settings: Value,
    /// Token accounting (chat completions and embeddings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenMeta>,
    /// Results returned (search only) — the ledger row's `tokens_in` there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<i64>,
    pub cost: CallCost,
    pub timing: TimingMeta,
}

impl RouterMeta {
    /// Insert as `x_router` into a JSON response object.
    pub fn attach(&self, body: &mut Value) {
        body["x_router"] = self.to_value();
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("RouterMeta serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> RouterMeta {
        RouterMeta {
            requested_model: "balanced".into(),
            model: "big-model".into(),
            provider: "p".into(),
            settings: serde_json::json!({"temperature": 0.2}),
            tokens: Some(TokenMeta::new(10, 5)),
            results: None,
            cost: CallCost::spent(0.5),
            timing: TimingMeta {
                total_ms: 12,
                latency_ms: 10,
                provider_ms: Some(9),
                ttft_ms: None,
                attempts: 1,
                fallbacks: 0,
            },
        }
    }

    #[test]
    fn serializes_the_documented_shape() {
        let v = meta().to_value();
        assert_eq!(v["model"], "big-model");
        assert_eq!(v["requested_model"], "balanced");
        assert_eq!(v["tokens"]["total"], 15);
        assert!(v["tokens"]["reasoning"].is_null());
        assert_eq!(v["tokens"]["estimated"], false);
        assert_eq!(v["cost"]["cost_usd"].as_f64(), Some(0.5));
        assert_eq!(v["cost"]["cache_hit"], false);
        assert!(v["cost"].get("saved_usd").is_none());
        assert_eq!(v["timing"]["provider_ms"], 9);
        assert!(v["timing"]["ttft_ms"].is_null());
        assert!(v.get("results").is_none(), "results is search-only");
    }

    #[test]
    fn search_shape_carries_results_not_tokens() {
        let mut m = meta();
        m.tokens = None;
        m.results = Some(3);
        m.cost = CallCost::cache_hit(0.035);
        let v = m.to_value();
        assert!(v.get("tokens").is_none());
        assert_eq!(v["results"], 3);
        assert_eq!(v["cost"]["cost_usd"].as_f64(), Some(0.0));
        assert_eq!(v["cost"]["saved_usd"].as_f64(), Some(0.035));
    }

    #[test]
    fn write_into_adds_cache_fields_only_on_a_hit() {
        let mut usage = serde_json::json!({});
        CallCost::spent(1.0).write_into(&mut usage);
        assert_eq!(usage, serde_json::json!({"cost_usd": 1.0}));
        let mut usage = serde_json::json!({});
        CallCost::cache_hit(2.0).write_into(&mut usage);
        assert_eq!(
            usage,
            serde_json::json!({"cost_usd": 0.0, "cache_hit": true, "saved_usd": 2.0})
        );
    }

    #[test]
    fn attach_inserts_x_router() {
        let mut body = serde_json::json!({"usage": {}});
        meta().attach(&mut body);
        assert_eq!(body["x_router"]["provider"], "p");
    }
}
