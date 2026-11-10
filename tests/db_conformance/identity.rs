//! Conformance scenarios: identity repositories.

#[allow(unused_imports)]
use crate::harness::each_backend;

use std::time::Duration;

use modelrouter::api::app::DatabaseProvider;
use modelrouter::db::models::{
    NewAdminUser, NewAdminUserFromOidc, NewApiKey, NewAuditLogEntry, NewSession, NewUser, User,
};
use modelrouter::db::repositories::admin_users::AdminUserRepository;
use modelrouter::db::repositories::audit::AuditRepository;
use modelrouter::db::repositories::users::UserRepository;

/// Rows ordered by `created_at` need distinct timestamps.
async fn tick() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

async fn user(db: &dyn DatabaseProvider, name: &str) -> User {
    UserRepository::create(db, NewUser { name: name.into(), email: None })
        .await
        .unwrap()
}

fn key(user_id: i64, hash: &str, project: Option<&str>) -> NewApiKey {
    NewApiKey {
        user_id,
        key_hash: hash.into(),
        label: Some(format!("label-{hash}")),
        expires_at: None,
        project: project.map(str::to_string),
        session_window_secs: None,
    }
}

fn admin(name: &str, role: &str) -> NewAdminUser {
    NewAdminUser { name: name.into(), password_hash: format!("hash-{name}"), role: role.into() }
}

// ── users ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn user_create_and_lookup() {
    each_backend(|db| async move {
        let created = UserRepository::create(
            &*db,
            NewUser { name: "alice".into(), email: Some("alice@example.com".into()) },
        )
        .await
        .unwrap();
        assert!(created.id > 0);
        assert_eq!(created.name, "alice");
        assert_eq!(created.email.as_deref(), Some("alice@example.com"));
        assert!(created.enabled);
        assert_eq!(created.metadata, "{}");
        assert!(created.spend_reset_at.is_none());
        assert!(created.api_key_id.is_none());
        assert!(!created.created_at.is_empty());

        let by_name = UserRepository::find_by_name(&*db, "alice").await.unwrap().unwrap();
        assert_eq!(by_name.id, created.id);
        assert_eq!(by_name.email, created.email);
        assert_eq!(by_name.created_at, created.created_at);

        let by_id = UserRepository::find_by_id(&*db, created.id).await.unwrap().unwrap();
        assert_eq!(by_id.name, "alice");

        assert!(UserRepository::find_by_name(&*db, "nobody").await.unwrap().is_none());
        assert!(UserRepository::find_by_id(&*db, created.id + 1000).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn user_duplicate_name_is_rejected() {
    each_backend(|db| async move {
        user(&*db, "bob").await;
        let dup = UserRepository::create(&*db, NewUser { name: "bob".into(), email: None }).await;
        assert!(dup.is_err(), "duplicate user name must fail");
        assert_eq!(UserRepository::list(&*db).await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn user_enable_disable_round_trip_and_list_order() {
    each_backend(|db| async move {
        let a = user(&*db, "a").await;
        tick().await;
        let b = user(&*db, "b").await;
        tick().await;
        let c = user(&*db, "c").await;

        UserRepository::set_enabled(&*db, c.id, false).await.unwrap();
        assert!(!UserRepository::find_by_id(&*db, c.id).await.unwrap().unwrap().enabled);

        // Enabled first, then newest first.
        let names: Vec<String> =
            UserRepository::list(&*db).await.unwrap().into_iter().map(|u| u.name).collect();
        assert_eq!(names, ["b", "a", "c"]);

        UserRepository::set_enabled(&*db, c.id, true).await.unwrap();
        assert!(UserRepository::find_by_id(&*db, c.id).await.unwrap().unwrap().enabled);
        let names: Vec<String> =
            UserRepository::list(&*db).await.unwrap().into_iter().map(|u| u.name).collect();
        assert_eq!(names, ["c", "b", "a"]);

        // Unknown id is a silent no-op.
        UserRepository::set_enabled(&*db, a.id + b.id + c.id + 1000, false).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn user_reset_spend_sets_timestamp() {
    each_backend(|db| async move {
        let u = user(&*db, "spender").await;
        let other = user(&*db, "other").await;
        UserRepository::reset_spend(&*db, u.id).await.unwrap();

        let after = UserRepository::find_by_id(&*db, u.id).await.unwrap().unwrap();
        let ts = after.spend_reset_at.expect("spend_reset_at set");
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "rfc3339: {ts}");
        assert!(UserRepository::find_by_id(&*db, other.id)
            .await
            .unwrap()
            .unwrap()
            .spend_reset_at
            .is_none());
    })
    .await;
}

// ── api keys ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn api_key_create_and_lookup_by_hash() {
    each_backend(|db| async move {
        let u = user(&*db, "keyholder").await;
        let created = db
            .create_api_key(NewApiKey {
                user_id: u.id,
                key_hash: "h1".into(),
                label: Some("primary".into()),
                expires_at: Some("2099-01-01T00:00:00+00:00".into()),
                project: Some("proj".into()),
                session_window_secs: Some(3600),
            })
            .await
            .unwrap();
        assert!(created.id > 0);
        assert_eq!(created.user_id, u.id);
        assert_eq!(created.key_hash, "h1");
        assert_eq!(created.label.as_deref(), Some("primary"));
        assert!(created.enabled);
        assert_eq!(created.expires_at.as_deref(), Some("2099-01-01T00:00:00+00:00"));
        assert_eq!(created.project.as_deref(), Some("proj"));
        assert!(created.disabled_at.is_none());
        assert_eq!(created.session_window_secs, Some(3600));

        let found = db.find_api_key_by_hash("h1").await.unwrap().unwrap();
        assert_eq!(found.id, created.id);
        assert_eq!(found.session_window_secs, Some(3600));
        assert_eq!(found.created_at, created.created_at);

        assert!(db.find_api_key_by_hash("missing").await.unwrap().is_none());

        // A window wider than i32 survives the round trip.
        let wide = db
            .create_api_key(NewApiKey { session_window_secs: Some(10_000_000_000), ..key(u.id, "h2", None) })
            .await
            .unwrap();
        assert_eq!(wide.session_window_secs, Some(10_000_000_000));
    })
    .await;
}

#[tokio::test]
async fn api_key_lookup_skips_disabled_but_not_expired() {
    each_backend(|db| async move {
        let u = user(&*db, "k").await;
        let k = db.create_api_key(key(u.id, "live", None)).await.unwrap();

        db.set_key_enabled(k.id, false).await.unwrap();
        assert!(db.find_api_key_by_hash("live").await.unwrap().is_none());
        db.set_key_enabled(k.id, true).await.unwrap();
        assert!(db.find_api_key_by_hash("live").await.unwrap().is_some());

        db.revoke_api_key(k.id).await.unwrap();
        assert!(db.find_api_key_by_hash("live").await.unwrap().is_none());

        // Expiry is enforced by the auth layer, not by the lookup.
        db.create_api_key(NewApiKey {
            expires_at: Some("2000-01-01T00:00:00+00:00".into()),
            ..key(u.id, "expired", None)
        })
        .await
        .unwrap();
        let expired = db.find_api_key_by_hash("expired").await.unwrap().unwrap();
        assert_eq!(expired.expires_at.as_deref(), Some("2000-01-01T00:00:00+00:00"));
    })
    .await;
}

#[tokio::test]
async fn api_key_duplicate_hash_is_rejected() {
    each_backend(|db| async move {
        let u = user(&*db, "dup").await;
        db.create_api_key(key(u.id, "same", None)).await.unwrap();
        assert!(db.create_api_key(key(u.id, "same", Some("p"))).await.is_err());
        assert_eq!(db.list_api_keys_for_user(u.id).await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn api_key_listing() {
    each_backend(|db| async move {
        let a = user(&*db, "a").await;
        let b = user(&*db, "b").await;
        let a1 = db.create_api_key(key(a.id, "a1", None)).await.unwrap();
        tick().await;
        let b1 = db.create_api_key(key(b.id, "b1", None)).await.unwrap();
        tick().await;
        let a2 = db.create_api_key(key(a.id, "a2", Some("p"))).await.unwrap();

        // Per user: by id, any status.
        db.set_key_enabled(a1.id, false).await.unwrap();
        let ids: Vec<i64> =
            db.list_api_keys_for_user(a.id).await.unwrap().into_iter().map(|k| k.id).collect();
        assert_eq!(ids, [a1.id, a2.id]);
        assert!(db.list_api_keys_for_user(a.id + b.id + 1000).await.unwrap().is_empty());

        // All: enabled first, then newest first.
        let ids: Vec<i64> =
            db.list_all_api_keys().await.unwrap().into_iter().map(|k| k.id).collect();
        assert_eq!(ids, [a2.id, b1.id, a1.id]);
    })
    .await;
}

#[tokio::test]
async fn api_key_disable_all_for_user_is_scoped() {
    each_backend(|db| async move {
        let a = user(&*db, "a").await;
        let b = user(&*db, "b").await;
        db.create_api_key(key(a.id, "a1", None)).await.unwrap();
        db.create_api_key(key(a.id, "a2", Some("p"))).await.unwrap();
        db.create_api_key(key(b.id, "b1", None)).await.unwrap();

        db.disable_all_keys_for_user(a.id).await.unwrap();
        assert!(db.list_api_keys_for_user(a.id).await.unwrap().iter().all(|k| !k.enabled));
        // Bulk disable does not stamp disabled_at.
        assert!(db.list_api_keys_for_user(a.id).await.unwrap().iter().all(|k| k.disabled_at.is_none()));
        assert!(db.find_api_key_by_hash("b1").await.unwrap().unwrap().enabled);
    })
    .await;
}

#[tokio::test]
async fn api_key_disable_stamps_and_revoke_does_not() {
    each_backend(|db| async move {
        let u = user(&*db, "u").await;
        let d = db.create_api_key(key(u.id, "d", None)).await.unwrap();
        let r = db.create_api_key(key(u.id, "r", Some("p"))).await.unwrap();

        db.disable_key(d.id).await.unwrap();
        db.revoke_api_key(r.id).await.unwrap();
        let keys = db.list_api_keys_for_user(u.id).await.unwrap();
        let d2 = keys.iter().find(|k| k.id == d.id).unwrap();
        let r2 = keys.iter().find(|k| k.id == r.id).unwrap();
        assert!(!d2.enabled);
        let ts = d2.disabled_at.as_deref().expect("disabled_at set");
        assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "rfc3339: {ts}");
        assert!(!r2.enabled);
        assert!(r2.disabled_at.is_none());

        // Re-enabling leaves the disabled_at stamp in place.
        db.set_key_enabled(d.id, true).await.unwrap();
        let d3 = db.find_api_key_by_hash("d").await.unwrap().unwrap();
        assert!(d3.enabled);
        assert!(d3.disabled_at.is_some());
    })
    .await;
}

#[tokio::test]
async fn api_key_rotation_within_a_project_group() {
    each_backend(|db| async move {
        let u = user(&*db, "rotator").await;
        let other = user(&*db, "other").await;
        assert!(db.find_key_by_user_project(u.id, None).await.unwrap().is_none());
        assert!(db.find_key_by_user_project(u.id, Some("p")).await.unwrap().is_none());

        let old = db.create_api_key(key(u.id, "old", Some("p"))).await.unwrap();
        let bare = db.create_api_key(key(u.id, "bare", None)).await.unwrap();
        db.create_api_key(key(other.id, "theirs", Some("p"))).await.unwrap();

        // Rotate: issue a replacement, disable the old one.
        tick().await;
        let new = db.create_api_key(key(u.id, "new", Some("p"))).await.unwrap();
        db.disable_key(old.id).await.unwrap();

        // Disabled keys still count as belonging to the group.
        let found = db.find_key_by_user_project(u.id, Some("p")).await.unwrap().unwrap();
        assert!(found.id == old.id || found.id == new.id);
        assert_eq!(db.find_key_by_user_project(u.id, None).await.unwrap().unwrap().id, bare.id);
        assert!(db.find_key_by_user_project(u.id, Some("q")).await.unwrap().is_none());

        let group: Vec<i64> = db
            .list_keys_for_group(u.id, Some("p"))
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.id)
            .collect();
        assert_eq!(group, [new.id, old.id]);
        let bare_group: Vec<i64> =
            db.list_keys_for_group(u.id, None).await.unwrap().into_iter().map(|k| k.id).collect();
        assert_eq!(bare_group, [bare.id]);
        assert!(db.list_keys_for_group(u.id, Some("q")).await.unwrap().is_empty());

        // A group whose keys are all disabled still lists and resolves.
        db.disable_key(new.id).await.unwrap();
        let ids: Vec<i64> = db
            .list_keys_for_group(u.id, Some("p"))
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.id)
            .collect();
        assert_eq!(ids, [new.id, old.id]);
        assert!(db.find_key_by_user_project(u.id, Some("p")).await.unwrap().is_some());
    })
    .await;
}

// ── admin users ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn admin_create_and_lookup() {
    each_backend(|db| async move {
        let created = AdminUserRepository::create(&*db, admin("root", "superadmin")).await.unwrap();
        assert!(created.id > 0);
        assert_eq!(created.name, "root");
        assert_eq!(created.password_hash, "hash-root");
        assert_eq!(created.role, "superadmin");
        assert!(created.enabled);
        assert!(created.last_login_at.is_none());
        assert!(created.oidc_subject.is_none());
        assert!(created.email.is_none());

        let by_name = AdminUserRepository::find_by_name(&*db, "root").await.unwrap().unwrap();
        assert_eq!(by_name.id, created.id);
        assert_eq!(by_name.created_at, created.created_at);
        let by_id = AdminUserRepository::find_by_id(&*db, created.id).await.unwrap().unwrap();
        assert_eq!(by_id.name, "root");

        assert!(AdminUserRepository::find_by_name(&*db, "ghost").await.unwrap().is_none());
        assert!(AdminUserRepository::find_by_id(&*db, created.id + 1000).await.unwrap().is_none());

        assert!(AdminUserRepository::create(&*db, admin("root", "viewer")).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn admin_list_enable_delete() {
    each_backend(|db| async move {
        let a = AdminUserRepository::create(&*db, admin("a", "viewer")).await.unwrap();
        let b = AdminUserRepository::create(&*db, admin("b", "admin")).await.unwrap();

        AdminUserRepository::set_enabled(&*db, a.id, false).await.unwrap();
        assert!(!AdminUserRepository::find_by_id(&*db, a.id).await.unwrap().unwrap().enabled);
        AdminUserRepository::set_enabled(&*db, a.id, true).await.unwrap();
        assert!(AdminUserRepository::find_by_id(&*db, a.id).await.unwrap().unwrap().enabled);

        // By id regardless of status.
        AdminUserRepository::set_enabled(&*db, a.id, false).await.unwrap();
        let ids: Vec<i64> =
            AdminUserRepository::list(&*db).await.unwrap().into_iter().map(|u| u.id).collect();
        assert_eq!(ids, [a.id, b.id]);

        AdminUserRepository::delete(&*db, a.id).await.unwrap();
        assert!(AdminUserRepository::find_by_id(&*db, a.id).await.unwrap().is_none());
        let ids: Vec<i64> =
            AdminUserRepository::list(&*db).await.unwrap().into_iter().map(|u| u.id).collect();
        assert_eq!(ids, [b.id]);
        // Deleting a missing row is a no-op.
        AdminUserRepository::delete(&*db, a.id).await.unwrap();

        // The name is free again.
        AdminUserRepository::create(&*db, admin("a", "viewer")).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn admin_last_login_and_password_rotation() {
    each_backend(|db| async move {
        let a = AdminUserRepository::create(&*db, admin("ops", "admin")).await.unwrap();
        AdminUserRepository::update_last_login(&*db, a.id).await.unwrap();
        let after = AdminUserRepository::find_by_id(&*db, a.id).await.unwrap().unwrap();
        let ts = after.last_login_at.expect("last_login_at set");
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "rfc3339: {ts}");

        AdminUserRepository::update_password_hash(&*db, a.id, "rotated").await.unwrap();
        let after = AdminUserRepository::find_by_name(&*db, "ops").await.unwrap().unwrap();
        assert_eq!(after.password_hash, "rotated");
        assert_eq!(after.role, "admin");
    })
    .await;
}

#[tokio::test]
async fn admin_oidc_create_and_lookup() {
    each_backend(|db| async move {
        assert!(db.find_by_oidc_subject("sub-1").await.unwrap().is_none());
        let created = db
            .create_from_oidc(NewAdminUserFromOidc {
                name: "sso".into(),
                email: "sso@example.com".into(),
                oidc_subject: "sub-1".into(),
                role: "viewer".into(),
            })
            .await
            .unwrap();
        assert_eq!(created.password_hash, "");
        assert_eq!(created.oidc_subject.as_deref(), Some("sub-1"));
        assert_eq!(created.email.as_deref(), Some("sso@example.com"));
        assert_eq!(created.role, "viewer");
        assert!(created.enabled);

        let found = db.find_by_oidc_subject("sub-1").await.unwrap().unwrap();
        assert_eq!(found.id, created.id);

        // Subjects are unique; password-only admins share the NULL subject freely.
        let dup = db
            .create_from_oidc(NewAdminUserFromOidc {
                name: "sso2".into(),
                email: "x@example.com".into(),
                oidc_subject: "sub-1".into(),
                role: "viewer".into(),
            })
            .await;
        assert!(dup.is_err(), "duplicate oidc subject must fail");
        AdminUserRepository::create(&*db, admin("p1", "viewer")).await.unwrap();
        AdminUserRepository::create(&*db, admin("p2", "viewer")).await.unwrap();
    })
    .await;
}

// ── sessions ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn session_find_or_create() {
    each_backend(|db| async move {
        let u = user(&*db, "s").await;
        let other = user(&*db, "t").await;

        let s1 = db
            .find_or_create(NewSession {
                user_id: u.id,
                external_id: Some("ext".into()),
                project: Some("p".into()),
            })
            .await
            .unwrap();
        assert!(s1.id > 0);
        assert_eq!(s1.user_id, u.id);
        assert_eq!(s1.external_id.as_deref(), Some("ext"));
        assert_eq!(s1.project.as_deref(), Some("p"));
        assert_eq!(s1.metadata, "{}");
        assert_eq!(s1.created_at, s1.last_seen);

        // Same user + external id returns the existing row, project ignored.
        let again = db
            .find_or_create(NewSession { user_id: u.id, external_id: Some("ext".into()), project: None })
            .await
            .unwrap();
        assert_eq!(again.id, s1.id);
        assert_eq!(again.project.as_deref(), Some("p"));

        // The external id is scoped to the user.
        let theirs = db
            .find_or_create(NewSession { user_id: other.id, external_id: Some("ext".into()), project: None })
            .await
            .unwrap();
        assert_ne!(theirs.id, s1.id);

        // No external id: always a fresh session.
        let anon1 = db
            .find_or_create(NewSession { user_id: u.id, external_id: None, project: None })
            .await
            .unwrap();
        let anon2 = db
            .find_or_create(NewSession { user_id: u.id, external_id: None, project: None })
            .await
            .unwrap();
        assert_ne!(anon1.id, anon2.id);
        assert!(anon1.external_id.is_none());
    })
    .await;
}

#[tokio::test]
async fn session_update_last_seen() {
    each_backend(|db| async move {
        let u = user(&*db, "s").await;
        let s = db
            .find_or_create(NewSession { user_id: u.id, external_id: Some("e".into()), project: None })
            .await
            .unwrap();
        tick().await;
        db.update_last_seen(s.id).await.unwrap();
        let after = db
            .find_or_create(NewSession { user_id: u.id, external_id: Some("e".into()), project: None })
            .await
            .unwrap();
        assert_eq!(after.id, s.id);
        assert_eq!(after.created_at, s.created_at);
        assert!(after.last_seen > s.last_seen, "{} > {}", after.last_seen, s.last_seen);
    })
    .await;
}

// ── rate limits ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn rate_limit_counts_per_user_and_window() {
    each_backend(|db| async move {
        let a = user(&*db, "a").await;
        let b = user(&*db, "b").await;

        assert_eq!(db.increment_and_get_count(a.id, "2026-01-01T00:00").await.unwrap(), 1);
        assert_eq!(db.increment_and_get_count(a.id, "2026-01-01T00:00").await.unwrap(), 2);
        assert_eq!(db.increment_and_get_count(a.id, "2026-01-01T00:00").await.unwrap(), 3);
        assert_eq!(db.increment_and_get_count(a.id, "2026-01-01T00:01").await.unwrap(), 1);
        assert_eq!(db.increment_and_get_count(b.id, "2026-01-01T00:00").await.unwrap(), 1);
    })
    .await;
}

#[tokio::test]
async fn rate_limit_cleanup_drops_older_windows() {
    each_backend(|db| async move {
        let a = user(&*db, "a").await;
        let b = user(&*db, "b").await;
        db.increment_and_get_count(a.id, "2026-01-01T00:00").await.unwrap();
        db.increment_and_get_count(b.id, "2026-01-01T00:00").await.unwrap();
        db.increment_and_get_count(a.id, "2026-01-01T00:01").await.unwrap();
        db.increment_and_get_count(a.id, "2026-01-01T00:02").await.unwrap();

        // Strictly older than the cutoff.
        assert_eq!(db.cleanup_old_windows("2026-01-01T00:01").await.unwrap(), 2);
        assert_eq!(db.cleanup_old_windows("2026-01-01T00:01").await.unwrap(), 0);

        // A dropped window starts over; a kept one keeps counting.
        assert_eq!(db.increment_and_get_count(a.id, "2026-01-01T00:00").await.unwrap(), 1);
        assert_eq!(db.increment_and_get_count(a.id, "2026-01-01T00:01").await.unwrap(), 2);
    })
    .await;
}

// ── audit ────────────────────────────────────────────────────────────────────

fn entry(actor_id: Option<i64>, actor: &str, action: &str) -> NewAuditLogEntry {
    NewAuditLogEntry {
        actor_id,
        actor_name: actor.into(),
        action: action.into(),
        target: Some(format!("target-{action}")),
        before_json: Some(r#"{"enabled":true}"#.into()),
        after_json: Some(r#"{"enabled":false}"#.into()),
    }
}

#[tokio::test]
async fn audit_create_round_trips() {
    each_backend(|db| async move {
        let a = AdminUserRepository::create(&*db, admin("auditor", "superadmin")).await.unwrap();
        let e = AuditRepository::create(&*db, entry(Some(a.id), "auditor", "user.disable"))
            .await
            .unwrap();
        assert!(e.id > 0);
        assert_eq!(e.actor_id, Some(a.id));
        assert_eq!(e.actor_name, "auditor");
        assert_eq!(e.action, "user.disable");
        assert_eq!(e.target.as_deref(), Some("target-user.disable"));
        assert_eq!(e.before_json.as_deref(), Some(r#"{"enabled":true}"#));
        assert_eq!(e.after_json.as_deref(), Some(r#"{"enabled":false}"#));
        assert!(chrono::DateTime::parse_from_rfc3339(&e.created_at).is_ok());

        // System actions carry no actor id and no payloads.
        let sys = AuditRepository::create(
            &*db,
            NewAuditLogEntry {
                actor_id: None,
                actor_name: "system".into(),
                action: "boot".into(),
                target: None,
                before_json: None,
                after_json: None,
            },
        )
        .await
        .unwrap();
        assert!(sys.actor_id.is_none());
        assert!(sys.target.is_none() && sys.before_json.is_none() && sys.after_json.is_none());
    })
    .await;
}

#[tokio::test]
async fn audit_list_pages_newest_first_and_filters_by_actor() {
    each_backend(|db| async move {
        let mut ids = Vec::new();
        for (actor, action) in [("x", "a1"), ("y", "a2"), ("x", "a3"), ("x", "a4")] {
            ids.push(AuditRepository::create(&*db, entry(None, actor, action)).await.unwrap().id);
            tick().await;
        }

        let page: Vec<i64> =
            AuditRepository::list(&*db, 10, 0).await.unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(page, [ids[3], ids[2], ids[1], ids[0]]);
        let page: Vec<i64> =
            AuditRepository::list(&*db, 2, 1).await.unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(page, [ids[2], ids[1]]);
        assert!(AuditRepository::list(&*db, 10, 4).await.unwrap().is_empty());

        let xs: Vec<String> = db
            .list_by_actor("x", 2)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect();
        assert_eq!(xs, ["a4", "a3"]);
        assert_eq!(db.list_by_actor("x", 10).await.unwrap().len(), 3);
        assert!(db.list_by_actor("nobody", 10).await.unwrap().is_empty());
    })
    .await;
}

// ── app settings ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn app_settings_get_set_overwrite() {
    each_backend(|db| async move {
        assert!(db.get_setting("storage").await.unwrap().is_none());

        db.set_setting("storage", r#"{"retain":true}"#).await.unwrap();
        assert_eq!(db.get_setting("storage").await.unwrap().as_deref(), Some(r#"{"retain":true}"#));

        db.set_setting("storage", r#"{"retain":false}"#).await.unwrap();
        assert_eq!(db.get_setting("storage").await.unwrap().as_deref(), Some(r#"{"retain":false}"#));

        db.set_setting("other", "{}").await.unwrap();
        assert_eq!(db.get_setting("storage").await.unwrap().as_deref(), Some(r#"{"retain":false}"#));
        assert_eq!(db.get_setting("other").await.unwrap().as_deref(), Some("{}"));
    })
    .await;
}
