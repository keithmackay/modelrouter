//! Per-model parameter schema.
//!
//! A caller that wants to set a model's parameters (a scoped alias override
//! pinning `deep` to a model at a given reasoning effort, say) needs to know
//! which parameters that model honours and in what range. The router already
//! knows: it strips `temperature` from models that reject it, clamps
//! `max_tokens` to the model's ceiling and translates `reasoning_effort` into
//! each provider's dialect ([`crate::router::model_capabilities`]). This module
//! publishes that knowledge as a schema, so callers never hard-code provider
//! facts, and validates a parameter set against it.
//!
//! The schema lists only what the router will actually send for the model:
//! `reasoning_effort` appears only where the provider adapter forwards a
//! reasoning control (probed through [`ProviderAdapter::effective_settings`]),
//! and only with the levels the model takes natively, so a validated value is
//! never silently clamped or dropped. `default: null` means the provider's own
//! default applies when the parameter is not set.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::schema::ModelCapabilityEntry;
use crate::providers::adapter::{NormalizedRequest, ProviderAdapter};
use crate::router::learned_capabilities::LearnedCapabilities;
use crate::router::model_capabilities::{max_output_tokens, resolve_reasoning, temperature_allowed};

/// Every reasoning level a caller may name, lowest first.
const REASONING_LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// One parameter a model honours.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParameterSpec {
    pub name: &'static str,
    /// `number`, `integer` or `string` (with `enum`).
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<&'static str>>,
    /// Always null today: the provider's default applies when unset.
    pub default: Value,
    pub description: &'static str,
}

/// The reasoning levels `model` takes natively through `adapter`: a level
/// the router would clamp to another (Anthropic has no `minimal`) is left out.
fn native_reasoning_levels(
    model: &str,
    adapter: &dyn ProviderAdapter,
    capabilities: &[ModelCapabilityEntry],
) -> Vec<&'static str> {
    REASONING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let Some(control) = resolve_reasoning(model, Some(level), capabilities) else {
                return false;
            };
            let native = control.effort == Some(*level) || (*level == "none" && control.disable_thinking);
            let probe = NormalizedRequest { model: model.to_string(), reasoning: Some(control), ..Default::default() };
            native && adapter.effective_settings(&probe).reasoning.is_some()
        })
        .collect()
}

/// The parameters `model` honours when routed through `adapter`.
pub fn parameter_schema(
    model: &str,
    adapter: &dyn ProviderAdapter,
    capabilities: &[ModelCapabilityEntry],
    learned: &LearnedCapabilities,
) -> Vec<ParameterSpec> {
    let mut specs = Vec::new();
    let levels = native_reasoning_levels(model, adapter, capabilities);
    if !levels.is_empty() {
        specs.push(ParameterSpec {
            name: "reasoning_effort",
            kind: "string",
            minimum: None,
            maximum: None,
            allowed: Some(levels),
            default: Value::Null,
            description: "How much the model reasons before answering",
        });
    }
    if temperature_allowed(model, capabilities, learned) {
        let anthropic = model.to_ascii_lowercase().contains("claude");
        specs.push(ParameterSpec {
            name: "temperature",
            kind: "number",
            minimum: Some(0.0),
            maximum: Some(if anthropic { 1.0 } else { 2.0 }),
            allowed: None,
            default: Value::Null,
            description: "Sampling temperature",
        });
    }
    specs.push(ParameterSpec {
        name: "max_tokens",
        kind: "integer",
        minimum: Some(1.0),
        maximum: max_output_tokens(model, capabilities).map(f64::from),
        allowed: None,
        default: Value::Null,
        description: "Most completion tokens the model may produce",
    });
    specs
}

/// Check `params` against `schema`. The error names `model` and the
/// parameter, and says what the model accepts.
pub fn validate_parameters(model: &str, schema: &[ParameterSpec], params: &Map<String, Value>) -> Result<(), String> {
    for (name, value) in params {
        let Some(spec) = schema.iter().find(|s| s.name == name) else {
            let known: Vec<&str> = schema.iter().map(|s| s.name).collect();
            return Err(format!(
                "model '{model}' does not support parameter '{name}' (it supports: {})",
                known.join(", ")
            ));
        };
        let in_range = |n: f64| spec.minimum.map_or(true, |min| n >= min) && spec.maximum.map_or(true, |max| n <= max);
        let ok = match spec.kind {
            "string" => value
                .as_str()
                .is_some_and(|v| spec.allowed.as_ref().map_or(true, |allowed| allowed.contains(&v))),
            "integer" => value.as_u64().is_some_and(|n| in_range(n as f64)),
            _ => value.as_f64().is_some_and(in_range),
        };
        if !ok {
            return Err(format!("model '{model}' parameter '{name}': {value} is not {}", describe(spec)));
        }
    }
    Ok(())
}

fn describe(spec: &ParameterSpec) -> String {
    if let Some(allowed) = &spec.allowed {
        return format!("one of {}", allowed.join(", "));
    }
    let bound = |b: Option<f64>| b.map_or_else(|| "any".to_string(), |v| v.to_string());
    format!("{} in {}..={}", if spec.kind == "integer" { "an integer" } else { "a number" }, bound(spec.minimum), bound(spec.maximum))
}

/// Overwrite the request body's parameters with `params` (already validated
/// for the routed model), so the router's ordinary resolution forwards them.
pub fn apply_parameters(body: &mut Value, params: &Map<String, Value>) {
    if let Some(obj) = body.as_object_mut() {
        for (name, value) in params {
            obj.insert(name.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::adapter::{CompletionResult, EffectiveSettings, SseStream};

    /// An adapter that forwards reasoning controls, like the Claude adapters.
    struct Reasons;
    /// An adapter that sends no reasoning control at all.
    struct Silent;

    #[async_trait::async_trait]
    impl ProviderAdapter for Reasons {
        async fn complete(&self, _req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
            unreachable!()
        }
        async fn stream(&self, _req: &NormalizedRequest) -> anyhow::Result<SseStream> {
            unreachable!()
        }
        fn effective_settings(&self, req: &NormalizedRequest) -> EffectiveSettings {
            EffectiveSettings { reasoning: req.reasoning, ..Default::default() }
        }
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for Silent {
        async fn complete(&self, _req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
            unreachable!()
        }
        async fn stream(&self, _req: &NormalizedRequest) -> anyhow::Result<SseStream> {
            unreachable!()
        }
    }

    fn names(schema: &[ParameterSpec]) -> Vec<&str> {
        schema.iter().map(|s| s.name).collect()
    }

    fn spec<'a>(schema: &'a [ParameterSpec], name: &str) -> &'a ParameterSpec {
        schema.iter().find(|s| s.name == name).unwrap()
    }

    #[test]
    fn a_claude_5_model_takes_native_efforts_and_no_temperature() {
        let schema = parameter_schema("vertex/anthropic/claude-opus-5-5", &Reasons, &[], &LearnedCapabilities::default());
        assert_eq!(names(&schema), ["reasoning_effort", "max_tokens"]);
        // Opus 5.5 cannot turn thinking off, and Anthropic has no `minimal`.
        assert_eq!(spec(&schema, "reasoning_effort").allowed.as_deref(), Some(&["low", "medium", "high", "xhigh", "max"][..]));
    }

    #[test]
    fn a_model_that_can_disable_thinking_offers_none() {
        let schema = parameter_schema("claude-sonnet-5", &Reasons, &[], &LearnedCapabilities::default());
        assert_eq!(spec(&schema, "reasoning_effort").allowed.as_ref().unwrap()[0], "none");
    }

    #[test]
    fn openai_reasoning_models_take_their_own_levels_and_ceiling() {
        let schema = parameter_schema("foundry/o3", &Reasons, &[], &LearnedCapabilities::default());
        assert_eq!(spec(&schema, "reasoning_effort").allowed.as_deref(), Some(&["low", "medium", "high"][..]));
        assert_eq!(spec(&schema, "max_tokens").maximum, Some(100000.0));
        assert_eq!(spec(&schema, "temperature").maximum, Some(2.0));
    }

    #[test]
    fn reasoning_is_left_out_where_the_adapter_would_not_send_it() {
        let schema = parameter_schema("foundry/o3", &Silent, &[], &LearnedCapabilities::default());
        assert_eq!(names(&schema), ["temperature", "max_tokens"]);
        let older = parameter_schema("claude-haiku-4-5", &Reasons, &[], &LearnedCapabilities::default());
        assert_eq!(names(&older), ["temperature", "max_tokens"]);
        assert_eq!(spec(&older, "temperature").maximum, Some(1.0));
    }

    #[test]
    fn validation_names_the_model_and_the_parameter() {
        let schema = parameter_schema("foundry/o3", &Reasons, &[], &LearnedCapabilities::default());
        let params = |v: Value| v.as_object().unwrap().clone();
        assert!(validate_parameters("foundry/o3", &schema, &params(serde_json::json!({"reasoning_effort": "high", "max_tokens": 4096, "temperature": 0.5}))).is_ok());
        let cases = [
            (serde_json::json!({"reasoning_effort": "max"}), "parameter 'reasoning_effort': \"max\" is not one of low, medium, high"),
            (serde_json::json!({"max_tokens": 200000}), "parameter 'max_tokens': 200000 is not an integer in 1..=100000"),
            (serde_json::json!({"max_tokens": 1.5}), "parameter 'max_tokens'"),
            (serde_json::json!({"temperature": 3}), "parameter 'temperature': 3 is not a number in 0..=2"),
            (serde_json::json!({"top_k": 5}), "does not support parameter 'top_k' (it supports: reasoning_effort, temperature, max_tokens)"),
        ];
        for (body, expected) in cases {
            let err = validate_parameters("foundry/o3", &schema, &params(body.clone())).unwrap_err();
            assert!(err.starts_with("model 'foundry/o3'"), "{err}");
            assert!(err.contains(expected), "{body}: {err}");
        }
    }

    #[test]
    fn apply_overwrites_the_callers_values() {
        let mut body = serde_json::json!({"model": "deep", "max_tokens": 100, "temperature": 0.2});
        let params = serde_json::json!({"max_tokens": 8000, "reasoning_effort": "high"});
        apply_parameters(&mut body, params.as_object().unwrap());
        assert_eq!(body["max_tokens"], 8000);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["temperature"], 0.2);
    }
}
