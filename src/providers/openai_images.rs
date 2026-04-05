use serde_json::Value;
use crate::config::schema::ProviderConfig;

pub struct OpenAIImageAdapter {
    api_key: String,
    api_base: String,
    client: reqwest::Client,
}

impl OpenAIImageAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        Self {
            api_key: config.api_key.clone(),
            api_base: config.api_base.clone()
                .unwrap_or_else(|| "https://api.openai.com".to_string()),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(config.timeout_secs))
                .build()
                .unwrap(),
        }
    }

    pub async fn generate_image(&self, body: &Value) -> anyhow::Result<Value> {
        let url = format!("{}/v1/images/generations", self.api_base);
        let resp = self.client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("image generation failed with status {status}: {text}");
        }
        let result: Value = resp.json().await?;
        Ok(result)
    }
}
