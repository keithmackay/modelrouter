use async_trait::async_trait;
use crate::db::models::{Group, GroupMembership};
use crate::db::repositories::groups::GroupRepository;
use super::{SqliteDb, now_utc};

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
    enabled: i64,
    created_at: String,
}

impl From<GroupRow> for Group {
    fn from(r: GroupRow) -> Self {
        Group {
            id: r.id,
            name: r.name,
            priority: r.priority,
            enabled: r.enabled != 0,
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
impl GroupRepository for SqliteDb {
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
            &format!("{SELECT_GROUP} WHERE id = ?"),
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Group::from))
    }

    async fn find_group_by_name(&self, name: &str) -> anyhow::Result<Option<Group>> {
        let row = sqlx::query_as::<_, GroupRow>(
            &format!("{SELECT_GROUP} WHERE name = ?"),
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Group::from))
    }

    async fn create_group(&self, name: &str, priority: i64) -> anyhow::Result<Group> {
        let now = now_utc();
        let id = sqlx::query(
            "INSERT INTO groups (name, priority, enabled, created_at) VALUES (?, ?, 1, ?)",
        )
        .bind(name)
        .bind(priority)
        .bind(&now)
        .execute(&self.pool)
        .await?
        .last_insert_rowid();

        let group = self.get_group(id).await?.ok_or_else(|| anyhow::anyhow!("group not found after insert"))?;
        Ok(group)
    }

    async fn set_group_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        let enabled_int: i64 = if enabled { 1 } else { 0 };
        let mut tx = self.pool.begin().await?;

        sqlx::query("UPDATE groups SET enabled = ? WHERE id = ?")
            .bind(enabled_int)
            .bind(id)
            .execute(&mut *tx)
            .await?;

        if !enabled {
            let now = now_utc();
            sqlx::query(
                "UPDATE group_memberships SET disabled_at = ? \
                 WHERE group_id = ? AND disabled_at IS NULL",
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
            &format!("{SELECT_MEMBERSHIP} WHERE gm.group_id = ? ORDER BY gm.id ASC"),
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
                "{SELECT_MEMBERSHIP} WHERE gm.group_id = ? AND gm.user_id = ? AND gm.disabled_at IS NULL"
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
        let id = sqlx::query(
            "INSERT INTO group_memberships (group_id, user_id, joined_at) VALUES (?, ?, ?)",
        )
        .bind(group_id)
        .bind(user_id)
        .bind(&now)
        .execute(&self.pool)
        .await?
        .last_insert_rowid();

        let row = sqlx::query_as::<_, GroupMembershipRow>(
            &format!("{SELECT_MEMBERSHIP} WHERE gm.id = ?"),
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(GroupMembership::from(row))
    }

    async fn disable_membership(&self, membership_id: i64) -> anyhow::Result<()> {
        let now = now_utc();
        sqlx::query(
            "UPDATE group_memberships SET disabled_at = ? WHERE id = ? AND disabled_at IS NULL",
        )
        .bind(&now)
        .bind(membership_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::groups::GroupRepository;
    use crate::db::repositories::users::UserRepository;
    use crate::db::models::NewUser;

    async fn db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    #[tokio::test]
    async fn create_and_list_groups() {
        let db = db().await;
        let g = GroupRepository::create_group(&db, "eng", 10).await.unwrap();
        assert_eq!(g.name, "eng");
        assert_eq!(g.priority, 10);
        assert!(g.enabled);

        let list = GroupRepository::list_groups(&db).await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_name_error() {
        let db = db().await;
        GroupRepository::create_group(&db, "eng", 0).await.unwrap();
        let result = GroupRepository::create_group(&db, "eng", 0).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn add_and_disable_member() {
        let db = db().await;
        let u = UserRepository::create(&db, NewUser { name: "alice".to_string(), email: None })
            .await.unwrap();
        let g = GroupRepository::create_group(&db, "eng", 0).await.unwrap();
        let m = GroupRepository::add_member(&db, g.id, u.id).await.unwrap();
        assert_eq!(m.user_name, "alice");
        assert!(m.disabled_at.is_none());

        GroupRepository::disable_membership(&db, m.id).await.unwrap();
        let active = GroupRepository::find_active_membership(&db, g.id, u.id).await.unwrap();
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn readd_disabled_member() {
        let db = db().await;
        let u = UserRepository::create(&db, NewUser { name: "bob".to_string(), email: None })
            .await.unwrap();
        let g = GroupRepository::create_group(&db, "ops", 0).await.unwrap();
        let m1 = GroupRepository::add_member(&db, g.id, u.id).await.unwrap();
        GroupRepository::disable_membership(&db, m1.id).await.unwrap();

        let m2 = GroupRepository::add_member(&db, g.id, u.id).await.unwrap();
        assert_ne!(m1.id, m2.id);
        assert!(m2.disabled_at.is_none());

        let memberships = GroupRepository::list_memberships(&db, g.id).await.unwrap();
        assert_eq!(memberships.len(), 2);
    }

    #[tokio::test]
    async fn disable_group_disables_memberships() {
        let db = db().await;
        let u1 = UserRepository::create(&db, NewUser { name: "u1".to_string(), email: None }).await.unwrap();
        let u2 = UserRepository::create(&db, NewUser { name: "u2".to_string(), email: None }).await.unwrap();
        let g = GroupRepository::create_group(&db, "team", 0).await.unwrap();
        GroupRepository::add_member(&db, g.id, u1.id).await.unwrap();
        GroupRepository::add_member(&db, g.id, u2.id).await.unwrap();

        GroupRepository::set_group_enabled(&db, g.id, false).await.unwrap();

        let group = GroupRepository::get_group(&db, g.id).await.unwrap().unwrap();
        assert!(!group.enabled);

        let ms = GroupRepository::list_memberships(&db, g.id).await.unwrap();
        assert!(ms.iter().all(|m| m.disabled_at.is_some()));
    }

    #[tokio::test]
    async fn reenable_group_leaves_memberships_disabled() {
        let db = db().await;
        let u = UserRepository::create(&db, NewUser { name: "carol".to_string(), email: None }).await.unwrap();
        let g = GroupRepository::create_group(&db, "research", 0).await.unwrap();
        let m = GroupRepository::add_member(&db, g.id, u.id).await.unwrap();
        GroupRepository::set_group_enabled(&db, g.id, false).await.unwrap();
        GroupRepository::set_group_enabled(&db, g.id, true).await.unwrap();

        let group = GroupRepository::get_group(&db, g.id).await.unwrap().unwrap();
        assert!(group.enabled);

        let ms = GroupRepository::list_memberships(&db, g.id).await.unwrap();
        let original = ms.iter().find(|x| x.id == m.id).unwrap();
        assert!(original.disabled_at.is_some());
    }
}
