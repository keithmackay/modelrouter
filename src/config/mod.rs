pub mod schema;
pub mod loader;
pub use schema::Settings;

use anyhow::Result;
use config::{Config, Environment, File};
use std::path::PathBuf;

pub fn load_from_path(path: &str) -> Result<Settings> {
    let settings = Config::builder()
        .add_source(File::with_name(path).required(false))
        .build()?
        .try_deserialize::<Settings>()?;
    Ok(settings)
}

pub fn load(path: Option<PathBuf>) -> Result<Settings> {
    let config_path = path
        .or_else(|| std::env::var("MODELROUTER_CONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".modelrouter/config.toml")
        });

    let settings = Config::builder()
        .add_source(File::from(config_path).required(false))
        .add_source(
            Environment::with_prefix("MODELROUTER")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        )
        .build()?
        .try_deserialize::<Settings>()?;

    Ok(settings)
}
