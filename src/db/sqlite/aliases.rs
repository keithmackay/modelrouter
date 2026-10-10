use async_trait::async_trait;

use crate::db::models::{ModelAlias, NewModelAlias, NewScopedAlias, ScopedAlias};
use crate::db::repositories::aliases::AliasRepository;
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct AliasRow {
    alias: String,
    target: String,
    created_by: Option<String>,
    created_at: String,
    updated_at: String,
}

impl From<AliasRow> for ModelAlias {
    fn from(r: AliasRow) -> Self {
        ModelAlias {
            alias: r.alias,
            target: r.target,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

const SELECT_COLS: &str = "alias, target, created_by, created_at, updated_at";

#[derive(sqlx::FromRow)]
struct ScopedAliasRow {
    tag_key: String,
    tag_value: String,
    alias: String,
    target: String,
    provider: String,
    model: String,
    expires_at: i64,
    created_by: Option<String>,
    created_at: String,
}

impl From<ScopedAliasRow> for ScopedAlias {
    fn from(r: ScopedAliasRow) -> Self {
        ScopedAlias {
            tag_key: r.tag_key,
            tag_value: r.tag_value,
            alias: r.alias,
            target: r.target,
            provider: r.provider,
            model: r.model,
            expires_at: r.expires_at,
            created_by: r.created_by,
            created_at: r.created_at,
        }
    }
}

const SCOPED_COLS: &str =
    "tag_key, tag_value, alias, target, provider, model, expires_at, created_by, created_at";

#[async_trait]
impl AliasRepository for SqliteDb {
    async fn list_aliases(&self) -> anyhow::Result<Vec<ModelAlias>> {
        let rows = sqlx::query_as::<_, AliasRow>(&format!(
            "SELECT {SELECT_COLS} FROM model_aliases ORDER BY alias"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ModelAlias::from).collect())
    }

    async fn get_alias(&self, alias: &str) -> anyhow::Result<Option<ModelAlias>> {
        let row = sqlx::query_as::<_, AliasRow>(&format!(
            "SELECT {SELECT_COLS} FROM model_aliases WHERE alias = ?"
        ))
        .bind(alias)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ModelAlias::from))
    }

    async fn upsert_alias(&self, new: NewModelAlias) -> anyhow::Result<ModelAlias> {
        let now = now_utc();
        sqlx::query(
            "INSERT INTO model_aliases (alias, target, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(alias) DO UPDATE SET target = excluded.target, updated_at = excluded.updated_at",
        )
        .bind(&new.alias)
        .bind(&new.target)
        .bind(&new.created_by)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, AliasRow>(&format!(
            "SELECT {SELECT_COLS} FROM model_aliases WHERE alias = ?"
        ))
        .bind(&new.alias)
        .fetch_one(&self.pool)
        .await?;
        Ok(ModelAlias::from(row))
    }

    async fn delete_alias(&self, alias: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM model_aliases WHERE alias = ?")
            .bind(alias)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_scoped_aliases(&self) -> anyhow::Result<Vec<ScopedAlias>> {
        let rows = sqlx::query_as::<_, ScopedAliasRow>(&format!(
            "SELECT {SCOPED_COLS} FROM scoped_aliases ORDER BY tag_key, tag_value, alias"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ScopedAlias::from).collect())
    }

    async fn replace_scoped_aliases(
        &self,
        tag_key: &str,
        tag_value: &str,
        aliases: &[NewScopedAlias],
        expires_at: i64,
        created_by: Option<&str>,
    ) -> anyhow::Result<Vec<ScopedAlias>> {
        let now = now_utc();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM scoped_aliases WHERE tag_key = ? AND tag_value = ?")
            .bind(tag_key)
            .bind(tag_value)
            .execute(&mut *tx)
            .await?;
        for entry in aliases {
            sqlx::query(
                "INSERT INTO scoped_aliases \
                 (tag_key, tag_value, alias, target, provider, model, expires_at, created_by, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(tag_key)
            .bind(tag_value)
            .bind(&entry.alias)
            .bind(&entry.target)
            .bind(&entry.provider)
            .bind(&entry.model)
            .bind(expires_at)
            .bind(created_by)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        let rows = sqlx::query_as::<_, ScopedAliasRow>(&format!(
            "SELECT {SCOPED_COLS} FROM scoped_aliases WHERE tag_key = ? AND tag_value = ? ORDER BY alias"
        ))
        .bind(tag_key)
        .bind(tag_value)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(ScopedAlias::from).collect())
    }

    async fn delete_expired_scoped_aliases(&self, now_epoch: i64) -> anyhow::Result<Vec<ScopedAlias>> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query_as::<_, ScopedAliasRow>(&format!(
            "SELECT {SCOPED_COLS} FROM scoped_aliases WHERE expires_at != 0 AND expires_at <= ? \
             ORDER BY tag_key, tag_value, alias"
        ))
        .bind(now_epoch)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM scoped_aliases WHERE expires_at != 0 AND expires_at <= ?")
            .bind(now_epoch)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(ScopedAlias::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    fn new_alias(alias: &str, target: &str) -> NewModelAlias {
        NewModelAlias {
            alias: alias.to_string(),
            target: target.to_string(),
            created_by: Some("tester".to_string()),
        }
    }

    #[tokio::test]
    async fn upsert_creates_then_replaces_target() {
        let db = test_db().await;
        let created = db.upsert_alias(new_alias("deep", "anthropic/claude-opus-4-6")).await.unwrap();
        assert_eq!(created.target, "anthropic/claude-opus-4-6");
        assert_eq!(created.created_by.as_deref(), Some("tester"));

        let updated = db.upsert_alias(new_alias("deep", "openai/gpt-5")).await.unwrap();
        assert_eq!(updated.target, "openai/gpt-5");

        // Upsert must not create a duplicate row.
        assert_eq!(db.list_aliases().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_and_delete_alias() {
        let db = test_db().await;
        db.upsert_alias(new_alias("fast", "openai/gpt-5-mini")).await.unwrap();
        assert!(db.get_alias("fast").await.unwrap().is_some());
        assert!(db.get_alias("missing").await.unwrap().is_none());

        assert!(db.delete_alias("fast").await.unwrap());
        assert!(!db.delete_alias("fast").await.unwrap());
        assert!(db.get_alias("fast").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_is_sorted_by_alias() {
        let db = test_db().await;
        db.upsert_alias(new_alias("zeta", "a/b")).await.unwrap();
        db.upsert_alias(new_alias("alpha", "c/d")).await.unwrap();
        let list = db.list_aliases().await.unwrap();
        assert_eq!(list[0].alias, "alpha");
        assert_eq!(list[1].alias, "zeta");
    }

    fn pin(alias: &str, model: &str) -> NewScopedAlias {
        NewScopedAlias {
            alias: alias.to_string(),
            target: format!("mock/{model}"),
            provider: "mock".to_string(),
            model: model.to_string(),
        }
    }

    #[tokio::test]
    async fn replacing_a_scope_swaps_its_whole_set_and_leaves_others() {
        let db = test_db().await;
        db.replace_scoped_aliases("tenant", "t1", &[pin("fast", "a"), pin("deep", "b")], 0, Some("tester"))
            .await
            .unwrap();
        db.replace_scoped_aliases("tenant", "t2", &[pin("deep", "c")], 0, None).await.unwrap();
        let rows = db.replace_scoped_aliases("tenant", "t1", &[pin("deep", "d")], 99, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].model.as_str(), rows[0].expires_at), ("d", 99));

        let all = db.list_scoped_aliases().await.unwrap();
        let keys: Vec<(&str, &str, &str)> =
            all.iter().map(|r| (r.tag_value.as_str(), r.alias.as_str(), r.model.as_str())).collect();
        assert_eq!(keys, [("t1", "deep", "d"), ("t2", "deep", "c")]);

        // An empty set clears the scope.
        assert!(db.replace_scoped_aliases("tenant", "t1", &[], 0, None).await.unwrap().is_empty());
        assert_eq!(db.list_scoped_aliases().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_expired_overrides_keeps_live_and_never_expiring_ones() {
        let db = test_db().await;
        db.replace_scoped_aliases("tenant", "old", &[pin("deep", "a")], 100, None).await.unwrap();
        db.replace_scoped_aliases("tenant", "live", &[pin("deep", "b")], 300, None).await.unwrap();
        db.replace_scoped_aliases("tenant", "forever", &[pin("deep", "c")], 0, None).await.unwrap();
        let gone = db.delete_expired_scoped_aliases(200).await.unwrap();
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].tag_value, "old");
        let left: Vec<String> = db.list_scoped_aliases().await.unwrap().into_iter().map(|r| r.tag_value).collect();
        assert_eq!(left, ["forever", "live"]);
    }
}
