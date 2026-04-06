use std::time::{Duration, Instant};
use dashmap::DashMap;
use serde::Deserialize;
use base64::Engine;

// ── State store ───────────────────────────────────────────────────────────────

struct PendingOidcState {
    code_verifier: String,
    created_at: Instant,
}

pub struct OidcStateStore {
    map: DashMap<String, PendingOidcState>,
}

impl OidcStateStore {
    pub fn new() -> Self {
        Self { map: DashMap::new() }
    }

    pub fn insert(&self, state: String, code_verifier: String) {
        self.cleanup_expired();
        self.map.insert(state, PendingOidcState {
            code_verifier,
            created_at: Instant::now(),
        });
    }

    /// Consume the state entry, returning the code_verifier if it exists and hasn't expired.
    pub fn take(&self, state: &str) -> Option<String> {
        // Check expiry before consuming the entry
        let expired = self.map.get(state)
            .map(|e| e.created_at.elapsed() > Duration::from_secs(300))
            .unwrap_or(false);
        if expired {
            self.map.remove(state);
            return None;
        }
        self.map.remove(state).map(|e| e.1.code_verifier)
    }

    fn cleanup_expired(&self) {
        self.map.retain(|_, v| v.created_at.elapsed() <= Duration::from_secs(300));
    }
}

// ── PKCE helpers ──────────────────────────────────────────────────────────────

/// Returns (code_verifier, code_challenge) pair using S256 method.
pub fn generate_pkce_pair() -> (String, String) {
    use rand::RngCore;
    use sha2::{Digest, Sha256};
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&hash);
    (verifier, challenge)
}

/// Verify that sha256(verifier) == base64url(challenge).
pub fn verify_pkce_challenge(verifier: &str, challenge: &str) -> bool {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(verifier.as_bytes());
    let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&hash);
    expected == challenge
}

/// Generate a random state token (32 bytes, base64url encoded).
pub fn generate_state() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

// ── Email allowlist check ─────────────────────────────────────────────────────

/// Returns true if the email is permitted.
/// If both lists are empty, all emails are permitted.
pub fn is_email_allowed(email: &str, allowed_emails: &[String], allowed_domains: &[String]) -> bool {
    if allowed_emails.is_empty() && allowed_domains.is_empty() {
        return true;
    }
    if allowed_emails.contains(&email.to_string()) {
        return true;
    }
    if let Some(domain) = email.split('@').nth(1) {
        if allowed_domains.contains(&domain.to_string()) {
            return true;
        }
    }
    false
}

// ── OIDC discovery ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OidcDiscovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
}

pub async fn fetch_discovery(issuer_url: &str, client: &reqwest::Client) -> anyhow::Result<OidcDiscovery> {
    let url = format!("{}/.well-known/openid-configuration", issuer_url.trim_end_matches('/'));
    let discovery = client.get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json::<OidcDiscovery>()
        .await?;
    Ok(discovery)
}

// ── Token exchange ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub id_token: String,
}

pub async fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    client: &reqwest::Client,
) -> anyhow::Result<TokenResponse> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code_verifier", code_verifier),
    ];
    let resp = client
        .post(token_endpoint)
        .form(&params)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json::<TokenResponse>()
        .await?;
    Ok(resp)
}

// ── ID token validation ───────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct IdTokenClaims {
    pub sub: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub exp: usize,
}

/// Fetch JWKS from jwks_uri, find the key matching the `kid` in the id_token header,
/// then validate the ID token signature and claims.
pub async fn validate_id_token(
    id_token: &str,
    jwks_uri: &str,
    client_id: &str,
    issuer_url: &str,
    client: &reqwest::Client,
) -> anyhow::Result<IdTokenClaims> {
    use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};

    // Parse header to get kid
    let header = decode_header(id_token)?;
    let kid = header.kid.ok_or_else(|| anyhow::anyhow!("ID token missing kid"))?;

    // Fetch JWKS
    let jwks: serde_json::Value = client
        .get(jwks_uri)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // Find the matching JWK
    let keys = jwks["keys"].as_array()
        .ok_or_else(|| anyhow::anyhow!("JWKS missing keys array"))?;
    let jwk_value = keys.iter()
        .find(|k| k["kid"].as_str() == Some(kid.as_str()))
        .ok_or_else(|| anyhow::anyhow!("No JWK found for kid={}", kid))?;

    let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(jwk_value.clone())?;
    let decoding_key = DecodingKey::from_jwk(&jwk)?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[client_id]);
    validation.set_issuer(&[issuer_url]);

    let token_data = decode::<IdTokenClaims>(id_token, &decoding_key, &validation)?;
    Ok(token_data.claims)
}

// ── Route handlers ────────────────────────────────────────────────────────────

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
};

use crate::api::app::AppState;

/// GET /admin/auth/oidc/login
/// Redirects the browser to the OIDC provider's authorization endpoint.
pub async fn oidc_login(State(state): State<AppState>) -> Response {
    let cfg = &state.settings.oidc;
    if !cfg.enabled {
        return (StatusCode::NOT_FOUND, "OIDC not enabled").into_response();
    }

    let http = reqwest::Client::new();
    let discovery = match fetch_discovery(&cfg.issuer_url, &http).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "OIDC discovery failed");
            return (StatusCode::BAD_GATEWAY, "OIDC discovery failed").into_response();
        }
    };

    let (verifier, challenge) = generate_pkce_pair();
    let state_token = generate_state();
    state.oidc_state.insert(state_token.clone(), verifier);

    let auth_url = {
        let mut url = match reqwest::Url::parse(&discovery.authorization_endpoint) {
            Ok(u) => u,
            Err(_) => return (StatusCode::BAD_GATEWAY, "Invalid authorization_endpoint").into_response(),
        };
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &cfg.client_id)
            .append_pair("redirect_uri", &cfg.redirect_uri)
            .append_pair("scope", "openid email profile")
            .append_pair("state", &state_token)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        url.to_string()
    };

    Redirect::temporary(&auth_url).into_response()
}

#[derive(serde::Deserialize)]
pub struct OidcCallbackQuery {
    pub code: String,
    pub state: String,
}

/// GET /admin/auth/oidc/callback
pub async fn oidc_callback(
    State(state): State<AppState>,
    Query(params): Query<OidcCallbackQuery>,
) -> Response {
    use crate::db::repositories::admin_users::AdminUserRepository;
    use crate::db::models::NewAdminUserFromOidc;
    use super::auth::{AdminClaims, issue_jwt};

    let cfg = &state.settings.oidc;
    if !cfg.enabled {
        return (StatusCode::NOT_FOUND, "OIDC not enabled").into_response();
    }

    // Verify state and retrieve code_verifier
    let code_verifier = match state.oidc_state.take(&params.state) {
        Some(v) => v,
        None => return (StatusCode::BAD_REQUEST, "Invalid or expired state").into_response(),
    };

    let http = reqwest::Client::new();

    let discovery = match fetch_discovery(&cfg.issuer_url, &http).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "OIDC discovery failed in callback");
            return (StatusCode::BAD_GATEWAY, "OIDC discovery failed").into_response();
        }
    };

    let token_resp = match exchange_code(
        &discovery.token_endpoint,
        &cfg.client_id,
        &cfg.client_secret,
        &params.code,
        &code_verifier,
        &cfg.redirect_uri,
        &http,
    ).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "OIDC token exchange failed");
            return (StatusCode::BAD_GATEWAY, "Token exchange failed").into_response();
        }
    };

    let claims = match validate_id_token(
        &token_resp.id_token,
        &discovery.jwks_uri,
        &cfg.client_id,
        &cfg.issuer_url,
        &http,
    ).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "OIDC ID token validation failed");
            return (StatusCode::UNAUTHORIZED, "ID token validation failed").into_response();
        }
    };

    let email = claims.email.unwrap_or_default();
    if !is_email_allowed(&email, &cfg.allowed_emails, &cfg.allowed_domains) {
        tracing::warn!(email = %email, "OIDC login rejected: email not in allow-list");
        return (StatusCode::FORBIDDEN, "Email not permitted").into_response();
    }

    // Find or create admin user
    let admin = match AdminUserRepository::find_by_oidc_subject(&*state.db, &claims.sub).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            // Auto-provision
            let name = claims.name.unwrap_or_else(|| email.clone());
            match AdminUserRepository::create_from_oidc(&*state.db, NewAdminUserFromOidc {
                name,
                email: email.clone(),
                oidc_subject: claims.sub.clone(),
                role: cfg.auto_provision_role.clone(),
            }).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to auto-provision OIDC admin");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Provisioning failed").into_response();
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "DB error looking up OIDC subject");
            return (StatusCode::INTERNAL_SERVER_ERROR, "DB error").into_response();
        }
    };

    if !admin.enabled {
        return (StatusCode::FORBIDDEN, "Account disabled").into_response();
    }

    // Issue JWT session cookie
    let exp = (chrono::Utc::now() + chrono::Duration::minutes(state.settings.auth.jwt_expiry_mins))
        .timestamp() as usize;
    let jwt_claims = AdminClaims { sub: admin.id, name: admin.name, role: admin.role, exp };
    let token = match issue_jwt(&jwt_claims, &state.settings.auth.jwt_secret) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "Failed to issue JWT");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
        }
    };

    // Update last login (fire and forget)
    let _ = AdminUserRepository::update_last_login(&*state.db, admin.id).await;

    let cookie = format!("mr_admin_session={}; Path=/; HttpOnly; SameSite=Lax", token);
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/admin".to_string()),
            (header::SET_COOKIE, cookie),
        ],
    ).into_response()
}
