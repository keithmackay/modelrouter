use serial_test::serial;

#[test]
#[serial]
fn default_settings_parse_without_config_file() {
    let s = modelrouter::config::load(Some("/nonexistent/path.toml".into()))
        .expect("should fall back to defaults");
    assert_eq!(s.server.port, 8080);
    assert_eq!(s.routing.default_model, "gpt-4o");
}

#[test]
#[serial]
fn env_var_overrides_config() {
    std::env::set_var("MODELROUTER_SERVER__PORT", "9090");
    let s = modelrouter::config::load(Some("/nonexistent/path.toml".into())).unwrap();
    assert_eq!(s.server.port, 9090);
    std::env::remove_var("MODELROUTER_SERVER__PORT");
}
