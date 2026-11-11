//! `POST /admin/api/pricing/quote`: what the router would charge for calls
//! already made, from token counts, with its own cost calculator (the same
//! `[[pricing]]` → live retail prices → built-in precedence as a live call).
//!
//! For a caller that recorded calls while the router had no price for them
//! and wants to re-price its own ledger from the router, instead of keeping a
//! private price list that drifts from the router's.
use axum::{extract::State, response::IntoResponse, Json};
use serde::Deserialize;

use crate::api::{admin::auth::AdminSession, app::AppState, error::ApiError};

/// At most this many quotes per request.
const MAX_QUOTES: usize = 5000;

#[derive(Debug, Deserialize)]
pub struct QuoteItem {
    /// The answering model as the ledger names it (`gpt-5.4-mini`,
    /// `foundry/gpt-5.4-mini`, or a pseudo-model such as `search/bing_grounding`).
    pub model: String,
    /// The WHOLE prompt, cache reads and writes included (OpenAI's
    /// `prompt_tokens`, and `x_router.tokens.prompt`).
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
    /// Flat-rate units (searches, images); with it the item is priced per
    /// unit (`CostCalculator::flat_rate`) and the token fields are ignored.
    #[serde(default)]
    pub units: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct QuoteRequest {
    pub items: Vec<QuoteItem>,
}

/// One quote: `priced` false (and `cost_usd` null) when the router has no
/// price for the model; never a $0 stand-in.
pub fn quote(calc: &crate::router::cost::CostCalculator, item: &QuoteItem) -> serde_json::Value {
    if let Some(units) = item.units {
        return match calc.flat_rate(&item.model) {
            Some(rate) => serde_json::json!({"model": item.model, "priced": true, "cost_usd": rate * f64::from(units)}),
            None => serde_json::json!({"model": item.model, "priced": false, "cost_usd": null}),
        };
    }
    if !calc.has_price(&item.model) {
        return serde_json::json!({"model": item.model, "priced": false, "cost_usd": null});
    }
    let uncached = item.prompt_tokens.saturating_sub(item.cache_read_tokens).saturating_sub(item.cache_write_tokens);
    let cost = calc.calculate_with_cache(&item.model, uncached, item.completion_tokens, item.cache_read_tokens, item.cache_write_tokens);
    let no_cache = calc.calculate(&item.model, item.prompt_tokens, item.completion_tokens);
    serde_json::json!({"model": item.model, "priced": true, "cost_usd": cost, "no_cache_cost_usd": no_cache})
}

/// POST /admin/api/pricing/quote
pub async fn quote_api(
    State(state): State<AppState>,
    _session: AdminSession,
    Json(body): Json<QuoteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if body.items.len() > MAX_QUOTES {
        return Err(ApiError::InvalidRequest(format!("at most {MAX_QUOTES} items per request")));
    }
    let quotes: Vec<serde_json::Value> = body.items.iter().map(|i| quote(&state.cost_calc, i)).collect();
    Ok(Json(serde_json::json!({ "quotes": quotes })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::PricingEntry;
    use crate::router::cost::CostCalculator;

    fn item(model: &str, prompt: u32, completion: u32, cache_read: u32) -> QuoteItem {
        QuoteItem { model: model.into(), prompt_tokens: prompt, completion_tokens: completion, cache_read_tokens: cache_read, cache_write_tokens: 0, units: None }
    }

    fn calc() -> CostCalculator {
        let c = CostCalculator::new();
        c.replace_live_pricing(&[
            PricingEntry { model: "gpt-5.4-mini".into(), input_per_million: 0.75, output_per_million: 4.5, cache_read_per_million: Some(0.075), cache_write_per_million: None },
            PricingEntry { model: "search/bing_grounding".into(), input_per_million: 0.014, output_per_million: 0.0, cache_read_per_million: None, cache_write_per_million: None },
        ]);
        c
    }

    #[test]
    fn prices_the_uncached_share_at_the_input_rate_and_cache_reads_at_their_own() {
        // 1M prompt of which 800K cached, 100K output: 0.2*0.75 + 0.8*0.075 + 0.1*4.5
        let q = quote(&calc(), &item("foundry/gpt-5.4-mini", 1_000_000, 100_000, 800_000));
        assert_eq!(q["priced"], true);
        assert!((q["cost_usd"].as_f64().unwrap() - (0.15 + 0.06 + 0.45)).abs() < 1e-9);
        assert!((q["no_cache_cost_usd"].as_f64().unwrap() - (0.75 + 0.45)).abs() < 1e-9);
    }

    #[test]
    fn an_unpriced_model_is_reported_unpriced_never_as_zero() {
        let q = quote(&calc(), &item("gpt-unknown", 1000, 10, 0));
        assert_eq!(q["priced"], false);
        assert!(q["cost_usd"].is_null());
    }

    #[test]
    fn flat_rate_units_price_searches() {
        let mut i = item("search/bing_grounding", 0, 0, 0);
        i.units = Some(300);
        let q = quote(&calc(), &i);
        assert!((q["cost_usd"].as_f64().unwrap() - 4.2).abs() < 1e-9);
        i.model = "search/tavily".into();
        assert_eq!(quote(&calc(), &i)["priced"], false);
    }
}
