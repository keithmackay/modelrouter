//! Scoped alias overrides.
//!
//! A scope is one attribution tag, `key = value`. Each scope may map any of
//! the caller's alias names (`fast`, `deep`, ...) to a pinned provider/model.
//! A request whose attribution tags carry the scope's tag resolves those
//! names through the scope first, then through the global aliases. This lets
//! an operator route one tenant, project or run to different models while
//! every caller keeps sending the same abstract names.
//!
//! Resolution order for a request: an experiment variant's overlay, then a
//! scoped override, then the global aliases. When several of a request's tags
//! name scopes that map the requested alias, the tag with the smallest key
//! wins (tags are a sorted map), so the answer never depends on header order.
//!
//! An override may also set request parameters for an alias (reasoning
//! effort, max tokens, temperature). They are validated against the pinned
//! model when written and overwrite the caller's values on the chat
//! completions surface; the other surfaces route the model only.
//!
//! The snapshot is rebuilt from the database after every write and on the
//! lifecycle tick, so every replica converges on the same overrides.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::db::models::ScopedAlias;

/// Where a scoped override sends one alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedTarget {
    pub tag_key: String,
    pub tag_value: String,
    pub provider: String,
    pub model: String,
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl ScopedTarget {
    /// The pinned `provider/model` expression the router resolves.
    pub fn expression(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

#[derive(Debug, Clone)]
struct Pin {
    provider: String,
    model: String,
    params: serde_json::Map<String, serde_json::Value>,
    expires_at: i64,
}

type ScopeKey = (String, String);

/// The live overrides: scope -> alias -> pin.
#[derive(Default)]
pub struct ScopedAliases {
    scopes: ArcSwap<HashMap<ScopeKey, HashMap<String, Pin>>>,
}

impl ScopedAliases {
    /// Replace the snapshot with these rows.
    pub fn store(&self, rows: &[ScopedAlias]) {
        let mut scopes: HashMap<ScopeKey, HashMap<String, Pin>> = HashMap::new();
        for row in rows {
            scopes
                .entry((row.tag_key.clone(), row.tag_value.clone()))
                .or_default()
                .insert(
                    row.alias.clone(),
                    Pin {
                        provider: row.provider.clone(),
                        model: row.model.clone(),
                        params: row.params.clone(),
                        expires_at: row.expires_at,
                    },
                );
        }
        self.scopes.store(Arc::new(scopes));
    }

    /// Number of scopes in the snapshot.
    pub fn scope_count(&self) -> usize {
        self.scopes.load().len()
    }

    /// The override for `requested` under these tags, if a live one applies.
    pub fn target_for(
        &self,
        tags: &BTreeMap<String, String>,
        requested: &str,
        now_epoch: i64,
    ) -> Option<ScopedTarget> {
        let scopes = self.scopes.load();
        if scopes.is_empty() {
            return None;
        }
        tags.iter().find_map(|(key, value)| {
            let pin = scopes.get(&(key.clone(), value.clone()))?.get(requested)?;
            if pin.expires_at != 0 && now_epoch >= pin.expires_at {
                return None;
            }
            Some(ScopedTarget {
                tag_key: key.clone(),
                tag_value: value.clone(),
                provider: pin.provider.clone(),
                model: pin.model.clone(),
                params: pin.params.clone(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, value: &str, alias: &str, model: &str, expires_at: i64) -> ScopedAlias {
        ScopedAlias {
            tag_key: key.into(),
            tag_value: value.into(),
            alias: alias.into(),
            target: format!("mock/{model}"),
            provider: "mock".into(),
            model: model.into(),
            expires_at,
            created_by: None,
            created_at: String::new(),
            params: serde_json::Map::new(),
        }
    }

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_matching_tag_maps_the_alias_and_others_fall_through() {
        let s = ScopedAliases::default();
        s.store(&[row("tenant", "t1", "deep", "model-b", 0)]);
        let hit = s.target_for(&tags(&[("tenant", "t1")]), "deep", 100).unwrap();
        assert_eq!(hit.expression(), "mock/model-b");
        assert_eq!((hit.tag_key.as_str(), hit.tag_value.as_str()), ("tenant", "t1"));
        assert!(s.target_for(&tags(&[("tenant", "t1")]), "fast", 100).is_none());
        assert!(s.target_for(&tags(&[("tenant", "t2")]), "deep", 100).is_none());
        assert!(s.target_for(&tags(&[]), "deep", 100).is_none());
    }

    #[test]
    fn an_expired_override_no_longer_applies() {
        let s = ScopedAliases::default();
        s.store(&[row("tenant", "t1", "deep", "model-b", 200)]);
        assert!(s.target_for(&tags(&[("tenant", "t1")]), "deep", 199).is_some());
        assert!(s.target_for(&tags(&[("tenant", "t1")]), "deep", 200).is_none());
    }

    #[test]
    fn the_smallest_tag_key_wins_when_two_scopes_match() {
        let s = ScopedAliases::default();
        s.store(&[row("run", "r1", "deep", "model-r", 0), row("project", "p1", "deep", "model-p", 0)]);
        let hit = s.target_for(&tags(&[("run", "r1"), ("project", "p1")]), "deep", 1).unwrap();
        assert_eq!(hit.model, "model-p");
    }

    #[test]
    fn a_hit_carries_the_aliases_parameters() {
        let s = ScopedAliases::default();
        let mut r = row("tenant", "t1", "deep", "model-b", 0);
        r.params = serde_json::json!({"reasoning_effort": "high"}).as_object().unwrap().clone();
        s.store(&[r]);
        let hit = s.target_for(&tags(&[("tenant", "t1")]), "deep", 1).unwrap();
        assert_eq!(hit.params["reasoning_effort"], "high");
    }

    #[test]
    fn store_replaces_the_snapshot() {
        let s = ScopedAliases::default();
        s.store(&[row("tenant", "t1", "deep", "model-b", 0)]);
        assert_eq!(s.scope_count(), 1);
        s.store(&[]);
        assert_eq!(s.scope_count(), 0);
        assert!(s.target_for(&tags(&[("tenant", "t1")]), "deep", 1).is_none());
    }
}
