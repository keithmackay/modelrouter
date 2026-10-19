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

use crate::providers::catalog::{CatalogModel, ProviderCatalog};
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
        });
    }
    models
}

/// The deployments a management-plane `accounts/deployments` list page
/// reports as `provisioningState: Succeeded`, and the page's `nextLink`.
///
/// Only `Succeeded` serves: `Creating`, `Updating`, `Failed`, `Deleting` and
/// `Canceled` deployments all refuse inference calls.
pub fn parse_succeeded_deployments(v: &serde_json::Value) -> (Vec<String>, Option<String>) {
    let names = v["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| {
            d["properties"]["provisioningState"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("succeeded"))
        })
        .filter_map(|d| d["name"].as_str().map(str::trim).filter(|n| !n.is_empty()))
        .map(str::to_string)
        .collect();
    let next = v["nextLink"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    (names, next)
}

/// Page bound for the deployment listing: an account holds far fewer
/// deployments than this many pages, so reaching it means a looping nextLink.
const MAX_DEPLOYMENT_PAGES: usize = 50;

fn foundry_model(name: &str) -> CatalogModel {
    CatalogModel {
        provider: "foundry".to_string(),
        name: name.to_string(),
        display_name: None,
    }
}

impl FoundryAdapter {
    /// Every deployment the account serves now, from the management plane.
    async fn live_deployments(&self, listing: &DeploymentListing) -> anyhow::Result<Vec<String>> {
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
            let (page, next) = parse_succeeded_deployments(&body);
            names.extend(page);
            url = next;
        }
        Ok(names)
    }
}

#[async_trait]
impl ProviderCatalog for FoundryAdapter {
    async fn list_models(&self) -> anyhow::Result<Vec<CatalogModel>> {
        // The account's live deployments, when the account is named: the
        // configured list narrowed to what is deployed and Succeeded, so a
        // picker never offers a name that would fail on its first call.
        if let Some(listing) = self.deployment_listing() {
            let live: std::collections::HashSet<String> =
                self.live_deployments(listing).await?.into_iter().collect();
            if self.configured_catalog().is_empty() {
                let mut all: Vec<&String> = live.iter().collect();
                all.sort();
                return Ok(all.into_iter().map(|n| foundry_model(n)).collect());
            }
            let (kept, missing): (Vec<&String>, Vec<&String>) =
                self.configured_catalog().iter().partition(|n| live.contains(*n));
            if !missing.is_empty() {
                tracing::warn!(
                    missing = ?missing,
                    "foundry: configured deployments not deployed (or not Succeeded) on the account; left out of the catalog"
                );
            }
            return Ok(kept.into_iter().map(|n| foundry_model(n)).collect());
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
}
