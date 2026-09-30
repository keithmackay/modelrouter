//! A mock OIDC identity provider that speaks real HTTP on localhost.
//!
//! The admin OIDC login flow is three server-to-server fetches — discovery,
//! token exchange, JWKS — wrapped around one signature validation. Unit-testing
//! the helpers in isolation leaves the handlers' wiring (and every error arm
//! between them) unexercised, so this mock stands up the whole issuer instead:
//! `modelrouter` drives its production `reqwest` client against it and the test
//! asserts on what came back out of `/admin/auth/oidc/callback`.
//!
//! Same pattern as `mock_llm` / `mock_anthropic`: bind `127.0.0.1:0`, serve from
//! an `axum` router, hand the caller the base URL. Nothing here is reachable
//! from `src/`.
//!
//! Every response is programmable at runtime, which is what makes the failure
//! arms testable: a test performs a *successful* `/login` to obtain a real state
//! token, then breaks one thing about the issuer before calling `/callback`.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

/// The RSA-2048 key the mock issuer signs ID tokens with, generated once per
/// test process so the production `validate_id_token` path runs for real
/// against a matching JWKS. Nothing is committed: a private key checked into the
/// tree trips secret scanners even when it grants access to nothing.
struct TestSigningKey {
    /// PKCS#8 PEM, the form `EncodingKey::from_rsa_pem` accepts.
    private_pem: String,
    /// Base64url big-endian modulus, the JWKS `n` member.
    modulus_b64url: String,
}

fn test_signing_key() -> &'static TestSigningKey {
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use aws_lc_rs::signature::KeyPair as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use base64::Engine;

    static KEY: OnceLock<TestSigningKey> = OnceLock::new();
    KEY.get_or_init(|| {
        let key_pair = KeyPair::generate(KeySize::Rsa2048).expect("RSA-2048 test key generates");
        let pkcs8 = key_pair.as_der().expect("test key encodes as PKCS#8");
        let body = STANDARD.encode(pkcs8.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
            .collect();
        // The label is spliced in so the source holds no literal PEM header for
        // secret scanners to match.
        let label = "PRIVATE KEY";
        let private_pem = format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            lines.join("\n")
        );
        let modulus = key_pair.public_key().modulus();
        TestSigningKey {
            private_pem,
            modulus_b64url: URL_SAFE_NO_PAD.encode(modulus.big_endian_without_leading_zero()),
        }
    })
}

/// The `kid` the mock signs with, and the one its JWKS advertises.
pub const TEST_KID: &str = "mock-oidc-key-1";

/// How the mock should answer discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryMode {
    /// Well-formed metadata pointing back at this server.
    Ok,
    /// 500, so the caller's `error_for_status()` trips.
    ServerError,
    /// 200 with a body that is not discovery metadata.
    Malformed,
    /// Well-formed, but `authorization_endpoint` is not a parseable URL.
    BadAuthorizationEndpoint,
}

/// How the mock should answer the token exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenMode {
    /// A signed ID token matching the configured claims.
    Ok,
    /// 400, as a provider rejecting the code or the PKCE verifier would.
    BadRequest,
    /// 200 with no `id_token` field, so deserialisation fails.
    MissingIdToken,
    /// A syntactically invalid JWT.
    Garbage,
    /// A correctly-signed token whose header carries no `kid`.
    NoKid,
    /// A correctly-signed token whose header names a `kid` the JWKS lacks.
    UnknownKid,
}

/// How the mock should answer the JWKS fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwksMode {
    /// One RSA key under `TEST_KID`.
    Ok,
    /// 500.
    ServerError,
    /// 200 with `keys` absent, so the handler's "missing keys array" arm runs.
    NoKeysArray,
    /// 200 with `keys: []`, so no key matches the token's `kid`.
    EmptyKeys,
}

/// The claim set the mock will sign into the next ID token.
#[derive(Debug, Clone)]
pub struct IdTokenClaimSpec {
    pub sub: String,
    pub email: Option<String>,
    pub name: Option<String>,
    /// Audience. Set to something other than the client_id to test rejection.
    pub aud: String,
    /// Issuer. Overridden to the mock's own base URL at start-up unless a test
    /// replaces it to test issuer rejection.
    pub iss: Option<String>,
    /// Seconds from now until expiry; negative mints an already-expired token.
    pub expires_in_secs: i64,
}

impl Default for IdTokenClaimSpec {
    fn default() -> Self {
        Self {
            sub: "mock-oidc|subject-1".to_string(),
            email: Some("alice@example.com".to_string()),
            name: Some("Alice Example".to_string()),
            aud: "mock-client-id".to_string(),
            iss: None,
            expires_in_secs: 3600,
        }
    }
}

struct MockOidcState {
    base_url: String,
    discovery: DiscoveryMode,
    token: TokenMode,
    jwks: JwksMode,
    claims: IdTokenClaimSpec,
    /// Form bodies the token endpoint received, so a test can assert the router
    /// really sent the PKCE verifier and client credentials it promised.
    token_requests: Vec<String>,
}

/// A running mock OIDC issuer. Dropping it leaves the server task to be reaped
/// with the test's runtime.
pub struct MockOidc {
    addr: SocketAddr,
    state: Arc<Mutex<MockOidcState>>,
}

impl MockOidc {
    /// Bind an ephemeral port and start serving a healthy issuer.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock OIDC issuer binds an ephemeral port");
        let addr = listener.local_addr().expect("mock OIDC issuer has an address");
        let base_url = format!("http://{addr}");

        let state = Arc::new(Mutex::new(MockOidcState {
            base_url: base_url.clone(),
            discovery: DiscoveryMode::Ok,
            token: TokenMode::Ok,
            jwks: JwksMode::Ok,
            claims: IdTokenClaimSpec::default(),
            token_requests: Vec::new(),
        }));

        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .route("/authorize", get(authorize))
            .with_state(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self { addr, state }
    }

    /// The `oidc.issuer_url` a config should point at.
    pub fn issuer_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_discovery(&self, mode: DiscoveryMode) {
        self.state.lock().unwrap().discovery = mode;
    }

    pub fn set_token(&self, mode: TokenMode) {
        self.state.lock().unwrap().token = mode;
    }

    pub fn set_jwks(&self, mode: JwksMode) {
        self.state.lock().unwrap().jwks = mode;
    }

    pub fn set_claims(&self, claims: IdTokenClaimSpec) {
        self.state.lock().unwrap().claims = claims;
    }

    /// Form-encoded bodies the token endpoint received, in order.
    pub fn token_requests(&self) -> Vec<String> {
        self.state.lock().unwrap().token_requests.clone()
    }

    /// Mint an ID token directly, for tests driving `validate_id_token` without
    /// going through the callback handler.
    pub fn mint_id_token(&self) -> String {
        let s = self.state.lock().unwrap();
        sign_id_token(&s.claims, &s.base_url, Some(TEST_KID))
    }

    /// The JWKS URI the mock advertises.
    pub fn jwks_uri(&self) -> String {
        format!("http://{}/jwks", self.addr)
    }
}

fn jwks_document() -> Value {
    json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": TEST_KID,
            "n": test_signing_key().modulus_b64url,
            "e": "AQAB",
        }]
    })
}

fn sign_id_token(spec: &IdTokenClaimSpec, base_url: &str, kid: Option<&str>) -> String {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

    let exp = (chrono::Utc::now() + chrono::Duration::seconds(spec.expires_in_secs)).timestamp();
    let mut claims = json!({
        "sub": spec.sub,
        "aud": spec.aud,
        "iss": spec.iss.clone().unwrap_or_else(|| base_url.to_string()),
        "exp": exp,
        "iat": chrono::Utc::now().timestamp(),
    });
    if let Some(email) = &spec.email {
        claims["email"] = json!(email);
    }
    if let Some(name) = &spec.name {
        claims["name"] = json!(name);
    }

    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(str::to_string);
    let key = EncodingKey::from_rsa_pem(test_signing_key().private_pem.as_bytes())
        .expect("test RSA PEM parses as an RS256 signing key");
    encode(&header, &claims, &key).expect("test ID token signs")
}

async fn discovery(State(state): State<Arc<Mutex<MockOidcState>>>) -> impl IntoResponse {
    let s = state.lock().unwrap();
    let base = s.base_url.clone();
    match s.discovery {
        DiscoveryMode::ServerError => {
            (StatusCode::INTERNAL_SERVER_ERROR, "issuer is down").into_response()
        }
        DiscoveryMode::Malformed => Json(json!({ "not": "discovery metadata" })).into_response(),
        DiscoveryMode::BadAuthorizationEndpoint => Json(json!({
            "issuer": base,
            "authorization_endpoint": "not a url at all",
            "token_endpoint": format!("{base}/token"),
            "jwks_uri": format!("{base}/jwks"),
        }))
        .into_response(),
        DiscoveryMode::Ok => Json(json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "jwks_uri": format!("{base}/jwks"),
        }))
        .into_response(),
    }
}

async fn jwks(State(state): State<Arc<Mutex<MockOidcState>>>) -> impl IntoResponse {
    let mode = state.lock().unwrap().jwks;
    match mode {
        JwksMode::ServerError => {
            (StatusCode::INTERNAL_SERVER_ERROR, "jwks unavailable").into_response()
        }
        JwksMode::NoKeysArray => Json(json!({ "no_keys_here": true })).into_response(),
        JwksMode::EmptyKeys => Json(json!({ "keys": [] })).into_response(),
        JwksMode::Ok => Json(jwks_document()).into_response(),
    }
}

async fn token(State(state): State<Arc<Mutex<MockOidcState>>>, body: String) -> impl IntoResponse {
    let mut s = state.lock().unwrap();
    s.token_requests.push(body);
    let base = s.base_url.clone();
    match s.token {
        TokenMode::BadRequest => {
            (StatusCode::BAD_REQUEST, "invalid_grant").into_response()
        }
        TokenMode::MissingIdToken => {
            Json(json!({ "access_token": "at", "token_type": "Bearer" })).into_response()
        }
        TokenMode::Garbage => Json(json!({
            "access_token": "at",
            "token_type": "Bearer",
            "id_token": "this-is-not-a-jwt",
        }))
        .into_response(),
        TokenMode::NoKid => Json(json!({
            "access_token": "at",
            "token_type": "Bearer",
            "id_token": sign_id_token(&s.claims, &base, None),
        }))
        .into_response(),
        TokenMode::UnknownKid => Json(json!({
            "access_token": "at",
            "token_type": "Bearer",
            "id_token": sign_id_token(&s.claims, &base, Some("a-kid-the-jwks-never-heard-of")),
        }))
        .into_response(),
        TokenMode::Ok => Json(json!({
            "access_token": "at",
            "token_type": "Bearer",
            "expires_in": 3600,
            "id_token": sign_id_token(&s.claims, &base, Some(TEST_KID)),
        }))
        .into_response(),
    }
}

/// Present only so the redirect target resolves if anything ever follows it.
/// modelrouter never fetches this server-side — the browser would.
async fn authorize() -> impl IntoResponse {
    (StatusCode::OK, "mock authorize page")
}
