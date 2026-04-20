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
        let (pub_, id) = parse_model_id("google/gemini-2.5-pro").unwrap();
        assert_eq!(pub_, Publisher::Google);
        assert_eq!(id, "gemini-2.5-pro");
    }

    #[test]
    fn anthropic_prefix_with_version_parses() {
        let (pub_, id) = parse_model_id("anthropic/claude-sonnet-4-6@20250514").unwrap();
        assert_eq!(pub_, Publisher::Anthropic);
        assert_eq!(id, "claude-sonnet-4-6@20250514");
    }

    #[test]
    fn bare_gemini_name_defaults_to_google() {
        let (pub_, id) = parse_model_id("gemini-2.5-flash").unwrap();
        assert_eq!(pub_, Publisher::Google);
        assert_eq!(id, "gemini-2.5-flash");
    }

    #[test]
    fn bare_claude_name_defaults_to_anthropic() {
        let (pub_, id) = parse_model_id("claude-opus-4-5@20250101").unwrap();
        assert_eq!(pub_, Publisher::Anthropic);
        assert_eq!(id, "claude-opus-4-5@20250101");
    }

    #[test]
    fn unknown_prefix_errors() {
        let err = parse_model_id("cohere/command-r").unwrap_err().to_string();
        assert!(err.contains("Unsupported Vertex publisher"), "got: {err}");
    }
}
