use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, RwLock};

pub struct CostCalculator {
    pricing: HashMap<String, ModelPricing>,
    /// Keys priced by the operator's `[[pricing]]` entries. They win over
    /// `live` prices, which win over the built-in table.
    configured: HashSet<String>,
    /// Prices fetched at run time (e.g. Foundry deployments from the Azure
    /// Retail Prices API), replaced wholesale on each refresh.
    live: RwLock<HashMap<String, ModelPricing>>,
    /// Pricing keys already reported as unpriced, so the warning fires once
    /// per model per process instead of on every request.
    warned_unpriced: Mutex<HashSet<String>>,
}

#[derive(Clone, Copy)]
struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
    /// Rate for tokens served from the provider's prompt cache. Defaults to
    /// `CACHE_READ_DISCOUNT` of `input_per_million` when not set explicitly.
    cache_read_per_million: Option<f64>,
    /// Rate for tokens written to the provider's prompt cache. Defaults to
    /// `CACHE_WRITE_PREMIUM` of `input_per_million` when not set explicitly.
    cache_write_per_million: Option<f64>,
}

/// Anthropic-style cache read tokens cost ~10% of a standard input token.
const CACHE_READ_DISCOUNT: f64 = 0.1;
/// Anthropic-style cache write (5-minute TTL) tokens cost ~125% of a standard input token.
const CACHE_WRITE_PREMIUM: f64 = 1.25;

impl ModelPricing {
    fn from_entry(entry: &crate::config::schema::PricingEntry) -> Self {
        Self {
            input_per_million: entry.input_per_million,
            output_per_million: entry.output_per_million,
            cache_read_per_million: entry.cache_read_per_million,
            cache_write_per_million: entry.cache_write_per_million,
        }
    }

    fn simple(input_per_million: f64, output_per_million: f64) -> Self {
        Self {
            input_per_million,
            output_per_million,
            cache_read_per_million: None,
            cache_write_per_million: None,
        }
    }

    /// Pricing with explicitly published cache rates.
    fn with_cache(
        input_per_million: f64,
        output_per_million: f64,
        cache_read_per_million: f64,
        cache_write_per_million: f64,
    ) -> Self {
        Self {
            input_per_million,
            output_per_million,
            cache_read_per_million: Some(cache_read_per_million),
            cache_write_per_million: Some(cache_write_per_million),
        }
    }

    fn cache_read_rate(&self) -> f64 {
        self.cache_read_per_million
            .unwrap_or(self.input_per_million * CACHE_READ_DISCOUNT)
    }

    fn cache_write_rate(&self) -> f64 {
        self.cache_write_per_million
            .unwrap_or(self.input_per_million * CACHE_WRITE_PREMIUM)
    }
}

impl CostCalculator {
    pub fn new() -> Self {
        let mut pricing = HashMap::new();
        // Anthropic models (as of early 2025)
        pricing.insert(
            "claude-opus-4-6".to_string(),
            ModelPricing::simple(15.0, 75.0),
        );
        pricing.insert(
            "claude-sonnet-4-6".to_string(),
            ModelPricing::simple(3.0, 15.0),
        );
        // Claude Opus 5.5 / Sonnet 5.5 / Haiku 4.5 — first-party list prices,
        // checked 2026-10-02. Opus 5.5 cache reads are 0.05x input, not the
        // default 0.1x, so its cache rates are explicit. Vertex AI's global
        // endpoint lists the same rates; its regional endpoints list ~10% higher.
        // Reference: https://platform.claude.com/docs/en/about-claude/pricing
        pricing.insert(
            "claude-opus-5-5".to_string(),
            ModelPricing::with_cache(4.0, 20.0, 0.20, 5.0),
        );
        pricing.insert(
            "claude-sonnet-5-5".to_string(),
            ModelPricing::with_cache(2.0, 10.0, 0.20, 2.50),
        );
        pricing.insert(
            "claude-haiku-4-5".to_string(),
            ModelPricing::simple(1.0, 5.0),
        );
        pricing.insert(
            "claude-3-5-sonnet-20241022".to_string(),
            ModelPricing::simple(3.0, 15.0),
        );
        pricing.insert(
            "claude-3-5-haiku-20241022".to_string(),
            ModelPricing::simple(0.80, 4.0),
        );
        pricing.insert(
            "claude-3-opus-20240229".to_string(),
            ModelPricing::simple(15.0, 75.0),
        );
        // OpenAI models
        pricing.insert("gpt-4o".to_string(), ModelPricing::simple(2.50, 10.0));
        pricing.insert("gpt-4o-mini".to_string(), ModelPricing::simple(0.15, 0.60));
        pricing.insert("gpt-4-turbo".to_string(), ModelPricing::simple(10.0, 30.0));
        pricing.insert("gpt-4".to_string(), ModelPricing::simple(30.0, 60.0));
        pricing.insert(
            "gpt-3.5-turbo".to_string(),
            ModelPricing::simple(0.50, 1.50),
        );
        // Gemini
        pricing.insert(
            "gemini-1.5-pro".to_string(),
            ModelPricing::simple(1.25, 5.0),
        );
        pricing.insert(
            "gemini-1.5-flash".to_string(),
            ModelPricing::simple(0.075, 0.30),
        );
        // Gemini 2.5 on Vertex — prompts ≤ 200K tier. Long-context tier is higher.
        // Reference: https://cloud.google.com/vertex-ai/generative-ai/pricing
        pricing.insert(
            "gemini-2.5-pro".to_string(),
            ModelPricing::simple(1.25, 10.0),
        );
        pricing.insert(
            "gemini-2.5-flash".to_string(),
            ModelPricing::simple(0.30, 2.50),
        );
        pricing.insert(
            "gemini-2.5-flash-lite".to_string(),
            ModelPricing::simple(0.10, 0.40),
        );
        // Gemini 3.x on Vertex: Standard tier, global endpoint, prompts <= 200K.
        // Non-global endpoints list ~10% higher; prompts > 200K and Priority
        // tier are higher still. Cache read is the published cached-input rate.
        // Gemini implicit caching has no write premium (explicit-cache storage
        // is billed per hour and is not modelled here), so cache writes are
        // priced at the input rate.
        // Reference: https://cloud.google.com/vertex-ai/generative-ai/pricing
        pricing.insert(
            "gemini-3.1-pro-preview".to_string(),
            ModelPricing::with_cache(2.00, 12.00, 0.20, 2.00),
        );
        pricing.insert(
            "gemini-3.5-flash".to_string(),
            ModelPricing::with_cache(1.50, 9.00, 0.15, 1.50),
        );
        pricing.insert(
            "gemini-3.5-flash-lite".to_string(),
            ModelPricing::with_cache(0.30, 2.50, 0.03, 0.30),
        );
        // Text/image/video input rate; audio input lists at 2x.
        pricing.insert(
            "gemini-3.1-flash-lite".to_string(),
            ModelPricing::with_cache(0.25, 1.50, 0.025, 0.25),
        );
        // OpenAI open-weight models on Vertex Model-as-a-Service. Vertex model
        // IDs carry a `-maas` suffix; the bare names are priced identically.
        // gpt-oss-120b lists no cached-input rate, so the default discount
        // applies to it.
        // Reference: https://cloud.google.com/vertex-ai/generative-ai/pricing
        for name in ["gpt-oss-120b", "gpt-oss-120b-maas"] {
            pricing.insert(name.to_string(), ModelPricing::simple(0.09, 0.36));
        }
        for name in ["gpt-oss-20b", "gpt-oss-20b-maas"] {
            pricing.insert(
                name.to_string(),
                ModelPricing::with_cache(0.07, 0.25, 0.007, 0.07),
            );
        }
        // DeepSeek models
        pricing.insert(
            "deepseek-chat".to_string(),
            ModelPricing::simple(0.14, 0.28),
        );
        pricing.insert(
            "deepseek-coder".to_string(),
            ModelPricing::simple(0.14, 0.28),
        );
        pricing.insert(
            "deepseek-reasoner".to_string(),
            ModelPricing::simple(0.55, 2.19),
        );
        // Alibaba Qwen (Tongyi)
        pricing.insert("qwen-max".to_string(), ModelPricing::simple(0.40, 1.20));
        pricing.insert("qwen-plus".to_string(), ModelPricing::simple(0.07, 0.21));
        pricing.insert("qwen-turbo".to_string(), ModelPricing::simple(0.05, 0.10));
        // ByteDance Doubao
        pricing.insert(
            "doubao-lite-4k".to_string(),
            ModelPricing::simple(0.10, 0.10),
        );
        pricing.insert(
            "doubao-lite-32k".to_string(),
            ModelPricing::simple(0.10, 0.10),
        );
        pricing.insert(
            "doubao-pro-4k".to_string(),
            ModelPricing::simple(0.80, 0.80),
        );
        pricing.insert(
            "doubao-pro-32k".to_string(),
            ModelPricing::simple(0.80, 0.80),
        );
        // Claude on Vertex — versioned IDs (@YYYYMMDD). Same rates as Anthropic direct.
        pricing.insert(
            "claude-opus-4-5@20250101".to_string(),
            ModelPricing::simple(15.0, 75.0),
        );
        pricing.insert(
            "claude-sonnet-4-6@20250514".to_string(),
            ModelPricing::simple(3.0, 15.0),
        );
        pricing.insert(
            "claude-sonnet-4-5@20250929".to_string(),
            ModelPricing::simple(3.0, 15.0),
        );
        pricing.insert(
            "claude-haiku-4-5@20251001".to_string(),
            ModelPricing::simple(1.0, 5.0),
        );
        // Unknown models cost 0 (Ollama etc.) and are reported once as unpriced.
        Self {
            pricing,
            configured: HashSet::new(),
            live: RwLock::new(HashMap::new()),
            warned_unpriced: Mutex::new(HashSet::new()),
        }
    }

    pub fn new_with_config(config_pricing: &[crate::config::schema::PricingEntry]) -> Self {
        let mut calc = Self::new();
        for entry in config_pricing {
            let key = entry.model.to_lowercase();
            calc.configured.insert(key.clone());
            calc.pricing.insert(key, ModelPricing::from_entry(entry));
        }
        calc
    }

    /// Replace the run-time prices with `entries` (keyed by model name, as
    /// `[[pricing]]` is). An operator's `[[pricing]]` entry for the same model
    /// still wins. Returns how many models now carry a live price.
    pub fn replace_live_pricing(&self, entries: &[crate::config::schema::PricingEntry]) -> usize {
        let fresh: HashMap<String, ModelPricing> = entries
            .iter()
            .map(|e| (e.model.to_lowercase(), ModelPricing::from_entry(e)))
            .collect();
        let count = fresh.len();
        match self.live.write() {
            Ok(mut live) => *live = fresh,
            Err(poisoned) => *poisoned.into_inner() = fresh,
        }
        count
    }

    /// Normalise a model name to the key the pricing table uses: strip the
    /// provider prefix (`anthropic/claude-haiku-4-5` -> `claude-haiku-4-5`)
    /// and lowercase.
    fn pricing_key(model: &str) -> String {
        let model_key = match model.find('/') {
            Some(pos) => &model[pos + 1..],
            None => model,
        };
        model_key.to_lowercase()
    }

    /// Resolve `model` to its pricing entry.
    ///
    /// Tries the prefix-stripped key first (`vertex/anthropic/claude-x` ->
    /// `anthropic/claude-x`), so a vendor-qualified pricing entry wins when the
    /// operator defined one. When that misses, falls back to the last path
    /// segment (`claude-x`) — operators usually price models by bare name, and
    /// multi-segment targets (e.g. Claude on Vertex) would otherwise never
    /// match.
    ///
    /// Each layer is searched in that key order: the operator's `[[pricing]]`
    /// first, then live prices, then the built-in table.
    fn resolve(&self, model: &str) -> Option<ModelPricing> {
        let key = Self::pricing_key(model);
        let basename = key.rfind('/').map(|pos| key[pos + 1..].to_string());
        let candidates: Vec<&str> = std::iter::once(key.as_str()).chain(basename.as_deref()).collect();
        if let Some(c) = candidates.iter().find(|c| self.configured.contains(**c)) {
            return self.pricing.get(*c).copied();
        }
        let live = match self.live.read() {
            Ok(live) => live,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(p) = candidates.iter().find_map(|c| live.get(*c)) {
            return Some(*p);
        }
        candidates.iter().find_map(|c| self.pricing.get(*c)).copied()
    }

    /// Whether `model` has a pricing entry. A model without one is recorded in
    /// the ledger at zero cost, so callers presenting cost figures use this to
    /// flag them as incomplete rather than free.
    /// A flat per-unit rate (dollars per query, image, ...) for a pseudo-model
    /// such as `search/bing_grounding`, matched on the whole key (no prefix
    /// stripping): the operator's `[[pricing]]` first, then live prices, then
    /// the built-in table. The rate is the entry's `input_per_million`.
    pub fn flat_rate(&self, key: &str) -> Option<f64> {
        let k = key.to_lowercase();
        if self.configured.contains(&k) {
            return self.pricing.get(&k).map(|p| p.input_per_million);
        }
        let live = match self.live.read() {
            Ok(live) => live,
            Err(poisoned) => poisoned.into_inner(),
        };
        live.get(&k).or_else(|| self.pricing.get(&k)).map(|p| p.input_per_million)
    }

    pub fn has_price(&self, model: &str) -> bool {
        self.resolve(model).is_some()
    }

    /// Cost for a request with no cache activity. Equivalent to
    /// `calculate_with_cache(model, prompt_tokens, completion_tokens, 0, 0)`.
    pub fn calculate(&self, model: &str, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        self.calculate_with_cache(model, prompt_tokens, completion_tokens, 0, 0)
    }

    /// Cost for a request that may have read from or written to the provider's prompt
    /// cache. `prompt_tokens` should be the *non-cached* input tokens (as reported by
    /// providers alongside `cache_read_tokens`/`cache_write_tokens`) so cached tokens
    /// aren't double-billed at the standard input rate.
    pub fn calculate_with_cache(
        &self,
        model: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    ) -> f64 {
        match self.resolve(model) {
            Some(p) => {
                (prompt_tokens as f64 / 1_000_000.0) * p.input_per_million
                    + (completion_tokens as f64 / 1_000_000.0) * p.output_per_million
                    + (cache_read_tokens as f64 / 1_000_000.0) * p.cache_read_rate()
                    + (cache_write_tokens as f64 / 1_000_000.0) * p.cache_write_rate()
            }
            None => {
                self.note_unpriced(model);
                0.0
            }
        }
    }

    /// Cost of the same request with no prompt cache: the cache-read and
    /// cache-write tokens priced at the standard input rate. Arguments are as
    /// for `calculate_with_cache` (`prompt_tokens` is the non-cached share).
    pub fn calculate_no_cache(
        &self,
        model: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    ) -> f64 {
        let whole_prompt = prompt_tokens
            .saturating_add(cache_read_tokens)
            .saturating_add(cache_write_tokens);
        self.calculate(model, whole_prompt, completion_tokens)
    }

    /// Record that `model` was costed without a pricing entry. Logs a warning
    /// the first time each model is seen, so a missing price shows up in the
    /// logs rather than passing as a silent zero in the cost ledger. Returns
    /// whether this call emitted the warning.
    pub fn note_unpriced(&self, model: &str) -> bool {
        let key = Self::pricing_key(model);
        let first = match self.warned_unpriced.lock() {
            Ok(mut seen) => seen.insert(key.clone()),
            Err(poisoned) => poisoned.into_inner().insert(key.clone()),
        };
        if first {
            tracing::warn!(
                model = %model,
                pricing_key = %key,
                "no pricing entry for model; its cost is recorded as $0 \
                 (add a [[pricing]] entry to the config to price it)"
            );
        }
        first
    }
}

impl Default for CostCalculator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_model_pricing() {
        let calc = CostCalculator::default();
        // DeepSeek: 0.14/M input
        let cost = calc.calculate("deepseek-chat", 1_000_000, 0);
        assert!((cost - 0.14).abs() < 0.001, "deepseek-chat input: {cost}");
        // Qwen: 0.40/M input
        let cost = calc.calculate("qwen-max", 1_000_000, 0);
        assert!((cost - 0.40).abs() < 0.001, "qwen-max input: {cost}");
        // Doubao: 0.80/M input
        let cost = calc.calculate("doubao-pro-32k", 1_000_000, 0);
        assert!((cost - 0.80).abs() < 0.001, "doubao-pro-32k input: {cost}");
    }

    #[test]
    fn cache_read_tokens_billed_at_discount() {
        let calc = CostCalculator::default();
        // claude-sonnet-4-6: $3/M input -> cache read defaults to 10% = $0.30/M
        let cost = calc.calculate_with_cache("claude-sonnet-4-6", 0, 0, 1_000_000, 0);
        assert!((cost - 0.30).abs() < 0.001, "cache read cost: {cost}");
    }

    #[test]
    fn cache_write_tokens_billed_at_premium() {
        let calc = CostCalculator::default();
        // claude-sonnet-4-6: $3/M input -> cache write defaults to 125% = $3.75/M
        let cost = calc.calculate_with_cache("claude-sonnet-4-6", 0, 0, 0, 1_000_000);
        assert!((cost - 3.75).abs() < 0.001, "cache write cost: {cost}");
    }

    #[test]
    fn no_cache_cost_prices_cached_prompt_tokens_at_the_input_rate() {
        let calc = CostCalculator::default();
        // claude-sonnet-5-5: $2/M input, $10/M output, $0.20/M cache read, $2.50/M cache write.
        let actual = calc.calculate_with_cache("anthropic/claude-sonnet-5-5", 1_000_000, 100_000, 4_000_000, 500_000);
        let no_cache = calc.calculate_no_cache("anthropic/claude-sonnet-5-5", 1_000_000, 100_000, 4_000_000, 500_000);
        assert!((actual - (2.0 + 1.0 + 0.8 + 1.25)).abs() < 1e-9, "actual: {actual}");
        assert!((no_cache - (5.5 * 2.0 + 1.0)).abs() < 1e-9, "no cache: {no_cache}");
        // No cache activity: the two figures agree.
        assert_eq!(
            calc.calculate_with_cache("claude-sonnet-5-5", 1000, 10, 0, 0),
            calc.calculate_no_cache("claude-sonnet-5-5", 1000, 10, 0, 0)
        );
    }

    #[test]
    fn has_price_normalises_prefix_and_case() {
        let calc = CostCalculator::default();
        assert!(calc.has_price("gpt-4o"));
        assert!(calc.has_price("openai/gpt-4o"));
        assert!(calc.has_price("GPT-4o"));
    }

    #[test]
    fn has_price_is_false_for_unknown_model() {
        let calc = CostCalculator::default();
        assert!(!calc.has_price("not-a-real-model"));
        assert!(!calc.has_price("ollama/llama3"));
        assert!(!calc.has_price("vertex/anthropic/not-a-real-model"));
    }

    #[test]
    fn three_segment_path_falls_back_to_basename() {
        use crate::config::schema::PricingEntry;
        let calc = CostCalculator::new_with_config(&[PricingEntry {
            model: "claude-x".into(),
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        }]);
        // Only the bare name is priced; the 3-segment pinned target must match it.
        assert!(calc.has_price("vertex/anthropic/claude-x"));
        assert!(calc.has_price("vertex/Anthropic/Claude-X"));
        let cost = calc.calculate("vertex/anthropic/claude-x", 1_000_000, 0);
        assert!((cost - 1.0).abs() < 0.001, "basename fallback cost: {cost}");
        let cost = calc.calculate_with_cache("vertex/anthropic/claude-x", 0, 1_000_000, 0, 0);
        assert!(
            (cost - 2.0).abs() < 0.001,
            "basename fallback output cost: {cost}"
        );
    }

    #[test]
    fn builtin_vertex_style_target_prices_via_basename() {
        let calc = CostCalculator::default();
        // Built-in table prices `claude-haiku-4-5`; the pinned 3-segment target
        // must resolve to it.
        assert!(calc.has_price("vertex/anthropic/claude-haiku-4-5"));
        let cost = calc.calculate("vertex/anthropic/claude-haiku-4-5", 1_000_000, 0);
        assert!((cost - 1.0).abs() < 0.001, "vertex haiku input: {cost}");
    }

    #[test]
    fn builtin_opus_and_sonnet_5_5_price_unpinned_vertex_targets() {
        let calc = CostCalculator::default();
        // Both are served on Vertex under the bare id, with no `@version` pin.
        for (model, input, output, cache_read, cache_write) in [
            ("vertex/anthropic/claude-opus-5-5", 4.0, 20.0, 0.20, 5.0),
            ("vertex/anthropic/claude-sonnet-5-5", 2.0, 10.0, 0.20, 2.50),
        ] {
            assert!(calc.has_price(model), "{model} must be priced");
            let cost = calc.calculate_with_cache(model, 1_000_000, 1_000_000, 0, 0);
            assert!((cost - (input + output)).abs() < 0.001, "{model} in+out: {cost}");
            let cost = calc.calculate_with_cache(model, 0, 0, 1_000_000, 0);
            assert!((cost - cache_read).abs() < 0.001, "{model} cache read: {cost}");
            let cost = calc.calculate_with_cache(model, 0, 0, 0, 1_000_000);
            assert!((cost - cache_write).abs() < 0.001, "{model} cache write: {cost}");
        }
    }

    #[test]
    fn builtin_haiku_4_5_pinned_vertex_target_uses_list_price() {
        let calc = CostCalculator::default();
        let cost = calc.calculate("vertex/anthropic/claude-haiku-4-5@20251001", 1_000_000, 1_000_000);
        assert!((cost - 6.0).abs() < 0.001, "pinned haiku in+out: {cost}");
    }

    #[test]
    fn vendor_prefixed_entry_wins_over_basename() {
        use crate::config::schema::PricingEntry;
        let calc = CostCalculator::new_with_config(&[
            PricingEntry {
                model: "anthropic/claude-x".into(),
                input_per_million: 5.0,
                output_per_million: 10.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
            PricingEntry {
                model: "claude-x".into(),
                input_per_million: 1.0,
                output_per_million: 2.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
        ]);
        // The stripped key `anthropic/claude-x` is more specific and must win
        // over the basename entry.
        let cost = calc.calculate("vertex/anthropic/claude-x", 1_000_000, 0);
        assert!((cost - 5.0).abs() < 0.001, "vendor-prefixed entry: {cost}");
        // A plain 2-segment path still uses the basename entry.
        let cost = calc.calculate("local/claude-x", 1_000_000, 0);
        assert!((cost - 1.0).abs() < 0.001, "basename entry: {cost}");
    }

    #[test]
    fn has_price_sees_models_added_through_config() {
        use crate::config::schema::PricingEntry;
        let calc = CostCalculator::new_with_config(&[PricingEntry {
            model: "custom-model".into(),
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        }]);
        assert!(calc.has_price("custom-model"));
        assert!(calc.has_price("local/Custom-Model"));
    }

    fn assert_cost(calc: &CostCalculator, model: &str, tokens: (u32, u32, u32, u32), want: f64) {
        let (input, output, read, write) = tokens;
        let got = calc.calculate_with_cache(model, input, output, read, write);
        assert!(
            (got - want).abs() < 1e-9,
            "{model} {tokens:?}: got {got}, want {want}"
        );
    }

    #[test]
    fn gemini_3_vertex_list_prices() {
        let calc = CostCalculator::default();
        const M: u32 = 1_000_000;
        // (model, input, output, cache read, cache write) per 1M tokens.
        let table = [
            ("google/gemini-3.1-pro-preview", 2.00, 12.00, 0.20, 2.00),
            ("google/gemini-3.5-flash", 1.50, 9.00, 0.15, 1.50),
            ("google/gemini-3.5-flash-lite", 0.30, 2.50, 0.03, 0.30),
            ("google/gemini-3.1-flash-lite", 0.25, 1.50, 0.025, 0.25),
        ];
        for (model, input, output, read, write) in table {
            assert!(calc.has_price(model), "{model} should be priced");
            assert!(
                calc.has_price(&format!("vertex/{model}")),
                "{model} via vertex path"
            );
            assert_cost(&calc, model, (M, 0, 0, 0), input);
            assert_cost(&calc, model, (0, M, 0, 0), output);
            assert_cost(&calc, model, (0, 0, M, 0), read);
            assert_cost(&calc, model, (0, 0, 0, M), write);
        }
    }

    #[test]
    fn gpt_oss_vertex_maas_list_prices() {
        let calc = CostCalculator::default();
        const M: u32 = 1_000_000;
        for model in ["openai/gpt-oss-20b-maas", "gpt-oss-20b"] {
            assert_cost(&calc, model, (M, 0, 0, 0), 0.07);
            assert_cost(&calc, model, (0, M, 0, 0), 0.25);
            assert_cost(&calc, model, (0, 0, M, 0), 0.007);
        }
        for model in ["openai/gpt-oss-120b-maas", "gpt-oss-120b"] {
            assert_cost(&calc, model, (M, 0, 0, 0), 0.09);
            assert_cost(&calc, model, (0, M, 0, 0), 0.36);
            // No published cached-input rate: default discount of input.
            assert_cost(&calc, model, (0, 0, M, 0), 0.09 * CACHE_READ_DISCOUNT);
        }
    }

    #[test]
    fn live_prices_beat_builtins_and_lose_to_configured_entries() {
        use crate::config::schema::PricingEntry;
        let entry = |model: &str, input: f64| PricingEntry {
            model: model.into(),
            input_per_million: input,
            output_per_million: 0.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        };
        let calc = CostCalculator::new_with_config(&[entry("gpt-4.1", 7.0)]);
        assert!(!calc.has_price("gpt-5.6-sol"));
        assert_eq!(calc.replace_live_pricing(&[entry("gpt-5.6-sol", 4.0), entry("GPT-4.1", 2.0)]), 2);
        assert!((calc.calculate("foundry/gpt-5.6-sol", 1_000_000, 0) - 4.0).abs() < 1e-9);
        // The operator's [[pricing]] entry outranks the live price.
        assert!((calc.calculate("foundry/gpt-4.1", 1_000_000, 0) - 7.0).abs() < 1e-9);
        // A refresh replaces the live set wholesale.
        calc.replace_live_pricing(&[]);
        assert!(!calc.has_price("gpt-5.6-sol"));
    }

    #[test]
    fn live_price_overrides_the_builtin_rate() {
        use crate::config::schema::PricingEntry;
        let calc = CostCalculator::new();
        let builtin = calc.calculate("claude-haiku-4-5", 1_000_000, 0);
        calc.replace_live_pricing(&[PricingEntry {
            model: "claude-haiku-4-5".into(),
            input_per_million: builtin + 1.0,
            output_per_million: 0.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        }]);
        assert!((calc.calculate("claude-haiku-4-5", 1_000_000, 0) - (builtin + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn search_engines_are_priced_only_by_config_or_live_prices() {
        use crate::config::schema::PricingEntry;
        let entry = |model: &str, rate: f64| PricingEntry {
            model: model.into(),
            input_per_million: rate,
            output_per_million: 0.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        };
        let calc = CostCalculator::new();
        // No built-in search price: an engine is priced by config or the live feed, or not at all.
        assert_eq!(calc.flat_rate("search/bing_grounding"), None);
        calc.replace_live_pricing(&[entry("search/bing_grounding", 0.015)]);
        assert_eq!(calc.flat_rate("search/bing_grounding"), Some(0.015));
        let configured = CostCalculator::new_with_config(&[entry("search/bing_grounding", 0.02)]);
        configured.replace_live_pricing(&[entry("search/bing_grounding", 0.015)]);
        assert_eq!(configured.flat_rate("search/bing_grounding"), Some(0.02));
    }

    #[test]
    fn unpriced_model_is_reported_once_per_model() {
        let calc = CostCalculator::default();
        assert_eq!(calc.calculate("ollama/llama3", 1000, 1000), 0.0);
        // Already reported: same model, differently cased or prefixed.
        assert!(!calc.note_unpriced("ollama/llama3"));
        assert!(!calc.note_unpriced("other/LLAMA3"));
        // A different unpriced model is reported on its own.
        assert!(calc.note_unpriced("mystery-model"));
        assert!(!calc.note_unpriced("mystery-model"));
    }

    #[test]
    fn priced_model_is_never_reported_unpriced() {
        let calc = CostCalculator::default();
        calc.calculate("gpt-4o", 1000, 1000);
        assert!(calc.warned_unpriced.lock().unwrap().is_empty());
    }

    #[test]
    fn calculate_matches_calculate_with_cache_when_no_cache() {
        let calc = CostCalculator::default();
        let a = calc.calculate("gpt-4o", 1000, 500);
        let b = calc.calculate_with_cache("gpt-4o", 1000, 500, 0, 0);
        assert_eq!(a, b);
    }
}
