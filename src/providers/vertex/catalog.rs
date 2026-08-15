//! Vertex publisher-models catalog discovery (issue #32).
//!
//! Lists what the connected GCP project can actually route to by querying the
//! Model Garden publisher catalogs for the publishers this adapter serves
//! (google, anthropic), using the adapter's existing google-cloud-auth
//! credentials — no new auth surface. Host rule matches the request path:
//! `global` uses the un-prefixed hostname, regions get a `{region}-` prefix.

use crate::providers::catalog::{CatalogModel, ProviderCatalog};
use crate::providers::vertex::adapter::VertexAdapter;
use anyhow::Context;
use async_trait::async_trait;

/// Publishers the Vertex adapter can dispatch to (see `dispatch.rs`).
const PUBLISHERS: &[&str] = &["google", "anthropic"];

/// Defensive cap on catalog pagination — the publisher catalogs are a few
/// hundred entries; anything past this is a paging bug, not data.
const MAX_PAGES: usize = 10;

/// Build the publisher-models catalog URL. `base` overrides scheme+host for
/// tests; production passes None and derives the host from the region exactly
/// like `build_endpoint_url`.
pub fn catalog_url(base: Option<&str>, region: &str, publisher: &str, page_token: Option<&str>) -> String {
    let origin = match base {
        Some(b) => b.trim_end_matches('/').to_string(),
        None if region == "global" => "https://aiplatform.googleapis.com".to_string(),
        None => format!("https://{region}-aiplatform.googleapis.com"),
    };
    let mut url = format!("{origin}/v1beta1/publishers/{publisher}/models");
    if let Some(token) = page_token {
        url.push_str(&format!("?pageToken={token}"));
    }
    url
}

/// Strip "publishers/{p}/models/" down to the bare model id.
fn model_id_from_resource_name(name: &str) -> Option<&str> {
    name.rsplit('/').next().filter(|s| !s.is_empty())
}

#[async_trait]
impl ProviderCatalog for VertexAdapter {
    async fn list_models(&self) -> anyhow::Result<Vec<CatalogModel>> {
        let token = self.token_provider().token().await?;
        let mut models = Vec::new();

        for publisher in PUBLISHERS {
            let mut page_token: Option<String> = None;
            for _ in 0..MAX_PAGES {
                let url = catalog_url(
                    self.catalog_base(),
                    self.region(),
                    publisher,
                    page_token.as_deref(),
                );
                let resp = self
                    .http_client()
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .with_context(|| format!("catalog request failed for publisher {publisher}"))?;
                let status = resp.status();
                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("catalog for publisher {publisher} returned {status}: {text}");
                }
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .with_context(|| format!("catalog response for {publisher} is not JSON"))?;

                for m in body["publisherModels"].as_array().into_iter().flatten() {
                    let Some(id) = m["name"].as_str().and_then(model_id_from_resource_name) else {
                        continue;
                    };
                    models.push(CatalogModel {
                        provider: "vertex".to_string(),
                        name: format!("{publisher}/{id}"),
                        display_name: m["displayName"].as_str().map(str::to_string),
                    });
                }

                page_token = body["nextPageToken"].as_str().map(str::to_string);
                if page_token.is_none() {
                    break;
                }
            }
        }
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::vertex::auth::StaticTokenProvider;
    use axum::{extract::Query, routing::get, Json, Router};
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn url_host_rule_matches_request_path() {
        assert_eq!(
            catalog_url(None, "global", "anthropic", None),
            "https://aiplatform.googleapis.com/v1beta1/publishers/anthropic/models"
        );
        assert_eq!(
            catalog_url(None, "us-central1", "google", None),
            "https://us-central1-aiplatform.googleapis.com/v1beta1/publishers/google/models"
        );
        assert_eq!(
            catalog_url(Some("http://127.0.0.1:9/"), "global", "google", Some("t2")),
            "http://127.0.0.1:9/v1beta1/publishers/google/models?pageToken=t2"
        );
    }

    async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn lists_both_publishers_and_paginates() {
        let router = Router::new()
            .route(
                "/v1beta1/publishers/google/models",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    if q.get("pageToken").map(String::as_str) == Some("p2") {
                        Json(serde_json::json!({
                            "publisherModels": [
                                {"name": "publishers/google/models/text-embedding-005"}
                            ]
                        }))
                    } else {
                        Json(serde_json::json!({
                            "publisherModels": [
                                {"name": "publishers/google/models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro"}
                            ],
                            "nextPageToken": "p2"
                        }))
                    }
                }),
            )
            .route(
                "/v1beta1/publishers/anthropic/models",
                get(|| async {
                    Json(serde_json::json!({
                        "publisherModels": [
                            {"name": "publishers/anthropic/models/claude-sonnet-4-5@20250929"}
                        ]
                    }))
                }),
            );
        let (base, _server) = serve(router).await;

        let adapter = VertexAdapter::with_token_provider(
            "proj".into(),
            "global".into(),
            Arc::new(StaticTokenProvider::new("tok".into())),
            5,
        )
        .unwrap()
        .with_catalog_base(base);

        let models = adapter.list_models().await.unwrap();
        let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "google/gemini-2.5-pro",
                "google/text-embedding-005",
                "anthropic/claude-sonnet-4-5@20250929",
            ]
        );
        assert_eq!(models[0].display_name.as_deref(), Some("Gemini 2.5 Pro"));
        assert!(models.iter().all(|m| m.provider == "vertex"));
    }

    #[tokio::test]
    async fn catalog_error_names_the_publisher() {
        let router = Router::new().route(
            "/v1beta1/publishers/google/models",
            get(|| async { (axum::http::StatusCode::FORBIDDEN, "nope") }),
        );
        let (base, _server) = serve(router).await;
        let adapter = VertexAdapter::with_token_provider(
            "proj".into(),
            "global".into(),
            Arc::new(StaticTokenProvider::new("tok".into())),
            5,
        )
        .unwrap()
        .with_catalog_base(base);

        let err = adapter.list_models().await.unwrap_err().to_string();
        assert!(err.contains("publisher google"), "{err}");
        assert!(err.contains("403"), "{err}");
    }
}
