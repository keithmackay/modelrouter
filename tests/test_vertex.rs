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
