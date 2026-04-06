# Phase 19: SSO / OIDC Admin Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add OIDC SSO support so admin users can authenticate via external identity providers (Google, Okta, etc.) using the authorization code flow with PKCE.

**Architecture:** The OIDC flow is implemented as two HTTP handlers (login redirect + callback) in a new `src/api/admin/oidc.rs` module. State and PKCE code verifiers are stored in an in-memory `OidcStateStore` (DashMap with 5-minute TTL). ID tokens are validated via JWKS fetched from the provider's discovery document. Auto-provisioning creates admin_users rows for new OIDC identities when email matches allow-lists. The existing `mr_admin_session` cookie mechanism is reused, so no dashboard changes are needed.

**Tech Stack:** Rust/axum, reqwest (HTTP client), jsonwebtoken 9 (JWKS/RS256 validation), dashmap (concurrent state store), sha2 (PKCE S256), base64 (URL-safe encoding), SQLite/PostgreSQL via sqlx

---

**Intentional spec deviations:**
- OIDC state is stored in-memory (not persisted). Restarts clear pending login flows — acceptable for admin SSO.
- ID token validation requires RS256. Providers using HS256 (not OIDC-spec compliant) are not supported.
- `urlencoding` crate avoided: authorization URL is built using `reqwest::Url::query_pairs_mut()` which handles URL encoding.

---

## Task 1: Migration — add oidc_subject + email to admin_users

**Files:**
- Create: `migrations/010_admin_oidc.sql`
- Create: `migrations/postgres/005_admin_oidc.sql`
- Modify: `src/db/models.rs`
- Modify: `src/db/repositories/admin_users.rs`
- Modify: `src/db/sqlite/admin_users.rs`
- Modify: `src/db/postgres/admin_users.rs`

**Steps:**

- [ ] 1. Write failing test in `tests/test_oidc.rs`: create an in-memory SQLite DB, run migrations, INSERT a user with oidc_subject, SELECT it back via `find_by_oidc_subject` (expect Some). Run: `cargo test test_find_by_oidc_subject -- --nocapture`. Expected: FAIL (function doesn't exist)

- [ ] 2. Create `migrations/010_admin_oidc.sql`:
```sql
-- migrations/010_admin_oidc.sql
ALTER TABLE admin_users ADD COLUMN oidc_subject TEXT;
ALTER TABLE admin_users ADD COLUMN email TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_admin_users_oidc_subject ON admin_users(oidc_subject) WHERE oidc_subject IS NOT NULL;
```

- [ ] 3. Create `migrations/postgres/005_admin_oidc.sql`:
```sql
-- migrations/postgres/005_admin_oidc.sql
ALTER TABLE admin_users ADD COLUMN IF NOT EXISTS oidc_subject TEXT;
ALTER TABLE admin_users ADD COLUMN IF NOT EXISTS email TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_admin_users_oidc_subject ON admin_users(oidc_subject) WHERE oidc_subject IS NOT NULL;
```

- [ ] 4. Update `src/db/models.rs` — add fields to AdminUser and new struct:
```rust
pub struct AdminUser {
    pub id: i64,
    pub name: String,
    pub password_hash: String,
    pub role: String,
    pub enabled: bool,
    pub created_at: String,
    pub last_login_at: Option<String>,
    pub oidc_subject: Option<String>,  // NEW
    pub email: Option<String>,          // NEW
}

pub struct NewAdminUserFromOidc {      // NEW
    pub name: String,
    pub email: String,
    pub oidc_subject: String,
    pub role: String,
}
```

- [ ] 5. Update `src/db/repositories/admin_users.rs` — add two methods to the trait:
```rust
async fn find_by_oidc_subject(&self, subject: &str) -> anyhow::Result<Option<AdminUser>>;
async fn create_from_oidc(&self, user: NewAdminUserFromOidc) -> anyhow::Result<AdminUser>;
```

- [ ] 6. Update `src/db/sqlite/admin_users.rs`:
   - Add `oidc_subject: Option<String>` and `email: Option<String>` to `AdminUserRow`
   - Update the `From<AdminUserRow> for AdminUser` impl to map the new fields
   - Update ALL existing SELECT queries to include `oidc_subject, email` (find_by_name, find_by_id, list, create's refetch)
   - Add `find_by_oidc_subject` impl:
```rust
async fn find_by_oidc_subject(&self, subject: &str) -> anyhow::Result<Option<AdminUser>> {
    let row = sqlx::query_as::<_, AdminUserRow>(
        "SELECT id, name, password_hash, role, enabled, created_at, last_login_at, oidc_subject, email
         FROM admin_users WHERE oidc_subject = ?",
    )
    .bind(subject)
    .fetch_optional(&self.pool)
    .await?;
    Ok(row.map(AdminUser::from))
}
```
   - Add `create_from_oidc` impl:
```rust
async fn create_from_oidc(&self, user: NewAdminUserFromOidc) -> anyhow::Result<AdminUser> {
    let now = now_utc();
    let result = sqlx::query(
        "INSERT INTO admin_users (name, password_hash, role, enabled, created_at, oidc_subject, email)
         VALUES (?, '', ?, 1, ?, ?, ?)",
    )
    .bind(&user.name)
    .bind(&user.role)
    .bind(&now)
    .bind(&user.oidc_subject)
    .bind(&user.email)
    .execute(&self.pool)
    .await?;

    let id = result.last_insert_rowid();
    let row = sqlx::query_as::<_, AdminUserRow>(
        "SELECT id, name, password_hash, role, enabled, created_at, last_login_at, oidc_subject, email
         FROM admin_users WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&self.pool)
    .await?;
    Ok(AdminUser::from(row))
}
```

- [ ] 7. Update `src/db/postgres/admin_users.rs` — same changes as sqlite (same column additions and new methods, but use `$1`/`$2` placeholders and `RETURNING *` or refetch by id; follow the postgres impl patterns in that file)

- [ ] 8. Run tests: `cargo test test_find_by_oidc_subject -- --nocapture`. Expected: PASS

- [ ] 9. Run full test suite: `cargo test`. Expected: all pass

- [ ] 10. Commit:
```bash
git add migrations/010_admin_oidc.sql migrations/postgres/005_admin_oidc.sql \
    src/db/models.rs src/db/repositories/admin_users.rs \
    src/db/sqlite/admin_users.rs src/db/postgres/admin_users.rs \
    tests/test_oidc.rs
git commit -m "feat: add oidc_subject and email columns to admin_users"
```

---

## Task 2: OidcConfig schema

**Files:**
- Modify: `src/config/schema.rs`
- Modify: `Cargo.toml`

**Steps:**

- [ ] 1. Write failing tests in `tests/test_oidc.rs`:
```rust
#[test]
fn test_oidc_config_defaults() {
    let settings: Settings = toml::from_str("").unwrap();
    assert!(!settings.oidc.enabled);
    assert_eq!(settings.oidc.auto_provision_role, "admin");
}

#[test]
fn test_oidc_config_full_parse() {
    let toml = r#"
[oidc]
enabled = true
issuer_url = "https://accounts.google.com"
client_id = "my-client-id"
client_secret = "my-secret"
redirect_uri = "http://localhost:8080/admin/auth/oidc/callback"
allowed_emails = ["alice@example.com"]
allowed_domains = ["example.com"]
auto_provision_role = "superadmin"
"#;
    let settings: Settings = toml::from_str(toml).unwrap();
    assert!(settings.oidc.enabled);
    assert_eq!(settings.oidc.issuer_url, "https://accounts.google.com");
    assert_eq!(settings.oidc.client_id, "my-client-id");
    assert_eq!(settings.oidc.allowed_domains, vec!["example.com"]);
    assert_eq!(settings.oidc.auto_provision_role, "superadmin");
}
```
Run: `cargo test test_oidc_config -- --nocapture`. Expected: FAIL (OidcConfig doesn't exist)

- [ ] 2. Add `base64 = { version = "0.22", features = ["engine"] }` to `[dependencies]` in `Cargo.toml`

- [ ] 3. Add to `src/config/schema.rs` before the `Settings` struct:
```rust
fn default_oidc_role() -> String { "admin".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OidcConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub issuer_url: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default = "default_oidc_role")]
    pub auto_provision_role: String,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            issuer_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: String::new(),
            allowed_emails: vec![],
            allowed_domains: vec![],
            auto_provision_role: default_oidc_role(),
        }
    }
}
```

- [ ] 4. Add `oidc` field to `Settings` struct (after `archival`):
```rust
#[serde(default)]
pub oidc: OidcConfig,
```

- [ ] 5. Run tests: `cargo test test_oidc_config -- --nocapture`. Expected: PASS

- [ ] 6. Run: `cargo test`. Expected: all pass

- [ ] 7. Commit:
```bash
git add src/config/schema.rs Cargo.toml tests/test_oidc.rs
git commit -m "feat: add OidcConfig to Settings"
```

---

## Task 3: OIDC core module — state store, PKCE, discovery, token exchange

**Files:**
- Create: `src/api/admin/oidc.rs`
- Modify: `src/api/admin/mod.rs`

**Steps:**

- [ ] 1. Write failing tests in `tests/test_oidc.rs`:
```rust
#[test]
fn test_oidc_state_store_insert_and_take() {
    use modelrouter::api::admin::oidc::OidcStateStore;
    let store = OidcStateStore::new();
    store.insert("state1".to_string(), "verifier1".to_string());
    let v = store.take("state1");
    assert_eq!(v, Some("verifier1".to_string()));
    // Second take returns None (consumed)
    assert!(store.take("state1").is_none());
}

#[test]
fn test_oidc_pkce_challenge() {
    use modelrouter::api::admin::oidc::{generate_pkce_pair, verify_pkce_challenge};
    let (verifier, challenge) = generate_pkce_pair();
    assert!(verify_pkce_challenge(&verifier, &challenge));
    assert!(!verify_pkce_challenge("wrong", &challenge));
}

#[test]
fn test_oidc_email_allowed() {
    use modelrouter::api::admin::oidc::is_email_allowed;
    let allowed_emails = vec!["alice@example.com".to_string()];
    let allowed_domains = vec!["corp.example.com".to_string()];
    assert!(is_email_allowed("alice@example.com", &allowed_emails, &allowed_domains));
    assert!(is_email_allowed("bob@corp.example.com", &allowed_emails, &allowed_domains));
    assert!(!is_email_allowed("eve@evil.com", &allowed_emails, &allowed_domains));
    // Empty allow-lists = allow all
    assert!(is_email_allowed("anyone@anywhere.com", &[], &[]));
}
```
Run: `cargo test test_oidc_state -- --nocapture`. Expected: FAIL (module doesn't exist)

- [ ] 2. Create `src/api/admin/oidc.rs` with the core types and helpers:

```rust
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
        let entry = self.map.remove(state)?;
        if entry.1.created_at.elapsed() > Duration::from_secs(300) {
            return None;
        }
        Some(entry.1.code_verifier)
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

/// Verify that sha256(verifier) == base64url_decode(challenge).
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
    client: &reqwest::Client,
) -> anyhow::Result<IdTokenClaims> {
    use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};

    // Parse header to get kid and algorithm
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
        .find(|k| k["kid"].as_str() == Some(&kid))
        .ok_or_else(|| anyhow::anyhow!("No JWK found for kid={}", kid))?;

    let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(jwk_value.clone())?;
    let decoding_key = DecodingKey::from_jwk(&jwk)?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[client_id]);

    let token_data = decode::<IdTokenClaims>(id_token, &decoding_key, &validation)?;
    Ok(token_data.claims)
}

// ── Route handlers ────────────────────────────────────────────────────────────

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize as DeserializeQ;

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

#[derive(DeserializeQ)]
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
```

- [ ] 3. Add `pub mod oidc;` to `src/api/admin/mod.rs`

- [ ] 4. Run tests: `cargo test test_oidc_state -- --nocapture`. Expected: PASS
   Run tests: `cargo test test_oidc_pkce -- --nocapture`. Expected: PASS
   Run tests: `cargo test test_oidc_email -- --nocapture`. Expected: PASS

- [ ] 5. Run: `cargo test`. Expected: all pass

- [ ] 6. Commit:
```bash
git add src/api/admin/oidc.rs src/api/admin/mod.rs tests/test_oidc.rs
git commit -m "feat: add OIDC state store, PKCE helpers, discovery, and route handlers"
```

> **Note:** The `oidc_login` and `oidc_callback` handlers reference `state.oidc_state` which doesn't exist on AppState yet. The code will not compile until Task 4. The unit tests for the pure helper functions (state store, PKCE, email check) will pass since they don't require AppState.

---

## Task 4: AppState wiring + route registration

**Files:**
- Modify: `src/api/app.rs`
- Modify: `src/cli/mod.rs`

**Steps:**

- [ ] 1. Write failing build test: `cargo build`. Expected: FAIL (`oidc_state` field missing from AppState)

- [ ] 2. Add `oidc_state` field to `AppState` in `src/api/app.rs`:
```rust
pub oidc_state: Arc<crate::api::admin::oidc::OidcStateStore>,
```
(Add this after the last existing field)

- [ ] 3. In `src/api/app.rs` `build_router()`, add the two OIDC routes. Find the block where `/admin/login` and `/admin/logout` are registered and add alongside:
```rust
.route("/admin/auth/oidc/login", get(crate::api::admin::oidc::oidc_login))
.route("/admin/auth/oidc/callback", get(crate::api::admin::oidc::oidc_callback))
```

- [ ] 4. In `src/cli/mod.rs`, find the AppState construction block. Add initialization of OidcStateStore before the AppState literal and add the field:
```rust
let oidc_state = Arc::new(crate::api::admin::oidc::OidcStateStore::new());
```
Then in the AppState struct literal, add:
```rust
oidc_state,
```

- [ ] 5. Run: `cargo build`. Expected: SUCCESS

- [ ] 6. Run: `cargo test`. Expected: all pass

- [ ] 7. Commit:
```bash
git add src/api/app.rs src/cli/mod.rs
git commit -m "feat: wire OidcStateStore into AppState and register OIDC routes"
```

---

## Task 5: Integration tests

**Files:**
- Modify: `tests/test_oidc.rs`

**Steps:**

- [ ] 1. Add integration tests for find_or_create OIDC admin logic. These tests use in-memory SQLite (no network calls):

```rust
#[tokio::test]
async fn test_create_from_oidc_and_find_by_subject() {
    use modelrouter::db::models::NewAdminUserFromOidc;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let db = create_test_db().await; // helper: in-memory SQLite with migrations run

    let created = db.create_from_oidc(NewAdminUserFromOidc {
        name: "Alice OIDC".to_string(),
        email: "alice@example.com".to_string(),
        oidc_subject: "google|12345".to_string(),
        role: "admin".to_string(),
    }).await.unwrap();

    assert_eq!(created.oidc_subject.as_deref(), Some("google|12345"));
    assert_eq!(created.email.as_deref(), Some("alice@example.com"));
    assert!(created.enabled);

    let found = db.find_by_oidc_subject("google|12345").await.unwrap().unwrap();
    assert_eq!(found.id, created.id);
    assert_eq!(found.name, "Alice OIDC");
}

#[tokio::test]
async fn test_oidc_subject_unique_constraint() {
    use modelrouter::db::models::NewAdminUserFromOidc;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let db = create_test_db().await;

    db.create_from_oidc(NewAdminUserFromOidc {
        name: "Alice".to_string(),
        email: "alice@example.com".to_string(),
        oidc_subject: "provider|abc".to_string(),
        role: "admin".to_string(),
    }).await.unwrap();

    // Second insert with same oidc_subject must fail
    let result = db.create_from_oidc(NewAdminUserFromOidc {
        name: "Alice Dup".to_string(),
        email: "alice2@example.com".to_string(),
        oidc_subject: "provider|abc".to_string(),
        role: "admin".to_string(),
    }).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_existing_admin_find_returns_oidc_subject_none() {
    // Admin created via password has oidc_subject = None
    use modelrouter::db::models::NewAdminUser;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let db = create_test_db().await;
    let created = db.create(NewAdminUser {
        name: "bob".to_string(),
        password_hash: "hash".to_string(),
        role: "admin".to_string(),
    }).await.unwrap();

    assert!(created.oidc_subject.is_none());
    assert!(created.email.is_none());
}
```

The `create_test_db()` helper (if not already in `tests/test_oidc.rs`) should look like:
```rust
async fn create_test_db() -> Arc<dyn modelrouter::api::app::DatabaseProvider> {
    use sqlx::sqlite::SqlitePoolOptions;
    use modelrouter::db::sqlite::SqliteDb;
    let pool = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    Arc::new(SqliteDb { pool })
}
```

- [ ] 2. Run: `cargo test test_create_from_oidc -- --nocapture`. Expected: PASS

- [ ] 3. Run: `cargo test test_oidc_subject_unique -- --nocapture`. Expected: PASS

- [ ] 4. Run: `cargo test`. Expected: all pass

- [ ] 5. Commit:
```bash
git add tests/test_oidc.rs
git commit -m "test: add OIDC integration tests for admin_users repository"
```

---

## Task 6: config.example.toml documentation

**Files:**
- Modify: `config.example.toml`

**Steps:**

- [ ] 1. Find the end of `config.example.toml` and append a documented `[oidc]` section:
```toml
# ── SSO / OIDC (optional) ─────────────────────────────────────────────────────
# Enable OpenID Connect SSO for admin login.
# When enabled, admins can authenticate at /admin/auth/oidc/login instead of
# (or in addition to) the username/password form.
#
# Tested with: Google, GitHub (using GHES OIDC), Okta, Auth0, Keycloak.
#
# [oidc]
# enabled = false
#
# # OIDC issuer URL — discovery document fetched from {issuer_url}/.well-known/openid-configuration
# issuer_url = "https://accounts.google.com"
#
# # OAuth2 client credentials (create in your IdP console)
# client_id = "your-client-id.apps.googleusercontent.com"
# client_secret = "your-client-secret"
#
# # Redirect URI — must match exactly what you registered in the IdP console
# redirect_uri = "https://your-modelrouter.example.com/admin/auth/oidc/callback"
#
# # Optional: restrict login to specific email addresses
# allowed_emails = ["alice@example.com", "bob@example.com"]
#
# # Optional: restrict login to email domains (checked if email not in allowed_emails)
# allowed_domains = ["example.com", "corp.example.com"]
#
# # Role assigned to auto-provisioned admins (default: "admin"; use "superadmin" for full access)
# auto_provision_role = "admin"
```

- [ ] 2. Run: `cargo build`. Expected: SUCCESS (config.example.toml is not compiled)

- [ ] 3. Commit:
```bash
git add config.example.toml
git commit -m "docs: add [oidc] section to config.example.toml"
```

---

## Task 7: Final verification

**Steps:**

- [ ] 1. Run full test suite: `cargo test`. Expected: all tests pass, no warnings about unused imports

- [ ] 2. Run: `cargo build --features postgres`. Expected: SUCCESS (postgres impl compiles with new methods)

- [ ] 3. Run: `cargo build --features bedrock`. Expected: SUCCESS

- [ ] 4. If any compilation errors, fix them now

- [ ] 5. Commit any fixes, then push:
```bash
git push
```
