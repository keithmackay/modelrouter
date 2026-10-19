//! Request extensions: optional code that a wrapper binary links in to
//! inspect, rewrite or refuse requests before they reach the response cache or
//! a provider.
//!
//! The stock `modelrouter` binary registers none, and then every hook is a
//! no-op that costs one empty-vector check. A wrapper binary depends on this
//! crate as a library and starts the router with
//! [`crate::cli::run_with_extensions`], so nothing in this crate needs to name
//! the extensions it may be run with.
//!
//! The chain, not each extension, enforces failing closed: a hook that returns
//! an error refuses the request.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::api::app::AppState;

/// What a hook knows about the request it is looking at.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub endpoint: &'static str,
    pub user_id: i64,
    pub api_key_id: Option<i64>,
    /// The request's project: the attribution project, else the API key's.
    pub project: Option<String>,
    pub correlation_id: Option<String>,
    pub tags: BTreeMap<String, String>,
}

/// The caller-written text a search request sends upstream. A hook may rewrite
/// any of it; the router sends what the hooks leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchText {
    pub query: String,
    pub instructions: Option<String>,
    pub context: Option<String>,
}

/// One rule a hook applied. Recorded in metrics, logs and the failure record,
/// so it must never carry the text the rule matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleHit {
    pub rule: String,
    pub category: String,
    pub action: String,
}

/// A refusal: the status and body the caller receives, and a reason for the
/// failure record. The body is the caller's; the reason is the router's log.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub status: u16,
    pub body: Value,
    pub reason: String,
}

/// What one hook decided.
#[derive(Debug, Clone, PartialEq)]
pub struct HookOutcome {
    /// `None` sends the (possibly rewritten) request on.
    pub refusal: Option<Refusal>,
    pub hits: Vec<RuleHit>,
    /// Top-level fields added to a successful response, so the caller can see
    /// what the hook did to its request. Never stored in the response cache: a
    /// cache hit carries the fields of the request it answers.
    pub response_fields: serde_json::Map<String, Value>,
}

impl HookOutcome {
    pub fn proceed() -> Self {
        Self { refusal: None, hits: Vec::new(), response_fields: serde_json::Map::new() }
    }
}

/// Resources an extension may use when it starts.
#[derive(Clone)]
pub struct StartContext {
    pub pool: Option<sqlx::SqlitePool>,
}

#[async_trait::async_trait]
pub trait RequestExtension: Send + Sync {
    /// A stable name, reported on `/health` and in metrics and logs.
    fn name(&self) -> &str;

    /// Called once before the router serves. An error stops the router.
    async fn start(&self, _ctx: StartContext) -> anyhow::Result<()> {
        Ok(())
    }

    /// The search hook. It may rewrite `text`. An error refuses the request.
    async fn on_search(&self, _ctx: &RequestContext, _text: &mut SearchText) -> anyhow::Result<HookOutcome> {
        Ok(HookOutcome::proceed())
    }

    /// Extra routes merged into the router, typically under `/admin/api/`.
    fn routes(&self) -> Option<axum::Router<AppState>> {
        None
    }

    /// Extra fields for this extension's entry on `/health`.
    fn status(&self) -> Value {
        json!({})
    }
}

/// The extensions this router runs with, in hook order.
#[derive(Clone, Default)]
pub struct Extensions(Arc<Vec<Arc<dyn RequestExtension>>>);

/// The chain's verdict on a search request.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchVerdict {
    /// Set when one hook refused; later hooks did not run.
    pub refusal: Option<(String, Refusal)>,
    /// Every rule applied, with the extension that applied it.
    pub hits: Vec<(String, RuleHit)>,
    /// Fields for a successful response; the first hook to set a key wins.
    pub response_fields: serde_json::Map<String, Value>,
}

impl SearchVerdict {
    /// Add the response fields to `body`, never replacing a field it has.
    pub fn annotate(&self, body: &mut Value) {
        if let Value::Object(map) = body {
            for (k, v) in &self.response_fields {
                map.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
}

/// The status a hook error answers with: the request was not sent, and the
/// fault is the router's, not the caller's.
pub const HOOK_ERROR_STATUS: u16 = 503;

impl Extensions {
    pub fn new(extensions: Vec<Arc<dyn RequestExtension>>) -> Self {
        Self(Arc::new(extensions))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn RequestExtension>> {
        self.0.iter()
    }

    /// Start every extension in order; the first error stops the router.
    pub async fn start(&self, ctx: StartContext) -> anyhow::Result<()> {
        for ext in self.iter() {
            ext.start(ctx.clone())
                .await
                .map_err(|e| anyhow::anyhow!("request extension `{}` failed to start: {e}", ext.name()))?;
        }
        Ok(())
    }

    /// Run every search hook in order. A hook error refuses the request
    /// (fail closed): the text is never sent unchecked.
    pub async fn on_search(&self, ctx: &RequestContext, text: &mut SearchText) -> SearchVerdict {
        let mut verdict = SearchVerdict { refusal: None, hits: Vec::new(), response_fields: serde_json::Map::new() };
        for ext in self.iter() {
            let name = ext.name().to_string();
            match ext.on_search(ctx, text).await {
                Ok(outcome) => {
                    verdict.hits.extend(outcome.hits.into_iter().map(|h| (name.clone(), h)));
                    for (k, v) in outcome.response_fields {
                        verdict.response_fields.entry(k).or_insert(v);
                    }
                    if let Some(refusal) = outcome.refusal {
                        verdict.refusal = Some((name, refusal));
                        return verdict;
                    }
                }
                Err(e) => {
                    let reason = format!("request extension `{name}` failed; request refused: {e}");
                    let body = json!({
                        "error": {
                            "message": format!("request extension `{name}` failed; the request was not sent"),
                            "type": "extension_error",
                            "code": "extension_error",
                        }
                    });
                    verdict.refusal = Some((name, Refusal { status: HOOK_ERROR_STATUS, body, reason }));
                    return verdict;
                }
            }
        }
        verdict
    }

    /// The `/health` block: one entry per extension.
    pub fn health(&self) -> Value {
        Value::Array(
            self.iter()
                .map(|ext| {
                    let mut entry = json!({ "name": ext.name() });
                    if let (Value::Object(extra), Value::Object(map)) = (ext.status(), &mut entry) {
                        for (k, v) in extra {
                            map.entry(k).or_insert(v);
                        }
                    }
                    entry
                })
                .collect(),
        )
    }

    /// Routes from every extension, merged.
    pub fn routes(&self) -> axum::Router<AppState> {
        self.iter()
            .filter_map(|ext| ext.routes())
            .fold(axum::Router::new(), |acc, r| acc.merge(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        name: &'static str,
        rewrite: Option<&'static str>,
        refuse: bool,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl RequestExtension for Fixed {
        fn name(&self) -> &str {
            self.name
        }
        async fn on_search(&self, _ctx: &RequestContext, text: &mut SearchText) -> anyhow::Result<HookOutcome> {
            if self.fail {
                anyhow::bail!("matcher unavailable");
            }
            if let Some(r) = self.rewrite {
                text.query = r.to_string();
            }
            let hits = vec![RuleHit { rule: "r1".into(), category: "c1".into(), action: "redact".into() }];
            let refusal = self.refuse.then(|| Refusal { status: 422, body: json!({"refused": true}), reason: "r1".into() });
            let mut response_fields = serde_json::Map::new();
            response_fields.insert("note".into(), json!(self.name));
            Ok(HookOutcome { refusal, hits, response_fields })
        }
        fn status(&self) -> Value {
            json!({ "version": 2, "name": "not-overridden" })
        }
    }

    fn text(q: &str) -> SearchText {
        SearchText { query: q.into(), instructions: None, context: None }
    }

    fn ext(f: Fixed) -> Arc<dyn RequestExtension> {
        Arc::new(f)
    }

    #[tokio::test]
    async fn no_extensions_leave_the_text_alone_and_record_nothing() {
        let mut t = text("plain query");
        let verdict = Extensions::default().on_search(&RequestContext::default(), &mut t).await;
        assert_eq!(verdict, SearchVerdict { refusal: None, hits: vec![], response_fields: serde_json::Map::new() });
        assert_eq!(t, text("plain query"));
    }

    #[tokio::test]
    async fn a_rewrite_reaches_the_request_and_its_hits_are_named() {
        let exts = Extensions::new(vec![ext(Fixed { name: "a", rewrite: Some("rewritten"), refuse: false, fail: false })]);
        let mut t = text("original");
        let verdict = exts.on_search(&RequestContext::default(), &mut t).await;
        assert_eq!(t.query, "rewritten");
        assert!(verdict.refusal.is_none());
        assert_eq!(verdict.hits[0].0, "a");
        assert_eq!(verdict.hits[0].1.rule, "r1");
    }

    #[tokio::test]
    async fn a_refusal_stops_the_chain() {
        let exts = Extensions::new(vec![
            ext(Fixed { name: "a", rewrite: None, refuse: true, fail: false }),
            ext(Fixed { name: "b", rewrite: Some("never"), refuse: false, fail: false }),
        ]);
        let mut t = text("q");
        let verdict = exts.on_search(&RequestContext::default(), &mut t).await;
        let (name, refusal) = verdict.refusal.expect("refused");
        assert_eq!((name.as_str(), refusal.status), ("a", 422));
        assert_eq!(t.query, "q", "a later hook must not run after a refusal");
    }

    #[tokio::test]
    async fn a_failing_hook_refuses_the_request() {
        let exts = Extensions::new(vec![ext(Fixed { name: "a", rewrite: None, refuse: false, fail: true })]);
        let verdict = exts.on_search(&RequestContext::default(), &mut text("q")).await;
        let (_, refusal) = verdict.refusal.expect("fail closed");
        assert_eq!(refusal.status, HOOK_ERROR_STATUS);
        assert_eq!(refusal.body["error"]["code"], "extension_error");
        assert!(!refusal.body.to_string().contains("matcher unavailable"), "the caller's body must not carry the internal error");
        assert!(refusal.reason.contains("matcher unavailable"), "the failure record keeps it");
    }

    #[tokio::test]
    async fn response_fields_merge_first_writer_wins_and_never_replace_the_body() {
        let exts = Extensions::new(vec![
            ext(Fixed { name: "a", rewrite: None, refuse: false, fail: false }),
            ext(Fixed { name: "b", rewrite: None, refuse: false, fail: false }),
        ]);
        let verdict = exts.on_search(&RequestContext::default(), &mut text("q")).await;
        let mut body = json!({ "results": [], "engine": "x" });
        verdict.annotate(&mut body);
        assert_eq!(body["note"], "a");
        let mut clash = json!({ "note": "the router's own" });
        verdict.annotate(&mut clash);
        assert_eq!(clash["note"], "the router's own");
    }

    #[test]
    fn health_lists_each_extension_and_its_status_without_renaming_it() {
        let exts = Extensions::new(vec![ext(Fixed { name: "a", rewrite: None, refuse: false, fail: false })]);
        assert_eq!(exts.health(), json!([{ "name": "a", "version": 2 }]));
        assert_eq!(Extensions::default().health(), json!([]));
    }
}
