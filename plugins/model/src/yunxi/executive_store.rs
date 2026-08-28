//! PostgreSQL persistence for the bounded Yunxi Executive state.
//!
//! The store deliberately persists validated Core values rather than model
//! prompts or runtime buffers.  JSONB keeps the migration surface small while
//! the extracted columns provide bounded queries, TTL cleanup, and event
//! deduplication.  All writes are serialized with the same owner/schema locks
//! used by the other Yunxi stores so erasure cannot race a new row.

use super::owner_lock::{self, DurableOwner};
use chrono::{Duration, Utc};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, Postgres};
use yunxi_core::{
    DecisionRecord, DecisionRecordPersistence, ExecutivePersistenceError, ExecutiveScope,
    ExecutiveSnapshot, ExecutiveStore, ExecutiveStoreFuture, Expectation, ExpectationStore,
    PlanState, PlanStore,
};

const MAX_SNAPSHOT_BYTES: usize = 128 * 1024;
const MAX_PLAN_BYTES: usize = 128 * 1024;
const MAX_EXPECTATION_BYTES: usize = 16 * 1024;
const MAX_DECISION_BYTES: usize = 32 * 1024;
const MAX_LIST_LIMIT: usize = 128;
const DECISION_TTL_DAYS: i64 = 7;
const EXECUTIVE_SCHEMA_VERSION: i16 = 3;
const GLOBAL_ERASURE_SCOPE_KEY: &str = "global";

#[derive(Debug, Clone)]
pub(crate) struct PostgresExecutiveStore {
    pool: PgPool,
}

#[derive(Debug, Clone)]
struct ScopeParts {
    key: String,
    kind: &'static str,
    id: Option<uuid::Uuid>,
    owner: DurableOwner,
}

impl PostgresExecutiveStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        for statement in [
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_executive_meta (
                singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
                schema_version SMALLINT NOT NULL DEFAULT 1 CHECK (schema_version >= 1),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
            "INSERT INTO yunxi_executive_meta (singleton) VALUES (TRUE) ON CONFLICT DO NOTHING",
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_executive_erasure_barriers (
                scope_key TEXT PRIMARY KEY
                    CHECK (scope_key = 'global'),
                generation BIGINT NOT NULL CHECK (generation >= 0),
                erased_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
            "INSERT INTO yunxi_executive_erasure_barriers (scope_key, generation, erased_at) VALUES ('global', 0, NOW()) ON CONFLICT (scope_key) DO NOTHING",
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_executive_snapshots (
                scope_key TEXT PRIMARY KEY
                    CHECK (octet_length(scope_key) BETWEEN 1 AND 256),
                scope_kind TEXT NOT NULL
                    CHECK (scope_kind IN ('global', 'person', 'conversation', 'goal')),
                scope_id UUID,
                version BIGINT NOT NULL CHECK (version >= 1),
                snapshot JSONB NOT NULL
                    CHECK (jsonb_typeof(snapshot) = 'object'
                       AND octet_length(snapshot::text) BETWEEN 2 AND 131072),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                CHECK (
                    (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                    OR (scope_kind = 'person' AND scope_id IS NOT NULL
                        AND scope_key = 'person:' || scope_id::text)
                    OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                        AND scope_key = 'conversation:' || scope_id::text)
                    OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                        AND scope_key = 'goal:' || scope_id::text)
                )
            )
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_plans (
                id UUID PRIMARY KEY,
                scope_key TEXT NOT NULL DEFAULT 'global'
                    CHECK (octet_length(scope_key) BETWEEN 1 AND 256),
                scope_kind TEXT NOT NULL DEFAULT 'global'
                    CHECK (scope_kind IN ('global', 'person', 'conversation', 'goal')),
                scope_id UUID,
                goal_id UUID NOT NULL,
                status TEXT NOT NULL
                    CHECK (status IN ('draft', 'active', 'paused', 'completed',
                                      'failed', 'cancelled', 'needs_revision')),
                current_step INTEGER NOT NULL CHECK (current_step >= 0 AND current_step <= 32),
                version BIGINT NOT NULL CHECK (version >= 1),
                revision_count SMALLINT NOT NULL CHECK (revision_count BETWEEN 0 AND 3),
                payload JSONB NOT NULL
                    CHECK (jsonb_typeof(payload) = 'object'
                       AND octet_length(payload::text) BETWEEN 2 AND 131072),
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                CHECK (updated_at >= created_at),
                CHECK (
                    (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                    OR (scope_kind = 'person' AND scope_id IS NOT NULL
                        AND scope_key = 'person:' || scope_id::text)
                    OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                        AND scope_key = 'conversation:' || scope_id::text)
                    OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                        AND scope_key = 'goal:' || scope_id::text)
                )
            )
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_plan_steps (
                plan_id UUID NOT NULL REFERENCES yunxi_plans(id) ON DELETE CASCADE,
                step_index INTEGER NOT NULL CHECK (step_index BETWEEN 0 AND 31),
                step_id UUID NOT NULL,
                kind JSONB NOT NULL CHECK (jsonb_typeof(kind) = 'object'),
                status TEXT NOT NULL
                    CHECK (status IN ('pending', 'active', 'completed', 'failed', 'skipped')),
                expected_result UUID,
                max_attempts SMALLINT NOT NULL CHECK (max_attempts BETWEEN 1 AND 8),
                backoff_seconds BIGINT NOT NULL CHECK (backoff_seconds BETWEEN 0 AND 86400),
                PRIMARY KEY (plan_id, step_index),
                UNIQUE (plan_id, step_id)
            )
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_expectations (
                id UUID PRIMARY KEY,
                scope_key TEXT NOT NULL DEFAULT 'global'
                    CHECK (octet_length(scope_key) BETWEEN 1 AND 256),
                scope_kind TEXT NOT NULL DEFAULT 'global'
                    CHECK (scope_kind IN ('global', 'person', 'conversation', 'goal')),
                scope_id UUID,
                source_action_id UUID NOT NULL,
                expected_event JSONB NOT NULL
                    CHECK (jsonb_typeof(expected_event) = 'object'
                       AND octet_length(expected_event::text) BETWEEN 2 AND 16384),
                confidence REAL NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
                expires_at TIMESTAMPTZ,
                status TEXT NOT NULL
                    CHECK (status IN ('pending', 'satisfied', 'violated', 'expired', 'cancelled')),
                payload JSONB NOT NULL
                    CHECK (jsonb_typeof(payload) = 'object'
                       AND octet_length(payload::text) BETWEEN 2 AND 16384),
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                CHECK (
                    (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                    OR (scope_kind = 'person' AND scope_id IS NOT NULL
                        AND scope_key = 'person:' || scope_id::text)
                    OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                        AND scope_key = 'conversation:' || scope_id::text)
                    OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                        AND scope_key = 'goal:' || scope_id::text)
                )
            )
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_decision_records (
                id UUID PRIMARY KEY,
                scope_key TEXT NOT NULL DEFAULT 'global'
                    CHECK (octet_length(scope_key) BETWEEN 1 AND 256),
                scope_kind TEXT NOT NULL DEFAULT 'global'
                    CHECK (scope_kind IN ('global', 'person', 'conversation', 'goal')),
                scope_id UUID,
                event_id UUID NOT NULL UNIQUE,
                record JSONB NOT NULL
                    CHECK (jsonb_typeof(record) = 'object'
                       AND octet_length(record::text) BETWEEN 2 AND 32768),
                created_at TIMESTAMPTZ NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                CHECK (expires_at > created_at),
                CHECK (
                    (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                    OR (scope_kind = 'person' AND scope_id IS NOT NULL
                        AND scope_key = 'person:' || scope_id::text)
                    OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                        AND scope_key = 'conversation:' || scope_id::text)
                    OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                        AND scope_key = 'goal:' || scope_id::text)
                )
            )
            "#,
            // Additive migrations from the first V3 schema are idempotent.
            // Existing rows remain in the historical global projection. Keep
            // these operations separate from the constraints below so a
            // legacy table can be normalized inside the same transaction.
            "ALTER TABLE yunxi_executive_meta ADD COLUMN IF NOT EXISTS schema_version SMALLINT NOT NULL DEFAULT 1",
            "ALTER TABLE yunxi_executive_meta ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()",
            "ALTER TABLE yunxi_executive_erasure_barriers ADD COLUMN IF NOT EXISTS generation BIGINT NOT NULL DEFAULT 0",
            "ALTER TABLE yunxi_executive_erasure_barriers ADD COLUMN IF NOT EXISTS erased_at TIMESTAMPTZ NOT NULL DEFAULT NOW()",
            // Older V3 installations may have created the barrier table with
            // the right columns but without the constraints used by the
            // generation protocol. Normalize those columns before adding the
            // checks below; invalid historical rows abort this transaction.
            "ALTER TABLE yunxi_executive_erasure_barriers ALTER COLUMN scope_key SET NOT NULL",
            "ALTER TABLE yunxi_executive_erasure_barriers ALTER COLUMN generation SET NOT NULL",
            "ALTER TABLE yunxi_executive_erasure_barriers ALTER COLUMN erased_at SET NOT NULL",
            r#"
            DO $$
            DECLARE
                primary_name TEXT;
                primary_columns SMALLINT[];
                scope_attnum SMALLINT;
            BEGIN
                SELECT attnum
                  INTO scope_attnum
                  FROM pg_attribute
                 WHERE attrelid = 'yunxi_executive_erasure_barriers'::regclass
                   AND attname = 'scope_key'
                   AND NOT attisdropped;
                IF scope_attnum IS NULL THEN
                    RAISE EXCEPTION 'yunxi_executive_erasure_barriers.scope_key is missing';
                END IF;

                SELECT conname, conkey
                  INTO primary_name, primary_columns
                  FROM pg_constraint
                 WHERE conrelid = 'yunxi_executive_erasure_barriers'::regclass
                   AND contype = 'p';

                IF primary_name IS NULL THEN
                    ALTER TABLE yunxi_executive_erasure_barriers
                        ADD CONSTRAINT yunxi_executive_erasure_barriers_pkey PRIMARY KEY (scope_key);
                ELSIF cardinality(primary_columns) <> 1
                   OR primary_columns[1] <> scope_attnum THEN
                    EXECUTE format(
                        'ALTER TABLE yunxi_executive_erasure_barriers DROP CONSTRAINT %I',
                        primary_name
                    );
                    ALTER TABLE yunxi_executive_erasure_barriers
                        ADD CONSTRAINT yunxi_executive_erasure_barriers_pkey PRIMARY KEY (scope_key);
                END IF;
            END $$;
            "#,
            r#"
            DO $$
            BEGIN
                IF NOT EXISTS (
                    SELECT 1
                      FROM pg_constraint
                     WHERE conrelid = 'yunxi_executive_erasure_barriers'::regclass
                       AND conname = 'yunxi_executive_erasure_barriers_scope_key_check'
                ) THEN
                    ALTER TABLE yunxi_executive_erasure_barriers
                        ADD CONSTRAINT yunxi_executive_erasure_barriers_scope_key_check
                        CHECK (scope_key = 'global');
                END IF;
                IF NOT EXISTS (
                    SELECT 1
                      FROM pg_constraint
                     WHERE conrelid = 'yunxi_executive_erasure_barriers'::regclass
                       AND conname = 'yunxi_executive_erasure_barriers_generation_check'
                ) THEN
                    ALTER TABLE yunxi_executive_erasure_barriers
                        ADD CONSTRAINT yunxi_executive_erasure_barriers_generation_check
                        CHECK (generation >= 0);
                END IF;
            END $$;
            "#,
            "ALTER TABLE yunxi_executive_erasure_barriers VALIDATE CONSTRAINT yunxi_executive_erasure_barriers_scope_key_check",
            "ALTER TABLE yunxi_executive_erasure_barriers VALIDATE CONSTRAINT yunxi_executive_erasure_barriers_generation_check",
            "ALTER TABLE yunxi_executive_snapshots ADD COLUMN IF NOT EXISTS scope_key TEXT",
            "ALTER TABLE yunxi_executive_snapshots ADD COLUMN IF NOT EXISTS scope_kind TEXT",
            "ALTER TABLE yunxi_executive_snapshots ADD COLUMN IF NOT EXISTS scope_id UUID",
            "ALTER TABLE yunxi_plans ADD COLUMN IF NOT EXISTS scope_key TEXT",
            "ALTER TABLE yunxi_plans ADD COLUMN IF NOT EXISTS scope_kind TEXT",
            "ALTER TABLE yunxi_plans ADD COLUMN IF NOT EXISTS scope_id UUID",
            "ALTER TABLE yunxi_expectations ADD COLUMN IF NOT EXISTS scope_key TEXT",
            "ALTER TABLE yunxi_expectations ADD COLUMN IF NOT EXISTS scope_kind TEXT",
            "ALTER TABLE yunxi_expectations ADD COLUMN IF NOT EXISTS scope_id UUID",
            "ALTER TABLE yunxi_decision_records ADD COLUMN IF NOT EXISTS scope_key TEXT",
            "ALTER TABLE yunxi_decision_records ADD COLUMN IF NOT EXISTS scope_kind TEXT",
            "ALTER TABLE yunxi_decision_records ADD COLUMN IF NOT EXISTS scope_id UUID",
            "ALTER TABLE yunxi_executive_snapshots ALTER COLUMN scope_key SET DEFAULT 'global'",
            "ALTER TABLE yunxi_executive_snapshots ALTER COLUMN scope_kind SET DEFAULT 'global'",
            "UPDATE yunxi_executive_snapshots SET scope_key = 'global' WHERE scope_key IS NULL",
            "UPDATE yunxi_executive_snapshots SET scope_kind = 'global' WHERE scope_kind IS NULL",
            "ALTER TABLE yunxi_executive_snapshots ALTER COLUMN scope_key SET NOT NULL",
            "ALTER TABLE yunxi_executive_snapshots ALTER COLUMN scope_kind SET NOT NULL",
            "ALTER TABLE yunxi_plans ALTER COLUMN scope_key SET DEFAULT 'global'",
            "ALTER TABLE yunxi_plans ALTER COLUMN scope_kind SET DEFAULT 'global'",
            "UPDATE yunxi_plans SET scope_key = 'global' WHERE scope_key IS NULL",
            "UPDATE yunxi_plans SET scope_kind = 'global' WHERE scope_kind IS NULL",
            "ALTER TABLE yunxi_plans ALTER COLUMN scope_key SET NOT NULL",
            "ALTER TABLE yunxi_plans ALTER COLUMN scope_kind SET NOT NULL",
            "ALTER TABLE yunxi_expectations ALTER COLUMN scope_key SET DEFAULT 'global'",
            "ALTER TABLE yunxi_expectations ALTER COLUMN scope_kind SET DEFAULT 'global'",
            "UPDATE yunxi_expectations SET scope_key = 'global' WHERE scope_key IS NULL",
            "UPDATE yunxi_expectations SET scope_kind = 'global' WHERE scope_kind IS NULL",
            "ALTER TABLE yunxi_expectations ALTER COLUMN scope_key SET NOT NULL",
            "ALTER TABLE yunxi_expectations ALTER COLUMN scope_kind SET NOT NULL",
            "ALTER TABLE yunxi_decision_records ALTER COLUMN scope_key SET DEFAULT 'global'",
            "ALTER TABLE yunxi_decision_records ALTER COLUMN scope_kind SET DEFAULT 'global'",
            "UPDATE yunxi_decision_records SET scope_key = 'global' WHERE scope_key IS NULL",
            "UPDATE yunxi_decision_records SET scope_kind = 'global' WHERE scope_kind IS NULL",
            "ALTER TABLE yunxi_decision_records ALTER COLUMN scope_key SET NOT NULL",
            "ALTER TABLE yunxi_decision_records ALTER COLUMN scope_kind SET NOT NULL",
            // Very early snapshots used a different primary key (or none at
            // all). scope_key is the durable identity now; convert the key
            // only after null legacy values have been filled above. Duplicate
            // legacy rows intentionally abort the transaction instead of
            // silently discarding one of them.
            r#"
            DO $$
            DECLARE
                primary_name TEXT;
                primary_columns SMALLINT[];
                scope_attnum SMALLINT;
            BEGIN
                SELECT attnum
                  INTO scope_attnum
                  FROM pg_attribute
                 WHERE attrelid = 'yunxi_executive_snapshots'::regclass
                   AND attname = 'scope_key'
                   AND NOT attisdropped;
                IF scope_attnum IS NULL THEN
                    RAISE EXCEPTION 'yunxi_executive_snapshots.scope_key is missing';
                END IF;

                SELECT conname, conkey
                  INTO primary_name, primary_columns
                  FROM pg_constraint
                 WHERE conrelid = 'yunxi_executive_snapshots'::regclass
                   AND contype = 'p';

                IF primary_name IS NULL THEN
                    ALTER TABLE yunxi_executive_snapshots
                        ADD CONSTRAINT yunxi_executive_snapshots_pkey PRIMARY KEY (scope_key);
                ELSIF cardinality(primary_columns) <> 1
                   OR primary_columns[1] <> scope_attnum THEN
                    EXECUTE format(
                        'ALTER TABLE yunxi_executive_snapshots DROP CONSTRAINT %I',
                        primary_name
                    );
                    ALTER TABLE yunxi_executive_snapshots
                        ADD CONSTRAINT yunxi_executive_snapshots_pkey PRIMARY KEY (scope_key);
                END IF;
            END $$;
            "#,
            r#"
            DO $$
            BEGIN
                IF NOT EXISTS (
                    SELECT 1 FROM pg_constraint
                    WHERE conrelid = 'yunxi_executive_snapshots'::regclass
                      AND conname = 'yunxi_executive_snapshots_scope_consistency'
                ) THEN
                    ALTER TABLE yunxi_executive_snapshots
                    ADD CONSTRAINT yunxi_executive_snapshots_scope_consistency CHECK (
                        (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                        OR (scope_kind = 'person' AND scope_id IS NOT NULL
                            AND scope_key = 'person:' || scope_id::text)
                        OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                            AND scope_key = 'conversation:' || scope_id::text)
                        OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                            AND scope_key = 'goal:' || scope_id::text)
                    );
                END IF;
            END $$
            "#,
            r#"
            DO $$
            BEGIN
                IF NOT EXISTS (
                    SELECT 1 FROM pg_constraint
                    WHERE conrelid = 'yunxi_plans'::regclass
                      AND conname = 'yunxi_plans_scope_consistency'
                ) THEN
                    ALTER TABLE yunxi_plans
                    ADD CONSTRAINT yunxi_plans_scope_consistency CHECK (
                        (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                        OR (scope_kind = 'person' AND scope_id IS NOT NULL
                            AND scope_key = 'person:' || scope_id::text)
                        OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                            AND scope_key = 'conversation:' || scope_id::text)
                        OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                            AND scope_key = 'goal:' || scope_id::text)
                    );
                END IF;
            END $$
            "#,
            r#"
            DO $$
            BEGIN
                IF NOT EXISTS (
                    SELECT 1 FROM pg_constraint
                    WHERE conrelid = 'yunxi_expectations'::regclass
                      AND conname = 'yunxi_expectations_scope_consistency'
                ) THEN
                    ALTER TABLE yunxi_expectations
                    ADD CONSTRAINT yunxi_expectations_scope_consistency CHECK (
                        (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                        OR (scope_kind = 'person' AND scope_id IS NOT NULL
                            AND scope_key = 'person:' || scope_id::text)
                        OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                            AND scope_key = 'conversation:' || scope_id::text)
                        OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                            AND scope_key = 'goal:' || scope_id::text)
                    );
                END IF;
            END $$
            "#,
            r#"
            DO $$
            BEGIN
                IF NOT EXISTS (
                    SELECT 1 FROM pg_constraint
                    WHERE conrelid = 'yunxi_decision_records'::regclass
                      AND conname = 'yunxi_decision_records_scope_consistency'
                ) THEN
                    ALTER TABLE yunxi_decision_records
                    ADD CONSTRAINT yunxi_decision_records_scope_consistency CHECK (
                        (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                        OR (scope_kind = 'person' AND scope_id IS NOT NULL
                            AND scope_key = 'person:' || scope_id::text)
                        OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                            AND scope_key = 'conversation:' || scope_id::text)
                        OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                            AND scope_key = 'goal:' || scope_id::text)
                    );
                END IF;
            END $$
            "#,
            "CREATE INDEX IF NOT EXISTS yunxi_executive_snapshots_scope_idx ON yunxi_executive_snapshots (scope_kind, scope_id, updated_at DESC)",
            "CREATE INDEX IF NOT EXISTS yunxi_plans_scope_status_idx ON yunxi_plans (scope_kind, scope_id, scope_key, status, updated_at DESC, id)",
            "CREATE INDEX IF NOT EXISTS yunxi_plans_goal_idx ON yunxi_plans (goal_id, status, updated_at DESC)",
            "CREATE INDEX IF NOT EXISTS yunxi_plan_steps_status_idx ON yunxi_plan_steps (plan_id, status, step_index)",
            "CREATE INDEX IF NOT EXISTS yunxi_expectations_scope_status_idx ON yunxi_expectations (scope_kind, scope_id, scope_key, status, updated_at DESC, id)",
            "CREATE INDEX IF NOT EXISTS yunxi_expectations_action_status_idx ON yunxi_expectations (source_action_id, status, updated_at DESC, id)",
            "CREATE INDEX IF NOT EXISTS yunxi_expectations_expiry_idx ON yunxi_expectations (expires_at, id) WHERE status = 'pending' AND expires_at IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS yunxi_decision_records_expiry_idx ON yunxi_decision_records (expires_at, id)",
            "CREATE INDEX IF NOT EXISTS yunxi_decision_records_scope_idx ON yunxi_decision_records (scope_kind, scope_id, scope_key, created_at DESC, id)",
            "UPDATE yunxi_executive_meta SET schema_version = 3, updated_at = NOW() WHERE singleton",
        ] {
            query(statement).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        self.validate_schema().await?;
        Ok(())
    }

    async fn validate_schema(&self) -> anyhow::Result<()> {
        for (table, column, udt, nullable) in [
            ("yunxi_executive_meta", "schema_version", "int2", "NO"),
            ("yunxi_executive_meta", "updated_at", "timestamptz", "NO"),
            (
                "yunxi_executive_erasure_barriers",
                "scope_key",
                "text",
                "NO",
            ),
            (
                "yunxi_executive_erasure_barriers",
                "generation",
                "int8",
                "NO",
            ),
            (
                "yunxi_executive_erasure_barriers",
                "erased_at",
                "timestamptz",
                "NO",
            ),
            ("yunxi_executive_snapshots", "scope_key", "text", "NO"),
            ("yunxi_executive_snapshots", "scope_kind", "text", "NO"),
            ("yunxi_executive_snapshots", "scope_id", "uuid", "YES"),
            ("yunxi_executive_snapshots", "version", "int8", "NO"),
            ("yunxi_executive_snapshots", "snapshot", "jsonb", "NO"),
            ("yunxi_plans", "id", "uuid", "NO"),
            ("yunxi_plans", "scope_key", "text", "NO"),
            ("yunxi_plans", "scope_kind", "text", "NO"),
            ("yunxi_plans", "scope_id", "uuid", "YES"),
            ("yunxi_plans", "payload", "jsonb", "NO"),
            ("yunxi_plan_steps", "plan_id", "uuid", "NO"),
            ("yunxi_plan_steps", "step_index", "int4", "NO"),
            ("yunxi_expectations", "id", "uuid", "NO"),
            ("yunxi_expectations", "scope_key", "text", "NO"),
            ("yunxi_expectations", "scope_kind", "text", "NO"),
            ("yunxi_expectations", "scope_id", "uuid", "YES"),
            ("yunxi_expectations", "payload", "jsonb", "NO"),
            ("yunxi_decision_records", "id", "uuid", "NO"),
            ("yunxi_decision_records", "scope_key", "text", "NO"),
            ("yunxi_decision_records", "scope_kind", "text", "NO"),
            ("yunxi_decision_records", "scope_id", "uuid", "YES"),
            ("yunxi_decision_records", "event_id", "uuid", "NO"),
            ("yunxi_decision_records", "record", "jsonb", "NO"),
        ] {
            let stored = query(
                r#"
                SELECT udt_name, is_nullable
                FROM information_schema.columns
                WHERE table_schema = current_schema()
                  AND table_name = $1
                  AND column_name = $2
                "#,
            )
            .bind(table)
            .bind(column)
            .fetch_optional(&self.pool)
            .await?;
            let Some(stored) = stored else {
                anyhow::bail!("Yunxi Executive schema is missing {table}.{column}");
            };
            let stored_type = stored.try_get::<String, _>("udt_name")?;
            let stored_nullable = stored.try_get::<String, _>("is_nullable")?;
            anyhow::ensure!(
                stored_type == udt && stored_nullable == nullable,
                "Yunxi Executive schema column {table}.{column} has type {stored_type}/{stored_nullable}, expected {udt}/{nullable}"
            );
        }
        let schema_version = query_scalar::<Postgres, i16>(
            "SELECT schema_version FROM yunxi_executive_meta WHERE singleton",
        )
        .fetch_one(&self.pool)
        .await?;
        anyhow::ensure!(
            schema_version >= EXECUTIVE_SCHEMA_VERSION,
            "Yunxi Executive schema version {schema_version} is older than required {EXECUTIVE_SCHEMA_VERSION}"
        );

        let barrier = query(
            "SELECT scope_key, generation
             FROM yunxi_executive_erasure_barriers
             WHERE scope_key = $1",
        )
        .bind(GLOBAL_ERASURE_SCOPE_KEY)
        .fetch_optional(&self.pool)
        .await?;
        let Some(barrier) = barrier else {
            anyhow::bail!("Yunxi Executive schema is missing the global erasure barrier row");
        };
        let barrier_scope = barrier.try_get::<String, _>("scope_key")?;
        let generation = barrier.try_get::<i64, _>("generation")?;
        anyhow::ensure!(
            barrier_scope == GLOBAL_ERASURE_SCOPE_KEY && generation >= 0,
            "Yunxi Executive global erasure barrier contains invalid scope or generation"
        );

        let barrier_rows =
            query_scalar::<Postgres, i64>("SELECT COUNT(*) FROM yunxi_executive_erasure_barriers")
                .fetch_one(&self.pool)
                .await?;
        anyhow::ensure!(
            barrier_rows == 1,
            "Yunxi Executive global erasure barrier must contain exactly one row"
        );

        for (table, constraint) in [
            (
                "yunxi_executive_erasure_barriers",
                "yunxi_executive_erasure_barriers_scope_key_check",
            ),
            (
                "yunxi_executive_erasure_barriers",
                "yunxi_executive_erasure_barriers_generation_check",
            ),
            (
                "yunxi_executive_snapshots",
                "yunxi_executive_snapshots_scope_consistency",
            ),
            ("yunxi_plans", "yunxi_plans_scope_consistency"),
            ("yunxi_expectations", "yunxi_expectations_scope_consistency"),
            (
                "yunxi_decision_records",
                "yunxi_decision_records_scope_consistency",
            ),
        ] {
            let present = query_scalar::<Postgres, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM pg_constraint AS constraint_row
                    JOIN pg_class AS table_row
                      ON table_row.oid = constraint_row.conrelid
                    JOIN pg_namespace AS namespace_row
                      ON namespace_row.oid = table_row.relnamespace
                    WHERE namespace_row.nspname = current_schema()
                      AND table_row.relname = $1
                      AND constraint_row.conname = $2
                      AND constraint_row.contype = 'c'
                      AND constraint_row.convalidated
                )
                "#,
            )
            .bind(table)
            .bind(constraint)
            .fetch_one(&self.pool)
            .await?;
            anyhow::ensure!(
                present,
                "Yunxi Executive schema is missing validated constraint {constraint}"
            );
        }

        for (table, primary_key) in [
            ("yunxi_executive_erasure_barriers", "scope_key"),
            ("yunxi_executive_snapshots", "scope_key"),
            ("yunxi_plans", "id"),
            ("yunxi_expectations", "id"),
            ("yunxi_decision_records", "id"),
        ] {
            let present = query_scalar::<Postgres, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM pg_constraint AS constraint_row
                    JOIN pg_class AS table_row ON table_row.oid = constraint_row.conrelid
                    JOIN pg_namespace AS namespace_row ON namespace_row.oid = table_row.relnamespace
                    JOIN pg_attribute AS attribute_row
                      ON attribute_row.attrelid = constraint_row.conrelid
                     AND attribute_row.attname = $2
                     AND NOT attribute_row.attisdropped
                    WHERE namespace_row.nspname = current_schema()
                      AND table_row.relname = $1
                      AND constraint_row.contype = 'p'
                      AND cardinality(constraint_row.conkey) = 1
                      AND constraint_row.conkey[1] = attribute_row.attnum
                )
                "#,
            )
            .bind(table)
            .bind(primary_key)
            .fetch_one(&self.pool)
            .await?;
            anyhow::ensure!(
                present,
                "Yunxi Executive schema has no primary key for {table}.{primary_key}"
            );
        }

        let event_unique = query_scalar::<Postgres, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM pg_index AS index_row
                JOIN pg_class AS table_row ON table_row.oid = index_row.indrelid
                JOIN pg_attribute AS attribute_row
                  ON attribute_row.attrelid = table_row.oid
                 AND attribute_row.attnum = ANY(index_row.indkey)
                JOIN pg_namespace AS namespace_row ON namespace_row.oid = table_row.relnamespace
                WHERE namespace_row.nspname = current_schema()
                  AND table_row.relname = 'yunxi_decision_records'
                  AND index_row.indisunique
                  AND index_row.indpred IS NULL
                  AND index_row.indexprs IS NULL
                  AND index_row.indnkeyatts = 1
                  AND attribute_row.attname = 'event_id'
                  AND index_row.indkey[0] = attribute_row.attnum
                )
                "#,
        )
        .fetch_one(&self.pool)
        .await?;
        anyhow::ensure!(
            event_unique,
            "Yunxi Executive decision records require a unique event_id index"
        );

        for (table, payload, maximum_bytes) in [
            ("yunxi_executive_snapshots", "snapshot", 131_072_usize),
            ("yunxi_plans", "payload", 131_072_usize),
            ("yunxi_expectations", "payload", 16_384_usize),
            ("yunxi_decision_records", "record", 32_768_usize),
        ] {
            let invalid_rows = query_scalar::<Postgres, bool>(&format!(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM {table}
                    WHERE scope_key IS NULL
                       OR octet_length(scope_key) NOT BETWEEN 1 AND 256
                       OR scope_kind IS NULL
                       OR scope_kind NOT IN ('global', 'person', 'conversation', 'goal')
                       OR (
                            (scope_kind = 'global' AND scope_id IS NULL AND scope_key = 'global')
                            OR (scope_kind = 'person' AND scope_id IS NOT NULL
                                AND scope_key = 'person:' || scope_id::text)
                            OR (scope_kind = 'conversation' AND scope_id IS NOT NULL
                                AND scope_key = 'conversation:' || scope_id::text)
                            OR (scope_kind = 'goal' AND scope_id IS NOT NULL
                                AND scope_key = 'goal:' || scope_id::text)
                       ) IS NOT TRUE
                       OR {payload} IS NULL
                       OR jsonb_typeof({payload}) IS DISTINCT FROM 'object'
                       OR octet_length({payload}::text) IS NULL
                       OR octet_length({payload}::text) NOT BETWEEN 2 AND {maximum_bytes}
                )
                "#
            ))
            .fetch_one(&self.pool)
            .await?;
            anyhow::ensure!(
                !invalid_rows,
                "Yunxi Executive table {table} contains rows outside its bounded schema"
            );
        }

        let invalid_snapshot_rows = query_scalar::<Postgres, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM yunxi_executive_snapshots
                WHERE version IS NULL OR version < 1
            )
            "#,
        )
        .fetch_one(&self.pool)
        .await?;
        anyhow::ensure!(
            !invalid_snapshot_rows,
            "Yunxi Executive snapshots contain invalid numeric bounds"
        );

        let invalid_plan_rows = query_scalar::<Postgres, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM yunxi_plans
                WHERE version IS NULL OR version < 1
                   OR current_step IS NULL OR current_step NOT BETWEEN 0 AND 32
                   OR revision_count IS NULL OR revision_count NOT BETWEEN 0 AND 3
                   OR created_at IS NULL OR updated_at IS NULL
                   OR updated_at < created_at
            )
            "#,
        )
        .fetch_one(&self.pool)
        .await?;
        anyhow::ensure!(
            !invalid_plan_rows,
            "Yunxi Executive plans contain invalid numeric bounds"
        );

        let invalid_decision_rows = query_scalar::<Postgres, bool>(
            "SELECT EXISTS (SELECT 1 FROM yunxi_decision_records WHERE created_at IS NULL OR expires_at IS NULL OR expires_at <= created_at)",
        )
        .fetch_one(&self.pool)
        .await?;
        anyhow::ensure!(
            !invalid_decision_rows,
            "Yunxi Executive decisions contain invalid retention bounds"
        );
        Ok(())
    }

    pub(crate) async fn load_bootstrap(
        &self,
    ) -> Result<Option<ExecutiveSnapshot>, ExecutivePersistenceError> {
        let Some(mut snapshot) = self.load(&ExecutiveScope::Global).await? else {
            return Ok(None);
        };
        if let Some(plan) = self
            .active_plans_for_scope(&ExecutiveScope::Global, 1)
            .await?
            .into_iter()
            .next()
        {
            snapshot.active_plan = Some(plan);
        }
        let expectations = self
            .pending_expectations_for_scope(&ExecutiveScope::Global, 8)
            .await?;
        if !expectations.is_empty() {
            snapshot.pending_expectations = expectations;
        }
        let decisions = self.recent_for_scope(&ExecutiveScope::Global, 8).await?;
        if !decisions.is_empty() {
            snapshot.recent_decisions = decisions;
        }
        snapshot
            .validate()
            .map_err(|reason| ExecutivePersistenceError::InvalidRequest {
                reason: format!("stored Executive snapshot is invalid: {reason}"),
            })?;
        Ok(Some(snapshot))
    }

    /// Persist the complete live projection in one bounded sequence.  Each
    /// projection is committed atomically. A failed write therefore leaves the
    /// previous snapshot and all of its derived rows intact.
    pub(crate) async fn save_runtime_snapshot(
        &self,
        snapshot: &ExecutiveSnapshot,
    ) -> Result<(), ExecutivePersistenceError> {
        self.save_runtime_snapshot_for_scope(&ExecutiveScope::Global, snapshot)
            .await
    }

    /// Persist a complete projection for one durable scope. The public Core
    /// ports remain intentionally narrow, while host bootstrap/erasure code
    /// can use the same validated scope for every derived row.
    pub(crate) async fn save_runtime_snapshot_for_scope(
        &self,
        scope: &ExecutiveScope,
        snapshot: &ExecutiveSnapshot,
    ) -> Result<(), ExecutivePersistenceError> {
        let parts = scope_parts(scope)?;
        let mut normalized = snapshot.clone();
        normalized.version = normalized.version.max(1);
        normalized
            .validate()
            .map_err(|reason| invalid(reason.to_owned()))?;
        let snapshot_value = encode_value(&normalized, MAX_SNAPSHOT_BYTES, "snapshot")?;
        let plan_value = normalized
            .active_plan
            .as_ref()
            .map(|plan| encode_value(plan, MAX_PLAN_BYTES, "plan"))
            .transpose()?;
        let expectation_values = normalized
            .pending_expectations
            .iter()
            .map(|expectation| {
                if expectation.status != yunxi_core::ExpectationStatus::Pending {
                    return Err(invalid(
                        "runtime projection may contain pending expectations only",
                    ));
                }
                let value = encode_value(expectation, MAX_EXPECTATION_BYTES, "expectation")?;
                let expected_event = serde_json::to_value(&expectation.expected_event)
                    .map_err(ExecutivePersistenceError::storage)?;
                Ok((expectation, value, expected_event))
            })
            .collect::<Result<Vec<_>, ExecutivePersistenceError>>()?;
        let decision_values = normalized
            .recent_decisions
            .iter()
            .map(|record| {
                let value = encode_value(record, MAX_DECISION_BYTES, "decision")?;
                let expires_at = record
                    .created_at
                    .checked_add_signed(Duration::days(DECISION_TTL_DAYS))
                    .ok_or_else(|| invalid("decision retention expiry is out of range"))?;
                Ok((record, value, expires_at))
            })
            .collect::<Result<Vec<_>, ExecutivePersistenceError>>()?;
        let version = checked_i64(normalized.version, "snapshot version")?;

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(ExecutivePersistenceError::storage)?;
        lock_scope_owner_for_write(&mut transaction, &parts).await?;

        let stored_version = query_scalar::<Postgres, i64>(
            "SELECT version FROM yunxi_executive_snapshots WHERE scope_key = $1 FOR UPDATE",
        )
        .bind(&parts.key)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(ExecutivePersistenceError::storage)?;
        if stored_version.is_some_and(|stored| stored > version) {
            return Err(ExecutivePersistenceError::Conflict);
        }
        query(
            r#"
            INSERT INTO yunxi_executive_snapshots
                (scope_key, scope_kind, scope_id, version, snapshot, updated_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (scope_key) DO UPDATE SET
                scope_kind = EXCLUDED.scope_kind,
                scope_id = EXCLUDED.scope_id,
                version = EXCLUDED.version,
                snapshot = EXCLUDED.snapshot,
                updated_at = NOW()
            "#,
        )
        .bind(&parts.key)
        .bind(parts.kind)
        .bind(parts.id)
        .bind(version)
        .bind(snapshot_value)
        .execute(&mut *transaction)
        .await
        .map_err(ExecutivePersistenceError::storage)?;

        if let Some(plan) = normalized.active_plan.as_ref() {
            query(
                "DELETE FROM yunxi_plans
                 WHERE scope_key = $1
                   AND status IN ('draft', 'active', 'paused', 'needs_revision')
                   AND id <> $2",
            )
            .bind(&parts.key)
            .bind(plan.id.into_uuid())
            .execute(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            upsert_plan_in_transaction(
                &mut transaction,
                plan,
                &parts,
                plan_value.expect("encoded active plan is present"),
            )
            .await?;
        } else {
            query(
                "DELETE FROM yunxi_plans
                 WHERE scope_key = $1
                   AND status IN ('draft', 'active', 'paused', 'needs_revision')",
            )
            .bind(&parts.key)
            .execute(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
        }

        // Pending expectations are a projection, not an append-only log.
        // Replacing only this scope removes rows that were satisfied,
        // cancelled, or evicted from the bounded in-memory snapshot.
        query("DELETE FROM yunxi_expectations WHERE scope_key = $1 AND status = 'pending'")
            .bind(&parts.key)
            .execute(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
        for (expectation, value, expected_event) in expectation_values {
            insert_or_update_expectation_in_transaction(
                &mut transaction,
                expectation,
                &parts,
                value,
                expected_event,
            )
            .await?;
        }

        query("DELETE FROM yunxi_decision_records WHERE scope_key = $1 AND expires_at <= NOW()")
            .bind(&parts.key)
            .execute(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
        for (record, value, expires_at) in decision_values {
            insert_decision_in_transaction(&mut transaction, record, &parts, value, expires_at)
                .await?;
        }

        transaction
            .commit()
            .await
            .map_err(ExecutivePersistenceError::storage)?;
        Ok(())
    }

    async fn active_plans_for_scope(
        &self,
        scope: &ExecutiveScope,
        limit: usize,
    ) -> Result<Vec<PlanState>, ExecutivePersistenceError> {
        let parts = scope_parts(scope)?;
        let limit = bounded_limit(limit);
        let rows = query(
            "SELECT payload AS plan FROM yunxi_plans
             WHERE scope_key = $1
               AND status IN ('draft', 'active', 'paused', 'needs_revision')
             ORDER BY updated_at DESC, id
             LIMIT $2",
        )
        .bind(&parts.key)
        .bind(checked_i64(limit, "scoped plan list limit")?)
        .fetch_all(&self.pool)
        .await
        .map_err(ExecutivePersistenceError::storage)?;
        decode_rows(rows, "plan", MAX_PLAN_BYTES)
    }

    async fn pending_expectations_for_scope(
        &self,
        scope: &ExecutiveScope,
        limit: usize,
    ) -> Result<Vec<Expectation>, ExecutivePersistenceError> {
        let parts = scope_parts(scope)?;
        let limit = bounded_limit(limit);
        let rows = query(
            "SELECT payload AS expectation FROM yunxi_expectations
             WHERE scope_key = $1
               AND status = 'pending'
               AND (expires_at IS NULL OR expires_at > NOW())
             ORDER BY updated_at DESC, id
             LIMIT $2",
        )
        .bind(&parts.key)
        .bind(checked_i64(limit, "scoped expectation list limit")?)
        .fetch_all(&self.pool)
        .await
        .map_err(ExecutivePersistenceError::storage)?;
        decode_rows(rows, "expectation", MAX_EXPECTATION_BYTES)
    }

    async fn recent_for_scope(
        &self,
        scope: &ExecutiveScope,
        limit: usize,
    ) -> Result<Vec<DecisionRecord>, ExecutivePersistenceError> {
        let parts = scope_parts(scope)?;
        let rows = query(
            "SELECT record AS decision FROM yunxi_decision_records
             WHERE scope_key = $1 AND expires_at > NOW()
             ORDER BY created_at DESC, id DESC
             LIMIT $2",
        )
        .bind(&parts.key)
        .bind(checked_i64(
            bounded_limit(limit),
            "scoped decision list limit",
        )?)
        .fetch_all(&self.pool)
        .await
        .map_err(ExecutivePersistenceError::storage)?;
        let mut records: Vec<DecisionRecord> = decode_rows(rows, "decision", MAX_DECISION_BYTES)?;
        records.reverse();
        Ok(records)
    }

    pub(crate) async fn erase_scope_data(
        &self,
        scope: &ExecutiveScope,
    ) -> Result<usize, ExecutivePersistenceError> {
        self.erase(scope).await
    }
}

impl ExecutiveStore for PostgresExecutiveStore {
    fn load<'a>(
        &'a self,
        scope: &'a ExecutiveScope,
    ) -> ExecutiveStoreFuture<'a, Option<ExecutiveSnapshot>> {
        Box::pin(async move {
            let parts = scope_parts(scope)?;
            let row = query(
                "SELECT scope_key, scope_kind, scope_id, version, snapshot
                 FROM yunxi_executive_snapshots WHERE scope_key = $1",
            )
            .bind(&parts.key)
            .fetch_optional(&self.pool)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let stored_parts = row_scope(&row)?;
            if stored_parts.key != parts.key {
                return Err(invalid("stored Executive scope does not match the request"));
            }
            let stored_version = row
                .try_get::<i64, _>("version")
                .map_err(ExecutivePersistenceError::storage)?;
            let stored_version = u64::try_from(stored_version)
                .map_err(|_| invalid("stored snapshot version is negative"))?;
            let value: Value = row
                .try_get("snapshot")
                .map_err(ExecutivePersistenceError::storage)?;
            let snapshot: ExecutiveSnapshot = decode_value(value, MAX_SNAPSHOT_BYTES, "snapshot")?;
            if snapshot.version != stored_version {
                return Err(invalid("stored snapshot version columns are inconsistent"));
            }
            Ok(Some(snapshot))
        })
    }

    fn save<'a>(
        &'a self,
        scope: &'a ExecutiveScope,
        snapshot: &'a ExecutiveSnapshot,
    ) -> ExecutiveStoreFuture<'a, ()> {
        Box::pin(async move {
            let parts = scope_parts(scope)?;
            let mut normalized = snapshot.clone();
            normalized.version = normalized.version.max(1);
            normalized
                .validate()
                .map_err(|reason| ExecutivePersistenceError::InvalidRequest {
                    reason: reason.to_owned(),
                })?;
            let value = encode_value(&normalized, MAX_SNAPSHOT_BYTES, "snapshot")?;
            let version = checked_i64(normalized.version, "snapshot version")?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            lock_scope_owner_for_write(&mut transaction, &parts).await?;
            let stored_version = query_scalar::<Postgres, i64>(
                "SELECT version FROM yunxi_executive_snapshots WHERE scope_key = $1 FOR UPDATE",
            )
            .bind(&parts.key)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            if let Some(stored_version) = stored_version {
                let stored_version = u64::try_from(stored_version)
                    .map_err(|_| invalid("stored snapshot version is negative"))?;
                if stored_version > normalized.version {
                    return Err(ExecutivePersistenceError::Conflict);
                }
            }
            query(
                r#"
                INSERT INTO yunxi_executive_snapshots
                    (scope_key, scope_kind, scope_id, version, snapshot, updated_at)
                VALUES ($1, $2, $3, $4, $5, NOW())
                ON CONFLICT (scope_key) DO UPDATE SET
                    scope_kind = EXCLUDED.scope_kind,
                    scope_id = EXCLUDED.scope_id,
                    version = EXCLUDED.version,
                    snapshot = EXCLUDED.snapshot,
                    updated_at = NOW()
                "#,
            )
            .bind(&parts.key)
            .bind(parts.kind)
            .bind(parts.id)
            .bind(version)
            .bind(value)
            .execute(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            Ok(())
        })
    }

    fn erase<'a>(&'a self, scope: &'a ExecutiveScope) -> ExecutiveStoreFuture<'a, usize> {
        Box::pin(async move {
            let parts = scope_parts(scope)?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            // Global erasure advances the generation while it owns the same
            // advisory lock used by every Global writer. The generation is
            // checked before and after lock acquisition so a transaction that
            // observed the old epoch cannot erase a newer epoch by accident.
            let global_generation = if parts.key == GLOBAL_ERASURE_SCOPE_KEY {
                Some(lock_global_owner_for_write(&mut transaction).await?)
            } else {
                lock_scope_owner_for_write(&mut transaction, &parts).await?;
                None
            };
            if let Some(expected_generation) = global_generation {
                advance_global_generation(&mut transaction, expected_generation).await?;
            }
            let removed = erase_scope_rows(&mut transaction, &parts).await?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            Ok(removed)
        })
    }
}

impl PlanStore for PostgresExecutiveStore {
    fn create<'a>(&'a self, plan: &'a PlanState) -> ExecutiveStoreFuture<'a, PlanState> {
        Box::pin(self.create_plan(plan, &ExecutiveScope::Global))
    }

    fn get(&self, id: yunxi_core::PlanId) -> ExecutiveStoreFuture<'_, Option<PlanState>> {
        Box::pin(async move {
            let row = query(
                "SELECT scope_key, scope_kind, scope_id, payload
                 FROM yunxi_plans WHERE id = $1",
            )
            .bind(id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            let Some(row) = row else {
                return Ok(None);
            };
            row_scope(&row)?;
            let value: Value = row
                .try_get("payload")
                .map_err(ExecutivePersistenceError::storage)?;
            decode_value(value, MAX_PLAN_BYTES, "plan").map(Some)
        })
    }

    fn update<'a>(&'a self, plan: &'a PlanState) -> ExecutiveStoreFuture<'a, PlanState> {
        Box::pin(async move {
            plan.validate()
                .map_err(|error| invalid(error.to_string()))?;
            let value = encode_value(plan, MAX_PLAN_BYTES, "plan")?;
            let version = checked_i64(plan.version, "plan version")?;
            let current_step = checked_i32(plan.current_step, "plan current step")?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            let row = query(
                "SELECT scope_key, scope_kind, scope_id
                 FROM yunxi_plans WHERE id = $1",
            )
            .bind(plan.id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            let Some(row) = row else {
                return Err(ExecutivePersistenceError::NotFound);
            };
            let parts = row_scope(&row)?;
            lock_scope_owner_for_write(&mut transaction, &parts).await?;
            let stored = query(
                "SELECT scope_key, scope_kind, scope_id, version
                 FROM yunxi_plans WHERE id = $1 FOR UPDATE",
            )
            .bind(plan.id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            let Some(stored) = stored else {
                return Err(ExecutivePersistenceError::NotFound);
            };
            let stored_parts = row_scope(&stored)?;
            if stored_parts.key != parts.key {
                return Err(ExecutivePersistenceError::Conflict);
            }
            let stored_version = stored
                .try_get::<i64, _>("version")
                .map_err(ExecutivePersistenceError::storage)?;
            let stored_version = u64::try_from(stored_version)
                .map_err(|_| invalid("stored plan version is negative"))?;
            if stored_version > plan.version {
                return Err(ExecutivePersistenceError::Conflict);
            }
            update_plan_in_transaction(
                &mut transaction,
                plan,
                &parts,
                value,
                version,
                current_step,
            )
            .await?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            Ok(plan.clone())
        })
    }

    fn delete(&self, id: yunxi_core::PlanId) -> ExecutiveStoreFuture<'_, bool> {
        Box::pin(async move {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            let row = query(
                "SELECT scope_key, scope_kind, scope_id
                 FROM yunxi_plans WHERE id = $1",
            )
            .bind(id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            let Some(row) = row else {
                transaction
                    .commit()
                    .await
                    .map_err(ExecutivePersistenceError::storage)?;
                return Ok(false);
            };
            let parts = row_scope(&row)?;
            lock_scope_owner_for_write(&mut transaction, &parts).await?;
            let result = query("DELETE FROM yunxi_plans WHERE id = $1")
                .bind(id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            Ok(result.rows_affected() == 1)
        })
    }
}

impl ExpectationStore for PostgresExecutiveStore {
    fn create<'a>(&'a self, expectation: &'a Expectation) -> ExecutiveStoreFuture<'a, Expectation> {
        Box::pin(async move {
            expectation
                .validate()
                .map_err(|reason| invalid(reason.to_owned()))?;
            let value = encode_value(expectation, MAX_EXPECTATION_BYTES, "expectation")?;
            let expected_event = serde_json::to_value(&expectation.expected_event)
                .map_err(ExecutivePersistenceError::storage)?;
            let parts = scope_parts(&ExecutiveScope::Global)?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            lock_scope_owner_for_write(&mut transaction, &parts).await?;
            let inserted = insert_expectation_in_transaction(
                &mut transaction,
                expectation,
                &parts,
                value,
                expected_event,
            )
            .await?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            if !inserted {
                return Err(ExecutivePersistenceError::Conflict);
            }
            Ok(expectation.clone())
        })
    }

    fn list_for_action(
        &self,
        action_id: yunxi_core::ActionId,
        limit: usize,
    ) -> ExecutiveStoreFuture<'_, Vec<Expectation>> {
        Box::pin(async move {
            let rows = query(
                "SELECT payload AS expectation FROM yunxi_expectations WHERE source_action_id = $1 ORDER BY updated_at DESC, id LIMIT $2",
            )
            .bind(action_id.as_uuid())
            .bind(checked_i64(bounded_limit(limit), "expectation list limit")?)
            .fetch_all(&self.pool)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            decode_rows(rows, "expectation", MAX_EXPECTATION_BYTES)
        })
    }

    fn update<'a>(&'a self, expectation: &'a Expectation) -> ExecutiveStoreFuture<'a, Expectation> {
        Box::pin(async move {
            expectation
                .validate()
                .map_err(|reason| invalid(reason.to_owned()))?;
            let value = encode_value(expectation, MAX_EXPECTATION_BYTES, "expectation")?;
            let expected_event = serde_json::to_value(&expectation.expected_event)
                .map_err(ExecutivePersistenceError::storage)?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            let row = query(
                "SELECT scope_key, scope_kind, scope_id
                 FROM yunxi_expectations WHERE id = $1",
            )
            .bind(expectation.id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            let Some(row) = row else {
                return Err(ExecutivePersistenceError::NotFound);
            };
            let parts = row_scope(&row)?;
            lock_scope_owner_for_write(&mut transaction, &parts).await?;
            let updated = update_expectation_in_transaction(
                &mut transaction,
                expectation,
                &parts,
                value,
                expected_event,
            )
            .await?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            if !updated {
                return Err(ExecutivePersistenceError::NotFound);
            }
            Ok(expectation.clone())
        })
    }

    fn delete(&self, id: yunxi_core::ExpectationId) -> ExecutiveStoreFuture<'_, bool> {
        Box::pin(async move {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            let row = query(
                "SELECT scope_key, scope_kind, scope_id
                 FROM yunxi_expectations WHERE id = $1",
            )
            .bind(id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            let Some(row) = row else {
                transaction
                    .commit()
                    .await
                    .map_err(ExecutivePersistenceError::storage)?;
                return Ok(false);
            };
            let parts = row_scope(&row)?;
            lock_scope_owner_for_write(&mut transaction, &parts).await?;
            let result = query("DELETE FROM yunxi_expectations WHERE id = $1")
                .bind(id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            Ok(result.rows_affected() == 1)
        })
    }
}

impl DecisionRecordPersistence for PostgresExecutiveStore {
    fn append<'a>(&'a self, record: &'a DecisionRecord) -> ExecutiveStoreFuture<'a, ()> {
        Box::pin(async move {
            record
                .validate()
                .map_err(|reason| invalid(reason.to_owned()))?;
            let value = encode_value(record, MAX_DECISION_BYTES, "decision")?;
            let created_at = record.created_at;
            let expires_at = created_at
                .checked_add_signed(Duration::days(DECISION_TTL_DAYS))
                .ok_or_else(|| invalid("decision retention expiry is out of range"))?;
            let parts = scope_parts(&ExecutiveScope::Global)?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            lock_scope_owner_for_write(&mut transaction, &parts).await?;
            query("DELETE FROM yunxi_decision_records WHERE expires_at <= NOW()")
                .execute(&mut *transaction)
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            insert_decision_in_transaction(&mut transaction, record, &parts, value, expires_at)
                .await?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            Ok(())
        })
    }

    fn recent(&self, limit: usize) -> ExecutiveStoreFuture<'_, Vec<DecisionRecord>> {
        Box::pin(async move {
            let rows = query(
                "SELECT record AS decision FROM yunxi_decision_records WHERE expires_at > NOW() ORDER BY created_at DESC, id DESC LIMIT $1",
            )
            .bind(checked_i64(bounded_limit(limit), "decision list limit")?)
            .fetch_all(&self.pool)
            .await
            .map_err(ExecutivePersistenceError::storage)?;
            let mut records: Vec<DecisionRecord> =
                decode_rows(rows, "decision", MAX_DECISION_BYTES)?;
            records.reverse();
            Ok(records)
        })
    }

    fn purge(&self) -> ExecutiveStoreFuture<'_, usize> {
        Box::pin(async move {
            let parts = scope_parts(&ExecutiveScope::Global)?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            lock_scope_owner_for_write(&mut transaction, &parts).await?;
            let result = query("DELETE FROM yunxi_decision_records WHERE expires_at <= NOW()")
                .execute(&mut *transaction)
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            let removed = checked_usize(result.rows_affected(), "purged decision count")?;
            transaction
                .commit()
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            Ok(removed)
        })
    }
}

async fn create_plan_with_scope(
    store: &PostgresExecutiveStore,
    plan: &PlanState,
    scope: &ExecutiveScope,
) -> Result<PlanState, ExecutivePersistenceError> {
    plan.validate()
        .map_err(|error| invalid(error.to_string()))?;
    let value = encode_value(plan, MAX_PLAN_BYTES, "plan")?;
    let parts = scope_parts(scope)?;
    let version = checked_i64(plan.version, "plan version")?;
    let current_step = checked_i32(plan.current_step, "plan current step")?;
    let mut transaction = store
        .pool
        .begin()
        .await
        .map_err(ExecutivePersistenceError::storage)?;
    lock_scope_owner_for_write(&mut transaction, &parts).await?;
    let result = query(
        r#"INSERT INTO yunxi_plans
           (id, scope_key, scope_kind, scope_id, goal_id, status, current_step, version, revision_count,
            payload, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
           ON CONFLICT (id) DO NOTHING"#,
    )
    .bind(plan.id.into_uuid())
    .bind(&parts.key)
    .bind(parts.kind)
    .bind(parts.id)
    .bind(plan.goal_id.into_uuid())
    .bind(enum_text(&plan.status)?)
    .bind(current_step)
    .bind(version)
    .bind(i16::from(plan.revision_count))
    .bind(value)
    .bind(plan.created_at)
    .bind(plan.updated_at)
    .execute(&mut *transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    if result.rows_affected() == 0 {
        return Err(ExecutivePersistenceError::Conflict);
    }
    insert_plan_steps(&mut transaction, plan).await?;
    transaction
        .commit()
        .await
        .map_err(ExecutivePersistenceError::storage)?;
    Ok(plan.clone())
}

impl PostgresExecutiveStore {
    async fn create_plan(
        &self,
        plan: &PlanState,
        scope: &ExecutiveScope,
    ) -> Result<PlanState, ExecutivePersistenceError> {
        create_plan_with_scope(self, plan, scope).await
    }
}

async fn upsert_plan_in_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    plan: &PlanState,
    parts: &ScopeParts,
    value: Value,
) -> Result<(), ExecutivePersistenceError> {
    let version = checked_i64(plan.version, "plan version")?;
    let current_step = checked_i32(plan.current_step, "plan current step")?;
    let result = query(
        r#"INSERT INTO yunxi_plans
           (id, scope_key, scope_kind, scope_id, goal_id, status, current_step, version, revision_count,
            payload, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
           ON CONFLICT (id) DO UPDATE SET
             scope_key = EXCLUDED.scope_key,
             scope_kind = EXCLUDED.scope_kind,
             scope_id = EXCLUDED.scope_id,
             goal_id = EXCLUDED.goal_id,
             status = EXCLUDED.status,
             current_step = EXCLUDED.current_step,
             version = EXCLUDED.version,
             revision_count = EXCLUDED.revision_count,
             payload = EXCLUDED.payload,
             created_at = EXCLUDED.created_at,
             updated_at = EXCLUDED.updated_at
           WHERE yunxi_plans.scope_key = EXCLUDED.scope_key"#,
    )
    .bind(plan.id.into_uuid())
    .bind(&parts.key)
    .bind(parts.kind)
    .bind(parts.id)
    .bind(plan.goal_id.into_uuid())
    .bind(enum_text(&plan.status)?)
    .bind(current_step)
    .bind(version)
    .bind(i16::from(plan.revision_count))
    .bind(value)
    .bind(plan.created_at)
    .bind(plan.updated_at)
    .execute(&mut **transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    if result.rows_affected() == 0 {
        return Err(ExecutivePersistenceError::Conflict);
    }
    replace_plan_steps(transaction, plan).await
}

async fn update_plan_in_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    plan: &PlanState,
    parts: &ScopeParts,
    value: Value,
    version: i64,
    current_step: i32,
) -> Result<(), ExecutivePersistenceError> {
    let result = query(
        r#"UPDATE yunxi_plans
           SET scope_kind = $3, scope_id = $4, goal_id = $5, status = $6,
               current_step = $7, version = $8, revision_count = $9,
               payload = $10, created_at = $11, updated_at = $12
           WHERE id = $1 AND scope_key = $2"#,
    )
    .bind(plan.id.into_uuid())
    .bind(&parts.key)
    .bind(parts.kind)
    .bind(parts.id)
    .bind(plan.goal_id.into_uuid())
    .bind(enum_text(&plan.status)?)
    .bind(current_step)
    .bind(version)
    .bind(i16::from(plan.revision_count))
    .bind(value)
    .bind(plan.created_at)
    .bind(plan.updated_at)
    .execute(&mut **transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    if result.rows_affected() != 1 {
        return Err(ExecutivePersistenceError::Conflict);
    }
    replace_plan_steps(transaction, plan).await
}

async fn insert_plan_steps(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    plan: &PlanState,
) -> Result<(), ExecutivePersistenceError> {
    for (index, step) in plan.steps.iter().enumerate() {
        let kind = serde_json::to_value(&step.kind).map_err(ExecutivePersistenceError::storage)?;
        query(
            r#"INSERT INTO yunxi_plan_steps
               (plan_id, step_index, step_id, kind, status, expected_result, max_attempts, backoff_seconds)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
        )
        .bind(plan.id.into_uuid())
        .bind(checked_i32(index, "plan step index")?)
        .bind(step.id.into_uuid())
        .bind(kind)
        .bind(enum_text(&step.status)?)
        .bind(step.expected_result.map(|id| id.into_uuid()))
        .bind(i16::from(step.retry_policy.max_attempts))
        .bind(checked_i64(
            step.retry_policy.backoff_seconds,
            "plan step backoff",
        )?)
        .execute(&mut **transaction)
        .await
        .map_err(ExecutivePersistenceError::storage)?;
    }
    Ok(())
}

async fn replace_plan_steps(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    plan: &PlanState,
) -> Result<(), ExecutivePersistenceError> {
    query("DELETE FROM yunxi_plan_steps WHERE plan_id = $1")
        .bind(plan.id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(ExecutivePersistenceError::storage)?;
    insert_plan_steps(transaction, plan).await
}

async fn insert_or_update_expectation_in_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    expectation: &Expectation,
    parts: &ScopeParts,
    value: Value,
    expected_event: Value,
) -> Result<bool, ExecutivePersistenceError> {
    let result = query(
        r#"INSERT INTO yunxi_expectations
           (id, scope_key, scope_kind, scope_id, source_action_id, expected_event,
            confidence, expires_at, status, payload, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW(), NOW())
           ON CONFLICT (id) DO UPDATE SET
             scope_key = EXCLUDED.scope_key,
             scope_kind = EXCLUDED.scope_kind,
             scope_id = EXCLUDED.scope_id,
             source_action_id = EXCLUDED.source_action_id,
             expected_event = EXCLUDED.expected_event,
             confidence = EXCLUDED.confidence,
             expires_at = EXCLUDED.expires_at,
             status = EXCLUDED.status,
             payload = EXCLUDED.payload,
             updated_at = NOW()
           WHERE yunxi_expectations.scope_key = EXCLUDED.scope_key"#,
    )
    .bind(expectation.id.into_uuid())
    .bind(&parts.key)
    .bind(parts.kind)
    .bind(parts.id)
    .bind(expectation.source_action_id.as_uuid())
    .bind(expected_event)
    .bind(expectation.confidence)
    .bind(expectation.expires_at)
    .bind(enum_text(&expectation.status)?)
    .bind(value)
    .execute(&mut **transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    Ok(result.rows_affected() == 1)
}

async fn insert_expectation_in_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    expectation: &Expectation,
    parts: &ScopeParts,
    value: Value,
    expected_event: Value,
) -> Result<bool, ExecutivePersistenceError> {
    let result = query(
        r#"INSERT INTO yunxi_expectations
           (id, scope_key, scope_kind, scope_id, source_action_id, expected_event,
            confidence, expires_at, status, payload, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW(), NOW())
           ON CONFLICT (id) DO NOTHING"#,
    )
    .bind(expectation.id.into_uuid())
    .bind(&parts.key)
    .bind(parts.kind)
    .bind(parts.id)
    .bind(expectation.source_action_id.as_uuid())
    .bind(expected_event)
    .bind(expectation.confidence)
    .bind(expectation.expires_at)
    .bind(enum_text(&expectation.status)?)
    .bind(value)
    .execute(&mut **transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    Ok(result.rows_affected() == 1)
}

async fn update_expectation_in_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    expectation: &Expectation,
    parts: &ScopeParts,
    value: Value,
    expected_event: Value,
) -> Result<bool, ExecutivePersistenceError> {
    let result = query(
        r#"UPDATE yunxi_expectations
           SET expected_event = $3, confidence = $4, expires_at = $5,
               status = $6, payload = $7, updated_at = NOW()
           WHERE id = $1 AND scope_key = $2"#,
    )
    .bind(expectation.id.into_uuid())
    .bind(&parts.key)
    .bind(expected_event)
    .bind(expectation.confidence)
    .bind(expectation.expires_at)
    .bind(enum_text(&expectation.status)?)
    .bind(value)
    .execute(&mut **transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    Ok(result.rows_affected() == 1)
}

async fn insert_decision_in_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    record: &DecisionRecord,
    parts: &ScopeParts,
    value: Value,
    expires_at: chrono::DateTime<Utc>,
) -> Result<bool, ExecutivePersistenceError> {
    let result = query(
        r#"INSERT INTO yunxi_decision_records
           (id, scope_key, scope_kind, scope_id, event_id, record, created_at, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           ON CONFLICT (event_id) DO NOTHING"#,
    )
    .bind(record.id.into_uuid())
    .bind(&parts.key)
    .bind(parts.kind)
    .bind(parts.id)
    .bind(record.event_id.into_uuid())
    .bind(value)
    .bind(record.created_at)
    .bind(expires_at)
    .execute(&mut **transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    Ok(result.rows_affected() == 1)
}

fn scope_parts(scope: &ExecutiveScope) -> Result<ScopeParts, ExecutivePersistenceError> {
    let parts = match scope {
        ExecutiveScope::Global => ScopeParts {
            key: "global".to_owned(),
            kind: "global",
            id: None,
            owner: DurableOwner::Global,
        },
        ExecutiveScope::Person { person_id } => {
            let id = person_id.into_uuid();
            ScopeParts {
                key: format!("person:{id}"),
                kind: "person",
                id: Some(id),
                owner: DurableOwner::Person(id),
            }
        }
        ExecutiveScope::Conversation { conversation_id } => {
            let id = conversation_id.into_uuid();
            ScopeParts {
                key: format!("conversation:{id}"),
                kind: "conversation",
                id: Some(id),
                owner: DurableOwner::Conversation(id),
            }
        }
        ExecutiveScope::Goal { goal_id } => {
            let id = goal_id.into_uuid();
            ScopeParts {
                key: format!("goal:{id}"),
                kind: "goal",
                id: Some(id),
                owner: DurableOwner::Global,
            }
        }
    };
    if parts.key.len() > 256 {
        return Err(invalid("Executive scope key exceeds its bound"));
    }
    Ok(parts)
}

async fn lock_scope_owner_for_write(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    parts: &ScopeParts,
) -> Result<(), ExecutivePersistenceError> {
    let owner_exists = match parts.owner {
        DurableOwner::Global if parts.key == GLOBAL_ERASURE_SCOPE_KEY => {
            lock_global_owner_for_write(transaction).await?;
            true
        }
        DurableOwner::Global => {
            owner_lock::lock_owner(transaction, DurableOwner::Global)
                .await
                .map_err(ExecutivePersistenceError::storage)?;
            true
        }
        owner => owner_lock::lock_and_owner_exists(transaction, owner)
            .await
            .map_err(ExecutivePersistenceError::storage)?,
    };
    if !owner_exists {
        return Err(invalid(format!(
            "Executive scope owner for {} does not exist",
            parts.key
        )));
    }
    Ok(())
}

/// Read the global erasure epoch from the barrier row. This deliberately uses
/// a normal statement snapshot: a writer must remember what it saw before it
/// waits for the advisory lock, then compare it with a fresh read after the
/// lock is acquired.
async fn read_global_generation(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
) -> Result<i64, ExecutivePersistenceError> {
    let generation = query_scalar::<Postgres, i64>(
        "SELECT generation
         FROM yunxi_executive_erasure_barriers
         WHERE scope_key = $1",
    )
    .bind(GLOBAL_ERASURE_SCOPE_KEY)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    let Some(generation) = generation else {
        return Err(invalid("global Executive erasure barrier row is missing"));
    };
    if generation < 0 {
        return Err(invalid("global Executive erasure generation is negative"));
    }
    Ok(generation)
}

/// Serialize a Global write against erasure and reject a stale transaction.
/// PostgreSQL's default READ COMMITTED isolation gives the second SELECT a
/// fresh view after an advisory-lock wait; a changed generation therefore
/// becomes an explicit Conflict instead of allowing stale data to reappear.
async fn lock_global_owner_for_write(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
) -> Result<i64, ExecutivePersistenceError> {
    let observed_generation = read_global_generation(transaction).await?;
    lock_global_owner_after_observation(transaction, observed_generation).await
}

async fn lock_global_owner_after_observation(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    observed_generation: i64,
) -> Result<i64, ExecutivePersistenceError> {
    owner_lock::lock_owner(transaction, DurableOwner::Global)
        .await
        .map_err(ExecutivePersistenceError::storage)?;
    let current_generation = read_global_generation(transaction).await?;
    if current_generation != observed_generation {
        return Err(ExecutivePersistenceError::Conflict);
    }
    Ok(current_generation)
}

/// Advance the Global epoch in the same transaction as the data deletion.
/// The compare-and-set predicate documents the protocol and turns an
/// unexpected barrier mutation into a conflict rather than silently losing an
/// erase boundary.
async fn advance_global_generation(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    expected_generation: i64,
) -> Result<(), ExecutivePersistenceError> {
    let next_generation = expected_generation
        .checked_add(1)
        .ok_or_else(|| invalid("global Executive erasure generation overflowed"))?;
    let result = query(
        "UPDATE yunxi_executive_erasure_barriers
         SET generation = $2, erased_at = NOW()
         WHERE scope_key = $1 AND generation = $3",
    )
    .bind(GLOBAL_ERASURE_SCOPE_KEY)
    .bind(next_generation)
    .bind(expected_generation)
    .execute(&mut **transaction)
    .await
    .map_err(ExecutivePersistenceError::storage)?;
    if result.rows_affected() != 1 {
        return Err(ExecutivePersistenceError::Conflict);
    }
    Ok(())
}

async fn erase_scope_rows(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    parts: &ScopeParts,
) -> Result<usize, ExecutivePersistenceError> {
    let mut removed = 0_usize;
    removed = removed
        .checked_add(checked_usize(
            query("DELETE FROM yunxi_executive_snapshots WHERE scope_key = $1")
                .bind(&parts.key)
                .execute(&mut **transaction)
                .await
                .map_err(ExecutivePersistenceError::storage)?
                .rows_affected(),
            "erased snapshot count",
        )?)
        .ok_or_else(|| invalid("erased row count overflow"))?;
    removed = removed
        .checked_add(checked_usize(
            query(
                "DELETE FROM yunxi_plan_steps
                 WHERE plan_id IN (SELECT id FROM yunxi_plans WHERE scope_key = $1)",
            )
            .bind(&parts.key)
            .execute(&mut **transaction)
            .await
            .map_err(ExecutivePersistenceError::storage)?
            .rows_affected(),
            "erased plan step count",
        )?)
        .ok_or_else(|| invalid("erased row count overflow"))?;
    removed = removed
        .checked_add(checked_usize(
            query("DELETE FROM yunxi_plans WHERE scope_key = $1")
                .bind(&parts.key)
                .execute(&mut **transaction)
                .await
                .map_err(ExecutivePersistenceError::storage)?
                .rows_affected(),
            "erased plan count",
        )?)
        .ok_or_else(|| invalid("erased row count overflow"))?;
    removed = removed
        .checked_add(checked_usize(
            query("DELETE FROM yunxi_expectations WHERE scope_key = $1")
                .bind(&parts.key)
                .execute(&mut **transaction)
                .await
                .map_err(ExecutivePersistenceError::storage)?
                .rows_affected(),
            "erased expectation count",
        )?)
        .ok_or_else(|| invalid("erased expectation count"))?;
    removed = removed
        .checked_add(checked_usize(
            query("DELETE FROM yunxi_decision_records WHERE scope_key = $1")
                .bind(&parts.key)
                .execute(&mut **transaction)
                .await
                .map_err(ExecutivePersistenceError::storage)?
                .rows_affected(),
            "erased decision count",
        )?)
        .ok_or_else(|| invalid("erased row count overflow"))?;
    Ok(removed)
}

pub(crate) fn scope_key(scope: &ExecutiveScope) -> Result<String, ExecutivePersistenceError> {
    scope_parts(scope).map(|parts| parts.key)
}

fn row_scope(row: &sqlx_postgres::PgRow) -> Result<ScopeParts, ExecutivePersistenceError> {
    let key: String = row
        .try_get("scope_key")
        .map_err(ExecutivePersistenceError::storage)?;
    let kind: String = row
        .try_get("scope_kind")
        .map_err(ExecutivePersistenceError::storage)?;
    let id: Option<uuid::Uuid> = row
        .try_get("scope_id")
        .map_err(ExecutivePersistenceError::storage)?;
    let scope = match (kind.as_str(), id) {
        ("global", None) => ExecutiveScope::Global,
        ("person", Some(id)) => ExecutiveScope::Person {
            person_id: yunxi_core::PersonId::from_uuid(id),
        },
        ("conversation", Some(id)) => ExecutiveScope::Conversation {
            conversation_id: yunxi_core::ConversationId::from_uuid(id),
        },
        ("goal", Some(id)) => ExecutiveScope::Goal {
            goal_id: yunxi_core::GoalId::from_uuid(id),
        },
        _ => return Err(invalid("stored Executive scope columns are inconsistent")),
    };
    let parts = scope_parts(&scope)?;
    if parts.kind != kind || parts.key != key {
        return Err(invalid(
            "stored Executive scope key does not match its columns",
        ));
    }
    Ok(parts)
}

fn checked_i64<T>(value: T, label: &str) -> Result<i64, ExecutivePersistenceError>
where
    T: TryInto<i64>,
{
    value
        .try_into()
        .map_err(|_| invalid(format!("{label} exceeds the PostgreSQL BIGINT boundary")))
}

fn checked_i32<T>(value: T, label: &str) -> Result<i32, ExecutivePersistenceError>
where
    T: TryInto<i32>,
{
    value
        .try_into()
        .map_err(|_| invalid(format!("{label} exceeds the PostgreSQL INTEGER boundary")))
}

fn checked_usize(value: u64, label: &str) -> Result<usize, ExecutivePersistenceError> {
    usize::try_from(value).map_err(|_| invalid(format!("{label} exceeds the host usize boundary")))
}

fn bounded_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIST_LIMIT)
}

fn enum_text<T: Serialize>(value: &T) -> Result<String, ExecutivePersistenceError> {
    let value = serde_json::to_value(value).map_err(ExecutivePersistenceError::storage)?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("Executive enum did not serialize as text"))
}

fn encode_value<T: Serialize>(
    value: &T,
    maximum_bytes: usize,
    label: &str,
) -> Result<Value, ExecutivePersistenceError> {
    let encoded = serde_json::to_value(value).map_err(ExecutivePersistenceError::storage)?;
    let bytes = serde_json::to_vec(&encoded).map_err(ExecutivePersistenceError::storage)?;
    if bytes.len() > maximum_bytes || !is_safe_payload(&encoded, 0) {
        return Err(invalid(format!(
            "{label} payload exceeds its safe persistence boundary"
        )));
    }
    Ok(encoded)
}

fn decode_value<T: DeserializeOwned + ValidateValue>(
    value: Value,
    maximum_bytes: usize,
    label: &str,
) -> Result<T, ExecutivePersistenceError> {
    let bytes = serde_json::to_vec(&value).map_err(ExecutivePersistenceError::storage)?;
    if bytes.len() > maximum_bytes || !is_safe_payload(&value, 0) {
        return Err(invalid(format!(
            "stored {label} payload exceeds its safe boundary"
        )));
    }
    let decoded = serde_json::from_value::<T>(value).map_err(|error| {
        ExecutivePersistenceError::InvalidRequest {
            reason: format!("stored {label} payload is invalid: {error}"),
        }
    })?;
    decoded.validate_value(label)?;
    Ok(decoded)
}

fn decode_rows<T: DeserializeOwned + ValidateValue>(
    rows: Vec<sqlx_postgres::PgRow>,
    column: &str,
    maximum_bytes: usize,
) -> Result<Vec<T>, ExecutivePersistenceError> {
    rows.into_iter()
        .map(|row| {
            let value: Value = row
                .try_get(column)
                .map_err(ExecutivePersistenceError::storage)?;
            decode_value(value, maximum_bytes, column)
        })
        .collect()
}

fn is_safe_payload(value: &Value, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match value {
        Value::Object(map) => map.iter().all(|(key, value)| {
            let normalized = key.to_ascii_lowercase();
            ![
                "prompt",
                "raw_prompt",
                "image_bytes",
                "kv_cache",
                "key_value_cache",
                "chain_of_thought",
                "api_key",
                "provider_url",
            ]
            .iter()
            .any(|forbidden| normalized == *forbidden)
                && is_safe_payload(value, depth + 1)
        }),
        Value::Array(items) => items.iter().all(|item| is_safe_payload(item, depth + 1)),
        _ => true,
    }
}

fn invalid(reason: impl Into<String>) -> ExecutivePersistenceError {
    ExecutivePersistenceError::InvalidRequest {
        reason: reason.into(),
    }
}

trait ValidateValue {
    fn validate_value(&self, label: &str) -> Result<(), ExecutivePersistenceError>;
}

impl ValidateValue for ExecutiveSnapshot {
    fn validate_value(&self, label: &str) -> Result<(), ExecutivePersistenceError> {
        self.validate()
            .map_err(|reason| invalid(format!("stored {label} is invalid: {reason}")))
    }
}

impl ValidateValue for PlanState {
    fn validate_value(&self, label: &str) -> Result<(), ExecutivePersistenceError> {
        self.validate()
            .map_err(|reason| invalid(format!("stored {label} is invalid: {reason}")))
    }
}

impl ValidateValue for Expectation {
    fn validate_value(&self, label: &str) -> Result<(), ExecutivePersistenceError> {
        self.validate()
            .map_err(|reason| invalid(format!("stored {label} is invalid: {reason}")))
    }
}

impl ValidateValue for DecisionRecord {
    fn validate_value(&self, label: &str) -> Result<(), ExecutivePersistenceError> {
        self.validate()
            .map_err(|reason| invalid(format!("stored {label} is invalid: {reason}")))
    }
}

#[cfg(test)]
mod tests {
    use super::PostgresExecutiveStore;
    use crate::database_test_support;
    use chrono::{Duration, Utc};
    use sqlx_core::query::query;
    use sqlx_core::query_scalar::query_scalar;
    use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
    use std::str::FromStr;
    use uuid::Uuid;
    use yunxi_core::executive::ExpectedEventPattern;
    use yunxi_core::{
        ActionId, ConversationId, DecisionDisposition, DecisionRecord, DecisionRecordPersistence,
        EventId, ExecutivePersistenceError, ExecutiveScope, ExecutiveSnapshot, ExecutiveStore,
        Expectation, ExpectationStore, GoalId, PersonId, PlanId, PlanState, PlanStatus, PlanStep,
        PlanStepKind, PlanStore,
    };

    struct TestDatabase {
        admin: PgPool,
        pool: PgPool,
        schema: String,
    }

    impl TestDatabase {
        async fn connect() -> Self {
            let database_url =
                std::env::var("DATABASE_URL").expect("requires PostgreSQL via DATABASE_URL");
            let admin = PgPoolOptions::new()
                .max_connections(2)
                .connect_with(
                    PgConnectOptions::from_str(&database_url)
                        .expect("DATABASE_URL should parse as PostgreSQL options"),
                )
                .await
                .expect("should connect to PostgreSQL");
            let schema = format!(
                "yunxi_exec_test_{}_{}",
                std::process::id(),
                Uuid::new_v4().simple()
            );
            query(&format!("CREATE SCHEMA \"{schema}\""))
                .execute(&admin)
                .await
                .expect("should create an isolated PostgreSQL schema");

            let pool = PgPoolOptions::new()
                .max_connections(4)
                .connect_with(
                    PgConnectOptions::from_str(&database_url)
                        .expect("DATABASE_URL should parse as PostgreSQL options")
                        .options([("search_path", schema.as_str())]),
                )
                .await
                .expect("should connect to the isolated PostgreSQL schema");
            for statement in [
                "CREATE TABLE yunxi_persons (id UUID PRIMARY KEY)",
                "CREATE TABLE yunxi_conversations (id UUID PRIMARY KEY)",
            ] {
                query(statement)
                    .execute(&pool)
                    .await
                    .expect("should create the minimal owner tables");
            }
            Self {
                admin,
                pool,
                schema,
            }
        }

        async fn cleanup(self) {
            self.pool.close().await;
            query(&format!("DROP SCHEMA \"{}\" CASCADE", self.schema))
                .execute(&self.admin)
                .await
                .expect("should remove the isolated PostgreSQL schema");
            self.admin.close().await;
        }
    }

    fn snapshot(version: u64) -> ExecutiveSnapshot {
        ExecutiveSnapshot {
            version,
            ..ExecutiveSnapshot::default()
        }
    }

    fn plan(now: chrono::DateTime<Utc>) -> PlanState {
        PlanState::new(
            PlanId::new(),
            GoalId::new(),
            vec![PlanStep::new(PlanStepKind::Observe)],
            now,
        )
        .expect("fixture plan should be valid")
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_executive_store_contracts_are_durable_bounded_and_scoped() {
        database_test_support::block_on(async {
            let database = TestDatabase::connect().await;
            let store = PostgresExecutiveStore::new(database.pool.clone());
            store
                .initialize_schema()
                .await
                .expect("first Executive migration should succeed");
            store
                .initialize_schema()
                .await
                .expect("Executive migration should be idempotent");

            let now = Utc::now();
            let global_plan = plan(now);
            let global_expectation = Expectation::new(
                ActionId::new(),
                ExpectedEventPattern::Custom("global-test-event".to_owned()),
                0.8,
                Some(now + Duration::hours(1)),
            );
            let global_decision =
                DecisionRecord::new(EventId::new(), DecisionDisposition::Reply, now);
            let mut global_snapshot = snapshot(2);
            global_snapshot.active_plan = Some(global_plan.clone());
            global_snapshot
                .pending_expectations
                .push(global_expectation.clone());
            global_snapshot
                .recent_decisions
                .push(global_decision.clone());

            ExecutiveStore::save(&store, &ExecutiveScope::Global, &global_snapshot)
                .await
                .expect("global snapshot should persist");
            assert_eq!(
                ExecutiveStore::load(&store, &ExecutiveScope::Global)
                    .await
                    .expect("global snapshot should load"),
                Some(global_snapshot.clone())
            );

            let person_id = PersonId::new();
            let conversation_id = ConversationId::new();
            query("INSERT INTO yunxi_persons (id) VALUES ($1)")
                .bind(person_id.into_uuid())
                .execute(&database.pool)
                .await
                .expect("should create the scoped person owner");
            query("INSERT INTO yunxi_conversations (id) VALUES ($1)")
                .bind(conversation_id.into_uuid())
                .execute(&database.pool)
                .await
                .expect("should create the scoped conversation owner");
            let person_scope = ExecutiveScope::Person { person_id };
            let conversation_scope = ExecutiveScope::Conversation { conversation_id };
            let person_snapshot = snapshot(3);
            let conversation_snapshot = snapshot(4);
            store
                .save_runtime_snapshot_for_scope(&person_scope, &person_snapshot)
                .await
                .expect("person snapshot should persist");
            store
                .save_runtime_snapshot_for_scope(&conversation_scope, &conversation_snapshot)
                .await
                .expect("conversation snapshot should persist");
            assert_eq!(
                ExecutiveStore::load(&store, &person_scope)
                    .await
                    .expect("person snapshot should load"),
                Some(person_snapshot)
            );
            assert_eq!(
                ExecutiveStore::load(&store, &conversation_scope)
                    .await
                    .expect("conversation snapshot should load"),
                Some(conversation_snapshot)
            );

            let mut stored_plan = plan(now);
            let created_plan = PlanStore::create(&store, &stored_plan)
                .await
                .expect("plan should persist");
            assert_eq!(
                PlanStore::get(&store, created_plan.id)
                    .await
                    .expect("plan should load"),
                Some(created_plan.clone())
            );
            stored_plan = created_plan.clone();
            stored_plan.version = 2;
            stored_plan.status = PlanStatus::Active;
            stored_plan
                .activate(now + Duration::seconds(1))
                .expect("fixture plan should activate");
            let updated_plan = PlanStore::update(&store, &stored_plan)
                .await
                .expect("plan update should persist");
            assert_eq!(updated_plan.version, 2);
            let step_count: i64 =
                query_scalar("SELECT COUNT(*) FROM yunxi_plan_steps WHERE plan_id = $1")
                    .bind(created_plan.id.into_uuid())
                    .fetch_one(&database.pool)
                    .await
                    .expect("plan steps should be durable");
            assert_eq!(step_count, 1);

            let mut expectation = Expectation::new(
                ActionId::new(),
                ExpectedEventPattern::Custom("crud-test-event".to_owned()),
                0.6,
                Some(now + Duration::hours(1)),
            );
            let created_expectation = ExpectationStore::create(&store, &expectation)
                .await
                .expect("expectation should persist");
            assert_eq!(
                ExpectationStore::list_for_action(&store, created_expectation.source_action_id, 8)
                    .await
                    .expect("expectation should list"),
                vec![created_expectation.clone()]
            );
            assert!(
                store
                    .pending_expectations_for_scope(&ExecutiveScope::Global, 8)
                    .await
                    .expect("pending expectations should list")
                    .iter()
                    .any(|item| item.id == created_expectation.id)
            );
            expectation = created_expectation.clone();
            expectation.status = yunxi_core::ExpectationStatus::Satisfied;
            ExpectationStore::update(&store, &expectation)
                .await
                .expect("expectation update should persist");
            assert!(
                ExpectationStore::delete(&store, expectation.id)
                    .await
                    .expect("expectation delete should persist")
            );
            assert!(
                !ExpectationStore::delete(&store, expectation.id)
                    .await
                    .expect("second expectation delete should be a no-op")
            );

            let expired_expectation = Expectation::new(
                ActionId::new(),
                ExpectedEventPattern::Custom("expired-test-event".to_owned()),
                0.5,
                Some(now - Duration::seconds(1)),
            );
            let expired_id = expired_expectation.id;
            ExpectationStore::create(&store, &expired_expectation)
                .await
                .expect("expired expectation should be accepted for cleanup");
            assert!(
                !store
                    .pending_expectations_for_scope(&ExecutiveScope::Global, 128)
                    .await
                    .expect("pending expectation query should work")
                    .iter()
                    .any(|item| item.id == expired_id)
            );

            let decision = DecisionRecord::new(EventId::new(), DecisionDisposition::Silent, now);
            DecisionRecordPersistence::append(&store, &decision)
                .await
                .expect("decision should append");
            DecisionRecordPersistence::append(&store, &decision)
                .await
                .expect("duplicate decision should be idempotent");
            let matching_decisions = DecisionRecordPersistence::recent(&store, 128)
                .await
                .expect("decisions should list")
                .into_iter()
                .filter(|item| item.event_id == decision.event_id)
                .count();
            assert_eq!(matching_decisions, 1);
            let expired_decision = DecisionRecord::new(
                EventId::new(),
                DecisionDisposition::Silent,
                now - Duration::days(8),
            );
            DecisionRecordPersistence::append(&store, &expired_decision)
                .await
                .expect("expired decision should append for TTL cleanup");
            assert!(
                DecisionRecordPersistence::recent(&store, 128)
                    .await
                    .expect("decision query should work")
                    .iter()
                    .all(|item| item.event_id != expired_decision.event_id)
            );
            assert!(
                DecisionRecordPersistence::purge(&store)
                    .await
                    .expect("decision purge should work")
                    >= 1
            );

            let newest = snapshot(5);
            ExecutiveStore::save(&store, &ExecutiveScope::Global, &newest)
                .await
                .expect("newer snapshot should persist");
            let stale = snapshot(4);
            assert!(matches!(
                ExecutiveStore::save(&store, &ExecutiveScope::Global, &stale).await,
                Err(ExecutivePersistenceError::Conflict)
            ));

            let malformed_goal = GoalId::new();
            query(
                "INSERT INTO yunxi_executive_snapshots
                    (scope_key, scope_kind, scope_id, version, snapshot)
                 VALUES ($1, 'goal', $2, 1, $3)",
            )
            .bind(format!("goal:{malformed_goal}"))
            .bind(malformed_goal.into_uuid())
            .bind(serde_json::json!({}))
            .execute(&database.pool)
            .await
            .expect("schema should accept an object for decode-time validation");
            assert!(matches!(
                ExecutiveStore::load(
                    &store,
                    &ExecutiveScope::Goal {
                        goal_id: malformed_goal
                    }
                )
                .await,
                Err(ExecutivePersistenceError::InvalidRequest { .. })
            ));
            let malformed_scope = query(
                "INSERT INTO yunxi_executive_snapshots
                    (scope_key, scope_kind, scope_id, version, snapshot)
                 VALUES ('malformed-scope', 'person', NULL, 1, $1)",
            )
            .bind(serde_json::to_value(snapshot(1)).expect("snapshot should encode"))
            .execute(&database.pool)
            .await;
            assert!(
                malformed_scope.is_err(),
                "database constraints should reject malformed scope columns"
            );

            let removed = ExecutiveStore::erase(&store, &person_scope)
                .await
                .expect("person scope should erase");
            assert!(removed >= 1);
            assert!(
                ExecutiveStore::load(&store, &person_scope)
                    .await
                    .expect("erased person scope should load")
                    .is_none()
            );
            assert!(
                ExecutiveStore::load(&store, &conversation_scope)
                    .await
                    .expect("other scope should remain readable")
                    .is_some()
            );

            let race_person = PersonId::new();
            query("INSERT INTO yunxi_persons (id) VALUES ($1)")
                .bind(race_person.into_uuid())
                .execute(&database.pool)
                .await
                .expect("should create the race owner");
            let race_scope = ExecutiveScope::Person {
                person_id: race_person,
            };
            let mut deleting = database
                .pool
                .begin()
                .await
                .expect("should begin owner deletion transaction");
            super::super::owner_lock::lock_owner(
                &mut deleting,
                super::super::owner_lock::DurableOwner::Person(race_person.into_uuid()),
            )
            .await
            .expect("owner deletion should acquire the canonical lock");
            query("DELETE FROM yunxi_persons WHERE id = $1")
                .bind(race_person.into_uuid())
                .execute(&mut *deleting)
                .await
                .expect("owner deletion should be pending");
            let race_store = store.clone();
            let race_scope_for_task = race_scope.clone();
            let race_snapshot = snapshot(6);
            let save_task = kovi::tokio::spawn(async move {
                race_store
                    .save_runtime_snapshot_for_scope(&race_scope_for_task, &race_snapshot)
                    .await
            });
            deleting
                .commit()
                .await
                .expect("owner deletion should commit");
            assert!(matches!(
                save_task.await.expect("owner race task should finish"),
                Err(ExecutivePersistenceError::InvalidRequest { .. })
            ));

            database.cleanup().await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_executive_store_migrates_legacy_snapshot_primary_key() {
        database_test_support::block_on(async {
            let database = TestDatabase::connect().await;
            let snapshot = snapshot(1);
            query(
                "CREATE TABLE yunxi_executive_snapshots (
                    legacy_id UUID PRIMARY KEY,
                    version BIGINT NOT NULL,
                    snapshot JSONB NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL
                )",
            )
            .execute(&database.pool)
            .await
            .expect("should create the legacy snapshot table");
            query(
                "INSERT INTO yunxi_executive_snapshots
                    (legacy_id, version, snapshot, updated_at)
                 VALUES ($1, $2, $3, NOW())",
            )
            .bind(Uuid::new_v4())
            .bind(1_i64)
            .bind(serde_json::to_value(&snapshot).expect("snapshot should encode"))
            .execute(&database.pool)
            .await
            .expect("should create a legacy snapshot row");

            let store = PostgresExecutiveStore::new(database.pool.clone());
            store
                .initialize_schema()
                .await
                .expect("legacy snapshot migration should succeed");
            store
                .initialize_schema()
                .await
                .expect("legacy snapshot migration should be idempotent");
            assert_eq!(
                ExecutiveStore::load(&store, &ExecutiveScope::Global)
                    .await
                    .expect("migrated snapshot should load"),
                Some(snapshot)
            );
            let primary_columns: i32 = query_scalar(
                "SELECT cardinality(conkey)
                 FROM pg_constraint
                 WHERE conrelid = 'yunxi_executive_snapshots'::regclass
                   AND contype = 'p'",
            )
            .fetch_one(&database.pool)
            .await
            .expect("migrated snapshot primary key should be queryable");
            assert_eq!(primary_columns, 1);

            database.cleanup().await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_global_erasure_generation_rejects_stale_writer_and_allows_new_writer() {
        database_test_support::block_on(async {
            let database = TestDatabase::connect().await;
            let store = PostgresExecutiveStore::new(database.pool.clone());
            store
                .initialize_schema()
                .await
                .expect("Executive migration should create the erasure barrier");
            store
                .initialize_schema()
                .await
                .expect("Executive migration should remain idempotent");

            let initial = snapshot(1);
            ExecutiveStore::save(&store, &ExecutiveScope::Global, &initial)
                .await
                .expect("initial global snapshot should persist");
            let initial_generation: i64 = query_scalar(
                "SELECT generation FROM yunxi_executive_erasure_barriers WHERE scope_key = 'global'",
            )
            .fetch_one(&database.pool)
            .await
            .expect("global generation should be present");

            // The writer takes its ordinary READ COMMITTED snapshot first and
            // deliberately waits before asking for the owner lock. This is
            // the stale transaction shape that can otherwise resurrect data.
            let writer_pool = database.pool.clone();
            let (observed_tx, observed_rx) = kovi::tokio::sync::oneshot::channel();
            let (start_lock_tx, start_lock_rx) = kovi::tokio::sync::oneshot::channel();
            let (lock_attempt_tx, lock_attempt_rx) = kovi::tokio::sync::oneshot::channel();
            let writer_task = kovi::tokio::spawn(async move {
                let mut transaction = writer_pool.begin().await.expect("writer transaction");
                let observed = super::read_global_generation(&mut transaction)
                    .await
                    .expect("writer should read the current generation");
                observed_tx
                    .send(observed)
                    .expect("generation observation receiver");
                start_lock_rx.await.expect("writer lock start signal");
                lock_attempt_tx
                    .send(())
                    .expect("writer lock attempt receiver");
                super::lock_global_owner_after_observation(&mut transaction, observed).await
            });
            assert_eq!(
                observed_rx.await.expect("writer generation observation"),
                initial_generation
            );

            // A second connection acquires the canonical Global lock after the
            // writer has observed the old epoch. The writer is then queued on
            // this lock while the eraser advances and commits the barrier.
            let mut eraser = database.pool.begin().await.expect("eraser transaction");
            let global_parts =
                super::scope_parts(&ExecutiveScope::Global).expect("global scope parts");
            let eraser_observed = super::read_global_generation(&mut eraser)
                .await
                .expect("eraser should read the current generation");
            assert_eq!(eraser_observed, initial_generation);
            super::super::owner_lock::lock_owner(
                &mut eraser,
                super::super::owner_lock::DurableOwner::Global,
            )
            .await
            .expect("eraser should acquire the canonical Global lock");

            start_lock_tx.send(()).expect("writer start receiver");
            lock_attempt_rx
                .await
                .expect("writer should attempt the locked generation check");
            kovi::tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                !writer_task.is_finished(),
                "writer must remain blocked while the eraser owns the Global lock"
            );

            super::advance_global_generation(&mut eraser, eraser_observed)
                .await
                .expect("eraser should advance the generation atomically");
            super::erase_scope_rows(&mut eraser, &global_parts)
                .await
                .expect("eraser should delete the complete Global projection");
            eraser
                .commit()
                .await
                .expect("eraser transaction should commit");

            // The writer's post-lock read sees the committed new generation,
            // so it is rejected before it can write the stale projection.
            let writer_result = writer_task
                .await
                .expect("writer task should finish after erasure");
            assert!(matches!(
                writer_result,
                Err(ExecutivePersistenceError::Conflict)
            ));

            let after_generation: i64 = query_scalar(
                "SELECT generation FROM yunxi_executive_erasure_barriers WHERE scope_key = 'global'",
            )
            .fetch_one(&database.pool)
            .await
            .expect("new generation should remain durable");
            assert_eq!(after_generation, initial_generation + 1);
            assert!(
                ExecutiveStore::load(&store, &ExecutiveScope::Global)
                    .await
                    .expect("erased global snapshot should load")
                    .is_none()
            );

            // A writer that starts after the barrier may use the new epoch.
            let replacement = snapshot(2);
            ExecutiveStore::save(&store, &ExecutiveScope::Global, &replacement)
                .await
                .expect("post-erasure writer should use the new generation");
            assert_eq!(
                ExecutiveStore::load(&store, &ExecutiveScope::Global)
                    .await
                    .expect("replacement snapshot should load"),
                Some(replacement)
            );

            database.cleanup().await;
        });
    }
}
