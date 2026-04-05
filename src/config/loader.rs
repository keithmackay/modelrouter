use anyhow::Result;
use crate::config::Settings;

pub struct SettingsLoader {
    path: String,
}

impl SettingsLoader {
    pub fn new(path: String) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Settings> {
        crate::config::load_from_path(&self.path)
    }
}
