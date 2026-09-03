//! Fixture that runs the real `modelrouter` binary as a child process.
//!
//! Existing tests build `AppState` by hand and call `build_router` in-process.
//! That covers handlers well and covers `main`, `serve`, config loading,
//! migrations and startup guards not at all — which is how a guard that refuses
//! to boot shipped while all 425 tests passed. This fixture closes that gap by
//! executing the compiled binary the way an operator does.
//!
//! Isolation rules the fixture enforces, because breaking either makes the
//! suite flaky or destructive:
//!   * every instance gets its own `TempDir` for config and database, so
//!     `~/.modelrouter` is never touched;
//!   * every port is discovered by binding `:0`, so two instances never collide.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Path to the binary cargo built for this test run. Provided by cargo for
/// integration tests, so no extra dependency is needed to locate it.
const BIN: &str = env!("CARGO_BIN_EXE_modelrouter");

/// How long to wait for the server to answer `/health` before failing.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Claim an ephemeral port by binding and releasing it.
///
/// There is an unavoidable race between release and the child binding it. It is
/// far smaller than the collision rate of a fixed port, and the alternative —
/// passing an inherited listener — is not something the binary supports.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener.local_addr().expect("port assigned").port()
}

fn random_secret() -> String {
    // Deterministic entropy is fine here: this secret protects nothing beyond a
    // temp directory that is deleted when the test ends. It exists only so the
    // startup guard sees a non-placeholder value.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos:0>64x}")
}

/// Options a test can vary without hand-writing a whole config.
pub struct RouterOptions {
    pub provider_base_url: String,
    pub cache_enabled: bool,
    /// Overrides the generated secret. Used by the tests that assert the
    /// startup guard refuses empty and placeholder values.
    pub jwt_secret: Option<String>,
}

impl RouterOptions {
    pub fn new(provider_base_url: impl Into<String>) -> Self {
        Self {
            provider_base_url: provider_base_url.into(),
            cache_enabled: false,
            jwt_secret: None,
        }
    }

    pub fn with_cache(mut self) -> Self {
        self.cache_enabled = true;
        self
    }

    pub fn with_jwt_secret(mut self, secret: impl Into<String>) -> Self {
        self.jwt_secret = Some(secret.into());
        self
    }
}

/// Render a config pointing the router at the mock provider.
///
/// `mock` is deliberately not a name `ProviderRegistry` knows, so it falls
/// through to `OpenAICompatAdapter` — the harness exercises the real adapter.
pub fn render_config(dir: &Path, port: u16, opts: &RouterOptions) -> String {
    let db_path = dir.join("router.db");
    let secret = opts.jwt_secret.clone().unwrap_or_else(random_secret);
    format!(
        r#"
[server]
host = "127.0.0.1"
port = {port}

[database]
path = "{db}"

[routing]
default_provider = "mock"
default_model = "mock-model"

[providers.mock]
api_base = "{base}"
api_key = "test-key-not-a-secret"

[auth]
jwt_secret = "{secret}"

[cache]
enabled = {cache}

[storage]
store_prompts = true
"#,
        port = port,
        db = db_path.display(),
        base = opts.provider_base_url,
        secret = secret,
        cache = opts.cache_enabled,
    )
}

/// A running `modelrouter serve`. Killed on drop, including on panic.
pub struct RouterProcess {
    child: Child,
    dir: TempDir,
    port: u16,
    config_path: PathBuf,
}

impl RouterProcess {
    /// Start the binary against an existing config file and wait for `/health`.
    ///
    /// Used by the `init` regression test, which must serve the config `init`
    /// itself produced rather than one the fixture wrote.
    pub async fn start_with_config(config_path: PathBuf, dir: TempDir) -> Self {
        let port = free_port();
        Self::spawn(dir, config_path, port).await
    }

    /// Start the binary and wait until it answers `/health`.
    pub async fn start(opts: RouterOptions) -> Self {
        let dir = tempfile::tempdir().expect("temp dir for router state");
        let port = free_port();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, render_config(dir.path(), port, &opts))
            .expect("write config");
        Self::spawn(dir, config_path, port).await
    }

    async fn spawn(dir: TempDir, config_path: PathBuf, port: u16) -> Self {

        let log = std::fs::File::create(dir.path().join("serve.log")).expect("create log");
        let err = log.try_clone().expect("clone log handle");

        // `serve` binds from its own --host/--port flags, which carry clap
        // defaults, so `[server] port` in the config is ignored entirely (see
        // docs/testing/e2e-harness.md, "Defects this tier found"). Pass the
        // port explicitly rather than relying on config that has no effect.
        let child = Command::new(BIN)
            .arg("serve")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .env("MODELROUTER_CONFIG", &config_path)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn modelrouter serve");

        let mut proc = Self {
            child,
            dir,
            port,
            config_path,
        };
        proc.wait_until_ready().await;
        proc
    }

    /// Poll `/health` until it answers. Never sleeps a fixed duration — a fixed
    /// sleep is both slower than it needs to be and flaky under load.
    async fn wait_until_ready(&mut self) {
        let url = format!("{}/health", self.base_url());
        let client = reqwest::Client::new();
        let deadline = Instant::now() + READY_TIMEOUT;

        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "modelrouter exited during startup with {status}\n--- log ---\n{}",
                    self.logs()
                );
            }
            if let Ok(resp) = client.get(&url).send().await {
                if resp.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        panic!(
            "modelrouter did not answer /health within {READY_TIMEOUT:?}\n--- log ---\n{}",
            self.logs()
        );
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.path().join("router.db")
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Captured stdout and stderr. Included in every startup failure so a
    /// failing test is diagnosable without re-running it by hand.
    pub fn logs(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("serve.log")).unwrap_or_default()
    }

    /// Create a user and an API key through the real CLI, returning the key.
    ///
    /// Shelling out rather than inserting rows keeps the fixture honest: if key
    /// creation breaks, these tests fail rather than papering over it.
    pub fn create_user_and_key(&self, user: &str) -> String {
        let ok = Command::new(BIN)
            .args(["user", "create", "--name", user])
            .env("MODELROUTER_CONFIG", &self.config_path)
            .output()
            .expect("run user create");
        assert!(
            ok.status.success(),
            "user create failed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );

        let out = Command::new(BIN)
            .args(["key", "create", "--user", user, "--project", "e2e"])
            .env("MODELROUTER_CONFIG", &self.config_path)
            .output()
            .expect("run key create");
        assert!(
            out.status.success(),
            "key create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let stdout = String::from_utf8_lossy(&out.stdout);
        extract_key(&stdout).unwrap_or_else(|| {
            panic!("no API key in `key create` output:\n{stdout}");
        })
    }
}

/// Pull the `mr-` prefixed key out of CLI output without depending on the
/// surrounding prose, which is free to change.
pub fn extract_key(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|tok| tok.starts_with("mr-") && tok.len() > 10)
        .map(|tok| tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-'))
        .map(str::to_string)
}

impl Drop for RouterProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run a CLI subcommand against a config and return (success, stdout, stderr).
pub fn run_cli(config: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(BIN)
        .args(args)
        .env("MODELROUTER_CONFIG", config)
        .output()
        .expect("run modelrouter CLI");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Attempt to start `serve` and return its exit output. Used by the tests that
/// assert the startup guard refuses a weak signing key: those must observe a
/// process that exits, not one that serves.
pub fn try_serve_expecting_exit(config: &Path) -> (bool, String) {
    let out = Command::new(BIN)
        .arg("serve")
        .env("MODELROUTER_CONFIG", config)
        .output()
        .expect("run modelrouter serve");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}
