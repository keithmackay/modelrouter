use crate::config::schema::ProviderConfig;
use crate::providers::search::{SearchAdapter, SearchRequest, SearchResponse, SearchResultItem};
use anyhow::Context;

pub struct TavilyAdapter {
    api_key: String,
    api_base: String,
    client: reqwest::Client,
}

impl TavilyAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        let api_base = config
            .api_base
            .clone()
            .unwrap_or_else(|| "https://api.tavily.com".to_string());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to build reqwest client");
        Self {
            api_key: config.api_key.clone(),
            api_base,
            client,
        }
    }
}

#[derive(serde::Deserialize)]
struct TavilySearchResponse {
    results: Vec<TavilyResult>,
}

#[derive(serde::Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    published_date: Option<String>,
}

#[async_trait::async_trait]
impl SearchAdapter for TavilyAdapter {
    async fn search(&self, req: &SearchRequest) -> anyhow::Result<SearchResponse> {
        let url = format!("{}/search", self.api_base);

        let mut body = serde_json::json!({
            "api_key": self.api_key,
            "query": req.query,
        });
        if let Some(max_results) = req.max_results {
            body["max_results"] = serde_json::json!(max_results);
        }

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send search request to Tavily")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Search provider returned {}: {}", status, text);
        }

        let parsed: TavilySearchResponse = resp
            .json()
            .await
            .context("Failed to parse Tavily search response")?;

        Ok(SearchResponse {
            results: parsed
                .results
                .into_iter()
                .map(|r| SearchResultItem {
                    title: r.title,
                    url: r.url,
                    snippet: r.content,
                    score: r.score,
                    published_date: r.published_date,
                })
                .collect(),
            engine: "tavily".to_string(),
        })
    }
}
