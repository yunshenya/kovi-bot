//! Structured "gag ledger" store: promises, running gags and grudges that the
//! bot owes or holds, per scope (global / person / conversation), bounded and
//! Postgres-backed. The host records entries (owner commands / extraction) and
//! injects open entries into reply context so she "remembers her debts".

use crate::config::GagLedgerConfig;
use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, Postgres};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Global/Conversation scopes are wired by future host paths.
pub(crate) enum GagScope {
    Global,
    Person(String),
    Conversation(String),
}

impl GagScope {
    fn kind_id(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Global => ("global", None),
            Self::Person(id) => ("person", Some(id)),
            Self::Conversation(id) => ("conversation", Some(id)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GagKind {
    Promise,
    Gag,
    Grudge,
}

impl GagKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Promise => "promise",
            Self::Gag => "gag",
            Self::Grudge => "grudge",
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Extra fields are surfaced by future list/UI paths.
pub(crate) struct GagEntry {
    pub id: Uuid,
    pub kind: GagKind,
    pub text: String,
    pub state: String,
    pub occurrence: i64,
    pub importance: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresGagStore {
    pool: PgPool,
    config: GagLedgerConfig,
}

impl PostgresGagStore {
    pub(crate) fn new(pool: PgPool, config: GagLedgerConfig) -> Self {
        Self { pool, config }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        query(
            "CREATE TABLE IF NOT EXISTS yunxi_gag_entries (
                id UUID PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_id TEXT,
                kind TEXT NOT NULL,
                text TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'open',
                occurrence BIGINT NOT NULL DEFAULT 1,
                importance INTEGER NOT NULL DEFAULT 50,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                last_mentioned_at TIMESTAMPTZ
            )",
        )
        .execute(&self.pool)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_gag_entries_scope_idx
             ON yunxi_gag_entries (scope_kind, scope_id, state)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record one entry. Bounded: prunes the oldest open entries of the same
    /// scope when the per-scope/global caps would be exceeded.
    pub(crate) async fn add(
        &self,
        scope: GagScope,
        kind: GagKind,
        text: &str,
        importance: u8,
    ) -> anyhow::Result<Uuid> {
        let (scope_kind, scope_id) = scope.kind_id();
        let now = Utc::now();
        let id = Uuid::new_v4();
        // Bound the scope: drop the oldest open entry of this scope if full.
        let scope_count: i64 = query_scalar(
            "SELECT count(*) FROM yunxi_gag_entries
             WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2 AND state = 'open'",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .fetch_one(&self.pool)
        .await?;
        if scope_count >= self.config.max_entries_per_scope() as i64 {
            query(
                "DELETE FROM yunxi_gag_entries
                 WHERE id IN (
                     SELECT id FROM yunxi_gag_entries
                     WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2 AND state = 'open'
                     ORDER BY created_at LIMIT 1
                 )",
            )
            .bind(scope_kind)
            .bind(scope_id)
            .execute(&self.pool)
            .await?;
        }
        let global_count: i64 =
            query_scalar("SELECT count(*) FROM yunxi_gag_entries WHERE state = 'open'")
                .fetch_one(&self.pool)
                .await?;
        if global_count >= self.config.max_global_entries() as i64 {
            query(
                "DELETE FROM yunxi_gag_entries
                 WHERE id IN (
                     SELECT id FROM yunxi_gag_entries
                     WHERE state = 'open' ORDER BY created_at LIMIT 1
                 )",
            )
            .execute(&self.pool)
            .await?;
        }
        query(
            "INSERT INTO yunxi_gag_entries
             (id, scope_kind, scope_id, kind, text, state, occurrence, importance, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, 'open', 1, $6, $7, $7)",
        )
        .bind(id)
        .bind(scope_kind)
        .bind(scope_id)
        .bind(kind.as_str())
        .bind(text)
        .bind(i64::from(importance.clamp(0, 100)))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Open entries for a scope (plus global entries), oldest first.
    pub(crate) async fn list_open(
        &self,
        scope: GagScope,
        limit: usize,
    ) -> anyhow::Result<Vec<GagEntry>> {
        let (scope_kind, scope_id) = scope.kind_id();
        let rows = query(
            "SELECT id, kind, text, state, occurrence, importance, created_at, updated_at
             FROM yunxi_gag_entries
             WHERE state = 'open'
               AND (scope_kind = 'global'
                    OR (scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2))
             ORDER BY created_at ASC
             LIMIT $3",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| GagEntry {
                id: row.get(0),
                kind: match row.get::<String, _>(1).as_str() {
                    "promise" => GagKind::Promise,
                    "grudge" => GagKind::Grudge,
                    _ => GagKind::Gag,
                },
                text: row.get(2),
                state: row.get(3),
                occurrence: row.get(4),
                importance: row.get(5),
                created_at: row.get(6),
                updated_at: row.get(7),
            })
            .collect())
    }

    /// Mark an entry fulfilled/voided by id. Returns false when not found.
    pub(crate) async fn fulfill(&self, id: Uuid) -> anyhow::Result<bool> {
        let result = query(
            "UPDATE yunxi_gag_entries SET state = 'fulfilled', updated_at = $2
             WHERE id = $1 AND state = 'open'",
        )
        .bind(id)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fulfill the single open entry whose id starts with `prefix` (the short
    /// id shown in the ledger list). Returns the matched id, or None when the
    /// prefix is missing/ambiguous/not open.
    pub(crate) async fn fulfill_by_prefix(&self, prefix: &str) -> anyhow::Result<Option<Uuid>> {
        let prefix = prefix.trim().to_lowercase();
        if prefix.is_empty() {
            return Ok(None);
        }
        let ids: Vec<Uuid> = query_scalar(
            "SELECT id FROM yunxi_gag_entries
             WHERE CAST(id AS TEXT) LIKE $1 AND state = 'open' LIMIT 2",
        )
        .bind(format!("{prefix}%"))
        .fetch_all(&self.pool)
        .await?;
        if ids.len() != 1 {
            return Ok(None);
        }
        let id = ids[0];
        if self.fulfill(id).await? {
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }

    #[allow(dead_code)] // wired by the future full-gag management surface.
    pub(crate) async fn void(&self, id: Uuid) -> anyhow::Result<bool> {
        let result = query(
            "UPDATE yunxi_gag_entries SET state = 'void', updated_at = $2
             WHERE id = $1 AND state = 'open'",
        )
        .bind(id)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete everything for a scope (data-deletion path), returns removed rows.
    pub(crate) async fn delete_for_scope(&self, scope: GagScope) -> anyhow::Result<u64> {
        let (scope_kind, scope_id) = scope.kind_id();
        let result = query(
            "DELETE FROM yunxi_gag_entries
             WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Prune old fulfilled/voided entries after the configured TTL.
    #[allow(dead_code)] // wired by the existing maintenance interval next.
    pub(crate) async fn prune_stale(&self) -> anyhow::Result<u64> {
        let cutoff = Utc::now() - chrono::Duration::days(self.config.entry_ttl_days() as i64);
        let result = query(
            "DELETE FROM yunxi_gag_entries
             WHERE state <> 'open' AND updated_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[allow(dead_code)]
fn _assert_postgres_type(_: &Postgres) {}
