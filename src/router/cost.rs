use std::collections::HashMap;

pub struct CostCalculator {
    pricing: HashMap<String, ModelPricing>,
}

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
    fn simple(input_per_million: f64, output_per_million: f64) -> Self {
        Self {
            input_per_million,
            output_per_million,
            cache_read_per_million: None,
            cache_write_per_million: None,
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
        pricing.insert(
            "claude-haiku-4-5".to_string(),
            ModelPricing::simple(0.80, 4.0),
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
        pricing.insert(
            "gpt-4o".to_string(),
            ModelPricing::simple(2.50, 10.0),
        );
        pricing.insert(
            "gpt-4o-mini".to_string(),
            ModelPricing::simple(0.15, 0.60),
        );
        pricing.insert(
            "gpt-4-turbo".to_string(),
            ModelPricing::simple(10.0, 30.0),
        );
        pricing.insert(
            "gpt-4".to_string(),
            ModelPricing::simple(30.0, 60.0),
        );
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
        pricing.insert(
            "qwen-max".to_string(),
            ModelPricing::simple(0.40, 1.20),
        );
        pricing.insert(
            "qwen-plus".to_string(),
            ModelPricing::simple(0.07, 0.21),
        );
        pricing.insert(
            "qwen-turbo".to_string(),
            ModelPricing::simple(0.05, 0.10),
        );
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
            ModelPricing::simple(0.80, 4.0),
        );
        // Unknown models return 0 (Ollama etc)
        Self { pricing }
    }

    pub fn new_with_config(config_pricing: &[crate::config::schema::PricingEntry]) -> Self {
        let mut calc = Self::new();
        for entry in config_pricing {
            calc.pricing.insert(
                entry.model.to_lowercase(),
                ModelPricing {
                    input_per_million: entry.input_per_million,
                    output_per_million: entry.output_per_million,
                    cache_read_per_million: entry.cache_read_per_million,
                    cache_write_per_million: entry.cache_write_per_million,
                },
            );
        }
        calc
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

    /// Whether `model` has a pricing entry. A model without one is recorded in
    /// the ledger at zero cost, so callers presenting cost figures use this to
    /// flag them as incomplete rather than free.
    pub fn has_price(&self, model: &str) -> bool {
        self.pricing.contains_key(&Self::pricing_key(model))
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
        match self.pricing.get(&Self::pricing_key(model)) {
            Some(p) => {
                (prompt_tokens as f64 / 1_000_000.0) * p.input_per_million
                    + (completion_tokens as f64 / 1_000_000.0) * p.output_per_million
                    + (cache_read_tokens as f64 / 1_000_000.0) * p.cache_read_rate()
                    + (cache_write_tokens as f64 / 1_000_000.0) * p.cache_write_rate()
            }
            None => 0.0,
        }
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

    #[test]
    fn calculate_matches_calculate_with_cache_when_no_cache() {
        let calc = CostCalculator::default();
        let a = calc.calculate("gpt-4o", 1000, 500);
        let b = calc.calculate_with_cache("gpt-4o", 1000, 500, 0, 0);
        assert_eq!(a, b);
    }
}
