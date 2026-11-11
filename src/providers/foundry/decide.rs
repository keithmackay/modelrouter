//! The Decisions API against a deployment in Azure AI Foundry.
//!
//! ```text
//! POST {base}/decisions[?api-version=...]
//! Authorization: Bearer <Entra token>   (or: api-key: <key>)
//! {"model": "<deployment>", "input": "<document>",
//!  "questions": [{"type": "predicate", "name": "...", "instructions": "..."}]}
//! ```
//!
//! The response carries `answers[]` in question order (a predicate answers
//! with a `probability`, the other question types with `choice` or `score`)
//! and a `usage` block. The request is passed through as the caller sent it,
//! with `model` set to the resolved deployment, and the response is returned
//! as it arrived: this adapter reads only `usage`, to price the call. The
//! wire shape is as documented at the time of writing and is not re-shaped
//! here, so a field the service adds or renames reaches the caller untouched.
//!
//! Status handling: a 4xx is the service refusing THIS request (an unknown
//! deployment, a deployment without the decision capability, a malformed
//! question) and is returned to the caller as [`UpstreamRejected`], never
//! retried. A 5xx or a transport failure is a provider error.
//!
//! ## A second upstream path: the resource's System One route
//!
//! Some decision model families are not served on `{base}/decisions` at all
//! (that path answers 404 for them) but on a route the resource serves beside
//! its data planes, `POST {resource}/providers/microsoft/v1/systemone`, which
//! takes the System One request shape: the document as `state`, the questions
//! as an object keyed by question id, and answers keyed the same way. The
//! deployments served there are listed under `systemone_deployments`; for
//! one of them the adapter re-shapes the request on the way out
//! ([`build_systemone_request_body`]) and the answers on the way back
//! ([`systemone_response_to_decision`]), so the caller keeps one Decisions
//! shape whichever route a deployment answers on. Only the shape is adapted:
//! question types, instructions, criteria and every other field travel as the
//! caller wrote them, and the service's refusal of one it does not take comes
//! back as the 4xx it was.

use anyhow::Context;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::azure_entra::TokenProvider;
use crate::providers::decision::{
    usage_tokens, DecisionAdapter, DecisionRequest, DecisionRequestInvalid, DecisionResult, UpstreamRejected,
};
use crate::providers::foundry::auth::FoundryAuth;
use crate::providers::foundry::endpoint::FoundryEndpoint;

pub struct FoundryDecisionAdapter {
    endpoint: FoundryEndpoint,
    auth: FoundryAuth,
    client: reqwest::Client,
    /// `decision_deployments`: when non-empty, the only deployments a decision
    /// request may name (beside `systemone_deployments`); anything else is
    /// refused before a call is made.
    decision_deployments: Vec<String>,
    /// `systemone_deployments`: the deployments answered on the resource's
    /// System One route rather than on `{base}/decisions`.
    systemone_deployments: Vec<String>,
}

/// Which upstream path a deployment is answered on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionRoute {
    /// `POST {base}/decisions`, the Decisions API shape as the caller sent it.
    Decisions,
    /// `POST {resource}/providers/microsoft/v1/systemone`, re-shaped both ways.
    SystemOne,
}

impl DecisionRoute {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Decisions => "decisions",
            Self::SystemOne => "systemone",
        }
    }
}

impl FoundryDecisionAdapter {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        let auth = FoundryAuth::entra_by_default(
            "foundry",
            config,
            endpoint.scope(),
            std::time::Duration::from_secs(config.timeout_secs.max(1)),
        )?;
        Self::build(endpoint, auth, config)
    }

    /// Test hook: caller-supplied token source, no Entra round trip.
    pub fn with_token_provider(
        config: &ProviderConfig,
        token_provider: Arc<dyn TokenProvider>,
    ) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        Self::build(endpoint, FoundryAuth::Entra(token_provider), config)
    }

    fn build(endpoint: FoundryEndpoint, auth: FoundryAuth, config: &ProviderConfig) -> anyhow::Result<Self> {
        // The client carries the provider's flat ceiling; each request
        // applies its own (tier-resolved) timeout on top, like chat.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .context("failed to build reqwest client for foundry decisions")?;
        let trimmed = |list: &[String]| {
            list.iter().map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect::<Vec<_>>()
        };
        let decision_deployments = trimmed(&config.decision_deployments);
        let systemone_deployments = trimmed(&config.systemone_deployments);
        tracing::info!(
            endpoint = endpoint.base(),
            surface = endpoint.surface().label(),
            scope = endpoint.scope(),
            auth = auth.label(),
            decision_deployments = ?decision_deployments,
            systemone_deployments = ?systemone_deployments,
            "foundry: decision adapter ready"
        );
        Ok(Self { endpoint, auth, client, decision_deployments, systemone_deployments })
    }

    /// Refuse, before any call, a deployment outside `decision_deployments`
    /// and `systemone_deployments` (a deployment listed for the System One
    /// route is a decision deployment by that listing).
    fn check_deployment(&self, model: &str) -> anyhow::Result<()> {
        if self.decision_deployments.is_empty() && self.systemone_deployments.is_empty() {
            return Ok(());
        }
        if self.decision_deployments.iter().chain(self.systemone_deployments.iter()).any(|d| d == model) {
            return Ok(());
        }
        Err(anyhow::Error::new(DecisionRequestInvalid(format!(
            "deployment \"{model}\" is not one of this endpoint's decision deployments \
             ({}); set [providers.foundry] decision_deployments (or systemone_deployments, for a \
             deployment served on the resource's System One route) to add it",
            self.decision_deployments.iter().chain(self.systemone_deployments.iter()).cloned().collect::<Vec<_>>().join(", ")
        ))))
    }

    /// The upstream path `model` is answered on.
    pub fn route_for(&self, model: &str) -> DecisionRoute {
        if self.systemone_deployments.iter().any(|d| d == model) {
            DecisionRoute::SystemOne
        } else {
            DecisionRoute::Decisions
        }
    }
}

/// The question ids a System One request uses, in the order the caller's
/// `questions` array named them: each question's `name`, or `q<n>` (1-based)
/// for one without a name, made unique with a numeric suffix when two
/// questions share a name, so an answer can always be put back in its place.
fn systemone_question_ids(questions: &[serde_json::Value]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::with_capacity(questions.len());
    for (index, question) in questions.iter().enumerate() {
        let base = question["name"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("q{}", index + 1));
        let mut id = base.clone();
        let mut suffix = 2;
        while ids.contains(&id) {
            id = format!("{base}_{suffix}");
            suffix += 1;
        }
        ids.push(id);
    }
    ids
}

/// The body for the System One route, and the question ids in the caller's
/// order (what [`systemone_response_to_decision`] needs to order the answers).
///
/// From the Decisions shape: `input` becomes `state` (a `state` the caller
/// already sent wins and `input` is then left alone), the `questions` array
/// becomes an object keyed by question id with each question's fields as
/// written except `name` (the key carries it), and `model` is the resolved
/// deployment. A body that already carries `questions` as an object is sent
/// as it is, its keys (in key order) being the order. Nothing else is read or changed.
pub fn build_systemone_request_body(req: &DecisionRequest) -> (serde_json::Value, Vec<String>) {
    let mut body = build_request_body(req);
    let Some(map) = body.as_object_mut() else { return (body, Vec::new()) };
    if !map.contains_key("state") {
        if let Some(input) = map.remove("input") {
            map.insert("state".to_string(), input);
        }
    }
    match map.get("questions") {
        Some(serde_json::Value::Array(questions)) => {
            let ids = systemone_question_ids(questions);
            let mut keyed = serde_json::Map::with_capacity(questions.len());
            for (id, question) in ids.iter().zip(questions.iter()) {
                let mut item = match question {
                    serde_json::Value::Object(fields) => fields.clone(),
                    other => {
                        let mut single = serde_json::Map::new();
                        single.insert("question".to_string(), other.clone());
                        single
                    }
                };
                item.remove("name");
                keyed.insert(id.clone(), serde_json::Value::Object(item));
            }
            map.insert("questions".to_string(), serde_json::Value::Object(keyed));
            (body, ids)
        }
        Some(serde_json::Value::Object(questions)) => {
            let ids = questions.keys().cloned().collect();
            (body, ids)
        }
        _ => (body, Vec::new()),
    }
}

/// The System One route's response in the Decisions shape: `answers` keyed by
/// question id becomes `answers[]` in `question_ids` order, each answer with
/// its `name` put back (an answer for an id the request did not name follows,
/// in key order; an id with no answer is left out, so the caller can see the
/// gap). A response whose `answers` is already an array, or that carries none,
/// is returned as it arrived. `usage` and every other field are untouched.
pub fn systemone_response_to_decision(mut body: serde_json::Value, question_ids: &[String]) -> serde_json::Value {
    let Some(map) = body.as_object_mut() else { return body };
    let Some(serde_json::Value::Object(keyed)) = map.get("answers").cloned() else { return body };
    let mut ordered: Vec<serde_json::Value> = Vec::with_capacity(keyed.len());
    let with_name = |id: &str, answer: &serde_json::Value| -> serde_json::Value {
        let mut item = match answer {
            serde_json::Value::Object(fields) => fields.clone(),
            other => {
                let mut single = serde_json::Map::new();
                single.insert("answer".to_string(), other.clone());
                single
            }
        };
        item.insert("name".to_string(), serde_json::Value::String(id.to_string()));
        serde_json::Value::Object(item)
    };
    for id in question_ids {
        if let Some(answer) = keyed.get(id) {
            ordered.push(with_name(id, answer));
        }
    }
    let mut extra: Vec<&String> = keyed.keys().filter(|k| !question_ids.contains(k)).collect();
    extra.sort();
    for id in extra {
        ordered.push(with_name(id, &keyed[id]));
    }
    map.insert("answers".to_string(), serde_json::Value::Array(ordered));
    body
}

/// The body on the wire: the caller's body with `model` set to the resolved
/// deployment. Nothing else is read, added or removed.
pub fn build_request_body(req: &DecisionRequest) -> serde_json::Value {
    let mut body = match &req.body {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        _ => serde_json::json!({}),
    };
    body["model"] = serde_json::Value::String(req.model.clone());
    body
}

#[async_trait::async_trait]
impl DecisionAdapter for FoundryDecisionAdapter {
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        self.auth.credential_report()
    }

    async fn decide(&self, req: &DecisionRequest) -> anyhow::Result<DecisionResult> {
        self.check_deployment(&req.model)?;
        let route = self.route_for(&req.model);
        let (url, body, question_ids) = match route {
            DecisionRoute::Decisions => (self.endpoint.decisions_url(), build_request_body(req), Vec::new()),
            DecisionRoute::SystemOne => {
                let (body, ids) = build_systemone_request_body(req);
                (self.endpoint.systemone_url(), body, ids)
            }
        };
        let question_count = match &body["questions"] {
            serde_json::Value::Array(q) => q.len(),
            serde_json::Value::Object(q) => q.len(),
            _ => 0,
        };
        tracing::debug!(
            url = %url,
            surface = self.endpoint.surface().label(),
            route = route.label(),
            auth = self.auth.label(),
            model = %req.model,
            questions = question_count,
            timeout_secs = req.timeout.as_secs(),
            "foundry: decision request"
        );

        let request = self
            .auth
            .apply(self.client.post(&url).timeout(req.timeout).json(&body))
            .await
            .context("failed to attach credentials to the Azure AI Foundry decision request")?;
        let resp = request
            .send()
            .await
            .with_context(|| format!("failed to send decision request to Azure AI Foundry at {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                url = %url,
                %status,
                model = %req.model,
                body = text.as_str(),
                "foundry: decision request failed"
            );
            if status.is_client_error() {
                // The service refused this request: the caller's to read and act
                // on, with the service's own body. Not retried, no fallback.
                return Err(anyhow::Error::new(UpstreamRejected::from_response(status.as_u16(), &text)));
            }
            anyhow::bail!(
                "Azure AI Foundry decisions returned {status} for deployment \"{}\" at {url}: {text}",
                req.model
            );
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Azure AI Foundry returned a decision body that is not JSON")?;
        let body = match route {
            DecisionRoute::Decisions => body,
            DecisionRoute::SystemOne => systemone_response_to_decision(body, &question_ids),
        };
        let (prompt_tokens, completion_tokens) = usage_tokens(&body);
        tracing::debug!(
            model = %req.model,
            route = route.label(),
            answers = body["answers"].as_array().map(|a| a.len()).unwrap_or(0),
            prompt_tokens,
            completion_tokens,
            "foundry: decision ok"
        );
        Ok(DecisionResult { body, prompt_tokens, completion_tokens })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str) -> DecisionRequest {
        DecisionRequest {
            model: model.into(),
            body: serde_json::json!({
                "model": "alias-the-caller-used",
                "input": "doc",
                "questions": [{"type": "predicate", "name": "q1", "instructions": "Is it?"}],
                "extra": {"kept": true}
            }),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    #[test]
    fn the_body_is_the_callers_with_the_resolved_deployment_as_model() {
        let body = build_request_body(&req("dep-1"));
        assert_eq!(body["model"], "dep-1");
        assert_eq!(body["input"], "doc");
        assert_eq!(body["questions"][0]["name"], "q1");
        // Unknown fields travel untouched: the router does not re-shape the request.
        assert_eq!(body["extra"]["kept"], true);
    }

    #[test]
    fn a_non_object_body_still_names_the_deployment() {
        let mut r = req("dep-1");
        r.body = serde_json::Value::Null;
        assert_eq!(build_request_body(&r), serde_json::json!({"model": "dep-1"}));
    }

    #[test]
    fn a_deployment_outside_the_configured_list_is_refused_by_name() {
        let config = ProviderConfig {
            foundry_endpoint: Some("https://res.services.ai.azure.com".into()),
            decision_deployments: vec!["dep-a".into(), " dep-b ".into(), "".into()],
            ..Default::default()
        };
        let adapter = FoundryDecisionAdapter::with_token_provider(
            &config,
            Arc::new(crate::providers::azure_entra::StaticTokenProvider::new("t")),
        )
        .unwrap();
        adapter.check_deployment("dep-a").unwrap();
        adapter.check_deployment("dep-b").unwrap();
        let err = adapter.check_deployment("dep-c").unwrap_err();
        let invalid = DecisionRequestInvalid::find(&err).expect("typed refusal");
        assert!(invalid.0.contains("\"dep-c\""), "{}", invalid.0);
        assert!(invalid.0.contains("dep-a, dep-b"), "{}", invalid.0);
    }

    #[test]
    fn a_systemone_deployment_is_a_decision_deployment_and_takes_the_systemone_route() {
        let config = ProviderConfig {
            foundry_endpoint: Some("https://res.services.ai.azure.com".into()),
            decision_deployments: vec!["dep-a".into()],
            systemone_deployments: vec![" dep-s1 ".into()],
            ..Default::default()
        };
        let adapter = FoundryDecisionAdapter::with_token_provider(
            &config,
            Arc::new(crate::providers::azure_entra::StaticTokenProvider::new("t")),
        )
        .unwrap();
        adapter.check_deployment("dep-s1").unwrap();
        assert_eq!(adapter.route_for("dep-s1"), DecisionRoute::SystemOne);
        assert_eq!(adapter.route_for("dep-a"), DecisionRoute::Decisions);
        let err = adapter.check_deployment("dep-c").unwrap_err();
        let invalid = DecisionRequestInvalid::find(&err).expect("typed refusal");
        assert!(invalid.0.contains("dep-a, dep-s1"), "{}", invalid.0);
        assert!(invalid.0.contains("systemone_deployments"), "{}", invalid.0);
    }

    #[test]
    fn the_systemone_body_keys_the_questions_by_name_and_sends_the_document_as_state() {
        let mut r = req("dep-s1");
        r.body["questions"] = serde_json::json!([
            {"type": "predicate", "name": "refund", "instructions": "Refund asked?"},
            {"type": "choice", "instructions": "Tone?", "criteria": {"calm": "calm", "angry": "angry"}},
            {"type": "score", "name": "refund", "instructions": "How strongly?"}
        ]);
        let (body, ids) = build_systemone_request_body(&r);
        assert_eq!(ids, vec!["refund", "q2", "refund_2"]);
        assert_eq!(body["model"], "dep-s1");
        assert_eq!(body["state"], "doc");
        assert!(body.get("input").is_none(), "input moved to state: {body}");
        assert_eq!(body["questions"]["refund"], serde_json::json!({"type": "predicate", "instructions": "Refund asked?"}));
        assert_eq!(body["questions"]["q2"]["criteria"]["angry"], "angry");
        assert_eq!(body["questions"]["refund_2"]["type"], "score");
        // Unknown fields still travel untouched.
        assert_eq!(body["extra"]["kept"], true);
    }

    #[test]
    fn a_state_the_caller_sent_wins_and_keyed_questions_pass_through() {
        let mut r = req("dep-s1");
        r.body["state"] = serde_json::json!({"already": "shaped"});
        r.body["questions"] = serde_json::json!({"b": {"type": "choice"}, "a": {"type": "score"}});
        let (body, ids) = build_systemone_request_body(&r);
        assert_eq!(body["state"], serde_json::json!({"already": "shaped"}));
        assert_eq!(body["input"], "doc");
        assert_eq!(body["questions"]["b"]["type"], "choice");
        // An object's keys are read in key order (serde_json maps are ordered by key).
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn keyed_answers_come_back_in_question_order_with_their_names_and_usage_untouched() {
        let upstream = serde_json::json!({
            "model": "dep-s1",
            "answers": {
                "q2": {"type": "choice", "choice": "calm", "probabilities": {"calm": 0.8, "angry": 0.2}, "confidence": 0.8},
                "refund": {"type": "predicate", "probability": 0.93},
                "stray": {"type": "score", "score": 3}
            },
            "usage": {"input_tokens": 1000, "output_tokens": 10}
        });
        let mapped = systemone_response_to_decision(upstream, &["refund".into(), "q2".into(), "missing".into()]);
        let answers = mapped["answers"].as_array().expect("array");
        assert_eq!(answers.iter().map(|a| a["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["refund", "q2", "stray"]);
        assert_eq!(answers[0]["probability"], 0.93);
        assert_eq!(answers[1]["choice"], "calm");
        assert_eq!(mapped["usage"]["input_tokens"], 1000);
        assert_eq!(usage_tokens(&mapped), (1000, 10));
        // Already an array, or no answers at all: returned as it arrived.
        let array = serde_json::json!({"answers": [{"name": "x"}]});
        assert_eq!(systemone_response_to_decision(array.clone(), &["x".into()]), array);
        let none = serde_json::json!({"error": "no"});
        assert_eq!(systemone_response_to_decision(none.clone(), &[]), none);
    }

    #[test]
    fn an_empty_list_passes_every_deployment_through() {
        let config = ProviderConfig {
            foundry_endpoint: Some("https://res.services.ai.azure.com".into()),
            ..Default::default()
        };
        let adapter = FoundryDecisionAdapter::with_token_provider(
            &config,
            Arc::new(crate::providers::azure_entra::StaticTokenProvider::new("t")),
        )
        .unwrap();
        adapter.check_deployment("anything").unwrap();
    }
}
