use modelrouter::config::schema::ProviderConfig;

#[test]
fn provider_config_has_project_and_credentials_path() {
    let config = ProviderConfig {
        api_key: String::new(),
        api_base: None,
        timeout_secs: 60,
        api_version: None,
        region: Some("us-east5".into()),
        project: Some("my-proj".into()),
        credentials_path: Some("/secrets/sa.json".into()),
    };
    assert_eq!(config.project.as_deref(), Some("my-proj"));
    assert_eq!(config.credentials_path.as_deref(), Some("/secrets/sa.json"));
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
    fn translate_sse_message_stop_emits_done() {
        let line = r#"data: {"type":"message_stop"}"#;
        let out = translate_sse_line(line).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("[DONE]"));
    }
}
