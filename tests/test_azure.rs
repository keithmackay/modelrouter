use modelrouter::config::schema::ProviderConfig;
use modelrouter::providers::azure_openai::AzureOpenAIAdapter;

#[test]
fn azure_adapter_builds_correct_url() {
    let config = ProviderConfig {
        api_key: "my-azure-key".to_string(),
        api_base: Some("https://my-resource.openai.azure.com/openai/deployments/my-gpt4".to_string()),
        api_version: Some("2024-02-01".to_string()),
        timeout_secs: 60,
    };
    let adapter = AzureOpenAIAdapter::new(&config);
    assert_eq!(
        adapter.chat_url(),
        "https://my-resource.openai.azure.com/openai/deployments/my-gpt4/chat/completions?api-version=2024-02-01"
    );
}

#[test]
fn azure_adapter_defaults_api_version() {
    let config = ProviderConfig {
        api_key: "key".to_string(),
        api_base: Some("https://resource.openai.azure.com/openai/deployments/gpt4".to_string()),
        api_version: None,
        timeout_secs: 60,
    };
    let adapter = AzureOpenAIAdapter::new(&config);
    assert!(adapter.chat_url().contains("api-version=2024-02-01"));
}

#[test]
fn azure_adapter_uses_api_base_fallback() {
    let config = ProviderConfig {
        api_key: "key".to_string(),
        api_base: None,
        api_version: None,
        timeout_secs: 60,
    };
    let adapter = AzureOpenAIAdapter::new(&config);
    let url = adapter.chat_url();
    assert!(url.contains("api-version="));
}
