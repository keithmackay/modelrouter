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
