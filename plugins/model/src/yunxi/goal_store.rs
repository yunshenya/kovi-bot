use super::owner_lock::{self, DurableOwner};
use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use uuid::Uuid;
use yunxi_core::{
    Goal, GoalDraft, GoalId, GoalKind, GoalOwner, GoalState, GoalStore, GoalStoreError,
    GoalStoreFuture,
};

const MAX_LIST_LIMIT: usize = 128;

#[derive(Debug, Clone)]
pub(crate) struct PostgresGoalStore {
    pool: PgPool,
}

impl PostgresGoalStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        for statement in [
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_goals (
                id UUID PRIMARY KEY,
                owner_kind TEXT NOT NULL
                    CHECK (owner_kind IN ('person', 'conversation', 'global')),
                owner_id UUID,
                kind TEXT NOT NULL
                    CHECK (kind IN ('personal', 'conversation', 'follow_up', 'project', 'system')),
                title TEXT NOT NULL
                    CHECK (octet_length(title) BETWEEN 1 AND 4096
                       AND char_length(title) BETWEEN 1 AND 1024),
                details TEXT
                    CHECK (details IS NULL
                       OR (octet_length(details) <= 16384
                           AND char_length(details) <= 8192)),
                state TEXT NOT NULL DEFAULT 'active'
                    CHECK (state IN ('active', 'paused', 'completed', 'cancelled')),
                due_at TIMESTAMPTZ,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                completed_at TIMESTAMPTZ,
                CHECK (
                    (owner_kind = 'global' AND owner_id IS NULL)
                    OR (owner_kind IN ('person', 'conversation') AND owner_id IS NOT NULL)
                ),
                CHECK (updated_at >= created_at),
                CHECK (
                    (state = 'completed' AND completed_at IS NOT NULL)
                    OR (state <> 'completed' AND completed_at IS NULL)
                )
            )
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_goals_owner_idx
                ON yunxi_goals (owner_kind, owner_id, state, updated_at DESC, id)
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_goals_due_idx
                ON yunxi_goals (due_at, id)
                WHERE state IN ('active', 'paused') AND due_at IS NOT NULL
            "#,
        ] {
            query(statement).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn create_inner(&self, draft: &GoalDraft) -> Result<Goal, GoalStoreError> {
        draft.validate().map_err(validation_error)?;
        let goal = Goal::from_draft(GoalId::new(), draft, Utc::now()).map_err(validation_error)?;
        let (owner_kind, owner_id) = owner_parts(goal.owner());
        let owner = durable_owner(goal.owner());
        let mut transaction = self.pool.begin().await.map_err(GoalStoreError::storage)?;
        if !owner_lock::lock_and_owner_exists(&mut transaction, owner)
            .await
            .map_err(GoalStoreError::storage)?
        {
            return Err(GoalStoreError::InvalidRequest {
                reason: format!("goal owner {owner:?} does not exist"),
            });
        }
        let row = query(
            r#"
            INSERT INTO yunxi_goals
                (id, owner_kind, owner_id, kind, title, details, state, due_at,
                 created_at, updated_at, completed_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            RETURNING *
            "#,
        )
        .bind(goal.id().into_uuid())
        .bind(owner_kind)
        .bind(owner_id)
        .bind(kind_name(goal.kind()))
        .bind(goal.title())
        .bind(goal.details())
        .bind(state_name(goal.state()))
        .bind(goal.due_at())
        .bind(goal.created_at())
        .bind(goal.updated_at())
        .bind(goal.completed_at())
        .fetch_one(&mut *transaction)
        .await
        .map_err(GoalStoreError::storage)?;
        let goal = row_to_goal(&row)?;
        transaction
            .commit()
            .await
            .map_err(GoalStoreError::storage)?;
        Ok(goal)
    }

    async fn get_inner(&self, id: GoalId) -> Result<Option<Goal>, GoalStoreError> {
        query("SELECT * FROM yunxi_goals WHERE id = $1")
            .bind(id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(GoalStoreError::storage)?
            .as_ref()
            .map(row_to_goal)
            .transpose()
    }

    async fn list_inner(
        &self,
        owner: &GoalOwner,
        limit: usize,
    ) -> Result<Vec<Goal>, GoalStoreError> {
        if limit > MAX_LIST_LIMIT {
            return Err(GoalStoreError::InvalidRequest {
                reason: format!("list limit exceeds {MAX_LIST_LIMIT}"),
            });
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (owner_kind, owner_id) = owner_parts(*owner);
        query(
            r#"
            SELECT * FROM yunxi_goals
            WHERE owner_kind = $1
              AND owner_id IS NOT DISTINCT FROM $2
            ORDER BY
                CASE state
                    WHEN 'active' THEN 0
                    WHEN 'paused' THEN 1
                    WHEN 'completed' THEN 2
                    ELSE 3
                END,
                COALESCE(due_at, 'infinity'::timestamptz),
                updated_at DESC,
                id
            LIMIT $3
            "#,
        )
        .bind(owner_kind)
        .bind(owner_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(GoalStoreError::storage)?
        .iter()
        .map(row_to_goal)
        .collect()
    }

    async fn update_inner(&self, goal: &Goal) -> Result<Goal, GoalStoreError> {
        goal.validate().map_err(validation_error)?;
        let mut transaction = self.pool.begin().await.map_err(GoalStoreError::storage)?;
        let row = query("SELECT * FROM yunxi_goals WHERE id = $1 FOR UPDATE")
            .bind(goal.id().into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(GoalStoreError::storage)?
            .ok_or(GoalStoreError::NotFound { id: goal.id() })?;
        let existing = row_to_goal(&row)?;
        if existing == *goal {
            transaction
                .commit()
                .await
                .map_err(GoalStoreError::storage)?;
            return Ok(existing);
        }
        if goal.updated_at() <= existing.updated_at() {
            return Err(GoalStoreError::Conflict);
        }

        // Goal currently exposes lifecycle transitions as its only mutation.
        // Replaying that transition also protects immutable fields and terminal states.
        let mut expected = existing;
        expected
            .transition(goal.state(), goal.updated_at())
            .map_err(validation_error)?;
        if expected != *goal {
            return Err(GoalStoreError::InvalidRequest {
                reason: "update must be a valid lifecycle transition of the stored goal"
                    .to_string(),
            });
        }

        let row = query(
            r#"
            UPDATE yunxi_goals
            SET state = $2, updated_at = $3, completed_at = $4
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(goal.id().into_uuid())
        .bind(state_name(goal.state()))
        .bind(goal.updated_at())
        .bind(goal.completed_at())
        .fetch_one(&mut *transaction)
        .await
        .map_err(GoalStoreError::storage)?;
        let updated = row_to_goal(&row)?;
        transaction
            .commit()
            .await
            .map_err(GoalStoreError::storage)?;
        Ok(updated)
    }

    async fn delete_inner(&self, id: GoalId) -> Result<bool, GoalStoreError> {
        query("DELETE FROM yunxi_goals WHERE id = $1")
            .bind(id.into_uuid())
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected() != 0)
            .map_err(GoalStoreError::storage)
    }
}

impl GoalStore for PostgresGoalStore {
    fn create<'a>(&'a self, draft: &'a GoalDraft) -> GoalStoreFuture<'a, Goal> {
        Box::pin(async move { self.create_inner(draft).await })
    }

    fn get(&self, id: GoalId) -> GoalStoreFuture<'_, Option<Goal>> {
        Box::pin(async move { self.get_inner(id).await })
    }

    fn list<'a>(&'a self, owner: &'a GoalOwner, limit: usize) -> GoalStoreFuture<'a, Vec<Goal>> {
        Box::pin(async move { self.list_inner(owner, limit).await })
    }

    fn update<'a>(&'a self, goal: &'a Goal) -> GoalStoreFuture<'a, Goal> {
        Box::pin(async move { self.update_inner(goal).await })
    }

    fn delete(&self, id: GoalId) -> GoalStoreFuture<'_, bool> {
        Box::pin(async move { self.delete_inner(id).await })
    }
}

fn owner_parts(owner: GoalOwner) -> (&'static str, Option<Uuid>) {
    match owner {
        GoalOwner::Person(id) => ("person", Some(id.into_uuid())),
        GoalOwner::Conversation(id) => ("conversation", Some(id.into_uuid())),
        GoalOwner::Global => ("global", None),
    }
}

const fn durable_owner(owner: GoalOwner) -> DurableOwner {
    match owner {
        GoalOwner::Person(id) => DurableOwner::Person(id.into_uuid()),
        GoalOwner::Conversation(id) => DurableOwner::Conversation(id.into_uuid()),
        GoalOwner::Global => DurableOwner::Global,
    }
}

const fn kind_name(kind: GoalKind) -> &'static str {
    match kind {
        GoalKind::Personal => "personal",
        GoalKind::Conversation => "conversation",
        GoalKind::FollowUp => "follow_up",
        GoalKind::Project => "project",
        GoalKind::System => "system",
    }
}

fn parse_kind(value: &str) -> Result<GoalKind, GoalStoreError> {
    match value {
        "personal" => Ok(GoalKind::Personal),
        "conversation" => Ok(GoalKind::Conversation),
        "follow_up" => Ok(GoalKind::FollowUp),
        "project" => Ok(GoalKind::Project),
        "system" => Ok(GoalKind::System),
        _ => Err(invalid_stored("kind")),
    }
}

const fn state_name(state: GoalState) -> &'static str {
    match state {
        GoalState::Active => "active",
        GoalState::Paused => "paused",
        GoalState::Completed => "completed",
        GoalState::Cancelled => "cancelled",
    }
}

fn parse_state(value: &str) -> Result<GoalState, GoalStoreError> {
    match value {
        "active" => Ok(GoalState::Active),
        "paused" => Ok(GoalState::Paused),
        "completed" => Ok(GoalState::Completed),
        "cancelled" => Ok(GoalState::Cancelled),
        _ => Err(invalid_stored("state")),
    }
}

fn row_to_goal(row: &sqlx_postgres::PgRow) -> Result<Goal, GoalStoreError> {
    let owner_kind = row
        .try_get::<String, _>("owner_kind")
        .map_err(GoalStoreError::storage)?;
    let owner_id = row
        .try_get::<Option<Uuid>, _>("owner_id")
        .map_err(GoalStoreError::storage)?;
    let owner = match (owner_kind.as_str(), owner_id) {
        ("person", Some(id)) => GoalOwner::Person(id.into()),
        ("conversation", Some(id)) => GoalOwner::Conversation(id.into()),
        ("global", None) => GoalOwner::Global,
        _ => return Err(invalid_stored("owner")),
    };
    let kind = parse_kind(
        &row.try_get::<String, _>("kind")
            .map_err(GoalStoreError::storage)?,
    )?;
    let state = parse_state(
        &row.try_get::<String, _>("state")
            .map_err(GoalStoreError::storage)?,
    )?;
    let created_at = row
        .try_get::<DateTime<Utc>, _>("created_at")
        .map_err(GoalStoreError::storage)?;
    let updated_at = row
        .try_get::<DateTime<Utc>, _>("updated_at")
        .map_err(GoalStoreError::storage)?;
    let completed_at = row
        .try_get::<Option<DateTime<Utc>>, _>("completed_at")
        .map_err(GoalStoreError::storage)?;
    if updated_at < created_at
        || (state == GoalState::Completed) != completed_at.is_some()
        || completed_at.is_some_and(|completed_at| completed_at < created_at)
    {
        return Err(invalid_stored("lifecycle"));
    }

    serde_json::from_value(serde_json::json!({
        "id": row.try_get::<Uuid, _>("id").map_err(GoalStoreError::storage)?,
        "owner": owner,
        "kind": kind,
        "title": row.try_get::<String, _>("title").map_err(GoalStoreError::storage)?,
        "details": row.try_get::<Option<String>, _>("details").map_err(GoalStoreError::storage)?,
        "state": state,
        "due_at": row.try_get::<Option<DateTime<Utc>>, _>("due_at").map_err(GoalStoreError::storage)?,
        "created_at": created_at,
        "updated_at": updated_at,
        "completed_at": completed_at,
    }))
    .map_err(|error| GoalStoreError::InvalidRequest {
        reason: format!("stored goal is invalid: {error}"),
    })
}

fn validation_error(error: yunxi_core::GoalValidationError) -> GoalStoreError {
    GoalStoreError::InvalidRequest {
        reason: error.to_string(),
    }
}

fn invalid_stored(field: &str) -> GoalStoreError {
    GoalStoreError::InvalidRequest {
        reason: format!("stored goal {field} is invalid"),
    }
}

#[cfg(test)]
mod tests {
    use super::PostgresGoalStore;
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use chrono::{Duration, Utc};
    use sqlx_core::query::query;
    use sqlx_postgres::PgPoolOptions;
    use uuid::Uuid;
    use yunxi_core::{GoalDraft, GoalKind, GoalOwner, GoalState, GoalStore, PersonId};

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_goal_store_round_trips_crud() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("should connect to PostgreSQL");
                PostgresIdentityStore::new(pool.clone())
                    .initialize_schema()
                    .await
                    .expect("should initialize identity schema");
                let store = PostgresGoalStore::new(pool.clone());
                store
                    .initialize_schema()
                    .await
                    .expect("should initialize goal schema");

                let person_id = PersonId::new();
                query("INSERT INTO yunxi_persons (id) VALUES ($1)")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should create canonical goal owner");
                let owner = GoalOwner::Person(person_id);
                let suffix = Uuid::new_v4();
                let due_at = Utc::now() + Duration::days(1);
                let draft = GoalDraft::new(
                    owner,
                    GoalKind::Project,
                    format!("postgres goal test {suffix}"),
                )
                .expect("should create goal draft")
                .with_details("isolated PostgreSQL goal-store test")
                .expect("should add goal details")
                .with_due_at(Some(due_at));

                let created = store.create(&draft).await.expect("should create goal");
                assert_eq!(created.owner(), owner);
                assert_eq!(created.kind(), GoalKind::Project);
                assert_eq!(created.state(), GoalState::Active);
                assert_eq!(created.title(), draft.title());
                assert_eq!(created.details(), draft.details());

                assert_eq!(
                    store.get(created.id()).await.expect("should get goal"),
                    Some(created.clone())
                );
                assert_eq!(
                    store
                        .list(&owner, 16)
                        .await
                        .expect("should list owner goals"),
                    vec![created.clone()]
                );

                let mut completed = created.clone();
                completed
                    .transition(
                        GoalState::Completed,
                        created.updated_at() + Duration::seconds(1),
                    )
                    .expect("should complete goal");
                let updated = store.update(&completed).await.expect("should update goal");
                assert_eq!(updated, completed);
                assert_eq!(
                    store
                        .get(created.id())
                        .await
                        .expect("should get updated goal"),
                    Some(completed)
                );

                assert!(
                    store
                        .delete(created.id())
                        .await
                        .expect("should delete goal")
                );
                assert_eq!(
                    store
                        .get(created.id())
                        .await
                        .expect("should observe deleted goal"),
                    None
                );
                assert!(
                    store
                        .list(&owner, 16)
                        .await
                        .expect("should list after delete")
                        .is_empty()
                );
                assert!(
                    !store
                        .delete(created.id())
                        .await
                        .expect("second delete should be idempotent")
                );
                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean canonical goal owner");
            });
    }
}
