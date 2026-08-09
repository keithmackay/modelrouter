//! Caller-supplied request attribution.
//!
//! The router already knows *who* paid (user, API key) and *which key's
//! project* the spend belongs to. It did not know what the **caller** considers
//! the unit of work — an engagement, a job, a run. Without that, a consuming app
//! has to keep its own parallel cost ledger.
//!
//! Attribution is metadata only. It never influences routing, provider choice,
//! pricing or the response-cache key: `"attribution"` is listed in
//! [`crate::router::cache::VOLATILE_FIELDS`], so two otherwise-identical requests
//! carrying different attribution share one cache entry.
//!
//! # Wire format
//!
//! The **primary** channel is a body extension field, because every metered
//! endpoint except `/v1/audio/transcriptions` already takes a JSON body, OpenAI
//! and Anthropic SDKs all expose an escape hatch for extra body fields
//! (`extra_body=` / `extraBody`), and a body field survives proxies that strip
//! unknown headers:
//!
//! ```json
//! {
//!   "model": "gpt-4o-mini",
//!   "messages": [...],
//!   "attribution": {
//!     "correlation_id": "eng-4711-run-3",
//!     "tags": { "engagement": "eng-4711", "phase": "research" }
//!   }
//! }
//! ```
//!
//! Headers are the **fallback**, for multipart endpoints and for callers behind
//! an SDK that will not forward unknown body fields:
//!
//! ```text
//! X-Attribution-Correlation-Id: eng-4711-run-3
//! X-Attribution-Tags: {"engagement":"eng-4711","phase":"research"}
//! ```
//!
//! When both are present the body wins, field by field.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Body field carrying attribution.
pub const BODY_FIELD: &str = "attribution";
/// Header carrying the correlation id (fallback channel).
pub const CORRELATION_HEADER: &str = "x-attribution-correlation-id";
/// Header carrying the tag map as a JSON object (fallback channel).
pub const TAGS_HEADER: &str = "x-attribution-tags";

/// Maximum length of a correlation id, in characters.
pub const MAX_CORRELATION_LEN: usize = 128;
/// Maximum number of tag entries on one request.
pub const MAX_TAGS: usize = 8;
/// Maximum length of a tag key, in characters.
pub const MAX_TAG_KEY_LEN: usize = 64;
/// Maximum length of a tag value, in characters.
pub const MAX_TAG_VALUE_LEN: usize = 128;
/// Maximum serialised size of the tag map, in bytes. Bounds the column and
/// stops the field from being used as a general-purpose data channel.
pub const MAX_TAGS_BYTES: usize = 1024;

/// Validated attribution for one request.
///
/// `BTreeMap` (not `HashMap`) so the persisted JSON has a stable key order:
/// two requests tagged the same way serialise byte-identically, which keeps
/// exact-match tag queries honest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    /// Opaque caller-side id for this request, e.g. a job or run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Free-form key/value tags, e.g. `{"engagement": "eng-4711"}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl Attribution {
    /// True when nothing was supplied.
    pub fn is_empty(&self) -> bool {
        self.correlation_id.is_none() && self.tags.is_empty()
    }

    /// Tags as the JSON object stored in `attribution_tags`. `{}` when empty,
    /// so the column is never null and callers can compare literally.
    pub fn tags_json(&self) -> String {
        serde_json::to_string(&self.tags).unwrap_or_else(|_| "{}".to_string())
    }

    /// Parse and validate from a request body plus headers. The body wins
    /// field by field; headers fill in what the body omitted.
    pub fn extract(
        body: &serde_json::Value,
        headers: &axum::http::HeaderMap,
    ) -> Result<Self, AttributionError> {
        let from_body = match body.get(BODY_FIELD) {
            None | Some(serde_json::Value::Null) => Attribution::default(),
            Some(v) => Self::from_value(v)?,
        };
        let mut out = from_body;

        if out.correlation_id.is_none() {
            if let Some(raw) = header_str(headers, CORRELATION_HEADER) {
                out.correlation_id = Some(validate_correlation_id(&raw)?);
            }
        }
        if out.tags.is_empty() {
            if let Some(raw) = header_str(headers, TAGS_HEADER) {
                let parsed: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
                    AttributionError("x-attribution-tags must be a JSON object".to_string())
                })?;
                out.tags = validate_tags(&parsed)?;
            }
        }
        Ok(out)
    }

    /// Parse and validate from headers only — for endpoints without a JSON body
    /// (`/v1/audio/transcriptions` is multipart).
    pub fn from_headers(headers: &axum::http::HeaderMap) -> Result<Self, AttributionError> {
        Self::extract(&serde_json::Value::Null, headers)
    }

    /// Parse and validate the value of the `attribution` body field.
    fn from_value(value: &serde_json::Value) -> Result<Self, AttributionError> {
        let obj = value.as_object().ok_or_else(|| {
            AttributionError("attribution must be an object".to_string())
        })?;

        for key in obj.keys() {
            if key != "correlation_id" && key != "tags" {
                return Err(AttributionError(format!(
                    "unknown attribution field: {}",
                    key
                )));
            }
        }

        let correlation_id = match obj.get("correlation_id") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(validate_correlation_id(s)?),
            Some(_) => {
                return Err(AttributionError(
                    "attribution.correlation_id must be a string".to_string(),
                ))
            }
        };

        let tags = match obj.get("tags") {
            None | Some(serde_json::Value::Null) => BTreeMap::new(),
            Some(v) => validate_tags(v)?,
        };

        Ok(Attribution { correlation_id, tags })
    }
}

/// Rejected attribution. Routes map this to a 400.
#[derive(Debug, Clone)]
pub struct AttributionError(pub String);

impl std::fmt::Display for AttributionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<AttributionError> for crate::api::error::ApiError {
    fn from(e: AttributionError) -> Self {
        crate::api::error::ApiError::InvalidRequest(e.0)
    }
}

fn header_str(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Correlation ids and tag values are printable ASCII without control
/// characters — enough for ids, uuids and slugs, and safe to echo into logs.
fn is_safe(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_graphic() || c == ' ')
}

/// Tag keys are narrower than values: they end up inside a JSON path
/// (`json_extract(attribution_tags, '$."key"')`) when the ledger is queried, so
/// quotes, backslashes and whitespace are excluded by construction rather than
/// escaped at every call site.
pub fn is_safe_tag_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

fn validate_correlation_id(raw: &str) -> Result<String, AttributionError> {
    let v = raw.trim();
    if v.is_empty() {
        return Err(AttributionError(
            "attribution.correlation_id must not be empty".to_string(),
        ));
    }
    if v.chars().count() > MAX_CORRELATION_LEN {
        return Err(AttributionError(format!(
            "attribution.correlation_id must be at most {} characters",
            MAX_CORRELATION_LEN
        )));
    }
    if !is_safe(v) {
        return Err(AttributionError(
            "attribution.correlation_id contains unsupported characters".to_string(),
        ));
    }
    Ok(v.to_string())
}

fn validate_tags(value: &serde_json::Value) -> Result<BTreeMap<String, String>, AttributionError> {
    let obj = value.as_object().ok_or_else(|| {
        AttributionError("attribution.tags must be an object".to_string())
    })?;
    if obj.len() > MAX_TAGS {
        return Err(AttributionError(format!(
            "attribution.tags must have at most {} entries",
            MAX_TAGS
        )));
    }

    let mut out = BTreeMap::new();
    for (k, v) in obj {
        let key = k.trim();
        if key.is_empty() {
            return Err(AttributionError(
                "attribution.tags keys must not be empty".to_string(),
            ));
        }
        if key.chars().count() > MAX_TAG_KEY_LEN {
            return Err(AttributionError(format!(
                "attribution.tags key '{}' exceeds {} characters",
                key, MAX_TAG_KEY_LEN
            )));
        }
        if !is_safe_tag_key(key) {
            return Err(AttributionError(format!(
                "attribution.tags key '{}' must contain only letters, digits, '_', '-', '.' or ':'",
                key
            )));
        }
        let val = match v {
            serde_json::Value::String(s) => s.trim().to_string(),
            // Numbers and booleans are a common accident; accept them rather
            // than fail a whole request over a JSON type.
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => {
                return Err(AttributionError(format!(
                    "attribution.tags value for '{}' must be a string, number or boolean",
                    key
                )))
            }
        };
        if val.chars().count() > MAX_TAG_VALUE_LEN {
            return Err(AttributionError(format!(
                "attribution.tags value for '{}' exceeds {} characters",
                key, MAX_TAG_VALUE_LEN
            )));
        }
        if !is_safe(&val) {
            return Err(AttributionError(format!(
                "attribution.tags value for '{}' contains unsupported characters",
                key
            )));
        }
        out.insert(key.to_string(), val);
    }

    let encoded = serde_json::to_string(&out).unwrap_or_default();
    if encoded.len() > MAX_TAGS_BYTES {
        return Err(AttributionError(format!(
            "attribution.tags must serialise to at most {} bytes",
            MAX_TAGS_BYTES
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::json;

    fn no_headers() -> HeaderMap {
        HeaderMap::new()
    }

    #[test]
    fn absent_attribution_is_empty() {
        let a = Attribution::extract(&json!({"model": "x"}), &no_headers()).unwrap();
        assert!(a.is_empty());
        assert_eq!(a.tags_json(), "{}");
    }

    #[test]
    fn parses_body_attribution() {
        let body = json!({
            "model": "x",
            "attribution": {
                "correlation_id": "eng-4711-run-3",
                "tags": {"engagement": "eng-4711", "phase": "research"}
            }
        });
        let a = Attribution::extract(&body, &no_headers()).unwrap();
        assert_eq!(a.correlation_id.as_deref(), Some("eng-4711-run-3"));
        assert_eq!(a.tags.get("engagement").unwrap(), "eng-4711");
        // BTreeMap ordering makes this byte-stable.
        assert_eq!(
            a.tags_json(),
            r#"{"engagement":"eng-4711","phase":"research"}"#
        );
    }

    #[test]
    fn header_fallback_fills_missing_fields() {
        let mut h = HeaderMap::new();
        h.insert(CORRELATION_HEADER, HeaderValue::from_static("job-9"));
        h.insert(
            TAGS_HEADER,
            HeaderValue::from_static(r#"{"engagement":"eng-1"}"#),
        );
        let a = Attribution::extract(&json!({}), &h).unwrap();
        assert_eq!(a.correlation_id.as_deref(), Some("job-9"));
        assert_eq!(a.tags.get("engagement").unwrap(), "eng-1");
    }

    #[test]
    fn body_wins_over_header() {
        let mut h = HeaderMap::new();
        h.insert(CORRELATION_HEADER, HeaderValue::from_static("from-header"));
        let body = json!({"attribution": {"correlation_id": "from-body"}});
        let a = Attribution::extract(&body, &h).unwrap();
        assert_eq!(a.correlation_id.as_deref(), Some("from-body"));
    }

    #[test]
    fn rejects_oversized_correlation_id() {
        let body = json!({"attribution": {"correlation_id": "x".repeat(MAX_CORRELATION_LEN + 1)}});
        assert!(Attribution::extract(&body, &no_headers()).is_err());
    }

    #[test]
    fn rejects_too_many_tags() {
        let mut tags = serde_json::Map::new();
        for i in 0..(MAX_TAGS + 1) {
            tags.insert(format!("k{}", i), json!("v"));
        }
        let body = json!({"attribution": {"tags": tags}});
        assert!(Attribution::extract(&body, &no_headers()).is_err());
    }

    #[test]
    fn rejects_oversized_tag_value() {
        let body = json!({
            "attribution": {"tags": {"k": "v".repeat(MAX_TAG_VALUE_LEN + 1)}}
        });
        assert!(Attribution::extract(&body, &no_headers()).is_err());
    }

    #[test]
    fn rejects_non_object_attribution() {
        assert!(Attribution::extract(&json!({"attribution": "nope"}), &no_headers()).is_err());
        assert!(Attribution::extract(&json!({"attribution": ["a"]}), &no_headers()).is_err());
    }

    #[test]
    fn rejects_unknown_attribution_field() {
        let body = json!({"attribution": {"correlation_id": "a", "secret": "b"}});
        assert!(Attribution::extract(&body, &no_headers()).is_err());
    }

    #[test]
    fn rejects_nested_tag_values() {
        let body = json!({"attribution": {"tags": {"k": {"nested": true}}}});
        assert!(Attribution::extract(&body, &no_headers()).is_err());
    }

    #[test]
    fn coerces_scalar_tag_values() {
        let body = json!({"attribution": {"tags": {"n": 42, "b": true}}});
        let a = Attribution::extract(&body, &no_headers()).unwrap();
        assert_eq!(a.tags.get("n").unwrap(), "42");
        assert_eq!(a.tags.get("b").unwrap(), "true");
    }

    #[test]
    fn rejects_tag_keys_that_would_break_a_json_path() {
        for bad in ["a\"b", "a\\b", "a b", "a$b"] {
            let body = json!({"attribution": {"tags": {bad: "v"}}});
            assert!(
                Attribution::extract(&body, &no_headers()).is_err(),
                "expected rejection of tag key {:?}",
                bad
            );
        }
        assert!(is_safe_tag_key("engagement.id-1:a_b"));
    }

    #[test]
    fn rejects_control_characters() {
        let body = json!({"attribution": {"correlation_id": "a\nb"}});
        assert!(Attribution::extract(&body, &no_headers()).is_err());
    }

    #[test]
    fn from_headers_ignores_body() {
        let mut h = HeaderMap::new();
        h.insert(CORRELATION_HEADER, HeaderValue::from_static("multipart-1"));
        let a = Attribution::from_headers(&h).unwrap();
        assert_eq!(a.correlation_id.as_deref(), Some("multipart-1"));
    }
}
