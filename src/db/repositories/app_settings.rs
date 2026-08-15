use async_trait::async_trait;

/// GUI-managed runtime settings (issue #4): JSON blobs keyed by section name
/// (e.g. `storage`). A stored row overrides the config-file value for that
/// section; no row means config.toml / defaults apply.
#[async_trait]
pub trait AppSettingsRepository: Send + Sync {
    async fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>>;
    async fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()>;
}
