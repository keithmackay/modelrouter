use std::collections::HashMap;

pub struct CostCalculator {
    pricing: HashMap<&'static str, ModelPricing>,
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
            "claude-opus-4-6",
            ModelPricing { input_per_million: 15.0, output_per_million: 75.0 },
        );
        pricing.insert(
            "claude-sonnet-4-6",
            ModelPricing { input_per_million: 3.0, output_per_million: 15.0 },
        );
        pricing.insert(
            "claude-haiku-4-5",
            ModelPricing { input_per_million: 0.80, output_per_million: 4.0 },
        );
        pricing.insert(
            "claude-3-5-sonnet-20241022",
            ModelPricing { input_per_million: 3.0, output_per_million: 15.0 },
        );
        pricing.insert(
            "claude-3-5-haiku-20241022",
            ModelPricing { input_per_million: 0.80, output_per_million: 4.0 },
        );
        pricing.insert(
            "claude-3-opus-20240229",
            ModelPricing { input_per_million: 15.0, output_per_million: 75.0 },
        );
        // OpenAI models
        pricing.insert(
            "gpt-4o",
            ModelPricing { input_per_million: 2.50, output_per_million: 10.0 },
        );
        pricing.insert(
            "gpt-4o-mini",
            ModelPricing { input_per_million: 0.15, output_per_million: 0.60 },
        );
        pricing.insert(
            "gpt-4-turbo",
            ModelPricing { input_per_million: 10.0, output_per_million: 30.0 },
        );
        pricing.insert(
            "gpt-4",
            ModelPricing { input_per_million: 30.0, output_per_million: 60.0 },
        );
        pricing.insert(
            "gpt-3.5-turbo",
            ModelPricing { input_per_million: 0.50, output_per_million: 1.50 },
        );
        // Gemini
        pricing.insert(
            "gemini-1.5-pro",
            ModelPricing { input_per_million: 1.25, output_per_million: 5.0 },
        );
        pricing.insert(
            "gemini-1.5-flash",
            ModelPricing { input_per_million: 0.075, output_per_million: 0.30 },
        );
        // Unknown models return 0 (Ollama etc)
        Self { pricing }
    }

    pub fn calculate(&self, model: &str, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        // Strip provider prefix (e.g. "anthropic/claude-haiku-4-5" -> "claude-haiku-4-5")
        let model_key = if let Some(pos) = model.find('/') {
            &model[pos + 1..]
        } else {
            model
        };
        let model_lower = model_key.to_lowercase();
        match self.pricing.get(model_lower.as_str()) {
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
