use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, Postgres};
use std::time::Duration;
use uuid::Uuid;
use yunxi_core::{
    MAX_OPEN_LOOP_SALIENCE, OpenLoop, OpenLoopDraft, OpenLoopKind, OpenLoopOwner, OpenLoopStatus,
    OpenLoopStore, OpenLoopStoreError, OpenLoopStoreFuture, OpenLoopValidationError,
};

const MAX_LIST_LIMIT: usize = 128;
const MAX_CLAIM_LIMIT: usize = 128;
const DEFAULT_OWNER_CAPACITY: usize = 32;
const DEFAULT_GLOBAL_CAPACITY: usize = 128;
const DEFAULT_CLAIM_LEASE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenLoopStoreConfig {
    pub(crate) max_open_loops_per_owner: usize,
    pub(crate) max_global_open_loops: usize,
    pub(crate) claim_lease: Duration,
}

impl Default for OpenLoopStoreConfig {
    fn default() -> Self {
        Self {
            max_open_loops_per_owner: DEFAULT_OWNER_CAPACITY,
            max_global_open_loops: DEFAULT_GLOBAL_CAPACITY,
            claim_lease: DEFAULT_CLAIM_LEASE,
        }
    }
}

impl OpenLoopStoreConfig {
    #[allow(dead_code)]
    fn validate(self) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.max_open_loops_per_owner > 0,
            "owner open-loop capacity must be positive"
        );
        anyhow::ensure!(
            self.max_global_open_loops > 0,
            "global open-loop capacity must be positive"
        );
        anyhow::ensure!(
            self.max_open_loops_per_owner <= 4_096,
            "owner open-loop capacity is too large"
        );
        anyhow::ensure!(
            self.max_global_open_loops <= 4_096,
            "global open-loop capacity is too large"
        );
        anyhow::ensure!(
            !self.claim_lease.is_zero(),
            "open-loop claim lease must be positive"
        );
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresOpenLoopStore {
    pool: PgPool,
    config: OpenLoopStoreConfig,
}

impl PostgresOpenLoopStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self {
            pool,
            config: OpenLoopStoreConfig {
                max_open_loops_per_owner: DEFAULT_OWNER_CAPACITY,
                max_global_open_loops: DEFAULT_GLOBAL_CAPACITY,
                claim_lease: DEFAULT_CLAIM_LEASE,
            },
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_config(pool: PgPool, config: OpenLoopStoreConfig) -> anyhow::Result<Self> {
        Ok(Self {
            pool,
            config: config.validate()?,
        })
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        for statement in [
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_open_loops (
                id UUID PRIMARY KEY,
                owner_kind TEXT NOT NULL
                    CHECK (owner_kind IN ('person', 'conversation', 'global')),
                owner_id UUID,
                kind TEXT NOT NULL
                    CHECK (kind IN ('follow_up', 'awaiting_outcome', 'future_event', 'promise', 'pending_question')),
                summary TEXT NOT NULL
                    CHECK (octet_length(summary) BETWEEN 1 AND 4096
                       AND char_length(summary) BETWEEN 1 AND 1024
                       AND btrim(summary) <> ''),
                source_message_id UUID,
                due_at TIMESTAMPTZ,
                expires_at TIMESTAMPTZ,
                salience SMALLINT NOT NULL DEFAULT 50
                    CHECK (salience BETWEEN 0 AND 100),
                status TEXT NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'triggered', 'resolved', 'expired', 'cancelled')),
                dedupe_key TEXT
                    CHECK (dedupe_key IS NULL OR octet_length(dedupe_key) BETWEEN 1 AND 512),
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                resolved_at TIMESTAMPTZ,
                triggered_at TIMESTAMPTZ,
                version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0),
                CHECK (
                    (owner_kind = 'global' AND owner_id IS NULL)
                    OR (owner_kind IN ('person', 'conversation') AND owner_id IS NOT NULL)
                ),
                CHECK (expires_at IS NULL OR due_at IS NULL OR expires_at >= due_at),
                CHECK (
                    (status IN ('resolved', 'expired', 'cancelled') AND resolved_at IS NOT NULL)
                    OR status IN ('open', 'triggered')
                )
            )
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_open_loops_due_idx
                ON yunxi_open_loops (due_at, id)
                WHERE status = 'open' AND due_at IS NOT NULL
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_open_loops_expiry_idx
                ON yunxi_open_loops (expires_at, id)
                WHERE status IN ('open', 'triggered') AND expires_at IS NOT NULL
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_open_loops_owner_status_idx
                ON yunxi_open_loops (owner_kind, owner_id, status, updated_at DESC, id)
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_open_loops_triggered_idx
                ON yunxi_open_loops (triggered_at, id)
                WHERE status = 'triggered' AND triggered_at IS NOT NULL
            "#,
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS yunxi_open_loops_active_dedupe_idx
                ON yunxi_open_loops (
                    owner_kind,
                    COALESCE(owner_id, '00000000-0000-0000-0000-000000000000'::uuid),
                    dedupe_key
                )
                WHERE dedupe_key IS NOT NULL AND status IN ('open', 'triggered')
            "#,
        ] {
            query(statement).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        self.validate_schema().await?;
        Ok(())
    }

    async fn validate_schema(&self) -> anyhow::Result<()> {
        for (table, column, udt_name) in [
            ("yunxi_open_loops", "id", "uuid"),
            ("yunxi_open_loops", "owner_kind", "text"),
            ("yunxi_open_loops", "owner_id", "uuid"),
            ("yunxi_open_loops", "kind", "text"),
            ("yunxi_open_loops", "summary", "text"),
            ("yunxi_open_loops", "source_message_id", "uuid"),
            ("yunxi_open_loops", "due_at", "timestamptz"),
            ("yunxi_open_loops", "expires_at", "timestamptz"),
            ("yunxi_open_loops", "salience", "int2"),
            ("yunxi_open_loops", "status", "text"),
            ("yunxi_open_loops", "dedupe_key", "text"),
            ("yunxi_open_loops", "created_at", "timestamptz"),
            ("yunxi_open_loops", "updated_at", "timestamptz"),
            ("yunxi_open_loops", "resolved_at", "timestamptz"),
            ("yunxi_open_loops", "triggered_at", "timestamptz"),
            ("yunxi_open_loops", "version", "int8"),
        ] {
            let exists = query_scalar::<Postgres, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM information_schema.columns
                    WHERE table_schema = current_schema()
                      AND table_name = $1
                      AND column_name = $2
                      AND udt_name = $3
                )
                "#,
            )
            .bind(table)
            .bind(column)
            .bind(udt_name)
            .fetch_one(&self.pool)
            .await?;
            anyhow::ensure!(
                exists,
                "Yunxi open-loop schema is missing {table}.{column} ({udt_name})"
            );
        }

        let primary_key = query_scalar::<Postgres, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM pg_constraint AS constraint_row
                JOIN pg_class AS table_row ON table_row.oid = constraint_row.conrelid
                JOIN pg_namespace AS namespace_row ON namespace_row.oid = table_row.relnamespace
                JOIN pg_attribute AS column_row
                  ON column_row.attrelid = table_row.oid
                 AND column_row.attname = 'id'
                 AND column_row.attnum = constraint_row.conkey[1]
                WHERE namespace_row.nspname = current_schema()
                  AND table_row.relname = 'yunxi_open_loops'
                  AND constraint_row.contype = 'p'
                  AND array_length(constraint_row.conkey, 1) = 1
            )
            "#,
        )
        .fetch_one(&self.pool)
        .await?;
        anyhow::ensure!(
            primary_key,
            "Yunxi open-loop schema requires a single-column id primary key"
        );

        let checks = query_scalar::<Postgres, String>(
            r#"
            SELECT pg_get_constraintdef(constraint_row.oid)
            FROM pg_constraint AS constraint_row
            JOIN pg_class AS table_row ON table_row.oid = constraint_row.conrelid
            JOIN pg_namespace AS namespace_row ON namespace_row.oid = table_row.relnamespace
            WHERE namespace_row.nspname = current_schema()
              AND table_row.relname = 'yunxi_open_loops'
              AND constraint_row.contype = 'c'
              AND constraint_row.convalidated
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        for required in [
            "owner_kind",
            "global",
            "owner_id",
            "follow_up",
            "awaiting_outcome",
            "future_event",
            "promise",
            "pending_question",
            "octet_length",
            "char_length",
            "4096",
            "1024",
            "512",
            "salience",
            "triggered",
            "expired",
            "cancelled",
            "resolved_at",
        ] {
            anyhow::ensure!(
                checks
                    .iter()
                    .any(|definition| definition.to_ascii_lowercase().contains(required)),
                "Yunxi open-loop schema is missing a validated CHECK containing {required}"
            );
        }

        for (index, required_fragments) in [
            (
                "yunxi_open_loops_due_idx",
                &["due_at", "status", "open"][..],
            ),
            (
                "yunxi_open_loops_expiry_idx",
                &["expires_at", "status", "triggered"][..],
            ),
            (
                "yunxi_open_loops_owner_status_idx",
                &["owner_kind", "owner_id", "status"][..],
            ),
            (
                "yunxi_open_loops_triggered_idx",
                &["triggered_at", "status", "triggered"][..],
            ),
            (
                "yunxi_open_loops_active_dedupe_idx",
                &["owner_kind", "coalesce", "dedupe_key", "triggered"][..],
            ),
        ] {
            let definition = query_scalar::<Postgres, String>(
                r#"
                SELECT pg_get_indexdef(index_row.oid)
                FROM pg_class AS index_row
                JOIN pg_namespace AS namespace_row ON namespace_row.oid = index_row.relnamespace
                WHERE namespace_row.nspname = current_schema()
                  AND index_row.relname = $1
                  AND index_row.relkind = 'i'
                "#,
            )
            .bind(index)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Yunxi open-loop schema is missing index {index}"))?;
            let definition = definition.to_ascii_lowercase();
            anyhow::ensure!(
                required_fragments
                    .iter()
                    .all(|fragment| definition.contains(fragment)),
                "Yunxi open-loop schema index {index} has an unexpected definition"
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    async fn create_inner(&self, draft: &OpenLoopDraft) -> Result<OpenLoop, OpenLoopStoreError> {
        draft.validate().map_err(validation_error)?;
        let now = Utc::now();
        let (owner_kind, owner_id, lock_key) = owner_parts(draft.owner());
        let dedupe_key = draft.dedupe_key().map(ToOwned::to_owned).or_else(|| {
            draft
                .source_message_id()
                .map(|message_id| format!("source-message:{message_id}"))
        });
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OpenLoopStoreError::storage)?;

        advisory_lock(&mut transaction, &lock_key).await?;
        expire_owner_rows(&mut transaction, owner_kind, owner_id, now).await?;

        if let Some(dedupe_key) = dedupe_key.as_deref() {
            query(
                r#"
                UPDATE yunxi_open_loops
                SET status = 'expired', resolved_at = COALESCE(resolved_at, $4),
                    triggered_at = NULL, updated_at = $4, version = version + 1
                WHERE owner_kind = $1
                  AND owner_id IS NOT DISTINCT FROM $2
                  AND dedupe_key = $3
                  AND status IN ('open', 'triggered')
                  AND expires_at IS NOT NULL
                  AND expires_at <= $4
                "#,
            )
            .bind(owner_kind)
            .bind(owner_id)
            .bind(dedupe_key)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(OpenLoopStoreError::storage)?;
        }
        if let Some(dedupe_key) = dedupe_key.as_deref()
            && let Some(existing) = fetch_row(
                &mut transaction,
                r#"
                SELECT * FROM yunxi_open_loops
                WHERE owner_kind = $1
                  AND owner_id IS NOT DISTINCT FROM $2
                  AND dedupe_key = $3
                  AND status IN ('open', 'triggered')
                ORDER BY created_at, id
                LIMIT 1
                FOR UPDATE
                "#,
                owner_kind,
                owner_id,
                dedupe_key,
            )
            .await?
        {
            let item = row_to_open_loop(&existing)?;
            transaction
                .commit()
                .await
                .map_err(OpenLoopStoreError::storage)?;
            return Ok(item);
        }

        let capacity = if draft.owner().is_global() {
            self.config.max_global_open_loops
        } else {
            self.config.max_open_loops_per_owner
        };
        let count = query_scalar::<Postgres, i64>(
            r#"
            SELECT COUNT(*)
            FROM yunxi_open_loops
            WHERE owner_kind = $1
              AND owner_id IS NOT DISTINCT FROM $2
              AND status IN ('open', 'triggered')
              AND (expires_at IS NULL OR expires_at > $3)
            "#,
        )
        .bind(owner_kind)
        .bind(owner_id)
        .bind(now)
        .fetch_one(&mut *transaction)
        .await
        .map_err(OpenLoopStoreError::storage)? as usize;
        if count >= capacity {
            return Err(OpenLoopStoreError::CapacityExceeded {
                owner: draft.owner(),
                limit: capacity,
            });
        }

        let id = yunxi_core::OpenLoopId::new();
        let inserted = query_scalar::<Postgres, Uuid>(
            r#"
            INSERT INTO yunxi_open_loops
                (id, owner_kind, owner_id, kind, summary, source_message_id,
                 due_at, expires_at, salience, status, dedupe_key,
                 created_at, updated_at, version)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'open', $10, $11, $11, 0)
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(id.into_uuid())
        .bind(owner_kind)
        .bind(owner_id)
        .bind(draft.kind().as_str())
        .bind(draft.summary())
        .bind(draft.source_message_id().map(|value| value.into_uuid()))
        .bind(draft.due_at())
        .bind(draft.expires_at())
        .bind(i16::from(draft.salience()))
        .bind(dedupe_key.as_deref())
        .bind(now)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(OpenLoopStoreError::storage)?;

        let row = if inserted.is_some() {
            fetch_by_id(&mut transaction, id).await?
        } else {
            let Some(dedupe_key) = dedupe_key.as_deref() else {
                return Err(OpenLoopStoreError::Conflict);
            };
            fetch_row(
                &mut transaction,
                r#"
                SELECT * FROM yunxi_open_loops
                WHERE owner_kind = $1
                  AND owner_id IS NOT DISTINCT FROM $2
                  AND dedupe_key = $3
                  AND status IN ('open', 'triggered')
                ORDER BY created_at, id
                LIMIT 1
                FOR UPDATE
                "#,
                owner_kind,
                owner_id,
                dedupe_key,
            )
            .await?
            .ok_or(OpenLoopStoreError::Conflict)?
        };
        let item = row_to_open_loop(&row)?;
        transaction
            .commit()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        Ok(item)
    }

    async fn get_inner(
        &self,
        id: yunxi_core::OpenLoopId,
    ) -> Result<Option<OpenLoop>, OpenLoopStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        let Some(row) = fetch_by_id_for_update(&mut transaction, id).await? else {
            transaction
                .commit()
                .await
                .map_err(OpenLoopStoreError::storage)?;
            return Ok(None);
        };
        let now = Utc::now();
        let status = row_status(&row)?;
        let expired = matches!(status, OpenLoopStatus::Open | OpenLoopStatus::Triggered)
            && row_expired(&row, now)?;
        let row = if expired {
            update_status_row(&mut transaction, id, OpenLoopStatus::Expired, now).await?
        } else {
            row
        };
        let item = row_to_open_loop(&row)?;
        transaction
            .commit()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        Ok(Some(item))
    }

    async fn list_inner(
        &self,
        owner: &OpenLoopOwner,
        limit: usize,
    ) -> Result<Vec<OpenLoop>, OpenLoopStoreError> {
        if limit > MAX_LIST_LIMIT {
            return Err(OpenLoopStoreError::InvalidRequest {
                reason: format!("list limit exceeds {MAX_LIST_LIMIT}"),
            });
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (owner_kind, owner_id, _) = owner_parts(*owner);
        let now = Utc::now();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        expire_owner_rows(&mut transaction, owner_kind, owner_id, now).await?;
        let rows = query(
            r#"
            SELECT * FROM yunxi_open_loops
            WHERE owner_kind = $1
              AND owner_id IS NOT DISTINCT FROM $2
              AND status IN ('open', 'triggered')
            ORDER BY COALESCE(due_at, 'infinity'::timestamptz), created_at, id
            LIMIT $3
            "#,
        )
        .bind(owner_kind)
        .bind(owner_id)
        .bind(limit as i64)
        .fetch_all(&mut *transaction)
        .await
        .map_err(OpenLoopStoreError::storage)?;
        let items = rows
            .iter()
            .map(row_to_open_loop)
            .collect::<Result<Vec<_>, _>>()?;
        transaction
            .commit()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        Ok(items)
    }

    async fn claim_due_inner(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<OpenLoop>, OpenLoopStoreError> {
        if limit > MAX_CLAIM_LIMIT {
            return Err(OpenLoopStoreError::InvalidRequest {
                reason: format!("claim limit exceeds {MAX_CLAIM_LIMIT}"),
            });
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        expire_rows(&mut transaction, now, limit).await?;
        let rows = query(
            r#"
            WITH candidates AS (
                SELECT id
                FROM yunxi_open_loops
                WHERE status = 'open'
                  AND due_at IS NOT NULL
                  AND due_at <= $1
                  AND (expires_at IS NULL OR expires_at > $1)
                ORDER BY due_at, id
                FOR UPDATE SKIP LOCKED
                LIMIT $2
            )
            UPDATE yunxi_open_loops AS item
            SET status = 'triggered', triggered_at = $1, updated_at = $1,
                version = item.version + 1
            FROM candidates
            WHERE item.id = candidates.id
            RETURNING item.*
            "#,
        )
        .bind(now)
        .bind(limit as i64)
        .fetch_all(&mut *transaction)
        .await
        .map_err(OpenLoopStoreError::storage)?;
        let mut items = rows
            .iter()
            .map(row_to_open_loop)
            .collect::<Result<Vec<_>, _>>()?;
        items.sort_by_key(|item| (item.due_at(), item.id()));
        transaction
            .commit()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        Ok(items)
    }

    async fn defer_inner(
        &self,
        id: yunxi_core::OpenLoopId,
        due_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<OpenLoop, OpenLoopStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        let row = fetch_by_id_for_update(&mut transaction, id)
            .await?
            .ok_or(OpenLoopStoreError::NotFound { id })?;
        let item = row_to_open_loop(&row)?;
        if item.status().is_terminal() {
            return Err(OpenLoopStoreError::InvalidTransition {
                from: item.status(),
                to: OpenLoopStatus::Open,
            });
        }
        if item
            .expires_at()
            .is_some_and(|expires_at| expires_at <= now)
        {
            let row = update_status_row(&mut transaction, id, OpenLoopStatus::Expired, now).await?;
            let result = row_to_open_loop(&row)?;
            transaction
                .commit()
                .await
                .map_err(OpenLoopStoreError::storage)?;
            return Ok(result);
        }
        if let (Some(due_at), Some(expires_at)) = (due_at, item.expires_at())
            && expires_at < due_at
        {
            return Err(OpenLoopStoreError::InvalidRequest {
                reason: OpenLoopValidationError::ExpiryBeforeDue.to_string(),
            });
        }
        let row = query(
            r#"
            UPDATE yunxi_open_loops
            SET status = 'open', due_at = $2, triggered_at = NULL,
                updated_at = $3, version = version + 1
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(id.into_uuid())
        .bind(due_at)
        .bind(now)
        .fetch_one(&mut *transaction)
        .await
        .map_err(OpenLoopStoreError::storage)?;
        let result = row_to_open_loop(&row)?;
        transaction
            .commit()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        Ok(result)
    }

    async fn transition_inner(
        &self,
        id: yunxi_core::OpenLoopId,
        target: OpenLoopStatus,
        now: DateTime<Utc>,
    ) -> Result<OpenLoop, OpenLoopStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        let row = fetch_by_id_for_update(&mut transaction, id)
            .await?
            .ok_or(OpenLoopStoreError::NotFound { id })?;
        let item = row_to_open_loop(&row)?;
        if item.status() == target {
            transaction
                .commit()
                .await
                .map_err(OpenLoopStoreError::storage)?;
            return Ok(item);
        }
        if item.status().is_terminal() {
            return Err(OpenLoopStoreError::InvalidTransition {
                from: item.status(),
                to: target,
            });
        }
        if !matches!(
            target,
            OpenLoopStatus::Resolved | OpenLoopStatus::Cancelled | OpenLoopStatus::Expired
        ) {
            return Err(OpenLoopStoreError::InvalidTransition {
                from: item.status(),
                to: target,
            });
        }
        let row = update_status_row(&mut transaction, id, target, now).await?;
        let result = row_to_open_loop(&row)?;
        transaction
            .commit()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        Ok(result)
    }

    async fn recover_stale_inner(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<usize, OpenLoopStoreError> {
        if limit > MAX_CLAIM_LIMIT {
            return Err(OpenLoopStoreError::InvalidRequest {
                reason: format!("recovery limit exceeds {MAX_CLAIM_LIMIT}"),
            });
        }
        if limit == 0 {
            return Ok(0);
        }
        let stale_before = now
            - ChronoDuration::from_std(self.config.claim_lease).map_err(|error| {
                OpenLoopStoreError::InvalidRequest {
                    reason: error.to_string(),
                }
            })?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        let result = query(
            r#"
            WITH stale AS (
                SELECT id
                FROM yunxi_open_loops
                WHERE status = 'triggered'
                  AND triggered_at IS NOT NULL
                  AND triggered_at <= $1
                ORDER BY triggered_at, id
                FOR UPDATE SKIP LOCKED
                LIMIT $2
            )
            UPDATE yunxi_open_loops AS item
            SET status = CASE
                    WHEN item.expires_at IS NOT NULL AND item.expires_at <= $3 THEN 'expired'
                    ELSE 'open'
                END,
                resolved_at = CASE
                    WHEN item.expires_at IS NOT NULL AND item.expires_at <= $3
                        THEN COALESCE(item.resolved_at, $3)
                    ELSE item.resolved_at
                END,
                triggered_at = NULL, updated_at = $3,
                version = item.version + 1
            FROM stale
            WHERE item.id = stale.id
            "#,
        )
        .bind(stale_before)
        .bind(limit as i64)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(OpenLoopStoreError::storage)?
        .rows_affected() as usize;
        transaction
            .commit()
            .await
            .map_err(OpenLoopStoreError::storage)?;
        Ok(result)
    }
}

impl OpenLoopStore for PostgresOpenLoopStore {
    fn create<'a>(&'a self, draft: &'a OpenLoopDraft) -> OpenLoopStoreFuture<'a, OpenLoop> {
        Box::pin(async move { self.create_inner(draft).await })
    }

    fn get<'a>(&'a self, id: yunxi_core::OpenLoopId) -> OpenLoopStoreFuture<'a, Option<OpenLoop>> {
        Box::pin(async move { self.get_inner(id).await })
    }

    fn list<'a>(
        &'a self,
        owner: &'a OpenLoopOwner,
        limit: usize,
    ) -> OpenLoopStoreFuture<'a, Vec<OpenLoop>> {
        Box::pin(async move { self.list_inner(owner, limit).await })
    }

    fn claim_due(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> OpenLoopStoreFuture<'_, Vec<OpenLoop>> {
        Box::pin(async move { self.claim_due_inner(now, limit).await })
    }

    fn defer(
        &self,
        id: yunxi_core::OpenLoopId,
        due_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async move { self.defer_inner(id, due_at, now).await })
    }

    fn resolve(
        &self,
        id: yunxi_core::OpenLoopId,
        now: DateTime<Utc>,
    ) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async move {
            self.transition_inner(id, OpenLoopStatus::Resolved, now)
                .await
        })
    }

    fn cancel(
        &self,
        id: yunxi_core::OpenLoopId,
        now: DateTime<Utc>,
    ) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async move {
            self.transition_inner(id, OpenLoopStatus::Cancelled, now)
                .await
        })
    }

    fn recover_stale_triggered(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> OpenLoopStoreFuture<'_, usize> {
        Box::pin(async move { self.recover_stale_inner(now, limit).await })
    }

    fn claim_lease(&self) -> Duration {
        self.config.claim_lease
    }
}

fn validation_error(error: OpenLoopValidationError) -> OpenLoopStoreError {
    OpenLoopStoreError::InvalidRequest {
        reason: error.to_string(),
    }
}

fn owner_parts(owner: OpenLoopOwner) -> (&'static str, Option<Uuid>, String) {
    match owner {
        OpenLoopOwner::Person(id) => ("person", Some(id.into_uuid()), format!("person:{id}")),
        OpenLoopOwner::Conversation(id) => (
            "conversation",
            Some(id.into_uuid()),
            format!("conversation:{id}"),
        ),
        OpenLoopOwner::Global => ("global", None, "global".to_string()),
    }
}

async fn advisory_lock(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    lock_key: &str,
) -> Result<(), OpenLoopStoreError> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_key)
        .execute(&mut **transaction)
        .await
        .map(|_| ())
        .map_err(OpenLoopStoreError::storage)
}

async fn expire_owner_rows(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    owner_kind: &str,
    owner_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<(), OpenLoopStoreError> {
    query(
        r#"
        WITH expired AS (
            SELECT id
            FROM yunxi_open_loops
            WHERE owner_kind = $1
              AND owner_id IS NOT DISTINCT FROM $2
              AND status IN ('open', 'triggered')
              AND expires_at IS NOT NULL
              AND expires_at <= $3
            ORDER BY expires_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT 128
        )
        UPDATE yunxi_open_loops AS item
        SET status = 'expired', resolved_at = COALESCE(item.resolved_at, $3),
            triggered_at = NULL, updated_at = $3, version = item.version + 1
        FROM expired
        WHERE item.id = expired.id
        "#,
    )
    .bind(owner_kind)
    .bind(owner_id)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map(|_| ())
    .map_err(OpenLoopStoreError::storage)
}

async fn expire_rows(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    now: DateTime<Utc>,
    limit: usize,
) -> Result<(), OpenLoopStoreError> {
    query(
        r#"
        WITH expired AS (
            SELECT id
            FROM yunxi_open_loops
            WHERE status IN ('open', 'triggered')
              AND expires_at IS NOT NULL
              AND expires_at <= $1
            ORDER BY expires_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT $2
        )
        UPDATE yunxi_open_loops AS item
        SET status = 'expired', resolved_at = COALESCE(item.resolved_at, $1),
            triggered_at = NULL, updated_at = $1, version = item.version + 1
        FROM expired
        WHERE item.id = expired.id
        "#,
    )
    .bind(now)
    .bind(limit as i64)
    .execute(&mut **transaction)
    .await
    .map(|_| ())
    .map_err(OpenLoopStoreError::storage)
}

async fn fetch_by_id(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    id: yunxi_core::OpenLoopId,
) -> Result<sqlx_postgres::PgRow, OpenLoopStoreError> {
    fetch_by_id_for_update(transaction, id)
        .await?
        .ok_or(OpenLoopStoreError::Conflict)
}

async fn fetch_by_id_for_update(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    id: yunxi_core::OpenLoopId,
) -> Result<Option<sqlx_postgres::PgRow>, OpenLoopStoreError> {
    query("SELECT * FROM yunxi_open_loops WHERE id = $1 FOR UPDATE")
        .bind(id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(OpenLoopStoreError::storage)
}

async fn fetch_row(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    statement: &str,
    owner_kind: &str,
    owner_id: Option<Uuid>,
    dedupe_key: &str,
) -> Result<Option<sqlx_postgres::PgRow>, OpenLoopStoreError> {
    query(statement)
        .bind(owner_kind)
        .bind(owner_id)
        .bind(dedupe_key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(OpenLoopStoreError::storage)
}

fn row_status(row: &sqlx_postgres::PgRow) -> Result<OpenLoopStatus, OpenLoopStoreError> {
    let value = row
        .try_get::<String, _>("status")
        .map_err(OpenLoopStoreError::storage)?;
    value.parse().map_err(validation_error)
}

fn row_expired(row: &sqlx_postgres::PgRow, now: DateTime<Utc>) -> Result<bool, OpenLoopStoreError> {
    Ok(row
        .try_get::<Option<DateTime<Utc>>, _>("expires_at")
        .map_err(OpenLoopStoreError::storage)?
        .is_some_and(|expires_at| expires_at <= now))
}

async fn update_status_row(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    id: yunxi_core::OpenLoopId,
    status: OpenLoopStatus,
    now: DateTime<Utc>,
) -> Result<sqlx_postgres::PgRow, OpenLoopStoreError> {
    query(
        r#"
        UPDATE yunxi_open_loops
        SET status = $2, resolved_at = $3, triggered_at = NULL,
            updated_at = $3, version = version + 1
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(id.into_uuid())
    .bind(status.as_str())
    .bind(now)
    .fetch_one(&mut **transaction)
    .await
    .map_err(OpenLoopStoreError::storage)
}

fn row_to_open_loop(row: &sqlx_postgres::PgRow) -> Result<OpenLoop, OpenLoopStoreError> {
    let owner_kind = row
        .try_get::<String, _>("owner_kind")
        .map_err(OpenLoopStoreError::storage)?;
    let owner_id = row
        .try_get::<Option<Uuid>, _>("owner_id")
        .map_err(OpenLoopStoreError::storage)?;
    let owner = match (owner_kind.as_str(), owner_id) {
        ("person", Some(id)) => OpenLoopOwner::Person(id.into()),
        ("conversation", Some(id)) => OpenLoopOwner::Conversation(id.into()),
        ("global", None) => OpenLoopOwner::Global,
        _ => {
            return Err(OpenLoopStoreError::InvalidRequest {
                reason: "stored open-loop owner shape is invalid".to_string(),
            });
        }
    };
    let kind = row
        .try_get::<String, _>("kind")
        .map_err(OpenLoopStoreError::storage)?
        .parse::<OpenLoopKind>()
        .map_err(validation_error)?;
    let status = row_status(row)?;
    let salience = row
        .try_get::<i16, _>("salience")
        .map_err(OpenLoopStoreError::storage)?;
    if !(0..=i16::from(MAX_OPEN_LOOP_SALIENCE)).contains(&salience) {
        return Err(OpenLoopStoreError::InvalidRequest {
            reason: "stored open-loop salience is invalid".to_string(),
        });
    }
    let version = row
        .try_get::<i64, _>("version")
        .map_err(OpenLoopStoreError::storage)?;
    if version < 0 {
        return Err(OpenLoopStoreError::InvalidRequest {
            reason: "stored open-loop version is invalid".to_string(),
        });
    }
    OpenLoop::restore(
        row.try_get::<Uuid, _>("id")
            .map_err(OpenLoopStoreError::storage)?
            .into(),
        owner,
        kind,
        row.try_get::<String, _>("summary")
            .map_err(OpenLoopStoreError::storage)?,
        row.try_get::<Option<Uuid>, _>("source_message_id")
            .map_err(OpenLoopStoreError::storage)?
            .map(Into::into),
        row.try_get::<Option<DateTime<Utc>>, _>("due_at")
            .map_err(OpenLoopStoreError::storage)?,
        row.try_get::<Option<DateTime<Utc>>, _>("expires_at")
            .map_err(OpenLoopStoreError::storage)?,
        salience as u8,
        status,
        row.try_get::<DateTime<Utc>, _>("created_at")
            .map_err(OpenLoopStoreError::storage)?,
        row.try_get::<DateTime<Utc>, _>("updated_at")
            .map_err(OpenLoopStoreError::storage)?,
        row.try_get::<Option<DateTime<Utc>>, _>("resolved_at")
            .map_err(OpenLoopStoreError::storage)?,
        row.try_get::<Option<DateTime<Utc>>, _>("triggered_at")
            .map_err(OpenLoopStoreError::storage)?,
        version as u64,
        row.try_get::<Option<String>, _>("dedupe_key")
            .map_err(OpenLoopStoreError::storage)?,
    )
    .map_err(validation_error)
}

#[cfg(test)]
mod tests {
    use super::OpenLoopStoreConfig;
    use std::time::Duration;

    #[test]
    fn store_config_is_bounded() {
        assert!(OpenLoopStoreConfig::default().validate().is_ok());
        assert!(
            OpenLoopStoreConfig {
                max_open_loops_per_owner: 0,
                ..OpenLoopStoreConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            OpenLoopStoreConfig {
                claim_lease: Duration::ZERO,
                ..OpenLoopStoreConfig::default()
            }
            .validate()
            .is_err()
        );
    }
}
