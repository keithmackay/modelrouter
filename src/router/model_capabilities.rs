//! Per-model sampling-parameter capabilities.
//!
//! Providers do not accept a uniform parameter set across their own model
//! families, and they change that set between versions. Anthropic's Claude 5
//! family rejects `temperature` outright — Vertex answers a request carrying
//! one with:
//!
//! ```text
//! 400 Bad Request
//! {"type":"error","error":{"type":"invalid_request_error",
//!  "message":"`temperature` is deprecated for this model."}}
//! ```
//!
//! while `claude-haiku-4-5`, behind the same provider, still honours it. A
//! caller addressing a routing alias (`deep`, `balanced`) cannot know which of
//! those it will reach — the alias exists precisely so it doesn't have to. The
//! router resolves the alias, so the router is the only component positioned to
//! know whether the resolved model accepts the parameter, and it strips the
//! ones the model would reject.
//!
//! Defaults below are probed or taken from the provider's published model
//! documentation. The temperature table matches by model family, so a point
//! release (`claude-opus-5-5`) or a version pin (`claude-opus-5@20260101`)
//! inherits its family's entry. Operators override any entry from config
//! without waiting on a release:
//!
//! ```toml
//! [[model_capabilities]]
//! model = "claude-opus-6"
//! supports_temperature = false
//! ```
//!
//! A model the table does not know yet degrades gracefully: when a provider
//! rejects a request because of its `temperature`, the router retries the call
//! once without it and records the model, durably, in
//! [`crate::router::learned_capabilities`] ([`temperature_rejection_retry`]).

use crate::config::schema::ModelCapabilityEntry;

/// Models known to reject `temperature`. Probed against Vertex on 2026-09-01;
/// every entry here returned `invalid_request_error` for `temperature: 0.3`
/// and succeeded with the parameter absent.
///
/// Keys are normalized model names (provider prefix stripped, lowercased) —
/// see [`normalize_model_key`].
///
/// Entries name a model *family*: an entry also covers every id that extends it
/// with a `-` suffix (see [`family_matches`]), so `claude-opus-5` covers
/// `claude-opus-5-5`. The point releases are still listed so the table states
/// what was verified.
const TEMPERATURE_UNSUPPORTED: &[&str] = &[
    "claude-opus-5",
    "claude-opus-5-5",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    // Sampling parameters were removed from Opus 4.7 and 4.8 as well
    // (documented; the request is a 400).
    "claude-opus-4-7",
    "claude-opus-4-8",
    // Both Fable 5 generations reject `temperature` (Vertex 400
    // "`temperature` is deprecated for this model"). claude-fable-5 was
    // missing from this list while its sibling was present — that single gap
    // killed the fable-5 arm of a downstream 3-way model experiment at the
    // first temperature-carrying call.
    "claude-fable-5",
    "claude-fable-5-1",
];

/// Reduce a routed model name to the key used for capability lookup.
///
/// Strips a leading provider segment (`anthropic/claude-opus-5` →
/// `claude-opus-5`) and lowercases, matching how
/// [`crate::router::cost::CostCalculator`] keys its pricing table so operators
/// write the same model string in both config blocks.
///
/// Every leading segment is stripped, not just the first: a routed target such
/// as `vertex/anthropic/claude-opus-5-5` names the provider *and* the publisher,
/// and must reach the same entry as the bare `claude-opus-5-5`.
fn normalize_model_key(model: &str) -> String {
    let key = match model.rfind('/') {
        Some(pos) => &model[pos + 1..],
        None => model,
    };
    key.to_lowercase()
}

/// Whether `key` is `family` itself or a release of it (`family-…`).
///
/// The `-` boundary keeps `claude-opus-5` from matching `claude-opus-50`.
fn family_matches(key: &str, family: &str) -> bool {
    key == family
        || (key.len() > family.len() && key.starts_with(family) && key.as_bytes()[family.len()] == b'-')
}

/// Whether `err` is a provider rejecting the request because of `temperature`:
/// a 4xx request error whose message names the parameter.
fn is_temperature_rejection(err: &anyhow::Error) -> bool {
    use crate::router::retry::RetryableError;
    matches!(RetryableError::classify_error(err), RetryableError::ClientError(_))
        && err.to_string().to_lowercase().contains("temperature")
}

/// The request to retry when a provider rejected `req` because of its
/// `temperature`: the same request without it. `None` when the failure is
/// anything else, or `req` carried no temperature. The caller records the
/// rejection ([`crate::router::learned_capabilities`]) so it is paid once.
pub fn temperature_rejection_retry(
    req: &crate::providers::adapter::NormalizedRequest,
    err: &anyhow::Error,
) -> Option<crate::providers::adapter::NormalizedRequest> {
    if req.temperature.is_none() || !is_temperature_rejection(err) {
        return None;
    }
    let mut retry = req.clone();
    retry.temperature = None;
    Some(retry)
}

/// Drop a Vertex-style `@YYYYMMDD` version suffix, if present.
///
/// Claude-on-Vertex is addressed both ways in practice — the alias table on
/// this deployment holds a bare `claude-fable-5-1` while the config example
/// pins `claude-opus-4-5@20251101` — and a parameter a model family rejects is
/// rejected by every snapshot of it. Matching only the exact string would let a
/// pinned deployment sail past the table it is listed in.
fn strip_version(key: &str) -> &str {
    match key.find('@') {
        Some(pos) => &key[..pos],
        None => key,
    }
}

/// Whether `model` accepts a `temperature` sampling parameter.
///
/// A config entry for the model wins over the built-in table, so an operator
/// can both *add* a model the build doesn't know about and *retract* a built-in
/// entry once a provider restores support. Next comes a model a provider has
/// rejected the parameter for in this process, then the built-in family table.
/// Unknown models are assumed to support it: the router must not silently drop
/// parameters it has no evidence are unwelcome.
///
/// Lookup tries the fully qualified name before the version-stripped one, so a
/// version-pinned entry overrides a family-wide one rather than the reverse.
pub fn supports_temperature(model: &str, overrides: &[ModelCapabilityEntry]) -> bool {
    temperature_override(model, overrides).unwrap_or_else(|| table_supports_temperature(model))
}

/// [`supports_temperature`] with learned entries between config and the
/// built-in table: config override, then a learned rejection of this exact
/// model, then the family table.
pub fn temperature_allowed(
    model: &str,
    overrides: &[ModelCapabilityEntry],
    learned: &crate::router::learned_capabilities::LearnedCapabilities,
) -> bool {
    if let Some(configured) = temperature_override(model, overrides) {
        return configured;
    }
    if let Some(supported) = learned.temperature(model) {
        return supported;
    }
    table_supports_temperature(model)
}

fn temperature_override(model: &str, overrides: &[ModelCapabilityEntry]) -> Option<bool> {
    let key = normalize_model_key(model);
    let base = strip_version(&key);
    let mut candidates = vec![key.as_str()];
    if base != key {
        candidates.push(base);
    }
    for candidate in &candidates {
        for entry in overrides {
            if normalize_model_key(&entry.model) == *candidate {
                if let Some(supported) = entry.supports_temperature {
                    return Some(supported);
                }
            }
        }
    }
    None
}

fn table_supports_temperature(model: &str) -> bool {
    let key = normalize_model_key(model);
    let base = strip_version(&key);
    !TEMPERATURE_UNSUPPORTED
        .iter()
        .any(|family| family_matches(base, family))
}

// ── Output ceilings ──────────────────────────────────────────────────────────
//
// A caller addressing an alias sizes `max_tokens` for the largest model the
// alias might reach; a provider rejects a value above the resolved model's
// completion ceiling outright ("max_tokens is too large: 128000. This model
// supports at most 32768 completion tokens"). The router knows the resolved
// model, so it clamps. Keyed like the temperature table (family match,
// provider prefix and `@version` ignored) and overridable from config with
// `max_output_tokens`.

/// Completion-token ceilings by model family, from the providers' published
/// model documentation.
const MAX_OUTPUT_TOKENS: &[(&str, u32)] = &[
    ("gpt-4.1", 32768),
    ("gpt-4o", 16384),
    ("gpt-5", 128000),
    ("gpt-5.1", 128000),
    ("gpt-5.2", 128000),
    ("gpt-5.3", 128000),
    ("gpt-5.4", 128000),
    ("gpt-5.5", 128000),
    ("gpt-5.6", 128000),
    ("o1", 100000),
    ("o3", 100000),
    ("o4", 100000),
];

/// Families that take `max_completion_tokens` and reject `max_tokens` on the
/// OpenAI-shaped surfaces (the reasoning generations). Every other model keeps
/// `max_tokens`, which non-OpenAI deployments behind the same surface expect.
const MAX_COMPLETION_TOKENS_FAMILIES: &[&str] = &["gpt-5", "gpt-5.1", "gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5", "gpt-5.6", "gpt-6", "gpt-6.1", "o1", "o3", "o4"];

/// Whether `model` takes `max_completion_tokens` instead of `max_tokens`.
pub fn uses_max_completion_tokens(model: &str) -> bool {
    let key = normalize_model_key(model);
    let base = strip_version(&key);
    MAX_COMPLETION_TOKENS_FAMILIES.iter().any(|family| family_matches(base, family))
}

/// The completion-token ceiling for `model`: a config entry, else the family
/// table, else none (an unknown model is sent what the caller asked for).
pub fn max_output_tokens(model: &str, overrides: &[ModelCapabilityEntry]) -> Option<u32> {
    let key = normalize_model_key(model);
    let base = strip_version(&key);
    let mut candidates = vec![key.as_str()];
    if base != key {
        candidates.push(base);
    }
    for candidate in &candidates {
        for entry in overrides {
            if normalize_model_key(&entry.model) == *candidate {
                if let Some(cap) = entry.max_output_tokens {
                    return Some(cap);
                }
            }
        }
    }
    // A dated snapshot (`gpt-4.1-2025-04-14`) is a release of its family, which
    // `family_matches` covers through the `-` boundary.
    MAX_OUTPUT_TOKENS
        .iter()
        .find(|(family, _)| family_matches(base, family))
        .map(|(_, cap)| *cap)
}

/// `requested` lowered to the model's ceiling when it is above it.
pub fn clamp_max_tokens(model: &str, requested: Option<u32>, overrides: &[ModelCapabilityEntry]) -> Option<u32> {
    let requested = requested?;
    match max_output_tokens(model, overrides) {
        Some(cap) if requested > cap => {
            tracing::debug!(model, requested, cap, "clamping max_tokens to the model's completion ceiling");
            Some(cap)
        }
        _ => Some(requested),
    }
}

// ── Reasoning (thinking / effort) controls ──────────────────────────────────
//
// OpenAI-shaped callers express "how hard should the model think" as
// `reasoning_effort` (`none` | `minimal` | `low` | `medium` | `high`, plus the
// `xhigh` / `max` levels some providers add). Anthropic-dialect backends spell
// it two other ways — `thinking: {type: "disabled"}` and
// `output_config: {effort}` — and which of those a model accepts differs by
// model generation:
//
// * Newer Claude models reason by default when the request carries no
//   `thinking` field, and bill that reasoning as output tokens. A caller that
//   sized `max_tokens` for its visible answer gets an empty or truncated reply
//   unless the router forwards its request to turn reasoning off.
// * Some of those models reject `thinking: {type: "disabled"}` (thinking is
//   always on); the only lever left is the lowest effort.
// * Older models do not reason unless asked, and reject `output_config.effort`
//   outright, so nothing may be sent to them.
//
// Tables are keyed like the temperature table (provider prefix stripped,
// lowercased, `@version` suffix ignored) and are overridable from config.

/// Models that reason when the request carries no `thinking` field.
const THINKS_BY_DEFAULT: &[&str] = &[
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "claude-opus-5",
    "claude-opus-5-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-mythos-5",
    "claude-mythos-5-1",
];

/// Models on which thinking is always on: an explicit
/// `thinking: {type: "disabled"}` is a 400.
const THINKING_ALWAYS_ON: &[&str] = &[
    "claude-opus-5-5",
    // Sonnet 5.5 rejects `{type: "disabled"}`; its only thinking-off form is a
    // different `thinking` type, so the lowest effort is the portable lever.
    "claude-sonnet-5-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-mythos-5",
    "claude-mythos-5-1",
];

/// Models that accept `output_config.effort`.
const EFFORT_SUPPORTED: &[&str] = &[
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "claude-opus-5",
    "claude-opus-5-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-mythos-5",
    "claude-mythos-5-1",
];

/// OpenAI-dialect reasoning models take `reasoning_effort` themselves, from a
/// per-family set of levels. The router forwards it on the OpenAI-shaped
/// surfaces only for these families; a level outside the set is clamped to
/// the nearest one the model accepts (see [`resolve_reasoning`]).
const OPENAI_REASONING_LEVELS: &[(&str, &[&str])] = &[
    ("gpt-5", &["minimal", "low", "medium", "high"]),
    ("gpt-5.1", &["low", "medium", "high"]),
    ("gpt-5.2", &["low", "medium", "high"]),
    ("gpt-5.3", &["low", "medium", "high"]),
    ("gpt-5.4", &["low", "medium", "high"]),
    ("gpt-5.5", &["low", "medium", "high"]),
    ("gpt-5.6", &["low", "medium", "high"]),
    ("gpt-6", &["low", "medium", "high"]),
    ("gpt-6.1", &["low", "medium", "high"]),
    ("o1", &["low", "medium", "high"]),
    ("o3", &["low", "medium", "high"]),
    ("o4", &["low", "medium", "high"]),
];

/// The `reasoning_effort` levels an OpenAI-dialect reasoning model accepts,
/// lowest first, or `None` for every other model. The longest matching
/// family wins, so `gpt-5.1` is not read as a `gpt-5` release.
pub fn openai_reasoning_levels(model: &str) -> Option<&'static [&'static str]> {
    let key = normalize_model_key(model);
    let base = strip_version(&key);
    OPENAI_REASONING_LEVELS
        .iter()
        .filter(|(family, _)| family_matches(base, family))
        .max_by_key(|(family, _)| family.len())
        .map(|(_, levels)| *levels)
}

/// Every reasoning level a caller may name, lowest first.
const REASONING_SCALE: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `requested` on an OpenAI-dialect model: the level itself when accepted,
/// else the nearest accepted level on the scale. `None` when unrecognised.
fn clamp_openai_level(levels: &'static [&'static str], requested: &str) -> Option<&'static str> {
    let rank = REASONING_SCALE.iter().position(|l| *l == requested)?;
    let rank_of = |level: &str| REASONING_SCALE.iter().position(|l| *l == level).unwrap_or(0);
    levels
        .iter()
        .copied()
        .min_by_key(|level| (rank_of(level) as i64 - rank as i64).abs() * 2 + i64::from(rank_of(level) < rank))
}

/// What a model accepts for controlling its reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingCapabilities {
    pub thinks_by_default: bool,
    pub can_disable_thinking: bool,
    pub supports_effort: bool,
}

/// Look `model` up in `overrides` (pinned name first, then family) for the
/// field `pick` selects; fall back to membership of the built-in `table`.
fn lookup_flag(
    model: &str,
    overrides: &[ModelCapabilityEntry],
    pick: impl Fn(&ModelCapabilityEntry) -> Option<bool>,
    table_default: impl Fn(&str) -> bool,
) -> bool {
    let key = normalize_model_key(model);
    let base = strip_version(&key);
    let mut candidates = vec![key.as_str()];
    if base != key {
        candidates.push(base);
    }
    for candidate in &candidates {
        for entry in overrides {
            if normalize_model_key(&entry.model) == *candidate {
                if let Some(v) = pick(entry) {
                    return v;
                }
            }
        }
    }
    candidates.iter().any(|c| table_default(c))
}

/// Resolve the reasoning controls `model` accepts. Unknown models neither
/// think by default nor accept effort, so nothing is sent to them.
pub fn thinking_capabilities(
    model: &str,
    overrides: &[ModelCapabilityEntry],
) -> ThinkingCapabilities {
    ThinkingCapabilities {
        thinks_by_default: lookup_flag(
            model,
            overrides,
            |e| e.thinks_by_default,
            |c| THINKS_BY_DEFAULT.contains(&c),
        ),
        // Stored inverted in the built-in table: the exception is the model
        // that cannot turn thinking off.
        can_disable_thinking: !lookup_flag(
            model,
            overrides,
            |e| e.can_disable_thinking.map(|v| !v),
            |c| THINKING_ALWAYS_ON.contains(&c),
        ),
        supports_effort: lookup_flag(
            model,
            overrides,
            |e| e.supports_effort,
            |c| EFFORT_SUPPORTED.contains(&c),
        ),
    }
}

/// Translate a caller's `reasoning_effort` into the controls `model` accepts.
///
/// * `none` — disable thinking on a model that thinks by default; where it
///   cannot be disabled, ask for the lowest effort instead. A model that does
///   not think by default already honours `none`, so nothing is sent.
/// * `minimal` / `low` — effort `low` (Anthropic has no `minimal`).
/// * `medium` / `high` / `xhigh` / `max` — that effort.
///
/// Effort is only sent to models that accept it. `None` means "send nothing":
/// the value was absent, unrecognised (logged), or not expressible for this
/// model — never a field the provider would reject.
pub fn resolve_reasoning(
    model: &str,
    reasoning_effort: Option<&str>,
    overrides: &[ModelCapabilityEntry],
) -> Option<crate::providers::adapter::ReasoningControl> {
    use crate::providers::adapter::ReasoningControl;
    let requested = reasoning_effort?.trim().to_ascii_lowercase();
    if let Some(levels) = openai_reasoning_levels(model) {
        let level = clamp_openai_level(levels, &requested);
        if level.is_none() {
            tracing::warn!(model, reasoning_effort = requested.as_str(), "unrecognised reasoning_effort; forwarding no reasoning control");
        }
        return level.map(|effort| ReasoningControl { disable_thinking: false, effort: Some(effort) });
    }
    let caps = thinking_capabilities(model, overrides);
    let effort = |level: &'static str| {
        caps.supports_effort.then(|| ReasoningControl {
            disable_thinking: false,
            effort: Some(level),
        })
    };
    match requested.as_str() {
        "none" => {
            if !caps.thinks_by_default {
                None
            } else if caps.can_disable_thinking {
                Some(ReasoningControl { disable_thinking: true, effort: None })
            } else {
                effort("low")
            }
        }
        "minimal" | "low" => effort("low"),
        "medium" => effort("medium"),
        "high" => effort("high"),
        "xhigh" => effort("xhigh"),
        "max" => effort("max"),
        other => {
            tracing::warn!(
                model,
                reasoning_effort = other,
                "unrecognised reasoning_effort; forwarding no reasoning control"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── max_output_tokens ───────────────────────────────────────────────────

    #[test]
    fn reasoning_families_take_max_completion_tokens() {
        for m in ["gpt-5", "gpt-5.5", "gpt-5.6-sol", "gpt-6-luna", "gpt-6.1-sol", "o3", "o4-mini", "foundry/o1"] {
            assert!(uses_max_completion_tokens(m), "{m}");
        }
        for m in ["gpt-4.1", "gpt-4o", "DeepSeek-V4-Pro", "Mistral-Large-3", "grok-4.7"] {
            assert!(!uses_max_completion_tokens(m), "{m}");
        }
    }

    #[test]
    fn gpt_5_and_o_series_have_published_output_ceilings() {
        assert_eq!(max_output_tokens("gpt-5", &[]), Some(128000));
        assert_eq!(max_output_tokens("gpt-5.5", &[]), Some(128000));
        assert_eq!(max_output_tokens("gpt-5.6-sol", &[]), Some(128000));
        assert_eq!(max_output_tokens("o3", &[]), Some(100000));
        assert_eq!(max_output_tokens("o4-mini", &[]), Some(100000));
    }

    #[test]
    fn gpt_4_1_family_caps_output_at_32768_including_mini_nano_and_pins() {
        for m in ["gpt-4.1", "gpt-4.1-mini", "gpt-4.1-nano", "foundry/gpt-4.1", "gpt-4.1-2025-04-14", "GPT-4.1@2025-04-14"] {
            assert_eq!(max_output_tokens(m, &[]), Some(32768), "{m}");
        }
    }

    #[test]
    fn unknown_models_have_no_output_cap() {
        assert_eq!(max_output_tokens("some-future-model", &[]), None);
        assert_eq!(max_output_tokens("claude-opus-5-5", &[]), None);
        // The `-` boundary: gpt-4.10 is not a gpt-4.1 release.
        assert_eq!(max_output_tokens("gpt-4.10", &[]), None);
    }

    #[test]
    fn a_config_entry_overrides_and_extends_the_output_cap_table() {
        let raise = ModelCapabilityEntry { model: "gpt-4.1".into(), max_output_tokens: Some(16384), ..Default::default() };
        let add = ModelCapabilityEntry { model: "my-deployment".into(), max_output_tokens: Some(4096), ..Default::default() };
        let caps = [raise, add];
        assert_eq!(max_output_tokens("gpt-4.1", &caps), Some(16384));
        assert_eq!(max_output_tokens("foundry/my-deployment", &caps), Some(4096));
        // An entry that only sets another capability leaves the table in force.
        assert_eq!(max_output_tokens("gpt-4.1-mini", &[entry("gpt-4.1-mini", Some(true))]), Some(32768));
    }

    #[test]
    fn clamp_max_tokens_lowers_only_a_request_above_the_cap() {
        assert_eq!(clamp_max_tokens("gpt-4.1", Some(128000), &[]), Some(32768));
        assert_eq!(clamp_max_tokens("gpt-4.1", Some(1000), &[]), Some(1000));
        assert_eq!(clamp_max_tokens("gpt-4.1", None, &[]), None);
        assert_eq!(clamp_max_tokens("claude-opus-5-5", Some(128000), &[]), Some(128000));
    }

    fn entry(model: &str, supports_temperature: Option<bool>) -> ModelCapabilityEntry {
        ModelCapabilityEntry { model: model.to_string(), supports_temperature, ..Default::default() }
    }

    #[test]
    fn unknown_models_are_assumed_to_support_temperature() {
        assert!(supports_temperature("some-future-model", &[]));
        assert!(supports_temperature("gpt-4o", &[]));
    }

    #[test]
    fn built_in_table_covers_the_claude_5_family() {
        assert!(!supports_temperature("claude-opus-5", &[]));
        assert!(!supports_temperature("claude-sonnet-5", &[]));
        assert!(!supports_temperature("claude-fable-5", &[]));
        assert!(!supports_temperature("anthropic/claude-fable-5", &[]));
        assert!(!supports_temperature("claude-fable-5-1", &[]));
    }

    /// The 4.5 model sits behind the same provider as the 5 family and still
    /// honours `temperature` — the reason this is a per-model table and not a
    /// per-provider switch.
    #[test]
    fn point_releases_pins_and_multi_segment_names_inherit_the_family_entry() {
        // The exact routed strings a tier alias resolves to.
        assert!(!supports_temperature("vertex/anthropic/claude-opus-5-5", &[]));
        assert!(!supports_temperature("vertex/anthropic/claude-sonnet-5-5", &[]));
        assert!(!supports_temperature("anthropic/claude-opus-5-5", &[]));
        // A future point release and a version pin of a listed family.
        assert!(!supports_temperature("vertex/anthropic/claude-sonnet-5-7", &[]));
        assert!(!supports_temperature("claude-opus-5-5@20261001", &[]));
        assert!(!supports_temperature("claude-opus-4-8", &[]));
        // The family boundary is a `-`, not any shared prefix.
        assert!(supports_temperature("claude-opus-50", &[]));
        assert!(supports_temperature("vertex/anthropic/claude-haiku-4-5@20251001", &[]));
        assert!(supports_temperature("claude-opus-4-6", &[]));
    }

    #[test]
    fn config_override_beats_the_family_table_for_a_point_release() {
        let overrides = vec![entry("claude-sonnet-5-5", Some(true))];
        assert!(supports_temperature("vertex/anthropic/claude-sonnet-5-5", &overrides));
        assert!(!supports_temperature("vertex/anthropic/claude-sonnet-5", &overrides));
    }

    fn request_with_temperature(model: &str) -> crate::providers::adapter::NormalizedRequest {
        crate::providers::adapter::NormalizedRequest {
            model: model.to_string(),
            temperature: Some(0.3),
            ..Default::default()
        }
    }

    #[test]
    fn a_temperature_rejection_yields_a_retry_without_it() {
        let model = "vertex/acme/model-x@v1";
        let err = anyhow::anyhow!(
            "Vertex returned 400 Bad Request: {{\"type\":\"error\",\"error\":{{\"type\":\"invalid_request_error\",\"message\":\"`temperature` is deprecated for this model.\"}}}}"
        );
        let retry = temperature_rejection_retry(&request_with_temperature(model), &err)
            .expect("a temperature rejection is retried");
        assert_eq!(retry.temperature, None);
        assert_eq!(retry.model, model);
    }

    #[test]
    fn other_failures_are_not_retried() {
        let model = "vertex/acme/model-x@v1";
        let unrelated = anyhow::anyhow!("Vertex returned 400 Bad Request: max_tokens too large");
        assert!(temperature_rejection_retry(&request_with_temperature(model), &unrelated).is_none());
        let server = anyhow::anyhow!("Vertex returned 503 Service Unavailable: temperature of the datacenter");
        assert!(temperature_rejection_retry(&request_with_temperature(model), &server).is_none());
        let rejection = anyhow::anyhow!("Vertex returned 400 Bad Request: `temperature` is deprecated");
        let no_temperature = crate::providers::adapter::NormalizedRequest {
            model: model.to_string(),
            ..Default::default()
        };
        assert!(temperature_rejection_retry(&no_temperature, &rejection).is_none());
    }

    #[test]
    fn precedence_is_config_then_learned_then_table() {
        use crate::router::learned_capabilities::LearnedCapabilities;
        let learned = LearnedCapabilities::default();
        let model = "vertex/acme/model-x@v1";
        assert!(temperature_allowed(model, &[], &learned));
        learned.learn_temperature_rejected(model, "rejected");
        assert!(!temperature_allowed(model, &[], &learned));
        // Another snapshot of the same model is unaffected.
        assert!(temperature_allowed("vertex/acme/model-x@v2", &[], &learned));
        // Config wins over the learned entry.
        let overrides = vec![entry("model-x@v1", Some(true))];
        assert!(temperature_allowed(model, &overrides, &learned));
        // The table still answers for models nothing was learned about.
        assert!(!temperature_allowed("vertex/anthropic/claude-opus-5-5", &[], &learned));
    }

    #[test]
    fn same_provider_older_model_keeps_temperature() {
        assert!(supports_temperature("claude-haiku-4-5", &[]));
        assert!(supports_temperature("anthropic/claude-haiku-4-5", &[]));
    }

    #[test]
    fn provider_prefix_and_case_do_not_affect_lookup() {
        assert!(!supports_temperature("anthropic/claude-opus-5", &[]));
        assert!(!supports_temperature("Anthropic/Claude-Opus-5", &[]));
        assert!(!supports_temperature("CLAUDE-OPUS-5", &[]));
    }

    #[test]
    fn config_adds_a_model_the_build_does_not_know() {
        let overrides = vec![entry("claude-opus-6", Some(false))];
        assert!(!supports_temperature("claude-opus-6", &overrides));
        assert!(!supports_temperature("anthropic/claude-opus-6", &overrides));
    }

    /// The override must be able to run the other way too, so a provider
    /// restoring support doesn't require a new build to exploit.
    #[test]
    fn config_retracts_a_built_in_entry() {
        let overrides = vec![entry("claude-opus-5", Some(true))];
        assert!(supports_temperature("claude-opus-5", &overrides));
    }

    #[test]
    fn entry_without_an_opinion_falls_through_to_the_built_in_table() {
        let overrides = vec![entry("claude-opus-5", None)];
        assert!(!supports_temperature("claude-opus-5", &overrides));
    }

    /// Vertex pins Claude snapshots as `<model>@<date>`. The parameter a family
    /// rejects is rejected by every snapshot of it, so the version-pinned form
    /// must resolve to the same answer as the bare one.
    #[test]
    fn a_version_pinned_model_matches_its_family_entry() {
        assert!(!supports_temperature("claude-opus-5@20260101", &[]));
        assert!(!supports_temperature("anthropic/claude-fable-5-1@20260214", &[]));
        assert!(supports_temperature("anthropic/claude-haiku-4-5@20251001", &[]));

        let overrides = vec![entry("claude-opus-6", Some(false))];
        assert!(!supports_temperature("anthropic/claude-opus-6@20260601", &overrides));
    }

    /// A pinned entry is more specific than a family-wide one and wins, so an
    /// operator can carve out the single snapshot that behaves differently.
    #[test]
    fn a_version_pinned_entry_overrides_the_family_wide_one() {
        let overrides = vec![
            entry("claude-opus-6", Some(false)),
            entry("claude-opus-6@20260601", Some(true)),
        ];
        assert!(supports_temperature("claude-opus-6@20260601", &overrides));
        assert!(!supports_temperature("claude-opus-6@20260101", &overrides));
        assert!(!supports_temperature("claude-opus-6", &overrides));
    }

    #[test]
    fn entries_for_other_models_do_not_leak() {
        let overrides = vec![entry("claude-haiku-4-5", Some(false))];
        assert!(!supports_temperature("claude-haiku-4-5", &overrides));
        assert!(supports_temperature("gpt-4o", &overrides));
    }

    // ── reasoning controls ──────────────────────────────────────────────────

    use crate::providers::adapter::ReasoningControl;

    const DISABLED: Option<ReasoningControl> =
        Some(ReasoningControl { disable_thinking: true, effort: None });

    fn eff(level: &'static str) -> Option<ReasoningControl> {
        Some(ReasoningControl { disable_thinking: false, effort: Some(level) })
    }

    /// The motivating case: a default-thinking model must receive an explicit
    /// disable when the caller asks for no reasoning, or it spends a small
    /// `max_tokens` on hidden thinking.
    #[test]
    fn none_disables_thinking_on_a_default_thinking_model() {
        assert_eq!(resolve_reasoning("claude-sonnet-5", Some("none"), &[]), DISABLED);
        assert_eq!(resolve_reasoning("anthropic/claude-sonnet-5", Some("none"), &[]), DISABLED);
        assert_eq!(resolve_reasoning("claude-opus-5@20260101", Some("NONE"), &[]), DISABLED);
    }

    #[test]
    fn none_falls_back_to_low_effort_where_thinking_is_always_on() {
        assert_eq!(resolve_reasoning("claude-fable-5-1", Some("none"), &[]), eff("low"));
        assert_eq!(resolve_reasoning("claude-opus-5-5", Some("none"), &[]), eff("low"));
        assert_eq!(resolve_reasoning("anthropic/claude-sonnet-5-5", Some("none"), &[]), eff("low"));
    }

    #[test]
    fn sonnet_5_5_rejects_temperature_and_accepts_effort() {
        assert!(!supports_temperature("claude-sonnet-5-5", &[]));
        assert!(!supports_temperature("anthropic/claude-sonnet-5-5", &[]));
        assert_eq!(resolve_reasoning("anthropic/claude-sonnet-5-5", Some("high"), &[]), eff("high"));
        let caps = thinking_capabilities("claude-sonnet-5-5", &[]);
        assert!(caps.thinks_by_default);
        assert!(!caps.can_disable_thinking);
        assert!(caps.supports_effort);
    }

    /// Older models don't think unless asked and reject both fields.
    #[test]
    fn nothing_is_sent_to_a_model_without_reasoning_controls() {
        for level in ["none", "minimal", "low", "medium", "high"] {
            assert_eq!(resolve_reasoning("claude-haiku-4-5@20251001", Some(level), &[]), None);
            assert_eq!(resolve_reasoning("claude-sonnet-4-5", Some(level), &[]), None);
        }
    }

    #[test]
    fn effort_levels_map_on_effort_capable_models() {
        assert_eq!(resolve_reasoning("claude-sonnet-5", Some("minimal"), &[]), eff("low"));
        assert_eq!(resolve_reasoning("claude-sonnet-5", Some("low"), &[]), eff("low"));
        assert_eq!(resolve_reasoning("claude-sonnet-5", Some("medium"), &[]), eff("medium"));
        assert_eq!(resolve_reasoning("claude-sonnet-5", Some("high"), &[]), eff("high"));
        assert_eq!(resolve_reasoning("claude-opus-4-8", Some("xhigh"), &[]), eff("xhigh"));
        assert_eq!(resolve_reasoning("claude-opus-5", Some("max"), &[]), eff("max"));
        // Opus 4.x thinks only when asked, so `none` needs no field there.
        assert_eq!(resolve_reasoning("claude-opus-4-8", Some("none"), &[]), None);
    }

    #[test]
    fn absent_or_unrecognised_effort_sends_nothing() {
        assert_eq!(resolve_reasoning("claude-sonnet-5", None, &[]), None);
        assert_eq!(resolve_reasoning("claude-sonnet-5", Some("extreme"), &[]), None);
    }

    #[test]
    fn config_declares_reasoning_controls_for_a_new_model() {
        let overrides = vec![ModelCapabilityEntry {
            model: "claude-sonnet-6".into(),
            thinks_by_default: Some(true),
            can_disable_thinking: Some(false),
            supports_effort: Some(true),
            ..Default::default()
        }];
        assert_eq!(
            thinking_capabilities("anthropic/claude-sonnet-6@20270101", &overrides),
            ThinkingCapabilities {
                thinks_by_default: true,
                can_disable_thinking: false,
                supports_effort: true
            }
        );
        assert_eq!(resolve_reasoning("claude-sonnet-6", Some("none"), &overrides), eff("low"));
    }

    #[test]
    fn config_retracts_a_built_in_reasoning_entry() {
        let overrides = vec![ModelCapabilityEntry {
            model: "claude-fable-5-1".into(),
            can_disable_thinking: Some(true),
            ..Default::default()
        }];
        assert_eq!(resolve_reasoning("claude-fable-5-1", Some("none"), &overrides), DISABLED);
        // Other fields keep their built-in values.
        assert!(thinking_capabilities("claude-fable-5-1", &overrides).supports_effort);
    }

    #[test]
    fn openai_reasoning_families_take_their_own_levels_and_clamp_the_rest() {
        assert_eq!(openai_reasoning_levels("foundry/gpt-5"), Some(&["minimal", "low", "medium", "high"][..]));
        assert_eq!(openai_reasoning_levels("gpt-5.1-mini"), Some(&["low", "medium", "high"][..]));
        assert_eq!(openai_reasoning_levels("o4-mini"), Some(&["low", "medium", "high"][..]));
        assert_eq!(openai_reasoning_levels("gpt-4.1"), None);
        assert_eq!(openai_reasoning_levels("claude-opus-5-5"), None);
        assert_eq!(resolve_reasoning("gpt-5", Some("minimal"), &[]), eff("minimal"));
        assert_eq!(resolve_reasoning("gpt-5", Some("none"), &[]), eff("minimal"));
        assert_eq!(resolve_reasoning("o3", Some("minimal"), &[]), eff("low"));
        assert_eq!(resolve_reasoning("o3", Some("none"), &[]), eff("low"));
        assert_eq!(resolve_reasoning("o3", Some("medium"), &[]), eff("medium"));
        assert_eq!(resolve_reasoning("o3", Some("max"), &[]), eff("high"));
        assert_eq!(resolve_reasoning("gpt-5.5", Some("xhigh"), &[]), eff("high"));
        assert_eq!(resolve_reasoning("o3", Some("extreme"), &[]), None);
        assert_eq!(resolve_reasoning("gpt-4.1", Some("high"), &[]), None);
    }

    #[test]
    fn unknown_models_get_no_reasoning_controls() {
        assert_eq!(
            thinking_capabilities("gpt-4o", &[]),
            ThinkingCapabilities {
                thinks_by_default: false,
                can_disable_thinking: true,
                supports_effort: false
            }
        );
        assert_eq!(resolve_reasoning("gpt-4o", Some("low"), &[]), None);
    }
}
