//! Vertex AI text-embedding adapter.
//!
//! Ported from Athena's own Vertex embedding client
//! (`thesis-validator/src/tools/embedding.ts`, `embedWithVertexAI` /
//! `embedBatchWithVertexAI`), which had been reaching Vertex directly through
//! `@google-cloud/aiplatform`. Moving it behind the gateway is what lets the
//! application hold no credential at all: on the GCP sandbox the only usable
//! identity is the VM's service account via ADC, and modelrouter is the single
//! component allowed to use it. It also brings embeddings under the same
//! budget, cache, failure-record and cost-ledger machinery as every other call.
//!
//! Kept deliberately identical to the TypeScript it replaces: the same
//! `{content, task_type}` instances, the same `outputDimensionality` parameter,
//! the same five-instances-per-request cap, the same `text-embedding-005`
//! defaults. Anything that differed would change retrieval behaviour silently.

use anyhow::Context;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::embedding::{EmbeddingAdapter, EmbeddingRequest, EmbeddingResult};
use crate::providers::vertex::adapter::build_predict_url;
use crate::providers::vertex::auth::{GoogleCloudAuthProvider, TokenProvider};

/// Vertex rejects a `:predict` call carrying more than five instances. Athena's
/// client encodes the same limit as `batchSize: 5`
/// (`thesis-validator/src/tools/embedding.ts`). A caller embedding a page of
/// evidence sends far more than five, so the adapter splits rather than letting
/// the whole batch fail.
const MAX_INSTANCES_PER_REQUEST: usize = 5;

/// Which region to embed in.
///
/// The chat models on this deployment run on `locations/global`; embeddings
/// cannot — `global` exposes no embedding endpoint. Rather than quietly
/// substituting a region that happens to work (a hidden default is precisely
/// the failure this gateway exists to prevent), refuse and name the field the
/// operator has to set. Mirrors Athena's separate `EMBEDDING_REGION`.
pub fn resolve_embedding_region(config: &ProviderConfig) -> anyhow::Result<String> {
    let region = config
        .embedding_region
        .as_deref()
        .or(config.region.as_deref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Vertex embeddings need a region: set `embedding_region` (or `region`) \
                 under [providers.vertex]. Vertex serves text-embedding-* regionally, \
                 e.g. embedding_region = \"us-central1\"."
            )
        })?;
    if region == "global" {
        anyhow::bail!(
            "Vertex serves no embedding endpoint on `global`, so the chat region cannot be \
             reused. Set `embedding_region` under [providers.vertex] to a regional location, \
             e.g. embedding_region = \"us-central1\"."
        );
    }
    Ok(region.to_string())
}

/// Split the caller's inputs into request-sized chunks, preserving order.
pub fn split_into_batches(inputs: &[String]) -> Vec<Vec<String>> {
    inputs
        .chunks(MAX_INSTANCES_PER_REQUEST)
        .map(<[String]>::to_vec)
        .collect()
}

/// Build one `:predict` body. `inputs` must already be batch-sized.
pub fn build_request_body_for(
    inputs: &[String],
    dimensions: Option<u32>,
    task_type: Option<&str>,
) -> serde_json::Value {
    let instances: Vec<serde_json::Value> = inputs
        .iter()
        .map(|text| {
            let mut instance = serde_json::json!({ "content": text });
            // Omitted rather than defaulted when unconfigured: Vertex's own
            // default is not the one Athena uses, and a mismatched task type
            // does not fail — it just retrieves worse.
            if let Some(t) = task_type {
                instance["task_type"] = serde_json::json!(t);
            }
            instance
        })
        .collect();

    let mut body = serde_json::json!({ "instances": instances });
    if let Some(dims) = dimensions {
        body["parameters"] = serde_json::json!({ "outputDimensionality": dims });
    }
    body
}

/// Build a `:predict` body from a whole request. Callers with more than
/// `MAX_INSTANCES_PER_REQUEST` inputs must use `split_into_batches` first.
pub fn build_request_body(req: &EmbeddingRequest, task_type: Option<&str>) -> serde_json::Value {
    build_request_body_for(&req.input, req.dimensions, task_type)
}

/// Parse one `:predict` response.
///
/// Athena's client noted that "Vertex predict responses carry no authoritative
/// token count here" — true of the Node client's decoded value, but the raw
/// REST response does carry `statistics.token_count` per prediction, so it is
/// read when present rather than estimated.
pub fn parse_response(v: serde_json::Value) -> anyhow::Result<EmbeddingResult> {
    let predictions = v["predictions"]
        .as_array()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| anyhow::anyhow!("No predictions returned from Vertex AI"))?;

    let mut embeddings = Vec::with_capacity(predictions.len());
    let mut prompt_tokens: u32 = 0;
    for prediction in predictions {
        let values = prediction["embeddings"]["values"].as_array().ok_or_else(|| {
            anyhow::anyhow!("Invalid embedding response structure from Vertex AI")
        })?;
        embeddings.push(
            values
                .iter()
                .map(|n| n.as_f64().unwrap_or(0.0) as f32)
                .collect::<Vec<f32>>(),
        );
        prompt_tokens += prediction["embeddings"]["statistics"]["token_count"]
            .as_u64()
            .unwrap_or(0) as u32;
    }

    Ok(EmbeddingResult {
        embeddings,
        prompt_tokens,
    })
}

pub struct VertexEmbeddingAdapter {
    project: String,
    region: String,
    task_type: Option<String>,
    token_provider: Arc<dyn TokenProvider>,
    client: reqwest::Client,
}

impl VertexEmbeddingAdapter {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let project = config
            .project
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Vertex embeddings need `project` under [providers.vertex]"))?;
        let region = resolve_embedding_region(config)?;
        let token_provider = Arc::new(GoogleCloudAuthProvider::new(
            config.credentials_path.as_deref(),
        )?);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build reqwest client for Vertex embeddings")?;
        Ok(Self {
            project,
            region,
            task_type: config.embedding_task_type.clone(),
            token_provider,
            client,
        })
    }

    async fn predict(&self, body: serde_json::Value, model: &str) -> anyhow::Result<EmbeddingResult> {
        let url = build_predict_url(&self.project, &self.region, model);
        let token = self.token_provider.token().await?;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send embedding request to Vertex AI")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Embedding provider returned {}: {}", status, text);
        }

        parse_response(
            resp.json()
                .await
                .context("Failed to parse Vertex embedding response")?,
        )
    }
}

#[async_trait::async_trait]
impl EmbeddingAdapter for VertexEmbeddingAdapter {
    async fn embed(&self, req: &EmbeddingRequest) -> anyhow::Result<EmbeddingResult> {
        // Strip any publisher prefix the router carried through
        // ("google/text-embedding-005"); the publisher is already in the URL.
        let model = req
            .model
            .rsplit_once('/')
            .map_or(req.model.as_str(), |(_, bare)| bare);

        let mut embeddings = Vec::with_capacity(req.input.len());
        let mut prompt_tokens: u32 = 0;
        // Sequential rather than concurrent: batches only appear for large
        // inputs, and firing them all at once is the quickest way to trip
        // Vertex's per-project embedding quota mid-run.
        for batch in split_into_batches(&req.input) {
            let body = build_request_body_for(&batch, req.dimensions, self.task_type.as_deref());
            let part = self.predict(body, model).await?;
            embeddings.extend(part.embeddings);
            prompt_tokens += part.prompt_tokens;
        }

        let result = EmbeddingResult {
            embeddings,
            prompt_tokens,
        };
        // Vertex honours `outputDimensionality`, but verifying costs nothing and
        // a wrong-width vector is worse than a failed call: it is stored, and it
        // silently corrupts every similarity comparison made against it after.
        result.verify_dimensions(req.dimensions)?;
        Ok(result)
    }
}
