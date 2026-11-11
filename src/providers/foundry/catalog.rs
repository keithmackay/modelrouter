//! Model discovery for the `foundry` provider (issues #23 / #32 / #34).
//!
//! ```text
//! GET {endpoint}/openai/v1/models
//! {"object": "list", "data": [{"id": "<deployment>", "object": "model", ...}]}
//! ```
//!
//! The ids this returns are DEPLOYMENT names on the resource, which is exactly
//! what `model` must be set to on the inference call — so a catalog entry can
//! be pasted straight into a `routing.model_aliases` value as
//! `foundry/<id>` and it will route.
//!
//! Credentials are the adapter's own; discovery opens no second auth surface.
//! See `endpoint::FoundryEndpoint::models_url` for why this always targets the
//! OpenAI-compatible listing even when inference is pointed at the Model
//! Inference surface (that surface has no listing operation, only a
//! per-deployment `/info`).

use anyhow::Context;
use async_trait::async_trait;

use std::collections::BTreeMap;

use crate::providers::catalog::{CatalogKind, CatalogModel, CatalogSource, ProviderCatalog};
use crate::providers::foundry::adapter::{DeploymentListing, FoundryAdapter};

/// Parse an OpenAI-shaped model listing into catalog entries.
///
/// Entries with no usable `id` are skipped rather than fatal: a resource can
/// carry deployment records the listing renders oddly, and one malformed row
/// should not cost the operator the rest of the catalog.
pub fn parse_models(v: &serde_json::Value) -> Vec<CatalogModel> {
    let mut models = Vec::new();
    for m in v["data"].as_array().into_iter().flatten() {
        let Some(id) = m["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            tracing::warn!(entry = %m, "foundry: catalog entry has no `id`; skipping");
            continue;
        };
        models.push(CatalogModel {
            provider: "foundry".to_string(),
            name: id.to_string(),
            // The listing carries no human label — `id` IS the deployment name
            // the operator chose. Inventing one from `owned_by` would be noise.
            display_name: None,
            kind: CatalogKind::Chat,
            source: None,
        });
    }
    models
}

/// One deployment as a management-plane `accounts/deployments` list page
/// reports it: the name an inference call addresses, the base model behind
/// it and the capability map Azure attaches (`chatCompletion`, `embeddings`,
/// `imageGenerations`, ... as `"true"` strings; serverless deployments often
/// carry none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDeployment {
    pub name: String,
    pub model_name: Option<String>,
    pub capabilities: BTreeMap<String, String>,
}

/// The deployments a management-plane `accounts/deployments` list page
/// reports as `provisioningState: Succeeded`, and the page's `nextLink`.
///
/// Only `Succeeded` serves: `Creating`, `Updating`, `Failed`, `Deleting` and
/// `Canceled` deployments all refuse inference calls.
pub fn parse_live_deployments(v: &serde_json::Value) -> (Vec<LiveDeployment>, Option<String>) {
    let deployments = v["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| {
            d["properties"]["provisioningState"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("succeeded"))
        })
        .filter_map(|d| {
            let name = d["name"].as_str().map(str::trim).filter(|n| !n.is_empty())?;
            let capabilities = d["properties"]["capabilities"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Some(LiveDeployment {
                name: name.to_string(),
                model_name: d["properties"]["model"]["name"].as_str().map(str::trim).filter(|m| !m.is_empty()).map(str::to_string),
                capabilities,
            })
        })
        .collect();
    let next = v["nextLink"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    (deployments, next)
}

/// The Succeeded deployment names of a list page (see [`parse_live_deployments`]).
pub fn parse_succeeded_deployments(v: &serde_json::Value) -> (Vec<String>, Option<String>) {
    let (deployments, next) = parse_live_deployments(v);
    (deployments.into_iter().map(|d| d.name).collect(), next)
}

fn capability_on(d: &LiveDeployment, key: &str) -> bool {
    d.capabilities
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(key) && v.eq_ignore_ascii_case("true"))
}

fn name_has(d: &LiveDeployment, needles: &[&str]) -> bool {
    let name = d.name.to_ascii_lowercase();
    let model = d.model_name.as_deref().unwrap_or("").to_ascii_lowercase();
    needles.iter().any(|n| name.contains(n) || model.contains(n))
}

/// What a discovered deployment can answer, from its capability map first
/// (Azure's own word) and the model's name when the map is empty, as it is
/// for most serverless (Models-as-a-Service) deployments. A deployment that
/// says nothing either way is chat: that is what a deployment is for unless
/// something marks it otherwise, and a wrong guess costs one refused call,
/// not a missing model.
pub fn classify_deployment(d: &LiveDeployment) -> CatalogKind {
    if capability_on(d, "embeddings") {
        return CatalogKind::Embedding;
    }
    if capability_on(d, "imageGenerations") {
        return CatalogKind::Image;
    }
    if ["audio", "realtime", "speech", "transcription", "textToSpeech"].iter().any(|k| capability_on(d, k)) {
        return CatalogKind::Audio;
    }
    if capability_on(d, "rerank") {
        return CatalogKind::Rerank;
    }
    if ["chatCompletion", "completion", "assistants", "responses", "agents"].iter().any(|k| capability_on(d, k)) {
        return CatalogKind::Chat;
    }
    if name_has(d, &["embed"]) {
        return CatalogKind::Embedding;
    }
    if name_has(d, &["rerank"]) {
        return CatalogKind::Rerank;
    }
    if name_has(d, &["whisper", "tts", "transcribe", "realtime", "speech"]) {
        return CatalogKind::Audio;
    }
    if name_has(d, &["dall-e", "dalle", "image", "flux", "sora", "stable-diffusion"]) {
        return CatalogKind::Image;
    }
    CatalogKind::Chat
}

/// Page bound for the deployment listing: an account holds far fewer
/// deployments than this many pages, so reaching it means a looping nextLink.
const MAX_DEPLOYMENT_PAGES: usize = 50;

fn foundry_model(name: &str) -> CatalogModel {
    CatalogModel {
        provider: "foundry".to_string(),
        name: name.to_string(),
        display_name: None,
        kind: CatalogKind::Chat,
        source: None,
    }
}

fn configured_model(name: &str) -> CatalogModel {
    CatalogModel { source: Some(CatalogSource::Configured), ..foundry_model(name) }
}

fn discovered_model(d: &LiveDeployment) -> CatalogModel {
    CatalogModel {
        provider: "foundry".to_string(),
        name: d.name.clone(),
        // The base model, when the deployment is not simply named after it,
        // so a picker can say what "my-chat-eu" is.
        display_name: d
            .model_name
            .as_ref()
            .filter(|m| !m.eq_ignore_ascii_case(&d.name))
            .map(|m| format!("{} ({m})", d.name)),
        kind: classify_deployment(d),
        source: Some(CatalogSource::Discovered),
    }
}

/// The configured catalog narrowed to the account's live deployments, and,
/// with `discover`, every other live deployment after it (sorted, classified,
/// `source: discovered`; `decision` names are never discovered as chat). An
/// empty configured list is every live deployment: as chat with no source
/// unless `discover`, when each is classified and discovered.
pub fn merge_live_catalog(
    configured: &[String],
    decision: &[String],
    live: &[LiveDeployment],
    discover: bool,
) -> (Vec<CatalogModel>, Vec<String>) {
    let by_name: BTreeMap<&str, &LiveDeployment> = live.iter().map(|d| (d.name.as_str(), d)).collect();
    if configured.is_empty() {
        let models = by_name
            .values()
            .map(|d| if discover { discovered_model(d) } else { foundry_model(&d.name) })
            .collect();
        return (models, Vec::new());
    }
    let (kept, missing): (Vec<&String>, Vec<&String>) = configured.iter().partition(|n| by_name.contains_key(n.as_str()));
    let mut models: Vec<CatalogModel> = kept.iter().map(|n| configured_model(n)).collect();
    if discover {
        models.extend(
            by_name
                .values()
                .filter(|d| !configured.contains(&d.name) && !decision.contains(&d.name))
                .map(|d| discovered_model(d)),
        );
    }
    (models, missing.into_iter().cloned().collect())
}

impl FoundryAdapter {
    /// Every deployment the account serves now, from the management plane.
    async fn live_deployments(&self, listing: &DeploymentListing) -> anyhow::Result<Vec<LiveDeployment>> {
        if matches!(listing.auth, crate::providers::azure_credentials::AzureAuth::ApiKey(_)) {
            anyhow::bail!(
                "foundry: account_resource_id needs an Entra credential (Resource Manager does not \
                 accept the api-key header); unset api_key or unset account_resource_id"
            );
        }
        let mut names = Vec::new();
        let mut url = Some(listing.url.clone());
        let mut pages = 0;
        while let Some(page_url) = url.take() {
            pages += 1;
            if pages > MAX_DEPLOYMENT_PAGES {
                anyhow::bail!("foundry: deployment listing exceeded {MAX_DEPLOYMENT_PAGES} pages");
            }
            let request = listing
                .auth
                .apply(self.http_client().get(&page_url))
                .await
                .context("failed to attach credentials to the Foundry deployment listing")?;
            let resp = request
                .send()
                .await
                .with_context(|| format!("failed to list Foundry deployments at {page_url}"))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(url = %page_url, %status, body = text.as_str(), "foundry: deployment listing refused");
                anyhow::bail!(
                    "Foundry deployment listing returned {status}: {text} — the identity needs read on the \
                     account (Microsoft.CognitiveServices/*/read, e.g. \"Cognitive Services User\")"
                );
            }
            let body: serde_json::Value =
                resp.json().await.context("Foundry deployment listing is not JSON")?;
            let (page, next) = parse_live_deployments(&body);
            names.extend(page);
            url = next;
        }
        Ok(names)
    }
}

/// The chat catalog with the decision-only deployments appended under their
/// own kind, so a chat picker can leave them out and a decisions picker can
/// find them. A name already in the chat catalog keeps its chat entry.
pub fn with_decision_catalog(models: Vec<CatalogModel>, decision: &[String]) -> Vec<CatalogModel> {
    with_kind_catalog(models, decision, crate::providers::catalog::CatalogKind::Decision)
}

/// The rerank-only deployments appended under kind `rerank`, by the same rule.
pub fn with_rerank_catalog(models: Vec<CatalogModel>, rerank: &[String]) -> Vec<CatalogModel> {
    with_kind_catalog(models, rerank, crate::providers::catalog::CatalogKind::Rerank)
}

fn with_kind_catalog(mut models: Vec<CatalogModel>, names: &[String], kind: crate::providers::catalog::CatalogKind) -> Vec<CatalogModel> {
    for name in names {
        if models.iter().any(|m| &m.name == name) {
            continue;
        }
        models.push(CatalogModel {
            provider: "foundry".to_string(),
            name: name.clone(),
            display_name: None,
            kind,
            source: None,
        });
    }
    models
}

#[async_trait]
impl ProviderCatalog for FoundryAdapter {
    /// The chat catalog, plus the decision-only deployments as kind `decision`
    /// and the rerank-only deployments as kind `rerank`.
    async fn list_models(&self) -> anyhow::Result<Vec<CatalogModel>> {
        let chat = self.chat_models().await?;
        Ok(with_rerank_catalog(with_decision_catalog(chat, self.decision_catalog()), self.rerank_catalog()))
    }
}

impl FoundryAdapter {
    /// Every chat-capable deployment the account serves now, from the management plane.
    async fn chat_models(&self) -> anyhow::Result<Vec<CatalogModel>> {
        // The account's live deployments, when the account is named: the
        // configured list narrowed to what is deployed and Succeeded, so a
        // picker never offers a name that would fail on its first call.
        if let Some(listing) = self.deployment_listing() {
            let live = self.live_deployments(listing).await?;
            let (models, missing) = merge_live_catalog(
                self.configured_catalog(),
                self.decision_catalog(),
                &live,
                self.discover_deployments(),
            );
            if !missing.is_empty() {
                tracing::warn!(
                    missing = ?missing,
                    "foundry: configured deployments not deployed (or not Succeeded) on the account; left out of the catalog"
                );
            }
            if self.discover_deployments() {
                let discovered: Vec<&str> = models
                    .iter()
                    .filter(|m| m.source == Some(CatalogSource::Discovered))
                    .map(|m| m.name.as_str())
                    .collect();
                tracing::debug!(count = discovered.len(), ?discovered, "foundry: deployments discovered beyond the configured list");
            }
            return Ok(models);
        }
        // Configured deployments are the authoritative catalog: the endpoint's
        // listing on an AI Services resource is the base-model catalog, not
        // what this resource can actually serve.
        if !self.configured_catalog().is_empty() {
            return Ok(self.configured_catalog().iter().map(|n| foundry_model(n)).collect());
        }
        let url = self.endpoint().models_url();
        tracing::debug!(
            url = %url,
            auth = self.auth().label(),
            "foundry: listing deployed models"
        );

        let request = self
            .auth()
            .apply(self.http_client().get(&url))
            .await
            .context("failed to attach credentials to the Azure AI Foundry catalog request")?;

        let resp = request
            .send()
            .await
            .with_context(|| format!("failed to list Azure AI Foundry models at {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                url = %url,
                %status,
                body = text.as_str(),
                "foundry: model listing refused"
            );
            anyhow::bail!("Azure AI Foundry model listing returned {status}: {text}");
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Azure AI Foundry model listing is not JSON")?;
        let models = parse_models(&body);
        tracing::debug!(count = models.len(), "foundry: catalog listed");
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_deployments_join_the_catalog_as_their_own_kind_and_never_shadow_a_chat_entry() {
        use crate::providers::catalog::CatalogKind;
        let chat = vec![foundry_model("gpt-chat"), foundry_model("both")];
        let out = with_decision_catalog(chat, &["luna-d".to_string(), "both".to_string(), "luna-d".to_string()]);
        let names: Vec<(&str, CatalogKind)> = out.iter().map(|m| (m.name.as_str(), m.kind)).collect();
        assert_eq!(
            names,
            vec![("gpt-chat", CatalogKind::Chat), ("both", CatalogKind::Chat), ("luna-d", CatalogKind::Decision)]
        );
        assert_eq!(serde_json::to_value(&out[2]).unwrap()["kind"], "decision");
        assert_eq!(serde_json::to_value(&out[0]).unwrap()["kind"], "chat");
    }

    #[test]
    fn ids_become_routable_model_names_under_the_foundry_provider() {
        let models = parse_models(&serde_json::json!({
            "object": "list",
            "data": [
                {"id": "gpt-4o-mini", "object": "model", "owned_by": "openai"},
                {"id": "my-llama-deployment", "object": "model"}
            ]
        }));
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|m| m.provider == "foundry"));
        assert_eq!(models[0].name, "gpt-4o-mini");
        assert_eq!(models[1].name, "my-llama-deployment");
        assert_eq!(models[0].display_name, None);
    }

    #[test]
    fn an_entry_without_an_id_is_skipped_not_fatal() {
        let models = parse_models(&serde_json::json!({
            "data": [{"object": "model"}, {"id": "  "}, {"id": "good"}]
        }));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "good");
    }

    #[test]
    fn a_listing_with_no_data_array_is_an_empty_catalog() {
        assert!(parse_models(&serde_json::json!({"object": "list"})).is_empty());
    }

    fn live(name: &str, model: Option<&str>, caps: &[(&str, &str)]) -> LiveDeployment {
        LiveDeployment {
            name: name.to_string(),
            model_name: model.map(str::to_string),
            capabilities: caps.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    #[test]
    fn a_list_page_yields_succeeded_deployments_with_their_model_and_capabilities() {
        let (page, next) = parse_live_deployments(&serde_json::json!({
            "value": [
                {"name": "gpt-4.1", "properties": {"provisioningState": "Succeeded", "model": {"name": "gpt-4.1", "format": "OpenAI"}, "capabilities": {"chatCompletion": "true", "maxContext": "1047576"}}},
                {"name": "emb", "properties": {"provisioningState": "Succeeded", "model": {"name": "text-embedding-3-large"}, "capabilities": {"embeddings": "true"}}},
                {"name": "failed", "properties": {"provisioningState": "Failed"}},
                {"name": "  ", "properties": {"provisioningState": "Succeeded"}}
            ],
            "nextLink": "https://m/next"
        }));
        assert_eq!(next.as_deref(), Some("https://m/next"));
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].name, "gpt-4.1");
        assert_eq!(page[0].capabilities.get("chatCompletion").map(String::as_str), Some("true"));
        assert_eq!(page[1].model_name.as_deref(), Some("text-embedding-3-large"));
        assert_eq!(parse_succeeded_deployments(&serde_json::json!({"value": [{"name": "a", "properties": {"provisioningState": "Succeeded"}}]})).0, vec!["a"]);
    }

    #[test]
    fn deployments_are_classified_from_capabilities_first_and_names_second() {
        assert_eq!(classify_deployment(&live("x", None, &[("embeddings", "true")])), CatalogKind::Embedding);
        assert_eq!(classify_deployment(&live("x", None, &[("imageGenerations", "true")])), CatalogKind::Image);
        assert_eq!(classify_deployment(&live("x", None, &[("realtime", "true")])), CatalogKind::Audio);
        assert_eq!(classify_deployment(&live("x", None, &[("chatCompletion", "true"), ("embeddings", "false")])), CatalogKind::Chat);
        // Serverless deployments carry no capability map: the names decide.
        assert_eq!(classify_deployment(&live("Cohere-embed-v4", Some("Cohere-embed-v4"), &[])), CatalogKind::Embedding);
        assert_eq!(classify_deployment(&live("ranker", Some("Cohere-rerank-v4.0-pro"), &[])), CatalogKind::Rerank);
        assert_eq!(classify_deployment(&live("whisper", None, &[])), CatalogKind::Audio);
        assert_eq!(classify_deployment(&live("dall-e-3", None, &[])), CatalogKind::Image);
        assert_eq!(classify_deployment(&live("DeepSeek-V4-Pro", Some("DeepSeek-V4-Pro"), &[])), CatalogKind::Chat);
    }

    // Break proof: with the `if discover` extension in merge_live_catalog disabled, this
    // fails on the second assertion (only the two configured rows come back).
    #[test]
    fn discovery_keeps_the_configured_seed_first_and_appends_the_rest_classified() {
        let live = vec![
            live("gpt-5", Some("gpt-5"), &[("chatCompletion", "true")]),
            live("emb", Some("text-embedding-3-large"), &[("embeddings", "true")]),
            live("gpt-4.1", Some("gpt-4.1"), &[("chatCompletion", "true")]),
            live("luna-d", Some("gpt-6-luna"), &[]),
            live("new-chat", Some("gpt-5.2"), &[("chatCompletion", "true")]),
        ];
        let configured = vec!["gpt-4.1".to_string(), "gpt-5".to_string(), "never-deployed".to_string()];
        let decision = vec!["luna-d".to_string()];

        let (off, missing) = merge_live_catalog(&configured, &decision, &live, false);
        assert_eq!(off.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["gpt-4.1", "gpt-5"]);
        assert!(off.iter().all(|m| m.source == Some(CatalogSource::Configured) && m.kind == CatalogKind::Chat));
        assert_eq!(missing, vec!["never-deployed"]);

        let (on, _) = merge_live_catalog(&configured, &decision, &live, true);
        let rows: Vec<(&str, CatalogKind, Option<CatalogSource>)> = on.iter().map(|m| (m.name.as_str(), m.kind, m.source)).collect();
        assert_eq!(
            rows,
            vec![
                ("gpt-4.1", CatalogKind::Chat, Some(CatalogSource::Configured)),
                ("gpt-5", CatalogKind::Chat, Some(CatalogSource::Configured)),
                ("emb", CatalogKind::Embedding, Some(CatalogSource::Discovered)),
                ("new-chat", CatalogKind::Chat, Some(CatalogSource::Discovered)),
            ],
            "a decision deployment is never discovered as chat, and discovered entries follow the seed sorted by name"
        );
        assert_eq!(on[3].display_name.as_deref(), Some("new-chat (gpt-5.2)"));
        assert_eq!(on[0].display_name, None);
        let json = serde_json::to_value(&on[2]).unwrap();
        assert_eq!(json["kind"], "embedding");
        assert_eq!(json["source"], "discovered");
        assert!(serde_json::to_value(&foundry_model("plain")).unwrap().get("source").is_none(), "no source when the provider does not distinguish");
    }

    #[test]
    fn with_no_configured_list_every_live_deployment_is_the_catalog() {
        let live = vec![live("b", None, &[("embeddings", "true")]), live("a", None, &[])];
        let (off, _) = merge_live_catalog(&[], &[], &live, false);
        assert_eq!(off.iter().map(|m| (m.name.as_str(), m.kind, m.source)).collect::<Vec<_>>(), vec![("a", CatalogKind::Chat, None), ("b", CatalogKind::Chat, None)]);
        let (on, _) = merge_live_catalog(&[], &[], &live, true);
        assert_eq!(on.iter().map(|m| (m.name.as_str(), m.kind)).collect::<Vec<_>>(), vec![("a", CatalogKind::Chat), ("b", CatalogKind::Embedding)]);
        assert!(on.iter().all(|m| m.source == Some(CatalogSource::Discovered)));
    }
}
