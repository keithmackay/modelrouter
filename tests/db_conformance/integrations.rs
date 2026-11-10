//! Conformance scenarios: integrations repositories.

#[allow(unused_imports)]
use crate::harness::each_backend;

use modelrouter::db::models::{HookMetric, NewAdminUser, NewMcpServer, NewUser};
use modelrouter::db::repositories::webhook_callbacks::NewWebhookCallback;
use modelrouter::db::repositories::{AdminUserRepository, UserRepository};

fn webhook(name: &str) -> NewWebhookCallback {
    NewWebhookCallback {
        name: name.to_string(),
        url: format!("https://example.com/{name}"),
        events: r#"["completion"]"#.to_string(),
        secret_header_name: None,
        secret_header_value: None,
    }
}

fn mcp(name: &str, owner_user_id: Option<i64>) -> NewMcpServer {
    NewMcpServer {
        name: name.to_string(),
        url: format!("https://example.com/{name}"),
        description: None,
        owner_user_id,
    }
}

fn metric(hook: &str, invoked_at: &str, duration_ms: i64, success: bool) -> HookMetric {
    HookMetric {
        hook_name: hook.to_string(),
        invoked_at: invoked_at.to_string(),
        duration_ms,
        success,
    }
}

// --- webhook_callbacks ---

#[tokio::test]
async fn webhook_create_returns_the_stored_row() {
    each_backend(|db| async move {
        let w = db
            .create_webhook(NewWebhookCallback {
                secret_header_name: Some("X-Secret".into()),
                secret_header_value: Some("s3cret".into()),
                ..webhook("first")
            })
            .await
            .unwrap();
        assert!(w.id > 0);
        assert_eq!(w.name, "first");
        assert_eq!(w.url, "https://example.com/first");
        assert_eq!(w.events, r#"["completion"]"#);
        assert_eq!(w.secret_header_name.as_deref(), Some("X-Secret"));
        assert_eq!(w.secret_header_value.as_deref(), Some("s3cret"));
        assert!(w.enabled);
        // Stored as the RFC 3339 string it was written with.
        assert!(chrono::DateTime::parse_from_rfc3339(&w.created_at).is_ok(), "{}", w.created_at);

        let listed = db.list_webhooks().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, w.id);
        assert_eq!(listed[0].created_at, w.created_at);
        assert_eq!(listed[0].secret_header_value.as_deref(), Some("s3cret"));
    })
    .await;
}

#[tokio::test]
async fn webhook_lists_are_ordered_by_id_and_enabled_filters() {
    each_backend(|db| async move {
        let a = db.create_webhook(webhook("a")).await.unwrap();
        let b = db.create_webhook(webhook("b")).await.unwrap();
        let c = db.create_webhook(webhook("c")).await.unwrap();
        assert!(a.id < b.id && b.id < c.id);

        db.set_webhook_enabled(b.id, false).await.unwrap();

        let all: Vec<_> = db.list_webhooks().await.unwrap().into_iter().map(|w| (w.id, w.enabled)).collect();
        assert_eq!(all, vec![(a.id, true), (b.id, false), (c.id, true)]);
        let enabled: Vec<_> = db.list_enabled_webhooks().await.unwrap().into_iter().map(|w| w.id).collect();
        assert_eq!(enabled, vec![a.id, c.id]);

        db.set_webhook_enabled(b.id, true).await.unwrap();
        let enabled: Vec<_> = db.list_enabled_webhooks().await.unwrap().into_iter().map(|w| w.id).collect();
        assert_eq!(enabled, vec![a.id, b.id, c.id]);
    })
    .await;
}

#[tokio::test]
async fn webhook_delete_removes_only_that_row() {
    each_backend(|db| async move {
        let a = db.create_webhook(webhook("a")).await.unwrap();
        let b = db.create_webhook(webhook("b")).await.unwrap();
        db.delete_webhook(a.id).await.unwrap();
        let ids: Vec<_> = db.list_webhooks().await.unwrap().into_iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![b.id]);

        // Unknown ids are not an error, for delete or toggle.
        db.delete_webhook(9_999).await.unwrap();
        db.set_webhook_enabled(9_999, false).await.unwrap();
        assert_eq!(db.list_webhooks().await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn webhook_lists_are_empty_on_a_fresh_database() {
    each_backend(|db| async move {
        assert!(db.list_webhooks().await.unwrap().is_empty());
        assert!(db.list_enabled_webhooks().await.unwrap().is_empty());
    })
    .await;
}

// --- mcp_servers ---

#[tokio::test]
async fn mcp_create_get_and_list() {
    each_backend(|db| async move {
        let owner = UserRepository::create(&*db, NewUser { name: "alice".into(), email: None })
            .await
            .unwrap();
        let s = db
            .create_mcp_server(NewMcpServer {
                description: Some("does stuff".into()),
                ..mcp("one", Some(owner.id))
            })
            .await
            .unwrap();
        assert!(s.id > 0);
        assert_eq!(s.name, "one");
        assert_eq!(s.url, "https://example.com/one");
        assert_eq!(s.description.as_deref(), Some("does stuff"));
        assert!(s.enabled);
        assert_eq!(s.owner_user_id, Some(owner.id));
        assert!(!s.created_at.is_empty());

        let unowned = db.create_mcp_server(mcp("two", None)).await.unwrap();
        assert_eq!(unowned.owner_user_id, None);
        assert_eq!(unowned.description, None);

        let got = db.get_mcp_server(s.id).await.unwrap().expect("server exists");
        assert_eq!(got.name, "one");
        assert_eq!(got.created_at, s.created_at);
        assert_eq!(got.owner_user_id, Some(owner.id));
        assert!(db.get_mcp_server(9_999).await.unwrap().is_none());

        let ids: Vec<_> = db.list_mcp_servers().await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![s.id, unowned.id]);
    })
    .await;
}

#[tokio::test]
async fn mcp_names_are_unique() {
    each_backend(|db| async move {
        db.create_mcp_server(mcp("dup", None)).await.unwrap();
        assert!(db.create_mcp_server(mcp("dup", None)).await.is_err());
        assert_eq!(db.list_mcp_servers().await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn mcp_update_changes_only_the_given_fields() {
    each_backend(|db| async move {
        let s = db.create_mcp_server(mcp("orig", None)).await.unwrap();

        let u = db
            .update_mcp_server(s.id, None, Some("https://new.example.com".into()), None, Some(false))
            .await
            .unwrap()
            .expect("updated row");
        assert_eq!(u.name, "orig");
        assert_eq!(u.url, "https://new.example.com");
        assert_eq!(u.description, None);
        assert!(!u.enabled);
        assert_eq!(u.created_at, s.created_at);

        let u = db
            .update_mcp_server(s.id, Some("renamed".into()), None, Some("desc".into()), Some(true))
            .await
            .unwrap()
            .expect("updated row");
        assert_eq!(u.name, "renamed");
        assert_eq!(u.url, "https://new.example.com");
        assert_eq!(u.description.as_deref(), Some("desc"));
        assert!(u.enabled);

        // No fields: returns the current row unchanged.
        let same = db.update_mcp_server(s.id, None, None, None, None).await.unwrap().expect("row");
        assert_eq!(same.name, "renamed");
        assert_eq!(same.description.as_deref(), Some("desc"));

        let stored = db.get_mcp_server(s.id).await.unwrap().unwrap();
        assert_eq!(stored.name, "renamed");
        assert!(stored.enabled);
    })
    .await;
}

#[tokio::test]
async fn mcp_update_of_a_missing_server_is_none() {
    each_backend(|db| async move {
        assert!(db
            .update_mcp_server(9_999, Some("x".into()), None, None, Some(false))
            .await
            .unwrap()
            .is_none());
        assert!(db.update_mcp_server(9_999, None, None, None, None).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn mcp_update_to_a_taken_name_fails() {
    each_backend(|db| async move {
        db.create_mcp_server(mcp("taken", None)).await.unwrap();
        let s = db.create_mcp_server(mcp("other", None)).await.unwrap();
        assert!(db
            .update_mcp_server(s.id, Some("taken".into()), None, None, None)
            .await
            .is_err());
        assert_eq!(db.get_mcp_server(s.id).await.unwrap().unwrap().name, "other");
    })
    .await;
}

#[tokio::test]
async fn mcp_delete_reports_whether_a_row_went() {
    each_backend(|db| async move {
        let a = db.create_mcp_server(mcp("a", None)).await.unwrap();
        let b = db.create_mcp_server(mcp("b", None)).await.unwrap();
        assert!(db.delete_mcp_server(a.id).await.unwrap());
        assert!(!db.delete_mcp_server(a.id).await.unwrap());
        assert!(!db.delete_mcp_server(9_999).await.unwrap());
        let ids: Vec<_> = db.list_mcp_servers().await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![b.id]);
    })
    .await;
}

// --- hooks ---

#[tokio::test]
async fn hook_grant_and_revoke_permissions() {
    each_backend(|db| async move {
        assert!(!db.has_permission("redact", "modify_request").await.unwrap());

        db.grant_permission("redact", "modify_request", None).await.unwrap();
        assert!(db.has_permission("redact", "modify_request").await.unwrap());
        // Scoped to both the hook and the capability.
        assert!(!db.has_permission("redact", "block").await.unwrap());
        assert!(!db.has_permission("other", "modify_request").await.unwrap());

        // Granting again is a no-op, not a conflict.
        db.grant_permission("redact", "modify_request", None).await.unwrap();
        assert!(db.has_permission("redact", "modify_request").await.unwrap());

        db.revoke_permission("redact", "modify_request").await.unwrap();
        assert!(!db.has_permission("redact", "modify_request").await.unwrap());
        // Revoking what isn't there is fine too.
        db.revoke_permission("redact", "modify_request").await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn hook_grant_records_the_granting_admin() {
    each_backend(|db| async move {
        let admin = AdminUserRepository::create(
            &*db,
            NewAdminUser { name: "root".into(), password_hash: "x".into(), role: "superadmin".into() },
        )
        .await
        .unwrap();
        db.grant_permission("audit", "read_response", Some(admin.id)).await.unwrap();
        assert!(db.has_permission("audit", "read_response").await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn hook_metrics_list_newest_first_for_one_hook() {
    each_backend(|db| async move {
        assert!(db.list_metrics("h").await.unwrap().is_empty());

        db.record_metric(metric("h", "2026-01-01T00:00:01+00:00", 12, true)).await.unwrap();
        db.record_metric(metric("h", "2026-01-01T00:00:03+00:00", 5_000_000_000, false)).await.unwrap();
        db.record_metric(metric("h", "2026-01-01T00:00:02+00:00", 0, true)).await.unwrap();
        db.record_metric(metric("other", "2026-01-01T00:00:09+00:00", 7, true)).await.unwrap();

        let got: Vec<_> = db
            .list_metrics("h")
            .await
            .unwrap()
            .into_iter()
            .map(|m| (m.hook_name, m.invoked_at, m.duration_ms, m.success))
            .collect();
        assert_eq!(
            got,
            vec![
                ("h".into(), "2026-01-01T00:00:03+00:00".into(), 5_000_000_000, false),
                ("h".into(), "2026-01-01T00:00:02+00:00".into(), 0, true),
                ("h".into(), "2026-01-01T00:00:01+00:00".into(), 12, true),
            ]
        );
        assert_eq!(db.list_metrics("other").await.unwrap().len(), 1);
    })
    .await;
}

/// `serve` runs this at startup; it must succeed whether or not grants exist.
#[tokio::test]
async fn startup_hook_permission_sync_runs() {
    use modelrouter::config::schema::{HooksConfig, PipelineHookConfig};

    each_backend(|db| async move {
        let hooks = HooksConfig {
            lifecycle: vec![],
            pipeline: vec![PipelineHookConfig {
                name: "redact".into(),
                event: "pre_request".into(),
                exec: "/bin/true".into(),
                capabilities: vec!["modify_request".into(), "block".into()],
                timeout_secs: 5,
                fail_open: false,
            }],
        };
        modelrouter::hooks::permissions::sync_hook_permissions(&db, &hooks).await.unwrap();
        db.grant_permission("redact", "modify_request", None).await.unwrap();
        modelrouter::hooks::permissions::sync_hook_permissions(&db, &hooks).await.unwrap();
        // The sync only reports; it never grants.
        assert!(!db.has_permission("redact", "block").await.unwrap());
    })
    .await;
}
