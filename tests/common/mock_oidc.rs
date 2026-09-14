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
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

/// TEST-ONLY RSA-2048 private key, generated for this file and used nowhere
/// else. It signs the ID tokens the mock issuer hands back so the production
/// `validate_id_token` path runs for real against a matching JWKS. It grants
/// access to nothing: no service, deployment or environment has ever trusted
/// it. Do not reuse it outside this test mock.
const TEST_RSA_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCwtZNG5PL0abYr
FGgGqaSMxxYmI4/ohyl4Zo+zzhdH/wh9UK3YuDQLyvvYdAzVkdMaZlgTPx5vWSxa
XxVHjuvMKOUOhNAuJHKyiO0KVfSdN5LE9Qbmn1Ta77AYteMdY26spma9vi5xRKIR
UK26cOsDdGHQfIRDrkxEvlVzGVGe5joeDsVFhLozRCwlwgufouI3MixGGGAjtCsj
3ZjYU449q1k/sfqJa3Xmp6MACWn6KCCjZLL2H88pxToh9bRT9zE6o7QqmL1zrfUz
UdZazlhDmEg9n0JaHehFrcdRVT2VgxtuP9xjRs7garAqxJiRWfOmyywGDOUlPxqp
M7z68sFZAgMBAAECggEADaDra2/T9xrCFH996GmkCcF/CVD709y8y47pbV4rOH/I
aVXUGp9WUESG+PI2XTBifio/h7h6Ae94Nr4Xl43DzNqok96VacGg4PkjgwUPGSME
nFEjh5zpP/t1cupwSj91dTZePSsSTOUUtM4qn/L+hSUwOBKqZsfmFPUzBO4CeAU7
YVtXHlqKufIBSneSyO23N0qCvfq7Yk+3R1DDii2scNq2PKRKu2J7eMZQuSgjmQY2
7wlfp+uzXSXw8+Yzc+BGbU2/1+rtFiNRAXE+IPnObKgTwWsIvDkAqr2DUIHL1dLh
5QMg5LMqo2qZSuYhcm+H7laiV3jxcoWJrT/fd7jaMwKBgQD519tFsmBKMZ9+dfFP
7vT0EhVqinQYCIv3FV2xf/60Apz/8D/ccBf2GBMkeH84+cKWNldpqjm+ed26WMCz
/yzl4d0/Qk36J9HJQNsxzqgrI1qDFUoE7L330y0dEdWBsomx1H/9oRm8iGHOK9Gs
xMXERddgjS4mK4VYKlgIszbSzwKBgQC1EFnqghDbqMUOTJPd7ZViTkCWqEunmb8a
tKxhQe8rskyDvELrixiNZJWvQRH3NDgPngnIJ5dk/47cM4JfnxRGjnp6MQ853CK3
i4jFaCrD1KyrEvhegz4crQrhZxMgnomyifizsO1qVyljYExSj3klVOuOLsh3CCk5
zro9gYhTVwKBgG4xZzOpRdDTbB4RlNoFcaJIa4uu/x8ufdT/ZnCIHGV2lZpIc1Id
WmQfICpAvxP5DHrGAu3Gt2ssQsASrwN0c2/8m2FwNAY2E8/ovASOuhs0n5IbDKd5
Zxvr1wTwPbPTc+mr6LuLl1dQ65pMN1E1BGjZyPF7szQAk/Jb0rIboP1/AoGBAJrG
rXYvVOXQcRJ2F3iAXVA5gDDJEFLmtFvJ0gkZaa+6rHl39uSOdKB5ORMk1oywkLOY
7tewMFRfuOk3Bt1iiNx/cub9BPz61pp7pqDJGLVqGWfrwXBZVEEDEuf3Snx5yU9b
bcN9HJXoiDKw4M06Y96rpuhVyXsm+Ma3lrB5B+XlAoGBAKhzAnOZ8aaFpvDypnZ2
qxrn+84KcQ5LdMRHc8ki+knTaRRgaNKQYEXExpem/ONX6n1/jamWbexlRGa/ozAk
UDTNzn4nyaKbyPR3WUN9x0uK0XssA556RqKxeQZjLHuBn2juvpsOFUCgOEfafpz/
pNdOtuRmRT3ZCnI0FQ3Dedmr
-----END PRIVATE KEY-----";

/// Base64url modulus of the key above — the public half, restated here so the
/// JWKS the mock serves is derivable without an RSA crate in the dev tree.
const TEST_RSA_MODULUS_B64URL: &str = "sLWTRuTy9Gm2KxRoBqmkjMcWJiOP6IcpeGaPs84XR_8IfVCt2Lg0C8r72HQM1ZHTGmZYEz8eb1ksWl8VR47rzCjlDoTQLiRysojtClX0nTeSxPUG5p9U2u-wGLXjHWNurKZmvb4ucUSiEVCtunDrA3Rh0HyEQ65MRL5VcxlRnuY6Hg7FRYS6M0QsJcILn6LiNzIsRhhgI7QrI92Y2FOOPatZP7H6iWt15qejAAlp-iggo2Sy9h_PKcU6IfW0U_cxOqO0Kpi9c631M1HWWs5YQ5hIPZ9CWh3oRa3HUVU9lYMbbj_cY0bO4GqwKsSYkVnzpsssBgzlJT8aqTO8-vLBWQ";

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
            "n": TEST_RSA_MODULUS_B64URL,
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
    let key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY_PEM.as_bytes())
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
