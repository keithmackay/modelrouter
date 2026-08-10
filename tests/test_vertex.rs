use modelrouter::config::schema::ProviderConfig;

#[test]
fn provider_config_has_project_and_credentials_path() {
    let config = ProviderConfig {
        region: Some("us-east5".into()),
        project: Some("my-proj".into()),
        credentials_path: Some("/secrets/sa.json".into()),
        ..Default::default()
    };
    assert_eq!(config.project.as_deref(), Some("my-proj"));
    assert_eq!(config.credentials_path.as_deref(), Some("/secrets/sa.json"));
}

/// `ProviderConfig::default()` must agree with what serde fills in for a
/// `[providers.x]` table that sets nothing — otherwise a config written by hand
/// and one built in code diverge, and the tests stop describing production.
#[test]
fn provider_config_default_matches_serde_defaults() {
    let from_toml: ProviderConfig = toml::from_str("").unwrap();
    let from_default = ProviderConfig::default();
    assert_eq!(from_default.timeout_secs, from_toml.timeout_secs);
    assert_eq!(from_default.api_key, from_toml.api_key);
    assert_eq!(from_default.embedding_region, from_toml.embedding_region);
    assert_eq!(from_default.embedding_task_type, from_toml.embedding_task_type);
}

#[cfg(feature = "vertex")]
mod dispatch_tests {
    use modelrouter::providers::vertex::dispatch::{parse_model_id, Publisher};

    #[test]
    fn gemini_prefix_parses_to_google() {
        let (publisher, id) = parse_model_id("google/gemini-2.5-pro").unwrap();
        assert_eq!(publisher, Publisher::Google);
        assert_eq!(id, "gemini-2.5-pro");
    }

    #[test]
    fn anthropic_prefix_with_version_parses() {
        let (publisher, id) = parse_model_id("anthropic/claude-sonnet-4-6@20250514").unwrap();
        assert_eq!(publisher, Publisher::Anthropic);
        assert_eq!(id, "claude-sonnet-4-6@20250514");
    }

    #[test]
    fn bare_gemini_name_defaults_to_google() {
        let (publisher, id) = parse_model_id("gemini-2.5-flash").unwrap();
        assert_eq!(publisher, Publisher::Google);
        assert_eq!(id, "gemini-2.5-flash");
    }

    #[test]
    fn bare_claude_name_defaults_to_anthropic() {
        let (publisher, id) = parse_model_id("claude-opus-4-5@20250101").unwrap();
        assert_eq!(publisher, Publisher::Anthropic);
        assert_eq!(id, "claude-opus-4-5@20250101");
    }

    #[test]
    fn unknown_prefix_errors() {
        let err = parse_model_id("cohere/command-r").unwrap_err().to_string();
        assert!(err.contains("Unsupported Vertex publisher"), "got: {err}");
    }
}

#[cfg(feature = "vertex")]
mod gemini_tests {
    use modelrouter::providers::adapter::NormalizedRequest;
    use modelrouter::providers::vertex::gemini::{
        translate_request, parse_response, translate_sse_line,
    };
    use serde_json::json;

    fn req(messages: serde_json::Value) -> NormalizedRequest {
        NormalizedRequest {
            model: "gemini-2.5-pro".into(),
            messages: messages.as_array().unwrap().clone(),
            stream: false,
            temperature: Some(0.7),
            max_tokens: Some(1024),
            extra_params: json!({}),
        }
    }

    #[test]
    fn translate_request_extracts_system_instruction() {
        let r = req(json!([
            {"role": "system", "content": "Be helpful."},
            {"role": "user", "content": "Hi"}
        ]));
        let body = translate_request(&r);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be helpful.");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "Hi");
    }

    #[test]
    fn translate_request_maps_assistant_to_model_role() {
        let r = req(json!([
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello!"}
        ]));
        let body = translate_request(&r);
        assert_eq!(body["contents"][1]["role"], "model");
    }

    #[test]
    fn translate_request_emits_generation_config() {
        let r = req(json!([{"role": "user", "content": "Hi"}]));
        let body = translate_request(&r);
        assert_eq!(body["generationConfig"]["temperature"], 0.7);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
    }

    #[test]
    fn parse_response_extracts_text_and_usage() {
        let resp = json!({
            "candidates": [{
                "content": {"parts": [{"text": "Hi there!"}], "role": "model"},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 4,
                "totalTokenCount": 16
            }
        });
        let cr = parse_response(resp).unwrap();
        assert_eq!(cr.content, "Hi there!");
        assert_eq!(cr.prompt_tokens, 12);
        assert_eq!(cr.completion_tokens, 4);
        assert_eq!(cr.finish_reason, "stop");
    }

    #[test]
    fn parse_response_maps_max_tokens_finish_reason() {
        let resp = json!({
            "candidates": [{
                "content": {"parts": [{"text": "..."}]},
                "finishReason": "MAX_TOKENS"
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
        });
        let cr = parse_response(resp).unwrap();
        assert_eq!(cr.finish_reason, "length");
    }

    #[test]
    fn parse_response_maps_safety_finish_reason() {
        let resp = json!({
            "candidates": [{
                "content": {"parts": [{"text": ""}]},
                "finishReason": "SAFETY"
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 0, "totalTokenCount": 1}
        });
        assert_eq!(parse_response(resp).unwrap().finish_reason, "content_filter");
    }

    #[test]
    fn translate_sse_line_emits_openai_chunk() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hi"}]}}]}"#;
        let out = translate_sse_line(line).unwrap();
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains(r#""delta":{"content":"Hi"}"#));
        assert!(out_str.contains(r#""object":"chat.completion.chunk""#));
    }

    #[test]
    fn translate_sse_line_skips_empty_lines() {
        assert!(translate_sse_line("").is_none());
        assert!(translate_sse_line("\n").is_none());
        assert!(translate_sse_line("event: ping").is_none());
    }
}

#[cfg(feature = "vertex")]
mod claude_tests {
    use modelrouter::providers::adapter::NormalizedRequest;
    use modelrouter::providers::vertex::claude::{
        translate_request, parse_response, translate_sse_line,
    };
    use serde_json::json;

    fn req(messages: serde_json::Value) -> NormalizedRequest {
        NormalizedRequest {
            model: "claude-sonnet-4-6@20250514".into(),
            messages: messages.as_array().unwrap().clone(),
            stream: false,
            temperature: Some(0.5),
            max_tokens: Some(2048),
            extra_params: json!({}),
        }
    }

    #[test]
    fn translate_request_includes_anthropic_version_and_omits_model() {
        let r = req(json!([{"role": "user", "content": "Hi"}]));
        let body = translate_request(&r);
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert!(body.get("model").is_none(), "model must live in URL, not body");
        assert_eq!(body["max_tokens"], 2048);
    }

    #[test]
    fn translate_request_extracts_system_text() {
        let r = req(json!([
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": "Hi"}
        ]));
        let body = translate_request(&r);
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn translate_request_defaults_max_tokens_when_missing() {
        let mut r = req(json!([{"role": "user", "content": "Hi"}]));
        r.max_tokens = None;
        let body = translate_request(&r);
        assert!(body["max_tokens"].as_u64().unwrap() > 0, "Anthropic requires max_tokens");
    }

    #[test]
    fn parse_response_extracts_text_and_usage() {
        let resp = json!({
            "content": [{"type": "text", "text": "Hello!"}],
            "usage": {"input_tokens": 9, "output_tokens": 2},
            "stop_reason": "end_turn"
        });
        let cr = parse_response(resp).unwrap();
        assert_eq!(cr.content, "Hello!");
        assert_eq!(cr.prompt_tokens, 9);
        assert_eq!(cr.completion_tokens, 2);
        assert_eq!(cr.finish_reason, "end_turn");
    }

    #[test]
    fn translate_sse_content_delta_becomes_openai_chunk() {
        let line = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        let out = translate_sse_line(line).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains(r#""delta":{"content":"Hi"}"#));
    }

    #[test]
    fn translate_sse_message_delta_emits_done() {
        let line = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#;
        let out = translate_sse_line(line).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("[DONE]"));
    }

    #[test]
    fn translate_sse_message_stop_returns_none() {
        let line = r#"data: {"type":"message_stop"}"#;
        assert!(translate_sse_line(line).is_none(),
            "message_stop is a trailing no-op; finalization happens on message_delta");
    }
}

#[cfg(feature = "vertex")]
mod auth_tests {
    use modelrouter::providers::vertex::auth::{StaticTokenProvider, TokenProvider};

    #[tokio::test]
    async fn static_provider_returns_configured_token() {
        let p = StaticTokenProvider::new("ya29.abc".into());
        assert_eq!(p.token().await.unwrap(), "ya29.abc");
    }
}

#[cfg(feature = "vertex")]
mod adapter_tests {
    use modelrouter::providers::vertex::adapter::build_endpoint_url;
    use modelrouter::providers::vertex::dispatch::Publisher;

    #[test]
    fn gemini_non_streaming_url() {
        let url = build_endpoint_url("my-proj", "us-central1", Publisher::Google, "gemini-2.5-pro", false);
        assert_eq!(url, "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent");
    }

    #[test]
    fn gemini_streaming_url_uses_sse_alt() {
        let url = build_endpoint_url("p", "us-central1", Publisher::Google, "gemini-2.5-flash", true);
        assert!(url.ends_with(":streamGenerateContent?alt=sse"));
    }

    #[test]
    fn anthropic_non_streaming_url_with_version_pin() {
        let url = build_endpoint_url("p", "us-east5", Publisher::Anthropic, "claude-sonnet-4-6@20250514", false);
        assert!(url.ends_with("/publishers/anthropic/models/claude-sonnet-4-6@20250514:rawPredict"));
    }

    #[test]
    fn anthropic_streaming_url() {
        let url = build_endpoint_url("p", "us-east5", Publisher::Anthropic, "claude-opus-4-5@20250101", true);
        assert!(url.ends_with(":streamRawPredict"));
    }

    #[test]
    fn global_region_uses_unprefixed_hostname() {
        let url = build_endpoint_url("p", "global", Publisher::Anthropic, "claude-sonnet-4-5@20250929", false);
        assert!(
            url.starts_with("https://aiplatform.googleapis.com/"),
            "global must use un-prefixed host, got: {url}"
        );
        assert!(
            url.contains("/locations/global/"),
            "path must still contain locations/global, got: {url}"
        );
    }

    #[test]
    fn global_region_gemini_streaming() {
        let url = build_endpoint_url("p", "global", Publisher::Google, "gemini-2.5-pro", true);
        assert!(url.starts_with("https://aiplatform.googleapis.com/"));
        assert!(url.ends_with(":streamGenerateContent?alt=sse"));
    }
}

/// Vertex text-embedding adapter.
///
/// Athena on the GCP sandbox has no provider key at all and must embed through
/// Vertex under the VM's service account. Before this adapter existed,
/// `embed_registry::get()` built an `OpenAIEmbeddingAdapter` for every provider
/// name, so there was no way to route an embedding to Vertex whatever the
/// config said.
#[cfg(feature = "vertex")]
mod embed_tests {
    use modelrouter::config::schema::ProviderConfig;
    use modelrouter::providers::embedding::EmbeddingRequest;
    use modelrouter::providers::vertex::adapter::build_predict_url;
    use modelrouter::providers::vertex::embed::{
        build_request_body, parse_response, resolve_embedding_region, split_into_batches,
    };
    use serde_json::json;

    fn config(region: Option<&str>, embedding_region: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            project: Some("my-proj".into()),
            region: region.map(Into::into),
            embedding_region: embedding_region.map(Into::into),
            ..Default::default()
        }
    }

    fn req(input: Vec<&str>, dimensions: Option<u32>) -> EmbeddingRequest {
        EmbeddingRequest {
            model: "text-embedding-005".into(),
            input: input.into_iter().map(Into::into).collect(),
            dimensions,
        }
    }

    #[test]
    fn predict_url_is_regional_and_uses_the_google_publisher() {
        assert_eq!(
            build_predict_url("my-proj", "us-central1", "text-embedding-005"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/google/models/text-embedding-005:predict"
        );
    }

    #[test]
    fn embedding_region_overrides_the_chat_region() {
        // The chat models run on `global`; embeddings cannot.
        let r = resolve_embedding_region(&config(Some("global"), Some("us-central1"))).unwrap();
        assert_eq!(r, "us-central1");
    }

    #[test]
    fn embedding_region_falls_back_to_region_when_unset() {
        let r = resolve_embedding_region(&config(Some("us-east5"), None)).unwrap();
        assert_eq!(r, "us-east5");
    }

    /// `locations/global` has no embedding endpoint. Silently substituting a
    /// region would be a hidden default — the exact defect class this round is
    /// about — so the adapter refuses and names the field the operator must set.
    #[test]
    fn global_region_is_refused_and_names_the_field_to_set() {
        let err = resolve_embedding_region(&config(Some("global"), None))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("embedding_region"),
            "error must name the field to set, got: {err}"
        );
        assert!(err.contains("global"), "error must name the bad value, got: {err}");
    }

    #[test]
    fn no_region_at_all_is_refused() {
        assert!(resolve_embedding_region(&config(None, None)).is_err());
    }

    #[test]
    fn request_body_carries_one_instance_per_input() {
        let body = build_request_body(&req(vec!["alpha", "beta"], None), None);
        let instances = body["instances"].as_array().unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0]["content"], "alpha");
        assert_eq!(instances[1]["content"], "beta");
    }

    /// Athena pins EMBEDDING_DIMENSIONS=768 and refuses to truncate or pad, so
    /// the width must be asked for, not hoped for.
    #[test]
    fn request_body_asks_for_the_pinned_width() {
        let body = build_request_body(&req(vec!["alpha"], Some(768)), None);
        assert_eq!(body["parameters"]["outputDimensionality"], 768);
    }

    #[test]
    fn request_body_omits_width_when_the_caller_did_not_pin_one() {
        let body = build_request_body(&req(vec!["alpha"], None), None);
        assert!(
            body.get("parameters").is_none()
                || body["parameters"].get("outputDimensionality").is_none(),
            "must not invent a width: {body}"
        );
    }

    /// Vertex's own default task type is not the one Athena uses, and mixing
    /// query- and document-typed vectors in one store degrades every later
    /// similarity comparison. The value is configured, never assumed.
    #[test]
    fn request_body_carries_the_configured_task_type() {
        let body = build_request_body(&req(vec!["alpha"], None), Some("RETRIEVAL_DOCUMENT"));
        assert_eq!(body["instances"][0]["task_type"], "RETRIEVAL_DOCUMENT");
    }

    #[test]
    fn request_body_omits_task_type_when_unconfigured() {
        let body = build_request_body(&req(vec!["alpha"], None), None);
        assert!(
            body["instances"][0].get("task_type").is_none(),
            "must not invent a task type: {body}"
        );
    }

    /// Vertex's `:predict` accepts at most 5 instances per call — Athena's
    /// client carries the same cap as `batchSize: 5` in
    /// `thesis-validator/src/tools/embedding.ts`. A caller embedding a page of
    /// evidence sends far more than 5, so the adapter must split rather than
    /// hand Vertex an over-long list and fail the whole batch.
    #[test]
    fn inputs_are_split_into_chunks_vertex_will_accept() {
        let inputs: Vec<String> = (0..12).map(|i| format!("chunk-{i}")).collect();
        let batches = split_into_batches(&inputs);
        assert_eq!(batches.len(), 3, "12 inputs at 5 per call");
        assert_eq!(batches[0].len(), 5);
        assert_eq!(batches[1].len(), 5);
        assert_eq!(batches[2].len(), 2);
        let flattened: Vec<&String> = batches.iter().flat_map(|b| b.iter()).collect();
        assert_eq!(flattened.len(), 12, "no input may be dropped");
        assert_eq!(flattened[11], "chunk-11", "order must be preserved");
    }

    #[test]
    fn a_single_input_is_one_batch() {
        assert_eq!(split_into_batches(&["only".to_string()]).len(), 1);
    }

    #[test]
    fn response_parsing_yields_one_vector_per_prediction() {
        let resp = json!({
            "predictions": [
                {"embeddings": {"values": [0.1, 0.2, 0.3], "statistics": {"token_count": 4}}},
                {"embeddings": {"values": [0.4, 0.5, 0.6], "statistics": {"token_count": 6}}}
            ]
        });
        let result = parse_response(resp).unwrap();
        assert_eq!(result.embeddings.len(), 2);
        assert_eq!(result.embeddings[0].len(), 3);
        assert_eq!(result.prompt_tokens, 10, "token counts sum across predictions");
    }

    #[test]
    fn response_without_predictions_is_an_error() {
        assert!(parse_response(json!({})).is_err());
    }

    /// The width guard already in `EmbeddingResult` must apply to Vertex too:
    /// a wrong-width vector is worse than a failed call because it is stored and
    /// silently corrupts every similarity comparison made against it.
    #[test]
    fn a_vector_of_the_wrong_width_is_rejected() {
        let resp = json!({
            "predictions": [
                {"embeddings": {"values": [0.1, 0.2, 0.3], "statistics": {"token_count": 1}}}
            ]
        });
        let result = parse_response(resp).unwrap();
        let err = result.verify_dimensions(Some(768)).unwrap_err().to_string();
        assert!(err.contains("768"), "got: {err}");
    }
}

#[cfg(feature = "vertex")]
mod embed_registry_tests {
    use modelrouter::config::schema::ProviderConfig;
    use modelrouter::providers::embed_registry::EmbeddingRegistry;
    use std::collections::HashMap;

    fn registry(vertex: ProviderConfig) -> EmbeddingRegistry {
        let mut configs = HashMap::new();
        configs.insert("vertex".to_string(), vertex);
        configs.insert("openai".to_string(), ProviderConfig::default());
        EmbeddingRegistry::new(configs)
    }

    /// `#[tokio::test]`, not `#[test]`: building the adapter constructs
    /// `GoogleCloudAuthProvider`, whose token cache registers with the Tokio
    /// reactor. Production always has one — the server is a Tokio app — so this
    /// is a harness requirement, not a constraint on the adapter.
    #[tokio::test]
    async fn vertex_provider_builds_a_vertex_adapter() {
        let r = registry(ProviderConfig {
            project: Some("my-proj".into()),
            region: Some("global".into()),
            embedding_region: Some("us-central1".into()),
            ..Default::default()
        });
        assert!(r.get("vertex").is_ok());
    }

    /// Proves the vertex arm was actually taken: the OpenAI adapter has no
    /// notion of a region and would happily construct here.
    #[test]
    fn vertex_provider_without_a_usable_region_fails_at_construction() {
        let r = registry(ProviderConfig {
            project: Some("my-proj".into()),
            region: Some("global".into()),
            ..Default::default()
        });
        let err = r.get("vertex").err().expect("must not construct").to_string();
        assert!(err.contains("embedding_region"), "got: {err}");
    }

    #[test]
    fn other_providers_still_get_the_openai_compatible_adapter() {
        // Ollama, Azure and LM Studio all depend on this staying the default arm.
        let r = registry(ProviderConfig::default());
        assert!(r.get("openai").is_ok());
    }
}

mod pricing_tests {
    use modelrouter::router::cost::CostCalculator;

    #[test]
    fn gemini_25_pro_has_pricing() {
        let calc = CostCalculator::new();
        let cost = calc.calculate("gemini-2.5-pro", 1_000_000, 1_000_000);
        assert!(cost > 0.0, "gemini-2.5-pro must have non-zero pricing (got {cost})");
    }

    #[test]
    fn claude_sonnet_4_6_vertex_versioned_has_pricing() {
        let calc = CostCalculator::new();
        let cost = calc.calculate("claude-sonnet-4-6@20250514", 1_000_000, 1_000_000);
        assert!(cost > 0.0, "claude-sonnet-4-6@20250514 must have non-zero pricing (got {cost})");
    }
}
