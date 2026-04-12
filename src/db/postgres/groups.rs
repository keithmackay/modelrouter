#![cfg(feature = "postgres")]

use async_trait::async_trait;
use crate::db::models::{Group, GroupMembership};
use crate::db::repositories::groups::GroupRepository;
use super::{PostgresDb, now_utc};

const SELECT_GROUP: &str =
    "SELECT id, name, priority, enabled, created_at FROM groups";

const SELECT_MEMBERSHIP: &str =
    "SELECT gm.id, gm.group_id, gm.user_id, u.name AS user_name, gm.joined_at, gm.disabled_at \
     FROM group_memberships gm JOIN users u ON u.id = gm.user_id";

#[derive(sqlx::FromRow)]
struct GroupRow {
    id: i64,
    name: String,
    priority: i64,
    enabled: bool,
    created_at: String,
}

impl From<GroupRow> for Group {
    fn from(r: GroupRow) -> Self {
        Group {
            id: r.id,
            name: r.name,
            priority: r.priority,
            enabled: r.enabled,
            created_at: r.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct GroupMembershipRow {
    id: i64,
    group_id: i64,
    user_id: i64,
    user_name: String,
    joined_at: String,
    disabled_at: Option<String>,
}

impl From<GroupMembershipRow> for GroupMembership {
    fn from(r: GroupMembershipRow) -> Self {
        GroupMembership {
            id: r.id,
            group_id: r.group_id,
            user_id: r.user_id,
            user_name: r.user_name,
            joined_at: r.joined_at,
            disabled_at: r.disabled_at,
        }
    }
}

#[async_trait]
impl GroupRepository for PostgresDb {
    async fn list_groups(&self) -> anyhow::Result<Vec<Group>> {
        let rows = sqlx::query_as::<_, GroupRow>(
            &format!("{SELECT_GROUP} ORDER BY priority DESC, id ASC"),
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Group::from).collect())
    }

    async fn get_group(&self, id: i64) -> anyhow::Result<Option<Group>> {
        let row = sqlx::query_as::<_, GroupRow>(
            &format!("{SELECT_GROUP} WHERE id = $1"),
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Group::from))
    }

    async fn find_group_by_name(&self, name: &str) -> anyhow::Result<Option<Group>> {
        let row = sqlx::query_as::<_, GroupRow>(
            &format!("{SELECT_GROUP} WHERE name = $1"),
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Group::from))
    }

    async fn create_group(&self, name: &str, priority: i64) -> anyhow::Result<Group> {
        let now = now_utc();
        let row = sqlx::query_as::<_, GroupRow>(
            "INSERT INTO groups (name, priority, enabled, created_at) \
             VALUES ($1, $2, true, $3) \
             RETURNING id, name, priority, enabled, created_at",
        )
        .bind(name)
        .bind(priority)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(Group::from(row))
    }

    async fn set_group_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("UPDATE groups SET enabled = $1 WHERE id = $2")
            .bind(enabled)
            .bind(id)
            .execute(&mut *tx)
            .await?;

        if !enabled {
            let now = now_utc();
            sqlx::query(
                "UPDATE group_memberships SET disabled_at = $1 \
                 WHERE group_id = $2 AND disabled_at IS NULL",
            )
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn list_memberships(&self, group_id: i64) -> anyhow::Result<Vec<GroupMembership>> {
        let rows = sqlx::query_as::<_, GroupMembershipRow>(
            &format!("{SELECT_MEMBERSHIP} WHERE gm.group_id = $1 ORDER BY gm.id ASC"),
        )
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(GroupMembership::from).collect())
    }

    async fn find_active_membership(
        &self,
        group_id: i64,
        user_id: i64,
    ) -> anyhow::Result<Option<GroupMembership>> {
        let row = sqlx::query_as::<_, GroupMembershipRow>(
            &format!(
                "{SELECT_MEMBERSHIP} WHERE gm.group_id = $1 AND gm.user_id = $2 AND gm.disabled_at IS NULL"
            ),
        )
        .bind(group_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(GroupMembership::from))
    }

    async fn add_member(&self, group_id: i64, user_id: i64) -> anyhow::Result<GroupMembership> {
        let now = now_utc();
        let row = sqlx::query_as::<_, GroupMembershipRow>(
            "INSERT INTO group_memberships (group_id, user_id, joined_at) \
             VALUES ($1, $2, $3) RETURNING id, group_id, user_id, \
             (SELECT name FROM users WHERE id = $2) AS user_name, joined_at, disabled_at",
        )
        .bind(group_id)
        .bind(user_id)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(GroupMembership::from(row))
    }

    async fn disable_membership(&self, membership_id: i64) -> anyhow::Result<()> {
        let now = now_utc();
        sqlx::query(
            "UPDATE group_memberships SET disabled_at = $1 WHERE id = $2 AND disabled_at IS NULL",
        )
        .bind(&now)
        .bind(membership_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_group_priority(&self, id: i64, priority: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE groups SET priority = $1 WHERE id = $2")
            .bind(priority)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
