// tests/test_bedrock.rs

#[test]
fn provider_config_region_defaults_to_none() {
    let config = modelrouter::config::schema::ProviderConfig {
        api_key: String::new(),
        api_base: None,
        timeout_secs: 60,
        api_version: None,
        region: None,
    };
    assert!(config.region.is_none());
}

#[cfg(feature = "bedrock")]
mod bedrock_translation {
    use modelrouter::providers::bedrock::{
        build_converse_messages, build_system_prompt, build_inference_config,
    };
    use serde_json::json;

    #[test]
    fn user_message_becomes_converse_format() {
        let messages = vec![json!({"role": "user", "content": "Hello"})];
        let result = build_converse_messages(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn system_message_is_excluded_from_converse_messages() {
        let messages = vec![
            json!({"role": "system", "content": "Be helpful."}),
            json!({"role": "user", "content": "Hi"}),
        ];
        let converse = build_converse_messages(&messages);
        assert_eq!(converse.len(), 1);
        assert_eq!(converse[0]["role"], "user");
    }

    #[test]
    fn system_message_appears_in_system_prompt() {
        let messages = vec![
            json!({"role": "system", "content": "Be helpful."}),
            json!({"role": "user", "content": "Hi"}),
        ];
        let system = build_system_prompt(&messages);
        assert_eq!(system, vec![json!({"text": "Be helpful."})]);
    }

    #[test]
    fn no_system_message_returns_empty_system() {
        let messages = vec![json!({"role": "user", "content": "Hello"})];
        assert!(build_system_prompt(&messages).is_empty());
    }

    #[test]
    fn inference_config_with_both_fields() {
        let config = build_inference_config(Some(0.5), Some(100));
        assert_eq!(config["temperature"], 0.5);
        assert_eq!(config["maxTokens"], 100);
    }

    #[test]
    fn inference_config_with_no_fields_is_empty_object() {
        let config = build_inference_config(None, None);
        assert!(config.as_object().unwrap().is_empty());
    }
}
