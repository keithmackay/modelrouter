//! The `usage` callback event: one per cost-ledger row the request path
//! records, built from the stored row so its id, timestamp and resolved
//! project are the values the ledger holds.
//!
//! The payload carries counts, money and the caller's attribution, never prompt
//! or response content, so it needs no redaction. Adding a field keeps
//! [`SCHEMA_VERSION`]; removing, renaming or changing the meaning of one bumps
//! it.

use serde_json::{json, Map, Value};

use crate::db::models::CostLedgerEntry;

/// Version of the payload shape built by [`from_ledger`].
pub const SCHEMA_VERSION: u32 = 1;

/// Event name, in the payload and in a webhook's `events` subscription list.
pub const EVENT: &str = "usage";

/// The `usage` payload for one stored ledger row.
///
/// `uncached_cost_usd` is the cost without the response cache
/// (`cost_usd + saved_usd`), the meaning it has in the router's reports. The
/// provider's prompt cache is described by the token counts, not folded into
/// it.
pub fn from_ledger(row: &CostLedgerEntry) -> Value {
    let tags = match serde_json::from_str::<Value>(&row.attribution_tags) {
        Ok(Value::Object(tags)) => tags,
        _ => Map::new(),
    };
    json!({
        "schema_version": SCHEMA_VERSION,
        "event": EVENT,
        "ledger_id": row.id,
        "occurred_at": row.created_at,
        "user_id": row.user_id,
        "project": row.project,
        "correlation_id": row.attribution_correlation_id,
        "tags": tags,
        "model": row.model,
        "provider": row.provider,
        "tokens": {
            "input": row.tokens_in,
            "output": row.tokens_out,
            "cache_read": row.cache_read_tokens,
            "cache_write": row.cache_write_tokens,
            "estimated": row.tokens_estimated,
        },
        "cost_usd": row.cost_usd,
        "cache_hit": row.cache_hit,
        "saved_usd": row.saved_usd,
        "uncached_cost_usd": row.cost_usd + row.saved_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::CostLedgerEntry;
    use serde_json::json;

    fn row() -> CostLedgerEntry {
        CostLedgerEntry {
            id: 48213,
            user_id: 7,
            prompt_id: 0,
            model: "claude-sonnet-4-5".into(),
            provider: "anthropic".into(),
            project: Some("website".into()),
            tokens_in: 1200,
            tokens_out: 340,
            cost_usd: 0.0234,
            created_at: "2026-11-14T15:02:11.123+00:00".into(),
            api_key_id: Some(3),
            cache_hit: false,
            saved_usd: 0.0,
            attribution_correlation_id: Some("run-42".into()),
            attribution_tags: r#"{"agent":"worker","source":"coordinator"}"#.into(),
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
            call_kind: "chat".into(),
            cache_read_tokens: 50000,
            cache_write_tokens: 0,
        }
    }

    #[test]
    fn a_priced_row_becomes_the_v1_payload() {
        assert_eq!(
            from_ledger(&row()),
            json!({
                "schema_version": 1,
                "event": "usage",
                "ledger_id": 48213,
                "occurred_at": "2026-11-14T15:02:11.123+00:00",
                "user_id": 7,
                "project": "website",
                "correlation_id": "run-42",
                "tags": {"agent": "worker", "source": "coordinator"},
                "model": "claude-sonnet-4-5",
                "provider": "anthropic",
                "tokens": {
                    "input": 1200,
                    "output": 340,
                    "cache_read": 50000,
                    "cache_write": 0,
                    "estimated": false
                },
                "cost_usd": 0.0234,
                "cache_hit": false,
                "saved_usd": 0.0,
                "uncached_cost_usd": 0.0234
            })
        );
    }

    #[test]
    fn a_response_cache_hit_costs_nothing_and_its_saving_is_the_uncached_cost() {
        let hit = CostLedgerEntry { cache_hit: true, cost_usd: 0.0, saved_usd: 0.5, ..row() };
        let p = from_ledger(&hit);
        assert_eq!(p["cache_hit"], true);
        assert_eq!(p["cost_usd"], 0.0);
        assert_eq!(p["saved_usd"], 0.5);
        assert_eq!(p["uncached_cost_usd"], 0.5);
    }

    #[test]
    fn a_row_without_attribution_has_null_ids_and_empty_tags() {
        let bare = CostLedgerEntry {
            project: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".into(),
            tokens_estimated: true,
            ..row()
        };
        let p = from_ledger(&bare);
        assert_eq!(p["project"], serde_json::Value::Null);
        assert_eq!(p["correlation_id"], serde_json::Value::Null);
        assert_eq!(p["tags"], json!({}));
        assert_eq!(p["tokens"]["estimated"], true);
    }

    #[test]
    fn stored_tags_that_are_not_an_object_are_sent_as_an_empty_object() {
        for stored in ["[]", "", "not json"] {
            let odd = CostLedgerEntry { attribution_tags: stored.into(), ..row() };
            assert_eq!(from_ledger(&odd)["tags"], json!({}), "stored tags {stored:?}");
        }
    }
}
