use std::collections::HashMap;

pub struct CostCalculator {
    pricing: HashMap<String, ModelPricing>,
}

struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
}

impl CostCalculator {
    pub fn new() -> Self {
        let mut pricing = HashMap::new();
        // Anthropic models (as of early 2025)
        pricing.insert(
            "claude-opus-4-6".to_string(),
            ModelPricing { input_per_million: 15.0, output_per_million: 75.0 },
        );
        pricing.insert(
            "claude-sonnet-4-6".to_string(),
            ModelPricing { input_per_million: 3.0, output_per_million: 15.0 },
        );
        pricing.insert(
            "claude-haiku-4-5".to_string(),
            ModelPricing { input_per_million: 0.80, output_per_million: 4.0 },
        );
        pricing.insert(
            "claude-3-5-sonnet-20241022".to_string(),
            ModelPricing { input_per_million: 3.0, output_per_million: 15.0 },
        );
        pricing.insert(
            "claude-3-5-haiku-20241022".to_string(),
            ModelPricing { input_per_million: 0.80, output_per_million: 4.0 },
        );
        pricing.insert(
            "claude-3-opus-20240229".to_string(),
            ModelPricing { input_per_million: 15.0, output_per_million: 75.0 },
        );
        // OpenAI models
        pricing.insert(
            "gpt-4o".to_string(),
            ModelPricing { input_per_million: 2.50, output_per_million: 10.0 },
        );
        pricing.insert(
            "gpt-4o-mini".to_string(),
            ModelPricing { input_per_million: 0.15, output_per_million: 0.60 },
        );
        pricing.insert(
            "gpt-4-turbo".to_string(),
            ModelPricing { input_per_million: 10.0, output_per_million: 30.0 },
        );
        pricing.insert(
            "gpt-4".to_string(),
            ModelPricing { input_per_million: 30.0, output_per_million: 60.0 },
        );
        pricing.insert(
            "gpt-3.5-turbo".to_string(),
            ModelPricing { input_per_million: 0.50, output_per_million: 1.50 },
        );
        // Gemini
        pricing.insert(
            "gemini-1.5-pro".to_string(),
            ModelPricing { input_per_million: 1.25, output_per_million: 5.0 },
        );
        pricing.insert(
            "gemini-1.5-flash".to_string(),
            ModelPricing { input_per_million: 0.075, output_per_million: 0.30 },
        );
        // Gemini 2.5 on Vertex — prompts ≤ 200K tier. Long-context tier is higher.
        // Reference: https://cloud.google.com/vertex-ai/generative-ai/pricing
        pricing.insert(
            "gemini-2.5-pro".to_string(),
            ModelPricing { input_per_million: 1.25, output_per_million: 10.0 },
        );
        pricing.insert(
            "gemini-2.5-flash".to_string(),
            ModelPricing { input_per_million: 0.30, output_per_million: 2.50 },
        );
        pricing.insert(
            "gemini-2.5-flash-lite".to_string(),
            ModelPricing { input_per_million: 0.10, output_per_million: 0.40 },
        );
        // DeepSeek models
        pricing.insert(
            "deepseek-chat".to_string(),
            ModelPricing { input_per_million: 0.14, output_per_million: 0.28 },
        );
        pricing.insert(
            "deepseek-coder".to_string(),
            ModelPricing { input_per_million: 0.14, output_per_million: 0.28 },
        );
        pricing.insert(
            "deepseek-reasoner".to_string(),
            ModelPricing { input_per_million: 0.55, output_per_million: 2.19 },
        );
        // Alibaba Qwen (Tongyi)
        pricing.insert(
            "qwen-max".to_string(),
            ModelPricing { input_per_million: 0.40, output_per_million: 1.20 },
        );
        pricing.insert(
            "qwen-plus".to_string(),
            ModelPricing { input_per_million: 0.07, output_per_million: 0.21 },
        );
        pricing.insert(
            "qwen-turbo".to_string(),
            ModelPricing { input_per_million: 0.05, output_per_million: 0.10 },
        );
        // ByteDance Doubao
        pricing.insert(
            "doubao-lite-4k".to_string(),
            ModelPricing { input_per_million: 0.10, output_per_million: 0.10 },
        );
        pricing.insert(
            "doubao-lite-32k".to_string(),
            ModelPricing { input_per_million: 0.10, output_per_million: 0.10 },
        );
        pricing.insert(
            "doubao-pro-4k".to_string(),
            ModelPricing { input_per_million: 0.80, output_per_million: 0.80 },
        );
        pricing.insert(
            "doubao-pro-32k".to_string(),
            ModelPricing { input_per_million: 0.80, output_per_million: 0.80 },
        );
        // Claude on Vertex — versioned IDs (@YYYYMMDD). Same rates as Anthropic direct.
        pricing.insert(
            "claude-opus-4-5@20250101".to_string(),
            ModelPricing { input_per_million: 15.0, output_per_million: 75.0 },
        );
        pricing.insert(
            "claude-sonnet-4-6@20250514".to_string(),
            ModelPricing { input_per_million: 3.0, output_per_million: 15.0 },
        );
        pricing.insert(
            "claude-sonnet-4-5@20250929".to_string(),
            ModelPricing { input_per_million: 3.0, output_per_million: 15.0 },
        );
        pricing.insert(
            "claude-haiku-4-5@20251001".to_string(),
            ModelPricing { input_per_million: 0.80, output_per_million: 4.0 },
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
                },
            );
        }
        calc
    }

    pub fn calculate(&self, model: &str, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        // Strip provider prefix (e.g. "anthropic/claude-haiku-4-5" -> "claude-haiku-4-5")
        let model_key = if let Some(pos) = model.find('/') {
            &model[pos + 1..]
        } else {
            model
        };
        let model_lower = model_key.to_lowercase();
        match self.pricing.get(&model_lower) {
            Some(p) => {
                (prompt_tokens as f64 / 1_000_000.0) * p.input_per_million
                    + (completion_tokens as f64 / 1_000_000.0) * p.output_per_million
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
}
