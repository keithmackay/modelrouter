use anyhow::Result;
use crate::config::Settings;

pub struct SettingsLoader {
    path: String,
}

impl SettingsLoader {
    pub fn new(path: String) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn load(&self) -> Result<Settings> {
        crate::config::load_from_path(&self.path)
    }
}

/// What one hot-reload tick should do.
#[derive(Debug)]
pub enum ReloadOutcome {
    /// The file is byte-for-byte what was last loaded: keep the live settings, log nothing.
    Unchanged,
    /// The file changed and parsed: swap these in.
    Reloaded(Box<Settings>),
    /// Reading or parsing failed. `report` is false when this is the same
    /// error the previous tick already warned about.
    Failed { error: String, report: bool },
}

/// Decides each hot-reload tick from the raw file bytes, so an unchanged
/// config neither re-swaps the settings nor logs, and a broken config is
/// warned about once rather than every tick.
#[derive(Default)]
pub struct ReloadWatcher {
    last_loaded: Option<Vec<u8>>,
    last_error: Option<String>,
}

impl ReloadWatcher {
    /// Start from the bytes the running settings were loaded from.
    pub fn seeded(bytes: Option<Vec<u8>>) -> Self {
        Self { last_loaded: bytes, last_error: None }
    }

    /// `read` is the file's current contents; `load` parses them into settings
    /// and is called only when they differ from the last successful load.
    pub fn tick(
        &mut self,
        read: std::io::Result<Vec<u8>>,
        load: impl FnOnce() -> Result<Settings>,
    ) -> ReloadOutcome {
        let bytes = match read {
            Ok(bytes) => bytes,
            Err(e) => return self.fail(format!("cannot read config: {e}")),
        };
        if self.last_loaded.as_deref() == Some(bytes.as_slice()) {
            self.last_error = None;
            return ReloadOutcome::Unchanged;
        }
        match load() {
            Ok(settings) => {
                self.last_loaded = Some(bytes);
                self.last_error = None;
                ReloadOutcome::Reloaded(Box::new(settings))
            }
            Err(e) => self.fail(e.to_string()),
        }
    }

    fn fail(&mut self, error: String) -> ReloadOutcome {
        let report = self.last_error.as_deref() != Some(error.as_str());
        self.last_error = Some(error.clone());
        ReloadOutcome::Failed { error, report }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Result<Settings> {
        Ok(Settings::default())
    }

    #[test]
    fn unchanged_bytes_do_not_reload_or_parse() {
        let mut w = ReloadWatcher::seeded(Some(b"a = 1".to_vec()));
        let outcome = w.tick(Ok(b"a = 1".to_vec()), || panic!("must not parse unchanged bytes"));
        assert!(matches!(outcome, ReloadOutcome::Unchanged));
    }

    #[test]
    fn changed_bytes_reload_once() {
        let mut w = ReloadWatcher::seeded(Some(b"a = 1".to_vec()));
        assert!(matches!(w.tick(Ok(b"a = 2".to_vec()), ok), ReloadOutcome::Reloaded(_)));
        assert!(matches!(w.tick(Ok(b"a = 2".to_vec()), ok), ReloadOutcome::Unchanged));
    }

    #[test]
    fn an_unseeded_watcher_loads_on_its_first_tick() {
        let mut w = ReloadWatcher::default();
        assert!(matches!(w.tick(Ok(b"a = 1".to_vec()), ok), ReloadOutcome::Reloaded(_)));
    }

    #[test]
    fn a_repeated_identical_failure_is_reported_once() {
        let mut w = ReloadWatcher::seeded(Some(b"good".to_vec()));
        let bad = || Err(anyhow::anyhow!("parse error"));
        assert!(matches!(w.tick(Ok(b"bad".to_vec()), bad), ReloadOutcome::Failed { report: true, .. }));
        assert!(matches!(w.tick(Ok(b"bad".to_vec()), bad), ReloadOutcome::Failed { report: false, .. }));
        assert!(matches!(w.tick(Ok(b"bad".to_vec()), bad), ReloadOutcome::Failed { report: false, .. }));
    }

    #[test]
    fn a_different_failure_is_reported_again() {
        let mut w = ReloadWatcher::seeded(Some(b"good".to_vec()));
        let e1 = || Err(anyhow::anyhow!("error one"));
        let e2 = || Err(anyhow::anyhow!("error two"));
        assert!(matches!(w.tick(Ok(b"bad1".to_vec()), e1), ReloadOutcome::Failed { report: true, .. }));
        assert!(matches!(w.tick(Ok(b"bad2".to_vec()), e2), ReloadOutcome::Failed { report: true, .. }));
    }

    #[test]
    fn a_failure_that_clears_and_returns_is_reported_again() {
        let mut w = ReloadWatcher::seeded(Some(b"good".to_vec()));
        let bad = || Err(anyhow::anyhow!("parse error"));
        assert!(matches!(w.tick(Ok(b"bad".to_vec()), bad), ReloadOutcome::Failed { report: true, .. }));
        // Reverted to the loaded file: the error has cleared.
        assert!(matches!(w.tick(Ok(b"good".to_vec()), ok), ReloadOutcome::Unchanged));
        assert!(matches!(w.tick(Ok(b"bad".to_vec()), bad), ReloadOutcome::Failed { report: true, .. }));
    }

    #[test]
    fn an_unreadable_file_keeps_the_live_settings_and_is_reported_once() {
        let mut w = ReloadWatcher::seeded(Some(b"good".to_vec()));
        let missing = || Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(matches!(w.tick(missing(), ok), ReloadOutcome::Failed { report: true, .. }));
        assert!(matches!(w.tick(missing(), ok), ReloadOutcome::Failed { report: false, .. }));
    }
}
