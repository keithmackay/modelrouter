//! Coverage tests for issue #80 — targeting specific uncovered paths to reach ≥80%.

mod common;

// ══════════════════════════════════════════════════════════════════════════════
// src/hooks/lifecycle.rs — payload builders and fire() behavior
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod lifecycle_coverage {
    use modelrouter::hooks::lifecycle;

    #[test]
    fn request_received_payload() {
        let p = lifecycle::request_received_payload("alice", "gpt-4", 3);
        assert_eq!(p["event"], "on_request_received");
        assert_eq!(p["user_name"], "alice");
        assert_eq!(p["model"], "gpt-4");
        assert_eq!(p["message_count"], 3);
        assert!(p.get("timestamp").is_some());
    }

    #[test]
    fn response_sent_payload() {
        let p = lifecycle::response_sent_payload("bob", "claude", "anthropic/claude", 0.05, 1200);
        assert_eq!(p["event"], "on_response_sent");
        assert_eq!(p["cost_usd"], 0.05);
        assert_eq!(p["latency_ms"], 1200);
    }

    #[test]
    fn budget_exceeded_payload() {
        let p = lifecycle::budget_exceeded_payload("charlie", "gpt-4o", 100.0, 105.0, "monthly");
        assert_eq!(p["event"], "on_budget_exceeded");
        assert_eq!(p["limit_usd"], 100.0);
        assert_eq!(p["spent_usd"], 105.0);
    }

    #[test]
    fn stream_complete_payload() {
        let p = lifecycle::stream_complete_payload("dave", "gemini", 500, 0.001);
        assert_eq!(p["event"], "on_stream_complete");
        assert_eq!(p["approx_tokens"], 500);
    }

    #[test]
    fn error_payload() {
        let p = lifecycle::error_payload("eve", "claude", "rate_limit", "quota exceeded");
        assert_eq!(p["event"], "on_error");
        assert_eq!(p["error_type"], "rate_limit");
        assert_eq!(p["message"], "quota exceeded");
    }

    #[test]
    fn user_disabled_payload() {
        let p = lifecycle::user_disabled_payload("frank", "admin");
        assert_eq!(p["event"], "on_user_disabled");
        assert_eq!(p["disabled_by"], "admin");
    }

    #[tokio::test]
    async fn fire_returns_immediately() {
        use modelrouter::config::schema::LifecycleHookConfig;
        use std::time::Instant;

        let hook = LifecycleHookConfig {
            name: "fast".to_string(),
            event: "test".to_string(),
            exec: "/bin/true".to_string(),
            timeout_secs: 2,
        };

        let start = Instant::now();
        let handle = lifecycle::fire(&hook, serde_json::json!({}));
        assert!(start.elapsed().as_millis() < 50, "fire() must be non-blocking");
        let _ = handle.await;
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// src/providers/bedrock.rs (feature-gated)
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(all(test, feature = "bedrock"))]
mod bedrock_coverage {
    use modelrouter::providers::bedrock::{
        build_converse_messages, build_inference_config, build_system_prompt,
    };

    #[test]
    fn build_converse_messages_filters_system() {
        let msgs = vec![
            serde_json::json!({"role": "system", "content": "Be helpful"}),
            serde_json::json!({"role": "user", "content": "Hi"}),
        ];
        let result = build_converse_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["content"][0]["text"], "Hi");
    }

    #[test]
    fn build_system_prompt_extracts_system() {
        let msgs = vec![
            serde_json::json!({"role": "system", "content": "Be concise"}),
            serde_json::json!({"role": "user", "content": "Hi"}),
        ];
        let result = build_system_prompt(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["text"], "Be concise");
    }

    #[test]
    fn inference_config_builder() {
        let cfg = build_inference_config(Some(0.7), Some(1024));
        assert_eq!(cfg["temperature"], 0.7);
        assert_eq!(cfg["maxTokens"], 1024);

        let cfg2 = build_inference_config(None, None);
        assert_eq!(cfg2.as_object().unwrap().len(), 0);
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// src/providers/vertex/adapter.rs — URL building
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod vertex_adapter_coverage {
    use modelrouter::providers::vertex::adapter::{build_endpoint_url, build_predict_url};
    use modelrouter::providers::vertex::dispatch::Publisher;

    #[test]
    fn build_endpoint_url_google_streaming() {
        let url = build_endpoint_url("proj", "us-central1", Publisher::Google, "gemini-2.0-flash", true);
        assert!(url.contains("us-central1-aiplatform.googleapis.com"));
        assert!(url.contains(":streamGenerateContent"));
        assert!(url.contains("?alt=sse"));
    }

    #[test]
    fn build_endpoint_url_google_non_streaming() {
        let url = build_endpoint_url("proj", "us-west1", Publisher::Google, "gemini-pro", false);
        assert!(url.contains("us-west1-aiplatform.googleapis.com"));
        assert!(url.contains(":generateContent"));
        assert!(!url.contains("alt=sse"));
    }

    #[test]
    fn build_endpoint_url_anthropic_streaming() {
        let url = build_endpoint_url("p", "us-east5", Publisher::Anthropic, "claude-3-5-sonnet-v2@20241022", true);
        assert!(url.contains("us-east5-aiplatform.googleapis.com"));
        assert!(url.contains("/publishers/anthropic/"));
        assert!(url.contains(":streamRawPredict"));
    }

    #[test]
    fn build_endpoint_url_anthropic_non_streaming() {
        let url = build_endpoint_url("p", "global", Publisher::Anthropic, "claude-opus-4@20250514", false);
        assert!(url.contains("aiplatform.googleapis.com"));
        assert!(!url.contains("global-aiplatform"));
        assert!(url.contains(":rawPredict"));
    }

    #[test]
    fn build_endpoint_url_maas() {
        let url = build_endpoint_url("p", "us-central1", Publisher::Maas, "publishers/mistralai/models/mistral-large", false);
        assert!(url.contains("/endpoints/openapi/chat/completions"));
    }

    #[test]
    fn predict_url_builder() {
        let url = build_predict_url("p", "global", "text-embedding-005");
        assert!(url.contains("aiplatform.googleapis.com"));
        assert!(url.contains(":predict"));

        let url2 = build_predict_url("p", "asia-southeast1", "text-multilingual-embedding-002");
        assert!(url2.contains("asia-southeast1-aiplatform.googleapis.com"));
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// src/api/admin/cache.rs — cost_row_key helper
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod cache_admin_coverage {
    use modelrouter::api::admin::cache::cost_row_key;

    #[test]
    fn cost_row_key_formats_consistently() {
        let key1 = cost_row_key(42, "gpt-4", Some("proj-a"), Some(10));
        assert_eq!(key1, "42|gpt-4|proj-a|10");

        let key2 = cost_row_key(42, "gpt-4", None, None);
        assert_eq!(key2, "42|gpt-4||");
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// src/api/admin/aliases.rs — cycle detection and sanitization
// ══════════════════════════════════════════════════════════════════════════════

// The cycle detection and URL sanitization helpers are already tested in the
// inline #[cfg(test)] mod at the bottom of aliases.rs — those tests run as
// part of `cargo test` and contribute to coverage.

// ══════════════════════════════════════════════════════════════════════════════
// src/api/admin/models.rs — HTML escape helper
// ══════════════════════════════════════════════════════════════════════════════

// models_admin_coverage: The `he` function is pub(crate), so it's covered by internal tests.
