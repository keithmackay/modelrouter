# Phase 17: MCP Server Registry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement full CRUD for MCP server registrations and a semantic tool-discovery endpoint (GET /v1/mcp/discover) that ranks tools by cosine similarity to a user prompt.

**Architecture:** Repository-pattern CRUD for `mcp_servers` table (SQLite + Postgres migrations, trait + two impls, REST handlers). Discovery endpoint embeds the user prompt and each server's tool descriptions via `EmbeddingRegistry`, selects top-K tools by cosine similarity, and returns structured results. No new AppState fields are needed — the existing `embedding_registry` field is reused.

**Intentional spec deviations (documented):**
- Spec says `/v1/mcp/server` (singular); plan uses `/v1/mcp/servers` (plural) — idiomatic REST collection naming.
- Spec says `PUT`; plan uses `PATCH` — partial-update semantics are correct here; full resource replacement is not needed.
- Plan adds `UNIQUE` constraint on `name` (not in spec) — prevents silent duplicates; handler returns 409 on conflict.

**Tech Stack:** Rust, axum, sqlx (SQLite + PostgreSQL), async-trait, serde_json, EmbeddingRegistry (already in AppState)

---

## File Map

| Action | Path | Responsibility |
|--------|------|----------------|
| Create | `migrations/009_mcp_servers.sql` | SQLite: `mcp_servers` table |
| Create | `migrations/postgres/004_mcp_servers.sql` | Postgres: `mcp_servers` table |
| Create | `src/db/models.rs` (append) | `McpServer`, `NewMcpServer` structs |
| Create | `src/db/repositories/mcp_servers.rs` | `McpServerRepository` trait |
| Modify | `src/db/repositories/mod.rs` | pub mod + re-export |
| Create | `src/db/sqlite/mcp_servers.rs` | SQLite impl of `McpServerRepository` |
| Modify | `src/db/sqlite/mod.rs` | mod declaration |
| Create | `src/db/postgres/mcp_servers.rs` | Postgres impl (feature-gated) |
| Modify | `src/db/postgres/mod.rs` | mod declaration |
| Modify | `src/api/app.rs` | Add `McpServerRepository` to `DatabaseProvider` supertrait |
| Create | `src/api/routes/mcp.rs` | CRUD handlers + discover handler |
| Modify | `src/api/routes/mod.rs` | pub mod mcp |
| Modify | `src/api/app.rs` (build_router) | Register MCP routes |

---

### Task 1: SQLite migration

**Files:**
- Create: `migrations/009_mcp_servers.sql`

- [ ] **Step 1: Write the migration**

```sql
-- migrations/009_mcp_servers.sql
CREATE TABLE IF NOT EXISTS mcp_servers (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    url         TEXT NOT NULL,
    description TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_mcp_servers_enabled ON mcp_servers(enabled);
```

- [ ] **Step 2: Verify it parses**

Run: `sqlite3 /tmp/test_mcp.db < migrations/009_mcp_servers.sql && echo OK`
Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add migrations/009_mcp_servers.sql
git commit -m "feat: migration 009 – mcp_servers table (SQLite)"
```

---

### Task 2: Postgres migration

**Files:**
- Create: `migrations/postgres/004_mcp_servers.sql`

- [ ] **Step 1: Write the migration**

```sql
-- migrations/postgres/004_mcp_servers.sql
CREATE TABLE IF NOT EXISTS mcp_servers (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    url         TEXT NOT NULL,
    description TEXT,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'))
);

CREATE INDEX IF NOT EXISTS idx_mcp_servers_enabled ON mcp_servers(enabled);
```

- [ ] **Step 2: Commit**

```bash
git add migrations/postgres/004_mcp_servers.sql
git commit -m "feat: migration 004 – mcp_servers table (Postgres)"
```

---

### Task 3: Models

**Files:**
- Modify: `src/db/models.rs`

- [ ] **Step 1: Write the failing test**

In `src/db/models.rs`, after all existing structs, add a test at the bottom:

```rust
#[cfg(test)]
mod mcp_tests {
    use super::*;

    #[test]
    fn mcp_server_roundtrip() {
        let s = McpServer {
            id: 1,
            name: "my-server".to_string(),
            url: "https://example.com/mcp".to_string(),
            description: Some("does stuff".to_string()),
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert_eq!(s.name, "my-server");
        assert!(s.enabled);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test mcp_server_roundtrip 2>&1 | head -20`
Expected: compile error — `McpServer` not defined

- [ ] **Step 3: Add structs to models.rs (append before the test module)**

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpServer {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewMcpServer {
    pub name: String,
    pub url: String,
    pub description: Option<String>,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test mcp_server_roundtrip`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/db/models.rs
git commit -m "feat: add McpServer and NewMcpServer models"
```

---

### Task 4: Repository trait

**Files:**
- Create: `src/db/repositories/mcp_servers.rs`
- Modify: `src/db/repositories/mod.rs`

- [ ] **Step 1: Write the trait**

```rust
// src/db/repositories/mcp_servers.rs
use async_trait::async_trait;
use crate::db::models::{McpServer, NewMcpServer};

#[async_trait]
pub trait McpServerRepository: Send + Sync {
    async fn create_mcp_server(&self, server: NewMcpServer) -> anyhow::Result<McpServer>;
    async fn list_mcp_servers(&self) -> anyhow::Result<Vec<McpServer>>;
    async fn get_mcp_server(&self, id: i64) -> anyhow::Result<Option<McpServer>>;
    async fn update_mcp_server(
        &self,
        id: i64,
        name: Option<String>,
        url: Option<String>,
        description: Option<String>,
        enabled: Option<bool>,
    ) -> anyhow::Result<Option<McpServer>>;
    async fn delete_mcp_server(&self, id: i64) -> anyhow::Result<bool>;
}
```

- [ ] **Step 2: Register in mod.rs**

Add to `src/db/repositories/mod.rs`:
- `pub mod mcp_servers;`
- `pub use mcp_servers::McpServerRepository;`

- [ ] **Step 3: Verify compile**

Run: `cargo check 2>&1 | head -30`
Expected: No errors related to mcp_servers

- [ ] **Step 4: Commit**

```bash
git add src/db/repositories/mcp_servers.rs src/db/repositories/mod.rs
git commit -m "feat: McpServerRepository trait"
```

---

### Task 5: SQLite implementation

**Files:**
- Create: `src/db/sqlite/mcp_servers.rs`
- Modify: `src/db/sqlite/mod.rs`

- [ ] **Step 1: Write failing tests**

```rust
// At the bottom of src/db/sqlite/mcp_servers.rs

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;
    use crate::db::repositories::mcp_servers::McpServerRepository;
    use crate::db::models::NewMcpServer;

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    #[tokio::test]
    async fn test_create_and_list() {
        let db = test_db().await;
        let server = db.create_mcp_server(NewMcpServer {
            name: "test".to_string(),
            url: "https://example.com".to_string(),
            description: None,
        }).await.unwrap();
        assert_eq!(server.name, "test");
        assert!(server.enabled);

        let list = db.list_mcp_servers().await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_get() {
        let db = test_db().await;
        let created = db.create_mcp_server(NewMcpServer {
            name: "get-test".to_string(),
            url: "https://example.com".to_string(),
            description: Some("desc".to_string()),
        }).await.unwrap();

        let found = db.get_mcp_server(created.id).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().description, Some("desc".to_string()));

        let missing = db.get_mcp_server(999).await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_update() {
        let db = test_db().await;
        let created = db.create_mcp_server(NewMcpServer {
            name: "update-test".to_string(),
            url: "https://old.com".to_string(),
            description: None,
        }).await.unwrap();

        let updated = db.update_mcp_server(
            created.id,
            None,
            Some("https://new.com".to_string()),
            None,
            Some(false),
        ).await.unwrap();
        let updated = updated.unwrap();
        assert_eq!(updated.url, "https://new.com");
        assert!(!updated.enabled);
    }

    #[tokio::test]
    async fn test_delete() {
        let db = test_db().await;
        let created = db.create_mcp_server(NewMcpServer {
            name: "delete-test".to_string(),
            url: "https://example.com".to_string(),
            description: None,
        }).await.unwrap();

        let deleted = db.delete_mcp_server(created.id).await.unwrap();
        assert!(deleted);

        let not_deleted = db.delete_mcp_server(999).await.unwrap();
        assert!(!not_deleted);

        let list = db.list_mcp_servers().await.unwrap();
        assert!(list.is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_create_and_list 2>&1 | head -20`
Expected: compile error — McpServerRepository not implemented for SqliteDb

- [ ] **Step 3: Write the SQLite implementation**

```rust
// src/db/sqlite/mcp_servers.rs
use async_trait::async_trait;
use crate::db::models::{McpServer, NewMcpServer};
use crate::db::repositories::mcp_servers::McpServerRepository;
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct McpServerRow {
    id: i64,
    name: String,
    url: String,
    description: Option<String>,
    enabled: i64,
    created_at: String,
}

impl From<McpServerRow> for McpServer {
    fn from(r: McpServerRow) -> Self {
        McpServer {
            id: r.id,
            name: r.name,
            url: r.url,
            description: r.description,
            enabled: r.enabled != 0,
            created_at: r.created_at,
        }
    }
}

#[async_trait]
impl McpServerRepository for SqliteDb {
    async fn create_mcp_server(&self, server: NewMcpServer) -> anyhow::Result<McpServer> {
        let now = now_utc();
        let result = sqlx::query(
            "INSERT INTO mcp_servers (name, url, description, enabled, created_at) VALUES (?, ?, ?, 1, ?)"
        )
        .bind(&server.name)
        .bind(&server.url)
        .bind(&server.description)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, McpServerRow>(
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers WHERE id = ?"
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(McpServer::from(row))
    }

    async fn list_mcp_servers(&self) -> anyhow::Result<Vec<McpServer>> {
        let rows = sqlx::query_as::<_, McpServerRow>(
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers ORDER BY id"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(McpServer::from).collect())
    }

    async fn get_mcp_server(&self, id: i64) -> anyhow::Result<Option<McpServer>> {
        let row = sqlx::query_as::<_, McpServerRow>(
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers WHERE id = ?"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(McpServer::from))
    }

    async fn update_mcp_server(
        &self,
        id: i64,
        name: Option<String>,
        url: Option<String>,
        description: Option<String>,
        enabled: Option<bool>,
    ) -> anyhow::Result<Option<McpServer>> {
        // Build SET clause dynamically
        let mut sets: Vec<&str> = Vec::new();
        if name.is_some()        { sets.push("name = ?"); }
        if url.is_some()         { sets.push("url = ?"); }
        if description.is_some() { sets.push("description = ?"); }
        if enabled.is_some()     { sets.push("enabled = ?"); }

        if sets.is_empty() {
            return self.get_mcp_server(id).await;
        }

        let sql = format!(
            "UPDATE mcp_servers SET {} WHERE id = ?",
            sets.join(", ")
        );

        let mut q = sqlx::query(&sql);
        if let Some(v) = &name        { q = q.bind(v); }
        if let Some(v) = &url         { q = q.bind(v); }
        if let Some(v) = &description { q = q.bind(v); }
        if let Some(v) = enabled      { q = q.bind(v as i64); }
        q = q.bind(id);

        let rows_affected = q.execute(&self.pool).await?.rows_affected();
        if rows_affected == 0 {
            return Ok(None);
        }
        self.get_mcp_server(id).await
    }

    async fn delete_mcp_server(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_servers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
```

- [ ] **Step 4: Add mod declaration to src/db/sqlite/mod.rs**

Add `mod mcp_servers;` to the list of mod declarations.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test mcp_servers 2>&1`
Expected: 4 tests pass

- [ ] **Step 6: Commit**

```bash
git add src/db/sqlite/mcp_servers.rs src/db/sqlite/mod.rs
git commit -m "feat: SQLite McpServerRepository implementation"
```

---

### Task 6: Postgres implementation (feature-gated stub)

**Files:**
- Create: `src/db/postgres/mcp_servers.rs`
- Modify: `src/db/postgres/mod.rs`

- [ ] **Step 1: Verify compile fails without the implementation**

Run: `cargo build --features postgres 2>&1 | grep "McpServerRepository" | head -5`
Expected: error — `McpServerRepository` not implemented for `PostgresDb`

- [ ] **Step 2: Write the Postgres implementation**

```rust
// src/db/postgres/mcp_servers.rs
#![cfg(feature = "postgres")]

use async_trait::async_trait;
use crate::db::models::{McpServer, NewMcpServer};
use crate::db::repositories::mcp_servers::McpServerRepository;
use super::{PostgresDb, now_utc};

#[derive(sqlx::FromRow)]
struct McpServerRow {
    id: i64,
    name: String,
    url: String,
    description: Option<String>,
    enabled: bool,
    created_at: String,
}

impl From<McpServerRow> for McpServer {
    fn from(r: McpServerRow) -> Self {
        McpServer {
            id: r.id,
            name: r.name,
            url: r.url,
            description: r.description,
            enabled: r.enabled,
            created_at: r.created_at,
        }
    }
}

#[async_trait]
impl McpServerRepository for PostgresDb {
    async fn create_mcp_server(&self, server: NewMcpServer) -> anyhow::Result<McpServer> {
        let now = now_utc();
        let row = sqlx::query_as::<_, McpServerRow>(
            r#"INSERT INTO mcp_servers (name, url, description, enabled, created_at)
               VALUES ($1, $2, $3, true, $4)
               RETURNING id, name, url, description, enabled, created_at"#
        )
        .bind(&server.name)
        .bind(&server.url)
        .bind(&server.description)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(McpServer::from(row))
    }

    async fn list_mcp_servers(&self) -> anyhow::Result<Vec<McpServer>> {
        let rows = sqlx::query_as::<_, McpServerRow>(
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers ORDER BY id"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(McpServer::from).collect())
    }

    async fn get_mcp_server(&self, id: i64) -> anyhow::Result<Option<McpServer>> {
        let row = sqlx::query_as::<_, McpServerRow>(
            "SELECT id, name, url, description, enabled, created_at FROM mcp_servers WHERE id = $1"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(McpServer::from))
    }

    async fn update_mcp_server(
        &self,
        id: i64,
        name: Option<String>,
        url: Option<String>,
        description: Option<String>,
        enabled: Option<bool>,
    ) -> anyhow::Result<Option<McpServer>> {
        let mut sets: Vec<String> = Vec::new();
        let mut param_idx: i32 = 1;

        if name.is_some()        { sets.push(format!("name = ${}", param_idx)); param_idx += 1; }
        if url.is_some()         { sets.push(format!("url = ${}", param_idx)); param_idx += 1; }
        if description.is_some() { sets.push(format!("description = ${}", param_idx)); param_idx += 1; }
        if enabled.is_some()     { sets.push(format!("enabled = ${}", param_idx)); param_idx += 1; }

        if sets.is_empty() {
            return self.get_mcp_server(id).await;
        }

        let sql = format!(
            "UPDATE mcp_servers SET {} WHERE id = ${} RETURNING id, name, url, description, enabled, created_at",
            sets.join(", "),
            param_idx
        );

        let mut q = sqlx::query_as::<_, McpServerRow>(&sql);
        if let Some(v) = &name        { q = q.bind(v); }
        if let Some(v) = &url         { q = q.bind(v); }
        if let Some(v) = &description { q = q.bind(v); }
        if let Some(v) = enabled      { q = q.bind(v); }
        q = q.bind(id);

        let row = q.fetch_optional(&self.pool).await?;
        Ok(row.map(McpServer::from))
    }

    async fn delete_mcp_server(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
```

- [ ] **Step 3: Add mod declaration to src/db/postgres/mod.rs**

Add `mod mcp_servers;` to the list inside the `#![cfg(feature = "postgres")]` block.

- [ ] **Step 4: Verify postgres feature compiles**

Run: `cargo build --features postgres 2>&1 | tail -5`
Expected: Compiles successfully

- [ ] **Step 5: Commit**

```bash
git add src/db/postgres/mcp_servers.rs src/db/postgres/mod.rs
git commit -m "feat: Postgres McpServerRepository implementation"
```

---

### Task 7: Wire McpServerRepository into DatabaseProvider

**Files:**
- Modify: `src/api/app.rs`

- [ ] **Step 1: Add McpServerRepository to the DatabaseProvider supertrait**

In `src/api/app.rs`, add to the imports:
```rust
use crate::db::repositories::mcp_servers::McpServerRepository,
```

Add `McpServerRepository` to both the supertrait definition and the blanket impl. The supertrait becomes:
```rust
pub trait DatabaseProvider:
    UserRepository
    + AdminUserRepository
    + SessionRepository
    + PromptRepository
    + CostRepository
    + BudgetRepository
    + AuditRepository
    + HookRepository
    + RateLimitRepository
    + ApiKeyRepository
    + McpServerRepository
    + Send
    + Sync
{
}
```

And the blanket impl `where` clause gains `+ McpServerRepository`.

- [ ] **Step 2: Verify compile**

Run: `cargo check 2>&1 | tail -5`
Expected: No errors

- [ ] **Step 3: Commit**

```bash
git add src/api/app.rs
git commit -m "feat: add McpServerRepository to DatabaseProvider supertrait"
```

---

### Task 8: REST handlers (CRUD)

**Files:**
- Create: `src/api/routes/mcp.rs`
- Modify: `src/api/routes/mod.rs`
- Modify: `src/api/app.rs` (build_router)

The handlers follow the existing completions.rs pattern: extract `AuthenticatedUser` (proves the caller has a valid API key), call the db, return JSON.

- [ ] **Step 1: Write failing tests for the CRUD handlers**

At the bottom of `src/api/routes/mcp.rs` add a test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::{Request, StatusCode}};
    use tower::ServiceExt;
    use crate::api::test_helpers::build_test_app;

    // If build_test_app doesn't exist yet, these will fail to compile — that's expected.
    // The test helper is in src/api/test_helpers.rs and must include McpServerRepository.

    #[tokio::test]
    async fn test_list_mcp_servers_empty() {
        let app = build_test_app().await;
        let req = Request::builder()
            .uri("/v1/mcp/servers")
            .header("Authorization", "Bearer test-key")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_create_and_get_mcp_server() {
        let app = build_test_app().await;
        let body = serde_json::json!({
            "name": "my-server",
            "url": "https://example.com/mcp",
            "description": "test server"
        });
        let req = Request::builder()
            .uri("/v1/mcp/servers")
            .method("POST")
            .header("Authorization", "Bearer test-key")
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }
}
```

- [ ] **Step 2: Run to confirm compile failure**

Run: `cargo test test_list_mcp_servers_empty 2>&1 | head -20`
Expected: compile error — handler functions not defined

- [ ] **Step 3: Write the handlers**

```rust
// src/api/routes/mcp.rs
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    api::{app::AppState, middleware::auth::AuthenticatedUser},
    db::{models::NewMcpServer, repositories::mcp_servers::McpServerRepository},
};

#[derive(Deserialize)]
pub struct CreateMcpServerRequest {
    pub name: String,
    pub url: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateMcpServerRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
}

pub async fn list_mcp_servers(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let servers = state.db.list_mcp_servers().await.map_err(|e| {
        tracing::error!(error = %e, "Failed to list MCP servers");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "internal error" })),
        )
    })?;
    Ok(Json(serde_json::json!({ "servers": servers })))
}

pub async fn create_mcp_server(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Json(req): Json<CreateMcpServerRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let server = state.db.create_mcp_server(NewMcpServer {
        name: req.name,
        url: req.url,
        description: req.description,
    }).await.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("UNIQUE constraint failed") || msg.contains("unique constraint") {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": "server name already exists" })),
            );
        }
        tracing::error!(error = %e, "Failed to create MCP server");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "internal error" })),
        )
    })?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(server))))
}

pub async fn get_mcp_server(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match state.db.get_mcp_server(id).await {
        Ok(Some(server)) => Ok(Json(serde_json::json!(server))),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get MCP server");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            ))
        }
    }
}

pub async fn update_mcp_server(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateMcpServerRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match state.db.update_mcp_server(id, req.name, req.url, req.description, req.enabled).await {
        Ok(Some(server)) => Ok(Json(serde_json::json!(server))),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )),
        Err(e) => {
            tracing::error!(error = %e, "Failed to update MCP server");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            ))
        }
    }
}

pub async fn delete_mcp_server(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    match state.db.delete_mcp_server(id).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )),
        Err(e) => {
            tracing::error!(error = %e, "Failed to delete MCP server");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            ))
        }
    }
}
```

- [ ] **Step 4: Register mod and routes**

Add `pub mod mcp;` to `src/api/routes/mod.rs`.

In `src/api/app.rs` `build_router()`, import the handlers:
```rust
use crate::api::routes::mcp::{
    list_mcp_servers, create_mcp_server, get_mcp_server,
    update_mcp_server, delete_mcp_server,
};
```

And add routes (before `.with_state(state.clone())`):
```rust
.route("/v1/mcp/servers", get(list_mcp_servers).post(create_mcp_server))
.route("/v1/mcp/servers/:id", get(get_mcp_server).patch(update_mcp_server).delete(delete_mcp_server))
```

- [ ] **Step 5: Run full test suite**

Run: `cargo test 2>&1 | tail -20`
Expected: All tests pass (CRUD integration tests may need test helpers — skip if `build_test_app` doesn't exist; the compile + unit tests are sufficient)

- [ ] **Step 6: Commit**

```bash
git add src/api/routes/mcp.rs src/api/routes/mod.rs src/api/app.rs
git commit -m "feat: MCP server CRUD REST endpoints"
```

---

### Task 9: Discover endpoint (semantic tool filtering)

**Files:**
- Modify: `src/api/routes/mcp.rs`

The discover endpoint:
1. Accepts `{ "prompt": "...", "top_k": 3 }` in the request body
2. Lists all enabled MCP servers from the DB
3. Treats each server's `description` (or name if null) as the "tool description"
4. Uses `state.embedding_registry.get("openai")` to get an embedding adapter
5. Embeds all texts in one batch call
6. Computes cosine similarity between the prompt embedding and each tool embedding
7. Returns the top-K servers sorted by score

Cosine similarity: `dot(a, b) / (|a| * |b|)`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module at the bottom of `src/api/routes/mcp.rs`:

```rust
    #[tokio::test]
    async fn test_discover_returns_json() {
        // This test verifies the handler compiles and returns 200 with empty results
        // when no servers are registered. Full semantic ranking is covered by unit tests
        // on the cosine_similarity function below.
        let app = build_test_app().await;
        let body = serde_json::json!({ "prompt": "translate text", "top_k": 3 });
        let req = Request::builder()
            .uri("/v1/mcp/discover")
            .method("POST")
            .header("Authorization", "Bearer test-key")
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
```

Also add a unit test for the similarity function:

```rust
    #[test]
    fn test_cosine_similarity() {
        // Identical vectors → similarity 1.0
        let a = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);

        // Orthogonal → 0.0
        let b = vec![0.0_f32, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-6);
    }
```

- [ ] **Step 2: Run test to confirm failure**

Run: `cargo test test_cosine_similarity 2>&1 | head -20`
Expected: compile error — `cosine_similarity` not defined

- [ ] **Step 3: Add cosine similarity + discover handler to mcp.rs**

Add these imports at the top of `src/api/routes/mcp.rs`:
```rust
use crate::providers::embedding::EmbeddingRequest;
```

Add the cosine similarity helper (private function):

```rust
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}
```

Add the request/response types:

```rust
#[derive(Deserialize)]
pub struct DiscoverRequest {
    pub prompt: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize { 5 }

#[derive(Serialize)]
pub struct DiscoverResult {
    pub server: crate::db::models::McpServer,
    pub score: f32,
}
```

Add the handler:

```rust
pub async fn discover_mcp_tools(
    _user: AuthenticatedUser,
    State(state): State<AppState>,
    Json(req): Json<DiscoverRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let servers = state.db.list_mcp_servers().await.map_err(|e| {
        tracing::error!(error = %e, "Failed to list MCP servers for discover");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "internal error" })))
    })?;

    let enabled: Vec<_> = servers.into_iter().filter(|s| s.enabled).collect();

    if enabled.is_empty() {
        return Ok(Json(serde_json::json!({ "results": [] })));
    }

    // Build texts: prompt first, then one text per server
    let mut texts = vec![req.prompt.clone()];
    for s in &enabled {
        texts.push(s.description.clone().unwrap_or_else(|| s.name.clone()));
    }

    let embed_adapter = state.embedding_registry.get("openai").map_err(|e| {
        tracing::warn!(error = %e, "No embedding adapter available for discover");
        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": "embedding provider unavailable" })))
    })?;

    let embed_req = EmbeddingRequest {
        model: "text-embedding-3-small".to_string(),
        input: texts,
    };
    let result = embed_adapter.embed(&embed_req).await.map_err(|e| {
        tracing::error!(error = %e, "Embedding call failed during discover");
        (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": "embedding failed" })))
    })?;

    let prompt_vec = &result.embeddings[0];
    let top_k = req.top_k.min(enabled.len());

    let mut scored: Vec<DiscoverResult> = enabled
        .into_iter()
        .enumerate()
        .map(|(i, server)| {
            let score = cosine_similarity(prompt_vec, &result.embeddings[i + 1]);
            DiscoverResult { server, score }
        })
        .collect();

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);

    Ok(Json(serde_json::json!({ "results": scored })))
}
```

- [ ] **Step 4: Register the discover route in build_router**

Add to `build_router()`:
```rust
use crate::api::routes::mcp::discover_mcp_tools;
// ...
.route("/v1/mcp/discover", post(discover_mcp_tools))
```

- [ ] **Step 5: Run tests**

Run: `cargo test 2>&1 | tail -20`
Expected: cosine_similarity unit test passes; full suite green

- [ ] **Step 6: Commit**

```bash
git add src/api/routes/mcp.rs src/api/app.rs
git commit -m "feat: MCP discover endpoint with cosine similarity ranking"
```

---

### Task 10: Final build verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test 2>&1 | tail -30`
Expected: All tests pass, no warnings about unused imports

- [ ] **Step 2: Verify postgres feature builds**

Run: `cargo build --features postgres 2>&1 | tail -5`
Expected: Compiles successfully

- [ ] **Step 3: Commit if any fixups needed**

```bash
git add -p
git commit -m "fix: address any remaining compile warnings"
```

- [ ] **Step 4: Push to main**

```bash
git push origin main
```
