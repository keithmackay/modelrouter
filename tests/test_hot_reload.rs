mod common;
use modelrouter::config::loader::SettingsLoader;

#[test]
fn settings_loader_reload_returns_updated_port() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[server]\nport = 8080\n").unwrap();

    let loader = SettingsLoader::new(path.to_str().unwrap().to_string());
    let s1 = loader.load().unwrap();
    assert_eq!(s1.server.port, 8080);

    std::fs::write(&path, "[server]\nport = 9090\n").unwrap();
    let s2 = loader.load().unwrap();
    assert_eq!(s2.server.port, 9090);
}
