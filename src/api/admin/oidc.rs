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
