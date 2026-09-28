//! Fake-endpoint tests for the Azure credential sources. Every endpoint is a
//! local axum server (or, for the Azure CLI, a shell script), so these pin the
//! request shapes Microsoft documents without reaching Azure.

use super::*;
use crate::providers::azure_entra::{COGNITIVE_SERVICES_SCOPE, FOUNDRY_PROJECT_SCOPE};
use crate::providers::credential_error::find_credential_expired;
use std::collections::VecDeque;

// ── fake HTTP endpoint ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Answers every request with the next queued `(status, body)`; the last one
/// repeats. Records what it saw.
struct Fake {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    async fn start(responses: Vec<(u16, String)>) -> Self {
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (q, s) = (queue.clone(), seen.clone());
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let (q, s) = (q.clone(), s.clone());
            async move {
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX)
                    .await
                    .unwrap_or_default();
                s.lock().unwrap().push(Seen {
                    path: parts.uri.path().to_string(),
                    query: parts.uri.query().unwrap_or_default().to_string(),
                    headers: parts
                        .headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
                        .collect(),
                    body: String::from_utf8_lossy(&body).to_string(),
                });
                let (status, body) = {
                    let mut q = q.lock().unwrap();
                    if q.len() > 1 {
                        q.pop_front().unwrap()
                    } else {
                        q.front().cloned().unwrap()
                    }
                };
                (axum::http::StatusCode::from_u16(status).unwrap(), body)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn hits(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn ok_token(token: &str, expires_in: u64) -> (u16, String) {
    (
        200,
        format!(r#"{{"access_token":"{token}","expires_in":{expires_in},"token_type":"Bearer"}}"#),
    )
}

/// A port nothing listens on: connection refused at once.
const DEAD: &str = "http://127.0.0.1:1";

fn settings(scope: &str) -> AzureSettings {
    AzureSettings {
        provider: "foundry".into(),
        scope: scope.into(),
        tenant_id: None,
        client_id: None,
        client_secret: None,
        federated_token_file: None,
        authority_host: DEAD.into(),
        imds_host: DEAD.into(),
        identity_endpoint: None,
        cli_program: "/nonexistent/az".into(),
        timeout: Duration::from_secs(5),
    }
}

fn chain(label: &str, source: Arc<dyn CredentialSource>) -> CredentialChain {
    CredentialChain::new("foundry", label, source)
        .with_timings(Duration::from_secs(2), Duration::from_millis(50))
}

fn secret_settings(authority: &str) -> AzureSettings {
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.authority_host = authority.into();
    s.tenant_id = Some("tenant-x".into());
    s.client_id = Some("client-x".into());
    s.client_secret = Some("s3cret".into());
    s
}

// ── managed identity ────────────────────────────────────────────────────────

#[tokio::test]
async fn imds_request_shape_and_caching() {
    let imds = Fake::start(vec![(
        200,
        r#"{"access_token":"mi-1","expires_in":"3600"}"#.into(),
    )])
    .await;
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.imds_host = imds.base.clone();
    s.client_id = Some("user-assigned-id".into());
    let src = ManagedIdentitySource::new(&s).unwrap();

    assert_eq!(src.token().await.unwrap(), "mi-1");
    assert_eq!(src.token().await.unwrap(), "mi-1");
    assert_eq!(imds.hits(), 1, "a fresh token is served from cache");

    let seen = &imds.seen()[0];
    assert_eq!(seen.path, "/metadata/identity/oauth2/token");
    assert_eq!(seen.header("metadata"), Some("true"));
    assert!(
        seen.query.contains("api-version=2018-02-01"),
        "{}",
        seen.query
    );
    assert!(
        seen.query
            .contains("resource=https%3A%2F%2Fcognitiveservices.azure.com"),
        "{}",
        seen.query
    );
    assert!(
        !seen.query.contains(".default"),
        "IMDS takes a resource: {}",
        seen.query
    );
    assert!(
        seen.query.contains("client_id=user-assigned-id"),
        "{}",
        seen.query
    );
    assert_eq!(src.kind(), "azure-managed-identity");
}

#[tokio::test]
async fn app_service_identity_endpoint_shape() {
    let expires_on = now_unix() + 3600;
    let ep = Fake::start(vec![(
        200,
        format!(r#"{{"access_token":"as-1","expires_on":"{expires_on}","token_type":"Bearer"}}"#),
    )])
    .await;
    let mut s = settings(FOUNDRY_PROJECT_SCOPE);
    s.identity_endpoint = Some((format!("{}/msi/token", ep.base), "header-value".into()));
    let src = ManagedIdentitySource::new(&s).unwrap();

    assert_eq!(src.token().await.unwrap(), "as-1");
    let seen = &ep.seen()[0];
    assert_eq!(seen.path, "/msi/token");
    assert_eq!(seen.header("x-identity-header"), Some("header-value"));
    assert!(seen.header("metadata").is_none());
    assert!(
        seen.query.contains("api-version=2019-08-01"),
        "{}",
        seen.query
    );
    assert!(
        seen.query.contains("resource=https%3A%2F%2Fai.azure.com"),
        "{}",
        seen.query
    );
    // expires_on an hour out: cached.
    src.token().await.unwrap();
    assert_eq!(ep.hits(), 1);
}

#[tokio::test]
async fn short_lived_token_is_refreshed_before_expiry() {
    let imds = Fake::start(vec![ok_token("first", 1), ok_token("second", 3600)]).await;
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.imds_host = imds.base.clone();
    let src = ManagedIdentitySource::new(&s).unwrap();

    assert_eq!(src.token().await.unwrap(), "first");
    // A 1s token is refreshed after half its life.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(src.token().await.unwrap(), "second");
    assert_eq!(imds.hits(), 2);
}

#[tokio::test]
async fn force_refresh_bypasses_the_cache() {
    let imds = Fake::start(vec![ok_token("a", 3600), ok_token("b", 3600)]).await;
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.imds_host = imds.base.clone();
    let src = ManagedIdentitySource::new(&s).unwrap();
    assert_eq!(src.token().await.unwrap(), "a");
    assert_eq!(src.force_refresh().await.unwrap(), "b");
}

// ── client secret and workload identity ─────────────────────────────────────

#[tokio::test]
async fn client_secret_posts_the_documented_form() {
    let login = Fake::start(vec![ok_token("sp-1", 3600)]).await;
    let src = AppRegistrationSource::client_secret(&secret_settings(&login.base)).unwrap();
    assert_eq!(src.token().await.unwrap(), "sp-1");
    let seen = &login.seen()[0];
    assert_eq!(seen.path, "/tenant-x/oauth2/v2.0/token");
    assert!(
        seen.body.contains("grant_type=client_credentials"),
        "{}",
        seen.body
    );
    assert!(seen.body.contains("client_id=client-x"), "{}", seen.body);
    assert!(seen.body.contains("client_secret=s3cret"), "{}", seen.body);
    assert!(
        seen.body
            .contains("scope=https%3A%2F%2Fcognitiveservices.azure.com%2F.default"),
        "{}",
        seen.body
    );
    assert_eq!(src.kind(), "azure-client-secret");
}

#[tokio::test]
async fn workload_identity_sends_the_federated_token_and_rereads_it() {
    let login = Fake::start(vec![ok_token("wi-1", 3600), ok_token("wi-2", 3600)]).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("token");
    std::fs::write(&file, "jwt-one\n").unwrap();
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.authority_host = login.base.clone();
    s.tenant_id = Some("tenant-w".into());
    s.client_id = Some("client-w".into());
    s.federated_token_file = Some(file.to_string_lossy().to_string());
    let src = AppRegistrationSource::workload_identity(&s).unwrap();

    assert_eq!(src.token().await.unwrap(), "wi-1");
    std::fs::write(&file, "jwt-two").unwrap();
    assert_eq!(src.force_refresh().await.unwrap(), "wi-2");

    let seen = login.seen();
    assert_eq!(seen[0].path, "/tenant-w/oauth2/v2.0/token");
    assert!(
        seen[0].body.contains("grant_type=client_credentials"),
        "{}",
        seen[0].body
    );
    assert!(
        seen[0]
            .body
            .contains("client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer"),
        "{}",
        seen[0].body
    );
    assert!(
        seen[0].body.contains("client_assertion=jwt-one&")
            || seen[0].body.ends_with("client_assertion=jwt-one"),
        "{}",
        seen[0].body
    );
    assert!(
        seen[1].body.contains("client_assertion=jwt-two"),
        "the rotated file is re-read: {}",
        seen[1].body
    );
    assert!(!seen[0].body.contains("client_secret"), "{}", seen[0].body);
    assert_eq!(src.kind(), "azure-workload-identity");
}

#[test]
fn explicit_sources_name_missing_settings() {
    let s = settings(COGNITIVE_SERVICES_SCOPE);
    let err = AppRegistrationSource::client_secret(&s)
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("[providers.foundry]") && err.contains("AZURE_TENANT_ID"),
        "{err}"
    );
    assert!(err.contains("AZURE_CLIENT_SECRET"), "{err}");
    let err = AppRegistrationSource::workload_identity(&s)
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("AZURE_FEDERATED_TOKEN_FILE"), "{err}");
}

// ── classification through the shared chain ─────────────────────────────────

#[tokio::test]
async fn aadsts_reauth_becomes_credential_expired() {
    let login = Fake::start(vec![(
        401,
        r#"{"error":"invalid_client","error_description":"AADSTS7000222: The provided client secret keys for app 'x' are expired."}"#.into(),
    )])
    .await;
    let src = AppRegistrationSource::client_secret(&secret_settings(&login.base)).unwrap();
    let chain = chain("client-secret", Arc::new(src));

    let err = chain.token().await.unwrap_err();
    let expired = find_credential_expired(&err).expect("typed credential_expired");
    assert_eq!(expired.provider, "foundry");
    assert_eq!(expired.credential_kind, "azure-client-secret");
    assert!(
        expired.remediation.contains("client secret"),
        "{}",
        expired.remediation
    );
    assert!(
        expired.detail.contains("AADSTS7000222"),
        "{}",
        expired.detail
    );

    // Cached: a second request does not call the endpoint again.
    chain.token().await.unwrap_err();
    assert_eq!(login.hits(), 1);

    let report = chain.report();
    assert_eq!(report.verdict.status, CredentialStatus::Expired);
    assert_eq!(report.kind, "azure-client-secret");
    assert!(!report.fallback_active);
}

#[tokio::test]
async fn server_errors_stay_transient() {
    for status in [500u16, 503, 429] {
        let login = Fake::start(vec![(
            status,
            r#"{"error":"temporarily_unavailable","error_description":"AADSTS50173 mentioned in a 5xx"}"#.into(),
        )])
        .await;
        let src = AppRegistrationSource::client_secret(&secret_settings(&login.base)).unwrap();
        let chain = chain("client-secret", Arc::new(src));
        let err = chain.token().await.unwrap_err();
        assert!(
            find_credential_expired(&err).is_none(),
            "{status} must stay transient: {err:#}"
        );
        assert_ne!(chain.report().verdict.status, CredentialStatus::Expired);
        // Not cached as dead: the next request tries again.
        chain.token().await.unwrap_err();
        assert_eq!(login.hits(), 2, "{status}");
    }
}

#[tokio::test]
async fn network_failure_stays_transient() {
    let src = ManagedIdentitySource::new(&settings(COGNITIVE_SERVICES_SCOPE)).unwrap();
    let chain = chain("managed-identity", Arc::new(src));
    let err = chain.token().await.unwrap_err();
    assert!(find_credential_expired(&err).is_none(), "{err:#}");
}

#[test]
fn every_documented_reauth_code_is_permanent() {
    for code in ["50173", "70043", "700082", "50076", "50079", "700024"] {
        let err: anyhow::Error = AzureTokenError {
            source_kind: "client secret",
            status: 400,
            body: format!(
                r#"{{"error":"invalid_request","error_description":"AADSTS{code}: text"}}"#
            ),
        }
        .into();
        assert!(is_azure_reauth(&err), "AADSTS{code}");
    }
    for oauth in ["invalid_grant", "interaction_required"] {
        let err: anyhow::Error = AzureTokenError {
            source_kind: "x",
            status: 400,
            body: format!(r#"{{"error":"{oauth}"}}"#),
        }
        .into();
        assert!(is_azure_reauth(&err), "{oauth}");
    }
}

#[test]
fn unrelated_aadsts_codes_are_not_reauth() {
    // AADSTS500011 (resource principal not found) is a configuration error
    // and must not match 50011-style prefixes of the reauth list.
    for body in [
        "AADSTS500011: resource not found",
        "AADSTS501730: made up",
        "AADSTS90002: tenant not found",
    ] {
        let err: anyhow::Error = AzureTokenError {
            source_kind: "x",
            status: 400,
            body: body.into(),
        }
        .into();
        assert!(!is_azure_reauth(&err), "{body}");
    }
    assert_eq!(
        aadsts_codes("x AADSTS70043: y aadsts50076 z AADSTS"),
        vec!["70043", "50076"]
    );
}

#[test]
fn cli_errors_asking_for_login_are_permanent() {
    let login: anyhow::Error = AzureCliError {
        code: "1".into(),
        stderr: "ERROR: The refresh token has expired. Please run 'az login' to setup account."
            .into(),
    }
    .into();
    assert!(is_azure_reauth(&login));
    let aadsts: anyhow::Error = AzureCliError {
        code: "1".into(),
        stderr: "ERROR: AADSTS50078: MFA expired".into(),
    }
    .into();
    assert!(is_azure_reauth(&aadsts));
    let network: anyhow::Error = AzureCliError {
        code: "1".into(),
        stderr:
            "ERROR: HTTPSConnectionPool(host='login.microsoftonline.com'): Max retries exceeded"
                .into(),
    }
    .into();
    assert!(!is_azure_reauth(&network));
}

// ── token document parsing ──────────────────────────────────────────────────

#[test]
fn every_documented_token_shape_parses() {
    let now = 1_700_000_000;
    let (t, d) = parse_token_response(r#"{"access_token":"a","expires_in":3599}"#, now).unwrap();
    assert_eq!((t.as_str(), d.as_secs()), ("a", 3599));
    let (_, d) = parse_token_response(r#"{"access_token":"a","expires_in":"86399"}"#, now).unwrap();
    assert_eq!(d.as_secs(), 86399);
    let (_, d) =
        parse_token_response(r#"{"access_token":"a","expires_on":"1700003600"}"#, now).unwrap();
    assert_eq!(d.as_secs(), 3600);
    let (t, d) =
        parse_token_response(r#"{"accessToken":"c","expires_on":1700000600}"#, now).unwrap();
    assert_eq!((t.as_str(), d.as_secs()), ("c", 600));
    let (_, d) = parse_token_response(r#"{"access_token":"a"}"#, now).unwrap();
    assert_eq!(d, UNSTATED_LIFETIME);
    // Older CLIs: only a local-time `expiresOn`.
    use chrono::TimeZone;
    let local = chrono::Local.timestamp_opt(now + 1200, 0).unwrap();
    let doc = format!(
        r#"{{"accessToken":"d","expiresOn":"{}"}}"#,
        local.format("%Y-%m-%d %H:%M:%S%.6f")
    );
    let (_, d) = parse_token_response(&doc, now).unwrap();
    assert_eq!(d.as_secs(), 1200);
    // Already expired: zero, refreshed on next use rather than negative.
    let (_, d) = parse_token_response(r#"{"access_token":"a","expires_on":"1"}"#, now).unwrap();
    assert_eq!(d.as_secs(), 0);
    assert!(parse_token_response(r#"{"expires_in":1}"#, now).is_err());
    assert!(parse_token_response("not json", now).is_err());
}

// ── Azure CLI ───────────────────────────────────────────────────────────────

#[cfg(unix)]
fn fake_az(dir: &std::path::Path, script_body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("az");
    std::fs::write(&path, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().to_string()
}

#[cfg(unix)]
fn cli_ok_script(dir: &std::path::Path, token: &str) -> String {
    let args = dir.join("args");
    fake_az(
        dir,
        &format!(
            "echo \"$@\" >> {}\necho '{{\"accessToken\":\"{token}\",\"expires_on\":4070908800,\"tokenType\":\"Bearer\"}}'",
            args.display()
        ),
    )
}

#[cfg(unix)]
const AZ_LOGIN_SCRIPT: &str = "echo \"ERROR: AADSTS70043: The refresh token has expired due to a sign-in frequency check. Please run 'az login' to setup account.\" >&2\nexit 1";

#[cfg(unix)]
#[tokio::test]
async fn cli_success_passes_resource_and_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.cli_program = cli_ok_script(dir.path(), "cli-tok");
    s.tenant_id = Some("tenant-c".into());
    let src = CliSource::new(&s);
    assert_eq!(src.token().await.unwrap(), "cli-tok");
    assert_eq!(src.token().await.unwrap(), "cli-tok");
    let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
    assert_eq!(args.lines().count(), 1, "cached: {args}");
    assert!(
        args.contains(
            "account get-access-token --resource https://cognitiveservices.azure.com --output json"
        ),
        "{args}"
    );
    assert!(args.contains("--tenant tenant-c"), "{args}");
    assert_eq!(src.kind(), "azure-cli");
}

#[cfg(unix)]
#[tokio::test]
async fn cli_login_expiry_becomes_credential_expired() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.cli_program = fake_az(dir.path(), AZ_LOGIN_SCRIPT);
    let chain = chain("cli", Arc::new(CliSource::new(&s)));
    let err = chain.token().await.unwrap_err();
    let expired = find_credential_expired(&err).expect("typed");
    assert_eq!(expired.credential_kind, "azure-cli");
    assert!(
        expired.remediation.contains("az login"),
        "{}",
        expired.remediation
    );
    assert!(
        expired.remediation.contains("managed-identity"),
        "{}",
        expired.remediation
    );
    assert_eq!(chain.report().verdict.status, CredentialStatus::Expired);
}

#[cfg(unix)]
#[tokio::test]
async fn cli_network_failure_is_transient() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.cli_program = fake_az(
        dir.path(),
        "echo 'ERROR: Connection reset by peer' >&2\nexit 1",
    );
    let chain = chain("cli", Arc::new(CliSource::new(&s)));
    let err = chain.token().await.unwrap_err();
    assert!(find_credential_expired(&err).is_none(), "{err:#}");
}

#[tokio::test]
async fn missing_cli_is_a_clear_transient_error() {
    let src = CliSource::new(&settings(COGNITIVE_SERVICES_SCOPE));
    let err = format!("{:#}", src.token().await.unwrap_err());
    assert!(err.contains("could not run the Azure CLI"), "{err}");
}

// ── default chain ───────────────────────────────────────────────────────────

fn default_source(s: AzureSettings) -> DefaultSource {
    DefaultSource::new(s).with_imds_probe_timeout(Duration::from_millis(500))
}

#[tokio::test]
async fn default_prefers_the_client_secret() {
    let login = Fake::start(vec![ok_token("sp", 3600)]).await;
    let src = default_source(secret_settings(&login.base));
    assert_eq!(src.kind(), "azure-default-unresolved");
    assert_eq!(src.verdict().status, CredentialStatus::Unknown);
    assert_eq!(src.token().await.unwrap(), "sp");
    assert_eq!(src.kind(), "azure-default-client-secret");
    assert_eq!(src.verdict().status, CredentialStatus::Warn);
    assert!(src.fallback().is_none(), "only a CLI login falls back");
}

#[tokio::test]
async fn default_uses_workload_identity_next() {
    let login = Fake::start(vec![ok_token("wi", 3600)]).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("token");
    std::fs::write(&file, "jwt").unwrap();
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.authority_host = login.base.clone();
    s.tenant_id = Some("t".into());
    s.client_id = Some("c".into());
    s.federated_token_file = Some(file.to_string_lossy().to_string());
    let src = default_source(s);
    assert_eq!(src.token().await.unwrap(), "wi");
    assert_eq!(src.kind(), "azure-default-workload-identity");
    assert_eq!(src.verdict().status, CredentialStatus::Ok);
}

#[tokio::test]
async fn default_uses_the_app_service_identity_endpoint_then() {
    let ep = Fake::start(vec![ok_token("as", 3600)]).await;
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.identity_endpoint = Some((format!("{}/msi", ep.base), "h".into()));
    let src = default_source(s);
    assert_eq!(src.token().await.unwrap(), "as");
    assert_eq!(src.kind(), "azure-default-managed-identity");
}

#[tokio::test]
async fn default_uses_imds_when_it_answers_and_reuses_the_probe_token() {
    let imds = Fake::start(vec![ok_token("imds", 3600)]).await;
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.imds_host = imds.base.clone();
    let src = default_source(s);
    assert_eq!(src.token().await.unwrap(), "imds");
    assert_eq!(src.token().await.unwrap(), "imds");
    assert_eq!(imds.hits(), 1);
    assert_eq!(src.kind(), "azure-default-managed-identity");
}

#[cfg(unix)]
#[tokio::test]
async fn default_falls_through_to_the_cli_off_azure() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.cli_program = cli_ok_script(dir.path(), "cli");
    let src = default_source(s);
    assert_eq!(src.token().await.unwrap(), "cli");
    assert_eq!(src.kind(), "azure-default-cli");
    assert_eq!(src.verdict().status, CredentialStatus::Warn);
}

#[cfg(unix)]
#[tokio::test]
async fn default_dead_cli_login_falls_back_to_managed_identity() {
    // IMDS refuses during resolution (so `default` settles on the CLI), then
    // answers when the fallback probes it.
    let imds = Fake::start(vec![(500, "warming up".into()), ok_token("mi-after", 3600)]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.imds_host = imds.base.clone();
    s.cli_program = fake_az(dir.path(), AZ_LOGIN_SCRIPT);
    let chain = chain("default", Arc::new(default_source(s)));

    assert_eq!(chain.token().await.unwrap(), "mi-after");
    let report = chain.report();
    assert!(report.fallback_active);
    assert_eq!(report.source, "default");
    assert_eq!(report.kind, "azure-managed-identity");
    assert_eq!(report.verdict.status, CredentialStatus::Ok);
    // The fallback sticks and serves from its cache.
    assert_eq!(chain.token().await.unwrap(), "mi-after");
    assert_eq!(imds.hits(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn default_dead_cli_login_without_managed_identity_is_expired() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.cli_program = fake_az(dir.path(), AZ_LOGIN_SCRIPT);
    let chain = chain("default", Arc::new(default_source(s)));

    let err = chain.token().await.unwrap_err();
    let expired = find_credential_expired(&err).expect("typed");
    assert_eq!(expired.credential_kind, "azure-default-cli");
    let report = chain.report();
    assert!(!report.fallback_active);
    assert_eq!(report.verdict.status, CredentialStatus::Expired);
    assert!(
        report.verdict.remediation.contains("az login"),
        "{}",
        report.verdict.remediation
    );
}

// ── verdicts and wiring ─────────────────────────────────────────────────────

#[test]
fn every_kind_maps_to_a_verdict() {
    let cases = [
        (
            AzureKind::ManagedIdentity,
            "azure-managed-identity",
            CredentialStatus::Ok,
        ),
        (
            AzureKind::WorkloadIdentity,
            "azure-workload-identity",
            CredentialStatus::Ok,
        ),
        (
            AzureKind::ClientSecret,
            "azure-client-secret",
            CredentialStatus::Warn,
        ),
        (AzureKind::Cli, "azure-cli", CredentialStatus::Warn),
    ];
    for (kind, label, status) in cases {
        assert_eq!(kind.label(), label);
        let v = kind.verdict("azure");
        assert_eq!(v.status, status, "{label}");
        assert!(!v.reason.is_empty(), "{label}");
        if status == CredentialStatus::Warn {
            assert!(
                v.remediation.contains("[providers.azure]"),
                "{label}: {}",
                v.remediation
            );
        }
        let (reason, remediation) = kind.expired_guidance("azure");
        assert!(!reason.is_empty() && !remediation.is_empty(), "{label}");
    }
}

#[tokio::test]
async fn unset_source_keeps_the_environment_behaviour() {
    let login = Fake::start(vec![ok_token("env-sp", 3600)]).await;
    let cred = AzureCredential::build(None, secret_settings(&login.base)).unwrap();
    assert_eq!(TokenProvider::token(&cred).await.unwrap(), "env-sp");
    let report = cred.report();
    assert_eq!(report.source, "environment");
    assert_eq!(report.kind, "azure-client-secret");

    let imds = Fake::start(vec![ok_token("env-mi", 3600)]).await;
    let mut s = settings(FOUNDRY_PROJECT_SCOPE);
    s.imds_host = imds.base.clone();
    s.tenant_id = Some("partial".into());
    let cred = AzureCredential::build(None, s).unwrap();
    assert_eq!(TokenProvider::token(&cred).await.unwrap(), "env-mi");
    assert_eq!(cred.report().kind, "azure-managed-identity");
}

#[test]
fn explicit_sources_report_their_label() {
    let mut s = settings(COGNITIVE_SERVICES_SCOPE);
    s.provider = "azure".into();
    for (source, label, kind) in [
        (
            AzureCredentialSource::ManagedIdentity,
            "managed-identity",
            "azure-managed-identity",
        ),
        (AzureCredentialSource::Cli, "cli", "azure-cli"),
        (
            AzureCredentialSource::Default,
            "default",
            "azure-default-unresolved",
        ),
    ] {
        let report = AzureCredential::build(Some(source), s.clone())
            .unwrap()
            .report();
        assert_eq!(
            (
                report.provider.as_str(),
                report.source.as_str(),
                report.kind.as_str()
            ),
            ("azure", label, kind)
        );
    }
}

fn provider_config(source: Option<&str>, api_key: &str) -> ProviderConfig {
    ProviderConfig {
        api_key: api_key.into(),
        credential_source: source.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn key_auth_stays_the_default_for_the_azure_provider() {
    let t = Duration::from_secs(5);
    let auth = AzureAuth::key_by_default(
        "azure",
        &provider_config(None, "k"),
        COGNITIVE_SERVICES_SCOPE,
        t,
    )
    .unwrap();
    assert_eq!(auth.label(), "api_key");
    assert!(auth.credential_report().is_none());

    let auth = AzureAuth::key_by_default(
        "azure",
        &provider_config(Some("managed-identity"), ""),
        COGNITIVE_SERVICES_SCOPE,
        t,
    )
    .unwrap();
    assert_eq!(auth.label(), "entra");
    let report = auth.credential_report().unwrap();
    assert_eq!(report.kind, "azure-managed-identity");
    assert_eq!(report.source, "managed-identity");
}

#[test]
fn entra_stays_the_default_for_foundry() {
    let t = Duration::from_secs(5);
    let auth = AzureAuth::entra_by_default(
        "foundry",
        &provider_config(None, "key"),
        COGNITIVE_SERVICES_SCOPE,
        t,
    )
    .unwrap();
    assert_eq!(auth.label(), "api_key");
    let auth = AzureAuth::entra_by_default(
        "foundry",
        &provider_config(Some("cli"), ""),
        COGNITIVE_SERVICES_SCOPE,
        t,
    )
    .unwrap();
    assert_eq!(auth.label(), "entra");
    assert_eq!(auth.credential_report().unwrap().kind, "azure-cli");
}

#[test]
fn config_validation_rejects_contradictions() {
    use crate::config::schema::validate_azure_credential;
    assert!(validate_azure_credential(&provider_config(Some("workload-identity"), "")).is_ok());
    let err = validate_azure_credential(&provider_config(Some("gcloud"), ""))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("managed-identity") && err.contains("cli"),
        "{err}"
    );
    let err = validate_azure_credential(&provider_config(Some("cli"), "key"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("api_key"), "{err}");

    let mut c = provider_config(Some("managed-identity"), "");
    c.azure_client_secret = Some("s".into());
    assert!(validate_azure_credential(&c).is_err());
    let mut c = provider_config(Some("client-secret"), "");
    c.azure_federated_token_file = Some("/f".into());
    assert!(validate_azure_credential(&c).is_err());
    let mut c = provider_config(None, "");
    c.azure_client_secret = Some("s".into());
    assert!(
        validate_azure_credential(&c).is_err(),
        "a secret with no source is ambiguous"
    );
    let mut c = provider_config(Some("managed-identity"), "");
    c.azure_client_id = Some("user-assigned".into());
    assert!(validate_azure_credential(&c).is_ok());
}
