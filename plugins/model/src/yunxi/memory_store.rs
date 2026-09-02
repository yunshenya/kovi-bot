//! Compatibility adapter from the platform-neutral MemoryStore port to the
//! existing bounded MemoryManager. QQ identifiers stay inside this module.

use super::identity_store::PostgresIdentityStore;
use super::owner_lock::{self, DurableOwner};
use crate::memory::{MemoryEntry, MemoryManager, MemoryType};
use chrono::{DateTime, Local, Utc};
use kovi::tokio::sync::Mutex as AsyncMutex;
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, Postgres};
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use uuid::Uuid;
use yunxi_core::{
    ConversationId, ConversationKind, Memory, MemoryDraft, MemoryId, MemoryKind, MemoryQuery,
    MemoryScope, MemoryStore, MemoryStoreError, MemoryStoreFuture, PersonId, lexical_terms,
};

const PRIVATE_CONTEXT: &str = "private_chat";
const GROUP_CONTEXT: &str = "group_chat";
const GLOBAL_CONTEXT: &str = "yunxi_global:";
const DIRECT_CONTEXT_PREFIX: &str = "yunxi_direct_chat:";
const FACT_CONTEXT_SUFFIX: &str = "|yunxi_kind=fact";
const MAX_LEGACY_SUBJECTS: usize = 32;
const MAX_MIGRATION_LEGACY_IDS: usize = 32;
const CORE_MEMORY_PROTECTED_IMPORTANCE: i16 = 70;

// A Core write is followed by the compatibility write into MemoryManager.
// Serializing that short window with maintenance prevents a local cleanup
// from deleting the Core row and then immediately resurrecting its legacy twin.
static MEMORY_RECONCILIATION_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

#[derive(Clone)]
pub(crate) struct PostgresMemoryStore {
    manager: Arc<MemoryManager>,
    #[allow(dead_code)]
    identities: Arc<PostgresIdentityStore>,
    pool: PgPool,
}

impl PostgresMemoryStore {
    pub(crate) fn new(
        manager: Arc<MemoryManager>,
        identities: Arc<PostgresIdentityStore>,
        pool: PgPool,
    ) -> Self {
        Self {
            manager,
            identities,
            pool,
        }
    }

    /// Additive Core memory table. Legacy rows remain untouched during the
    /// migration; the table is ready for dual-read/dual-write rollout.
    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        // The compatibility projection participates in the same transactions
        // as Core memory writes. Keep its minimal schema available even in
        // isolated tools/tests that initialize the Core adapter directly.
        query(
            r#"CREATE TABLE IF NOT EXISTS kovi_bot_memories (
                id TEXT PRIMARY KEY,
                subject_id BIGINT,
                scope_type TEXT CHECK (scope_type IN ('private', 'group') OR scope_type IS NULL),
                context TEXT NOT NULL,
                occurred_at TIMESTAMPTZ NOT NULL,
                importance SMALLINT NOT NULL,
                payload JSONB NOT NULL
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "ALTER TABLE kovi_bot_memories ADD COLUMN IF NOT EXISTS scope_type TEXT CHECK (scope_type IN ('private', 'group') OR scope_type IS NULL)",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_memories (
                id UUID PRIMARY KEY,
                scope_kind TEXT NOT NULL CHECK (scope_kind IN ('person', 'conversation', 'global')),
                scope_id UUID,
                kind TEXT NOT NULL CHECK (kind IN ('conversation', 'profile', 'event', 'preference', 'emotion', 'fact')),
                content TEXT NOT NULL CHECK (octet_length(content) BETWEEN 1 AND 8192),
                importance SMALLINT NOT NULL CHECK (importance BETWEEN 0 AND 100),
                tags JSONB NOT NULL DEFAULT '[]'::jsonb,
                occurred_at TIMESTAMPTZ NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                CHECK ((scope_kind = 'global' AND scope_id IS NULL) OR (scope_kind <> 'global' AND scope_id IS NOT NULL))
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_memories_scope_idx
             ON yunxi_memories (scope_kind, scope_id, occurred_at DESC)",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_memories_retention_idx
             ON yunxi_memories (importance DESC, occurred_at DESC, created_at DESC, id DESC)",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Apply the same retention and capacity policy to the canonical Core
    /// table that the legacy MemoryManager already applies to its cache.
    /// Cleanup is quiet when nothing changed; the maintenance loop reports
    /// only actual removals.
    pub(crate) async fn cleanup(&self, now: DateTime<Utc>) -> Result<u64, MemoryStoreError> {
        let _reconciliation_guard = MEMORY_RECONCILIATION_LOCK.lock().await;
        let config = crate::config::get().memory().clone();
        self.cleanup_with_limits(now, config.retention_days(), config.max_entries())
            .await
    }

    async fn cleanup_with_limits(
        &self,
        now: DateTime<Utc>,
        retention_days: i64,
        max_entries: usize,
    ) -> Result<u64, MemoryStoreError> {
        if retention_days <= 0 || max_entries == 0 {
            return Err(MemoryStoreError::InvalidRequest {
                reason: "memory cleanup limits must be positive".to_owned(),
            });
        }
        let max_entries =
            i64::try_from(max_entries).map_err(|_| MemoryStoreError::InvalidRequest {
                reason: "memory.max_entries exceeds PostgreSQL BIGINT".to_owned(),
            })?;
        let _legacy_save_guard = self.manager.acquire_save_lock().await;
        let mut transaction = self.pool.begin().await.map_err(MemoryStoreError::storage)?;
        // Serialize global capacity cleanup with other bot processes.
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(MemoryStoreError::storage)?;
        let cutoff = now - chrono::Duration::days(retention_days);
        let expired_rows = query(
            "DELETE FROM yunxi_memories
             WHERE occurred_at < $1 AND importance < $2
             RETURNING id, scope_kind, scope_id, kind",
        )
        .bind(cutoff)
        .bind(CORE_MEMORY_PROTECTED_IMPORTANCE)
        .fetch_all(&mut *transaction)
        .await
        .map_err(MemoryStoreError::storage)?;
        let capped_rows = query(
            r#"
            WITH ranked AS (
                SELECT id,
                       ROW_NUMBER() OVER (
                           ORDER BY importance DESC, occurred_at DESC,
                                    created_at DESC, id DESC
                       ) AS retention_rank
                FROM yunxi_memories
            ), evictable AS (
                SELECT id FROM ranked WHERE retention_rank > $1
            )
            DELETE FROM yunxi_memories AS memory
            USING evictable
            WHERE memory.id = evictable.id
            RETURNING memory.id, memory.scope_kind, memory.scope_id, memory.kind
            "#,
        )
        .bind(max_entries)
        .fetch_all(&mut *transaction)
        .await
        .map_err(MemoryStoreError::storage)?;
        let mut deleted = decode_deleted_memories(expired_rows)?;
        deleted.extend(decode_deleted_memories(capped_rows)?);
        // Remove the compatibility copy through the same transaction. The
        // cache is updated only after commit, so a failed delete rolls back
        // both database tables together.
        let mut deleted_legacy_ids = Vec::new();
        for memory in &deleted {
            deleted_legacy_ids.extend(self.delete_legacy_copy(&mut transaction, memory).await?);
        }
        transaction
            .commit()
            .await
            .map_err(MemoryStoreError::storage)?;
        for id in deleted_legacy_ids {
            self.manager.remove_cached_memory(&id).await;
        }
        u64::try_from(deleted.len()).map_err(|_| MemoryStoreError::InvalidRequest {
            reason: "memory cleanup result exceeds u64".to_owned(),
        })
    }

    /// Return only legacy rows that a completed backfill explicitly inserted
    /// for this canonical target. Dry-run and comparison-only ledger entries
    /// are not ownership proof. The extra probe row makes an unexpectedly
    /// large ledger fail closed instead of silently truncating cleanup.
    async fn migration_inserted_legacy_ids(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        target_id: Uuid,
    ) -> Result<Vec<String>, MemoryStoreError> {
        let table_exists = query_scalar::<Postgres, bool>(
            "SELECT to_regclass(current_schema() || '.yunxi_memory_migration_items') IS NOT NULL",
        )
        .fetch_one(&mut **transaction)
        .await
        .map_err(MemoryStoreError::storage)?;
        if !table_exists {
            return Ok(Vec::new());
        }
        let probe_limit = i64::try_from(MAX_MIGRATION_LEGACY_IDS + 1).unwrap_or(i64::MAX);
        let ids = query_scalar::<Postgres, String>(
            "SELECT DISTINCT legacy_id FROM yunxi_memory_migration_items
             WHERE target_id = $1 AND inserted IS TRUE
             ORDER BY legacy_id LIMIT $2",
        )
        .bind(target_id)
        .bind(probe_limit)
        .fetch_all(&mut **transaction)
        .await
        .map_err(MemoryStoreError::storage)?;
        if ids.len() > MAX_MIGRATION_LEGACY_IDS {
            return Err(MemoryStoreError::InvalidRequest {
                reason: format!(
                    "migration ledger has more than {MAX_MIGRATION_LEGACY_IDS} inserted rows for one target"
                ),
            });
        }
        Ok(ids)
    }

    async fn delete_legacy_copy(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        memory: &DeletedCoreMemory,
    ) -> Result<Vec<String>, MemoryStoreError> {
        let canonical_id = memory.id.to_string();
        // Gather every possible projection before resolving the live identity
        // mapping. A migrated row can have a non-UUID legacy ID, and the
        // mapping may already have been unlinked; either case must not make us
        // return early and leave the old row readable forever.
        let ledger_ids = self
            .migration_inserted_legacy_ids(transaction, memory.id)
            .await?;
        let mut candidate_ids = vec![canonical_id.clone()];
        candidate_ids.extend(ledger_ids.iter().cloned());
        candidate_ids.sort_unstable();
        candidate_ids.dedup();

        let scope = match (memory.scope_kind.as_str(), memory.scope_id) {
            ("global", None) => MemoryScope::Global,
            ("person", Some(id)) => MemoryScope::Person(PersonId::from_uuid(id)),
            ("conversation", Some(id)) => MemoryScope::Conversation(ConversationId::from_uuid(id)),
            _ => {
                return Err(MemoryStoreError::InvalidRequest {
                    reason: "canonical memory has an invalid scope".to_owned(),
                });
            }
        };
        // Resolve the compatibility scope before deleting anything. Every
        // legacy DELETE carries the subject/context predicate; this prevents
        // a canonical ID from removing an unrelated scope's old row.
        let mut deleted_ids = Vec::new();
        match self.legacy_scope_in_transaction(transaction, scope).await {
            Ok(legacy_scope) => {
                let base_context = legacy_scope.context;
                let fact_context = format!("{base_context}{FACT_CONTEXT_SUFFIX}");
                let contexts = if memory.kind == MemoryKind::Fact.as_str() {
                    vec![fact_context, base_context]
                } else {
                    vec![base_context, fact_context]
                };
                let subject_ids = if legacy_scope.subject_ids.is_empty() {
                    vec![None]
                } else {
                    legacy_scope.subject_ids.iter().copied().map(Some).collect()
                };
                for candidate_id in &candidate_ids {
                    for subject_id in &subject_ids {
                        for context in &contexts {
                            if let Some(id) = self
                                .manager
                                .delete_memory_for_domain_scope_in_transaction(
                                    transaction,
                                    candidate_id,
                                    *subject_id,
                                    context,
                                )
                                .await
                                .map_err(storage_error)?
                            {
                                deleted_ids.push(id);
                            }
                        }
                    }
                }
            }
            Err(MemoryStoreError::UnsupportedScope { .. }) => {
                // The durable migration ledger proves these IDs were derived
                // from this Core target. If the live alias mapping is gone,
                // use exact candidate deletes rather than guessing a numeric
                // subject/context that may now belong to a different person.
                // When the live mapping is gone, only a migration-ledger ID
                // is durable proof that an arbitrary legacy row belongs to
                // this Core target. Never let sorting or a canonical UUID
                // fallback turn an unproven same-ID row into a deletion.
                for candidate_id in &ledger_ids {
                    if let Some(id) = self
                        .manager
                        .delete_memory_by_id_in_transaction(transaction, candidate_id)
                        .await
                        .map_err(storage_error)?
                    {
                        deleted_ids.push(id);
                    }
                }
            }
            Err(error) => return Err(error),
        }
        Ok(deleted_ids)
    }

    #[allow(dead_code)]
    async fn legacy_scope(&self, scope: MemoryScope) -> Result<LegacyScope, MemoryStoreError> {
        match scope {
            MemoryScope::Global => Ok(LegacyScope {
                subject_ids: Vec::new(),
                context: GLOBAL_CONTEXT.to_owned(),
            }),
            MemoryScope::Person(person_id) => {
                let subject_ids = self
                    .identities
                    .qq_external_identities_for_person(person_id)
                    .await
                    .map_err(MemoryStoreError::storage)?
                    .into_iter()
                    .filter_map(|external| parse_positive_decimal(&external))
                    .take(MAX_LEGACY_SUBJECTS)
                    .collect::<Vec<_>>();
                if subject_ids.is_empty() {
                    return Err(MemoryStoreError::UnsupportedScope { scope });
                }
                Ok(LegacyScope {
                    subject_ids,
                    context: PRIVATE_CONTEXT.to_owned(),
                })
            }
            MemoryScope::Conversation(conversation_id) => {
                let external = self
                    .identities
                    .qq_external_conversations_for_id(conversation_id)
                    .await
                    .map_err(MemoryStoreError::storage)?;
                if external.is_empty() {
                    return Err(MemoryStoreError::UnsupportedScope { scope });
                }
                for (external, stored_kind) in external {
                    if let Some(group_id) = external
                        .strip_prefix("group:")
                        .filter(|value| !value.contains(':'))
                        .and_then(parse_positive_decimal)
                    {
                        if stored_kind == ConversationKind::Group {
                            return Ok(LegacyScope {
                                subject_ids: vec![group_id],
                                context: GROUP_CONTEXT.to_owned(),
                            });
                        }
                        continue;
                    }
                    if parse_direct_external(&external).is_some()
                        && stored_kind == ConversationKind::Direct
                    {
                        return Ok(LegacyScope {
                            // Legacy private memory is person-scoped. A direct
                            // conversation is narrower and must not merge two
                            // bot accounts that happen to share a peer ID.
                            subject_ids: Vec::new(),
                            context: format!("{DIRECT_CONTEXT_PREFIX}{conversation_id}"),
                        });
                    }
                }
                Err(invalid_mapping(
                    "QQ conversation external mapping has an unknown shape or kind",
                ))
            }
        }
    }

    /// Resolve a legacy projection while the caller's transaction is open.
    /// Using the same connection avoids pool starvation for a small pool and
    /// keeps cleanup's identity view consistent with its Core deletes.
    async fn legacy_scope_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        scope: MemoryScope,
    ) -> Result<LegacyScope, MemoryStoreError> {
        match scope {
            MemoryScope::Global => Ok(LegacyScope {
                subject_ids: Vec::new(),
                context: GLOBAL_CONTEXT.to_owned(),
            }),
            MemoryScope::Person(person_id) => {
                let rows = query(
                    "SELECT external_id
                     FROM yunxi_external_identities
                     WHERE platform = 'qq' AND person_id = $1
                     ORDER BY external_id LIMIT 32",
                )
                .bind(person_id.into_uuid())
                .fetch_all(&mut **transaction)
                .await
                .map_err(MemoryStoreError::storage)?;
                let external_ids = rows
                    .into_iter()
                    .map(|row| {
                        row.try_get::<String, _>("external_id")
                            .map_err(MemoryStoreError::storage)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let subject_ids = external_ids
                    .into_iter()
                    .filter_map(|external| parse_positive_decimal(&external))
                    .take(MAX_LEGACY_SUBJECTS)
                    .collect::<Vec<_>>();
                if subject_ids.is_empty() {
                    return Err(MemoryStoreError::UnsupportedScope { scope });
                }
                Ok(LegacyScope {
                    subject_ids,
                    context: PRIVATE_CONTEXT.to_owned(),
                })
            }
            MemoryScope::Conversation(conversation_id) => {
                let rows = query(
                    "SELECT external.external_id, conversation.kind
                     FROM yunxi_external_conversations AS external
                     JOIN yunxi_conversations AS conversation
                       ON conversation.id = external.conversation_id
                     WHERE external.platform = 'qq'
                       AND external.conversation_id = $1
                     ORDER BY external.external_id LIMIT 2",
                )
                .bind(conversation_id.into_uuid())
                .fetch_all(&mut **transaction)
                .await
                .map_err(MemoryStoreError::storage)?;
                if rows.is_empty() {
                    return Err(MemoryStoreError::UnsupportedScope { scope });
                }
                for row in rows {
                    let external = row
                        .try_get::<String, _>("external_id")
                        .map_err(MemoryStoreError::storage)?;
                    let stored_kind = row
                        .try_get::<String, _>("kind")
                        .map_err(MemoryStoreError::storage)?;
                    let stored_kind = ConversationKind::from_str(&stored_kind)
                        .map_err(MemoryStoreError::storage)?;
                    if let Some(group_id) = external
                        .strip_prefix("group:")
                        .filter(|value| !value.contains(':'))
                        .and_then(parse_positive_decimal)
                    {
                        if stored_kind == ConversationKind::Group {
                            return Ok(LegacyScope {
                                subject_ids: vec![group_id],
                                context: GROUP_CONTEXT.to_owned(),
                            });
                        }
                        continue;
                    }
                    if parse_direct_external(&external).is_some()
                        && stored_kind == ConversationKind::Direct
                    {
                        return Ok(LegacyScope {
                            subject_ids: Vec::new(),
                            context: format!("{DIRECT_CONTEXT_PREFIX}{conversation_id}"),
                        });
                    }
                }
                Err(invalid_mapping(
                    "QQ conversation external mapping has an unknown shape or kind",
                ))
            }
        }
    }

    async fn write_core_memory(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        id: MemoryId,
        draft: &MemoryDraft,
    ) -> Result<(), MemoryStoreError> {
        let (scope_kind, scope_id) = scope_parts(draft.scope());
        let tags = serde_json::to_value(draft.tags()).map_err(|error| {
            MemoryStoreError::InvalidRequest {
                reason: error.to_string(),
            }
        })?;
        query(
            "INSERT INTO yunxi_memories
                (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id.into_uuid())
        .bind(scope_kind)
        .bind(scope_id)
        .bind(draft.kind().as_str())
        .bind(draft.content())
        .bind(i16::from(draft.importance()))
        .bind(tags)
        .bind(draft.occurred_at())
        .execute(&mut **transaction)
        .await
        .map_err(MemoryStoreError::storage)?;
        Ok(())
    }

    async fn read_core_memories(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        query_input: &MemoryQuery,
    ) -> Result<Vec<Memory>, MemoryStoreError> {
        let (scope_kind, scope_id) = scope_parts(query_input.scope());
        let minimum = i16::from(query_input.min_importance().unwrap_or(0));
        // Read a bounded candidate window before ranking. Fetching only the
        // requested number here lets recency in SQL hide an older, more
        // relevant memory from the merged Core/legacy result set.
        let candidate_limit = memory_candidate_limit(query_input.limit());
        let search_text = normalize_search_text(query_input.text());
        let search_terms = lexical_terms(&search_text);
        let rows = query(
            r#"
            WITH candidates AS (
                (
                    SELECT id, kind, content, importance, tags, occurred_at, created_at
                    FROM yunxi_memories
                    WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2
                      AND importance >= $3
                      AND (
                            $4 = ''
                            OR STRPOS(LOWER(content), LOWER($4)) > 0
                            OR EXISTS (
                                SELECT 1
                                FROM UNNEST($5::TEXT[]) AS term
                                WHERE STRPOS(LOWER(content), LOWER(term)) > 0
                                   OR EXISTS (
                                        SELECT 1
                                        FROM jsonb_array_elements_text(
                                            CASE WHEN jsonb_typeof(tags) = 'array'
                                                 THEN tags ELSE '[]'::jsonb END
                                        ) AS tag(value)
                                        WHERE STRPOS(LOWER(tag.value), LOWER(term)) > 0
                                   )
                            )
                      )
                    ORDER BY occurred_at DESC, id DESC
                    LIMIT $6
                )
                UNION
                (
                    SELECT id, kind, content, importance, tags, occurred_at, created_at
                    FROM yunxi_memories
                    WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2
                      AND importance >= $3
                      AND (
                            $4 = ''
                            OR STRPOS(LOWER(content), LOWER($4)) > 0
                            OR EXISTS (
                                SELECT 1
                                FROM UNNEST($5::TEXT[]) AS term
                                WHERE STRPOS(LOWER(content), LOWER(term)) > 0
                                   OR EXISTS (
                                        SELECT 1
                                        FROM jsonb_array_elements_text(
                                            CASE WHEN jsonb_typeof(tags) = 'array'
                                                 THEN tags ELSE '[]'::jsonb END
                                        ) AS tag(value)
                                        WHERE STRPOS(LOWER(tag.value), LOWER(term)) > 0
                                   )
                            )
                      )
                    ORDER BY importance DESC, occurred_at DESC, id DESC
                    LIMIT $6
                )
            )
            SELECT id, kind, content, importance, tags, occurred_at, created_at
            FROM candidates
            ORDER BY importance DESC, occurred_at DESC, created_at DESC, id DESC
            LIMIT $7
            "#,
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(minimum)
        .bind(&search_text)
        .bind(search_terms)
        .bind(i64::try_from(candidate_limit).unwrap_or(128))
        .bind(i64::try_from(candidate_limit.saturating_mul(2)).unwrap_or(256))
        .fetch_all(&mut **transaction)
        .await
        .map_err(MemoryStoreError::storage)?;
        let memories = rows
            .into_iter()
            .map(|row| {
                let id = MemoryId::from_uuid(row.try_get("id").map_err(MemoryStoreError::storage)?);
                let kind = row
                    .try_get::<String, _>("kind")
                    .map_err(MemoryStoreError::storage)?
                    .parse()
                    .map_err(|error: yunxi_core::MemoryValidationError| {
                        MemoryStoreError::InvalidRequest {
                            reason: error.to_string(),
                        }
                    })?;
                let content = row
                    .try_get::<String, _>("content")
                    .map_err(MemoryStoreError::storage)?;
                let occurred_at = row
                    .try_get("occurred_at")
                    .map_err(MemoryStoreError::storage)?;
                let created_at = row
                    .try_get("created_at")
                    .map_err(MemoryStoreError::storage)?;
                let importance = u8::try_from(
                    row.try_get::<i16, _>("importance")
                        .map_err(MemoryStoreError::storage)?,
                )
                .unwrap_or(0);
                // A hand-edited/old row must not make the whole recall fail.
                // Treat a non-array JSON value as an empty tag list and keep
                // the textual memory available to the model.
                let tags_value: serde_json::Value =
                    row.try_get("tags").map_err(MemoryStoreError::storage)?;
                let tags = if tags_value.is_array() {
                    serde_json::from_value::<Vec<String>>(tags_value).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let draft = MemoryDraft::new(query_input.scope(), kind, content, occurred_at)
                    .map_err(|error| MemoryStoreError::InvalidRequest {
                        reason: error.to_string(),
                    })?
                    .with_importance(importance)
                    .and_then(|draft| draft.with_tags(tags))
                    .map_err(|error| MemoryStoreError::InvalidRequest {
                        reason: error.to_string(),
                    })?;
                Memory::from_draft(id, &draft, created_at).map_err(|error| {
                    MemoryStoreError::InvalidRequest {
                        reason: error.to_string(),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(memories
            .into_iter()
            .filter(|memory| search_text.is_empty() || memory_matches_text(memory, &search_text))
            .collect())
    }
}

impl MemoryStore for PostgresMemoryStore {
    fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
        Box::pin(async move {
            let _reconciliation_guard = MEMORY_RECONCILIATION_LOCK.lock().await;
            draft
                .validate()
                .map_err(|error| MemoryStoreError::InvalidRequest {
                    reason: error.to_string(),
                })?;
            let id = MemoryId::new();
            // The manager guard and PostgreSQL advisory lock use the same
            // order as cleanup, preventing local compaction and other bot
            // processes from observing only one side of the projection.
            let _legacy_save_guard = self.manager.acquire_save_lock().await;
            let mut transaction = self.pool.begin().await.map_err(MemoryStoreError::storage)?;
            owner_lock::lock_memory_maintenance(&mut transaction)
                .await
                .map_err(MemoryStoreError::storage)?;
            let owner = memory_owner(draft.scope());
            if !owner_lock::lock_and_owner_exists(&mut transaction, owner)
                .await
                .map_err(MemoryStoreError::storage)?
            {
                return Err(MemoryStoreError::InvalidRequest {
                    reason: format!("memory owner {owner:?} does not exist"),
                });
            }
            // Resolve the compatibility projection from the same transaction
            // after the owner lock. An identity unlink/reassignment cannot
            // change the subject or conversation behind this write.
            let legacy_scope = match self
                .legacy_scope_in_transaction(&mut transaction, draft.scope())
                .await
            {
                Ok(scope) => Some(scope),
                Err(MemoryStoreError::UnsupportedScope { .. }) => None,
                Err(error) => return Err(error),
            };
            let legacy_entry = legacy_scope.map(|scope| {
                let context = if draft.kind() == MemoryKind::Fact {
                    format!("{}{FACT_CONTEXT_SUFFIX}", scope.context)
                } else {
                    scope.context
                };
                MemoryEntry {
                    id: id.to_string(),
                    content: draft.content().to_string(),
                    timestamp: draft.occurred_at().with_timezone(&Local),
                    memory_type: legacy_kind(draft.kind()),
                    importance: importance_to_legacy(draft.importance()),
                    tags: draft.tags().to_vec(),
                    context,
                    subject_id: scope.subject_ids.first().copied(),
                }
            });
            self.write_core_memory(&mut transaction, id, draft).await?;
            let duplicate_id = if let Some(entry) = &legacy_entry {
                self.manager
                    .upsert_memory_in_transaction(&mut transaction, entry)
                    .await
                    .map_err(storage_error)?
            } else {
                None
            };
            transaction
                .commit()
                .await
                .map_err(MemoryStoreError::storage)?;
            if let Some(entry) = legacy_entry {
                self.manager
                    .publish_memory_after_transaction(entry, duplicate_id)
                    .await;
            }
            Memory::from_draft(id, draft, Utc::now()).map_err(|error| {
                MemoryStoreError::InvalidRequest {
                    reason: error.to_string(),
                }
            })
        })
    }

    fn recall<'a>(&'a self, query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>> {
        Box::pin(async move {
            let mut transaction = self.pool.begin().await.map_err(MemoryStoreError::storage)?;
            owner_lock::lock_memory_read(&mut transaction)
                .await
                .map_err(MemoryStoreError::storage)?;
            // A shared maintenance barrier lets concurrent recalls proceed in
            // parallel while keeping the Core snapshot and legacy mapping
            // consistent with writers and identity unlink operations.
            let mut memories = self.read_core_memories(&mut transaction, query).await?;
            let legacy_scope = match self
                .legacy_scope_in_transaction(&mut transaction, query.scope())
                .await
            {
                Ok(scope) => Some(scope),
                Err(MemoryStoreError::UnsupportedScope { .. }) => None,
                Err(error) => return Err(error),
            };
            let Some(scope) = legacy_scope else {
                transaction
                    .commit()
                    .await
                    .map_err(MemoryStoreError::storage)?;
                rank_memories(&mut memories, query.text());
                memories.truncate(query.limit());
                return Ok(memories);
            };
            let fetch_limit = memory_candidate_limit(query.limit());
            let mut entries = Vec::new();
            let subject_ids = if scope.subject_ids.is_empty() {
                vec![None]
            } else {
                scope.subject_ids.iter().copied().map(Some).collect()
            };
            for subject_id in subject_ids {
                let scoped = self
                    .manager
                    .get_recent_memories_for_domain_scope_in_transaction(
                        &mut transaction,
                        subject_id,
                        &scope.context,
                        fetch_limit,
                    )
                    .await
                    .map_err(MemoryStoreError::storage)?;
                entries.extend(scoped);
            }
            let mut seen_ids = HashSet::new();
            entries.retain(|entry| seen_ids.insert(entry.id.clone()));
            let matches = entries
                .into_iter()
                .filter(|entry| {
                    query.min_importance().is_none_or(|minimum| {
                        u16::from(entry.importance) * 10 >= u16::from(minimum)
                    })
                })
                .filter(|entry| query.text().trim().is_empty() || matches_text(entry, query.text()))
                .collect::<Vec<_>>();
            memories.extend(
                matches
                    .into_iter()
                    .filter_map(|entry| to_core_memory(query.scope(), entry).ok())
                    .collect::<Vec<_>>(),
            );
            transaction
                .commit()
                .await
                .map_err(MemoryStoreError::storage)?;
            let mut seen = HashSet::new();
            memories.retain(|memory| seen.insert(memory.id()));
            rank_memories(&mut memories, query.text());
            memories.truncate(query.limit());
            Ok(memories)
        })
    }

    fn forget(&self, scope: MemoryScope, id: MemoryId) -> MemoryStoreFuture<'_, bool> {
        Box::pin(async move {
            let _reconciliation_guard = MEMORY_RECONCILIATION_LOCK.lock().await;
            let (scope_kind, scope_id) = scope_parts(scope);
            // Keep a forget operation ordered with the maintenance transaction
            // so a just-deleted row cannot be reintroduced by a stale writer.
            let _legacy_save_guard = self.manager.acquire_save_lock().await;
            let mut transaction = self.pool.begin().await.map_err(MemoryStoreError::storage)?;
            owner_lock::lock_memory_maintenance(&mut transaction)
                .await
                .map_err(MemoryStoreError::storage)?;
            // Resolve identity mappings inside the same transaction as the
            // scoped delete. If the mapping disappeared, fail closed for the
            // compatibility table rather than guessing another subject.
            let legacy_scope = match self
                .legacy_scope_in_transaction(&mut transaction, scope)
                .await
            {
                Ok(scope) => Some(scope),
                Err(MemoryStoreError::UnsupportedScope { .. }) => None,
                Err(error) => return Err(error),
            };
            let canonical_id = id.to_string();
            let ledger_ids = self
                .migration_inserted_legacy_ids(&mut transaction, id.into_uuid())
                .await?;
            let has_migration_ledger = !ledger_ids.is_empty();
            let mut candidate_ids = vec![canonical_id.clone()];
            candidate_ids.extend(ledger_ids.iter().cloned());
            candidate_ids.sort_unstable();
            candidate_ids.dedup();
            let core_deleted = query(
                "DELETE FROM yunxi_memories WHERE id = $1 AND scope_kind = $2
                 AND scope_id IS NOT DISTINCT FROM $3",
            )
            .bind(id.into_uuid())
            .bind(scope_kind)
            .bind(scope_id)
            .execute(&mut *transaction)
            .await
            .map_err(MemoryStoreError::storage)?
            .rows_affected()
                > 0;
            let mut deleted_legacy_ids = Vec::new();
            if let Some(scope) = legacy_scope {
                let subject_ids = if scope.subject_ids.is_empty() {
                    vec![None]
                } else {
                    scope.subject_ids.iter().copied().map(Some).collect()
                };
                let fact_context = format!("{}{FACT_CONTEXT_SUFFIX}", scope.context);
                let contexts = [scope.context.as_str(), fact_context.as_str()];
                for candidate_id in &candidate_ids {
                    for subject_id in &subject_ids {
                        for context in &contexts {
                            if let Some(actual_id) = self
                                .manager
                                .delete_memory_for_domain_scope_in_transaction(
                                    &mut transaction,
                                    candidate_id,
                                    *subject_id,
                                    context,
                                )
                                .await
                                .map_err(storage_error)?
                            {
                                deleted_legacy_ids.push(actual_id);
                            }
                        }
                    }
                }
            } else if has_migration_ledger {
                // A migration ledger entry is durable proof that the
                // non-canonical candidate belongs to this Core ID. If the
                // live identity mapping is gone, clean those exact legacy
                // rows while deliberately leaving an unproven canonical ID
                // untouched.
                for candidate_id in &ledger_ids {
                    if let Some(actual_id) = self
                        .manager
                        .delete_memory_by_id_in_transaction(&mut transaction, candidate_id)
                        .await
                        .map_err(storage_error)?
                    {
                        deleted_legacy_ids.push(actual_id);
                    }
                }
            }
            transaction
                .commit()
                .await
                .map_err(MemoryStoreError::storage)?;
            let deleted_legacy = !deleted_legacy_ids.is_empty();
            for actual_id in deleted_legacy_ids {
                self.manager.remove_cached_memory(&actual_id).await;
            }
            Ok(core_deleted || deleted_legacy)
        })
    }
}

#[derive(Debug, Clone)]
struct LegacyScope {
    subject_ids: Vec<i64>,
    context: String,
}

#[derive(Debug)]
struct DeletedCoreMemory {
    id: Uuid,
    scope_kind: String,
    scope_id: Option<Uuid>,
    kind: String,
}

fn decode_deleted_memories(
    rows: Vec<sqlx_postgres::PgRow>,
) -> Result<Vec<DeletedCoreMemory>, MemoryStoreError> {
    rows.into_iter()
        .map(|row| {
            Ok(DeletedCoreMemory {
                id: row.try_get("id").map_err(MemoryStoreError::storage)?,
                scope_kind: row
                    .try_get("scope_kind")
                    .map_err(MemoryStoreError::storage)?,
                scope_id: row.try_get("scope_id").map_err(MemoryStoreError::storage)?,
                kind: row.try_get("kind").map_err(MemoryStoreError::storage)?,
            })
        })
        .collect()
}

fn scope_parts(scope: MemoryScope) -> (&'static str, Option<Uuid>) {
    match scope {
        MemoryScope::Person(id) => ("person", Some(id.into_uuid())),
        MemoryScope::Conversation(id) => ("conversation", Some(id.into_uuid())),
        MemoryScope::Global => ("global", None),
    }
}

const fn memory_owner(scope: MemoryScope) -> DurableOwner {
    match scope {
        MemoryScope::Person(id) => DurableOwner::Person(id.into_uuid()),
        MemoryScope::Conversation(id) => DurableOwner::Conversation(id.into_uuid()),
        MemoryScope::Global => DurableOwner::Global,
    }
}

fn parse_direct_external(value: &str) -> Option<(i64, i64)> {
    let mut parts = value.strip_prefix("direct:")?.split(':');
    let self_id = parse_positive_decimal(parts.next()?)?;
    let peer_id = parse_positive_decimal(parts.next()?)?;
    parts.next().is_none().then_some((self_id, peer_id))
}

fn parse_positive_decimal(value: &str) -> Option<i64> {
    // QQ route IDs are canonical decimal strings. Rust's integer parser also
    // accepts a leading `+` and leading zeroes; accepting those aliases here
    // could make a malformed identity purge another user's subject rows.
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let parsed = value.parse::<i64>().ok()?;
    (parsed > 0 && parsed.to_string() == value).then_some(parsed)
}

fn invalid_mapping(reason: &'static str) -> MemoryStoreError {
    MemoryStoreError::InvalidRequest {
        reason: reason.to_string(),
    }
}

fn storage_error(error: anyhow::Error) -> MemoryStoreError {
    MemoryStoreError::storage(std::io::Error::other(error.to_string()))
}

fn legacy_kind(kind: MemoryKind) -> MemoryType {
    match kind {
        MemoryKind::Conversation => MemoryType::Conversation,
        MemoryKind::Profile => MemoryType::UserProfile,
        MemoryKind::Event | MemoryKind::Fact => MemoryType::Event,
        MemoryKind::Preference => MemoryType::Preference,
        MemoryKind::Emotion => MemoryType::Emotion,
    }
}

fn importance_to_legacy(importance: u8) -> u8 {
    // Legacy storage has ten buckets. Preserve any non-zero Core importance
    // instead of truncating 1..9 to an apparently forgotten value.
    importance.saturating_add(9).saturating_div(10).min(10)
}

/// Keep database and legacy reads bounded while leaving enough candidates for
/// the merged ranking to recover older, high-value memories.
fn memory_candidate_limit(limit: usize) -> usize {
    limit.saturating_mul(4).clamp(1, 128)
}

fn matches_text(entry: &MemoryEntry, query: &str) -> bool {
    let query = normalize_search_text(query);
    if query.is_empty() {
        return true;
    }
    let searchable = searchable_text(&entry.content, &entry.tags);
    if has_search_signal(&query) && searchable.contains(&query) {
        return true;
    }
    lexical_terms(&query)
        .iter()
        .any(|term| searchable.contains(term))
}

fn normalize_search_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn searchable_text(content: &str, tags: &[String]) -> String {
    normalize_search_text(&format!("{content} {}", tags.join(" ")))
}

fn has_search_signal(value: &str) -> bool {
    value.chars().any(char::is_alphanumeric)
}

fn memory_matches_text(memory: &Memory, query: &str) -> bool {
    text_relevance(memory.content(), memory.tags(), query) > 0
}

/// Rank records in a stable order that keeps semantic matches and important
/// facts ahead of merely recent chat noise. Freshness is intentionally the
/// final signal so it cannot displace an equally relevant, high-importance
/// long-term memory.
fn rank_memories(memories: &mut [Memory], query: &str) {
    let now = Utc::now();
    memories.sort_by(|left, right| {
        memory_rank(right, query, now)
            .cmp(&memory_rank(left, query, now))
            .then_with(|| right.occurred_at().cmp(&left.occurred_at()))
            .then_with(|| right.created_at().cmp(&left.created_at()))
            .then_with(|| right.id().cmp(&left.id()))
    });
}

fn memory_rank(memory: &Memory, query: &str, now: DateTime<Utc>) -> (u16, u8, u16) {
    (
        text_relevance(memory.content(), memory.tags(), query),
        memory.importance(),
        freshness_score(memory.occurred_at(), now),
    )
}

/// Return a 0..200 lexical score: an exact phrase match receives a bonus,
/// while token coverage still lets a multi-word query find naturally worded
/// memories. Empty queries intentionally contribute no lexical score.
fn text_relevance(content: &str, tags: &[String], query: &str) -> u16 {
    let query = normalize_search_text(query);
    if query.is_empty() {
        return 0;
    }
    let searchable = searchable_text(content, tags);
    let terms = lexical_terms(&query);
    let exact_bonus = if has_search_signal(&query) && searchable.contains(&query) {
        100
    } else {
        0
    };
    if terms.is_empty() {
        return exact_bonus;
    }
    let matched_terms = terms
        .iter()
        .filter(|term| searchable.contains(term.as_str()))
        .count();
    if matched_terms == 0 {
        return 0;
    }
    let coverage = u16::try_from(matched_terms.saturating_mul(100) / terms.len()).unwrap_or(100);
    exact_bonus.saturating_add(coverage)
}

fn freshness_score(occurred_at: DateTime<Utc>, now: DateTime<Utc>) -> u16 {
    const FRESHNESS_HORIZON_DAYS: i64 = 365;
    let age_days = now
        .signed_duration_since(occurred_at)
        .num_days()
        .clamp(0, FRESHNESS_HORIZON_DAYS);
    u16::try_from(
        ((FRESHNESS_HORIZON_DAYS - age_days).saturating_mul(100)) / FRESHNESS_HORIZON_DAYS,
    )
    .unwrap_or(0)
}

fn to_core_memory(scope: MemoryScope, entry: MemoryEntry) -> Result<Memory, MemoryStoreError> {
    let id = Uuid::parse_str(&entry.id)
        .map(MemoryId::from_uuid)
        .unwrap_or_else(|_| {
            MemoryId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_URL, entry.id.as_bytes()))
        });
    let occurred_at: DateTime<Utc> = entry.timestamp.with_timezone(&Utc);
    let draft = MemoryDraft::new(
        scope,
        core_kind(entry.memory_type, &entry.context),
        entry.content,
        occurred_at,
    )
    .map_err(|error| MemoryStoreError::InvalidRequest {
        reason: error.to_string(),
    })?
    .with_importance(entry.importance.saturating_mul(10))
    .and_then(|draft| draft.with_tags(entry.tags))
    .map_err(|error| MemoryStoreError::InvalidRequest {
        reason: error.to_string(),
    })?;
    Memory::from_draft(id, &draft, occurred_at).map_err(|error| MemoryStoreError::InvalidRequest {
        reason: error.to_string(),
    })
}

fn core_kind(kind: MemoryType, context: &str) -> MemoryKind {
    match kind {
        MemoryType::Conversation => MemoryKind::Conversation,
        MemoryType::UserProfile | MemoryType::GroupInfo => MemoryKind::Profile,
        MemoryType::Event if context.ends_with(FACT_CONTEXT_SUFFIX) => MemoryKind::Fact,
        MemoryType::Event => MemoryKind::Event,
        MemoryType::Preference => MemoryKind::Preference,
        MemoryType::Emotion => MemoryKind::Emotion,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DIRECT_CONTEXT_PREFIX, FACT_CONTEXT_SUFFIX, GLOBAL_CONTEXT, PRIVATE_CONTEXT,
        PostgresMemoryStore, core_kind, importance_to_legacy, matches_text, memory_candidate_limit,
        parse_direct_external, parse_positive_decimal, rank_memories, text_relevance,
    };
    use crate::memory::{MemoryEntry, MemoryManager, MemoryType};
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use crate::yunxi::memory_migration::MemoryMigrationService;
    use chrono::{Duration, Utc};
    use sqlx_core::query::query;
    use sqlx_core::query_scalar::query_scalar;
    use sqlx_core::row::Row;
    use sqlx_postgres::{PgPoolOptions, Postgres};
    use std::sync::Arc;
    use uuid::Uuid;
    use yunxi_core::{
        ConversationId, Memory, MemoryDraft, MemoryId, MemoryKind, MemoryQuery, MemoryScope,
        MemoryStore, MemoryStoreError, PersonId,
    };

    #[test]
    fn external_numeric_mapping_never_accepts_non_positive_values() {
        assert_eq!(parse_positive_decimal("123"), Some(123));
        assert_eq!(parse_positive_decimal("0"), None);
        assert_eq!(parse_positive_decimal("-1"), None);
        assert_eq!(parse_positive_decimal("01"), None);
        assert_eq!(parse_positive_decimal("+123"), None);
        assert_eq!(parse_positive_decimal("nickname"), None);
    }

    #[test]
    fn direct_external_keys_are_strictly_bounded() {
        assert_eq!(parse_direct_external("direct:10:20"), Some((10, 20)));
        assert_eq!(parse_direct_external("direct:10:20:30"), None);
        assert_eq!(parse_direct_external("direct:bot:20"), None);
        assert_eq!(parse_direct_external("direct:0:20"), None);
    }

    #[test]
    fn fact_kind_and_importance_keep_a_bounded_legacy_projection() {
        assert_eq!(
            core_kind(MemoryType::Event, "private_chat|yunxi_kind=fact"),
            MemoryKind::Fact
        );
        assert_eq!(
            core_kind(MemoryType::Event, "private_chat"),
            MemoryKind::Event
        );
        assert_eq!(importance_to_legacy(0), 0);
        assert_eq!(importance_to_legacy(1), 1);
        assert_eq!(importance_to_legacy(100), 10);
    }

    #[test]
    fn memory_candidate_window_stays_bounded() {
        assert_eq!(memory_candidate_limit(1), 4);
        assert_eq!(memory_candidate_limit(8), 32);
        assert_eq!(memory_candidate_limit(32), 128);
        assert_eq!(memory_candidate_limit(usize::MAX), 128);
    }

    #[test]
    fn lexical_matching_handles_cjk_without_whitespace_and_tags() {
        let entry = MemoryEntry {
            id: "cjk-match".to_owned(),
            content: "用户喜欢爵士音乐".to_owned(),
            timestamp: chrono::Local::now(),
            memory_type: MemoryType::Preference,
            importance: 8,
            tags: vec!["夜间偏好".to_owned()],
            context: "private_chat".to_owned(),
            subject_id: Some(1),
        };
        assert!(matches_text(&entry, "喜欢音乐"));
        assert!(matches_text(&entry, "夜间偏好"));
        assert!(!matches_text(&entry, "%"));
        assert!(text_relevance(&entry.content, &entry.tags, "喜欢音乐") > 0);
    }

    fn test_memory(
        scope: MemoryScope,
        content: &str,
        importance: u8,
        occurred_at: chrono::DateTime<Utc>,
    ) -> Memory {
        let draft = MemoryDraft::new(scope, MemoryKind::Fact, content, occurred_at)
            .expect("test memory content should validate")
            .with_importance(importance)
            .expect("test memory importance should validate");
        Memory::from_draft(MemoryId::new(), &draft, occurred_at).expect("test memory should build")
    }

    #[test]
    fn hybrid_ranking_prefers_an_older_exact_match_over_recent_noise() {
        let scope = MemoryScope::Person(PersonId::new());
        let now = Utc::now();
        let older_match = test_memory(
            scope,
            "user likes weekend tea",
            70,
            now - Duration::days(90),
        );
        let recent_partial = test_memory(
            scope,
            "user mentioned tea at lunch",
            90,
            now - Duration::hours(1),
        );
        let mut memories = vec![recent_partial, older_match.clone()];

        rank_memories(&mut memories, "weekend tea");

        assert_eq!(memories[0].id(), older_match.id());
    }

    #[test]
    fn empty_query_prefers_importance_before_freshness() {
        let scope = MemoryScope::Conversation(ConversationId::new());
        let now = Utc::now();
        let durable_fact = test_memory(
            scope,
            "用户明确要求不要泄露私人信息",
            100,
            now - Duration::days(180),
        );
        let recent_chat = test_memory(scope, "今天吃了午饭", 20, now);
        let mut memories = vec![recent_chat, durable_fact.clone()];

        rank_memories(&mut memories, "");

        assert_eq!(memories[0].id(), durable_fact.id());
    }

    #[test]
    fn hybrid_ranking_uses_freshness_as_the_final_tiebreaker() {
        let scope = MemoryScope::Global;
        let now = Utc::now();
        let fresh = test_memory(scope, "共同话题", 60, now - Duration::hours(1));
        let stale = test_memory(scope, "共同话题", 60, now - Duration::days(30));
        let fresh_id = fresh.id();
        let mut memories = vec![stale, fresh];

        rank_memories(&mut memories, "共同话题");

        assert_eq!(memories[0].id(), fresh_id);
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_memory_v2_round_trips_person_and_conversation_scopes() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("should connect to PostgreSQL");
                let identities = Arc::new(PostgresIdentityStore::new(pool.clone()));
                identities
                    .initialize_schema()
                    .await
                    .expect("should initialize identity schema");

                let suffix = Uuid::new_v4().simple().to_string();
                let person_id = PersonId::new();
                let conversation_id = ConversationId::new();
                let person_scope = MemoryScope::Person(person_id);
                let conversation_scope = MemoryScope::Conversation(conversation_id);
                let legacy_path = std::env::temp_dir()
                    .join(format!("yunxi-memory-v2-{suffix}.json"))
                    .to_string_lossy()
                    .into_owned();
                let manager = Arc::new(MemoryManager::new(&legacy_path));
                let store = PostgresMemoryStore::new(
                    Arc::clone(&manager),
                    Arc::clone(&identities),
                    pool.clone(),
                );
                store
                    .initialize_schema()
                    .await
                    .expect("should initialize memory v2 schema");
                // The production manager creates this compatibility table before
                // the Core adapter starts. Keep the isolated contract test
                // self-contained so cleanup can verify both storage layers.
                query(
                    r#"
                    CREATE TABLE IF NOT EXISTS kovi_bot_memories (
                        id TEXT PRIMARY KEY,
                        subject_id BIGINT,
                        scope_type TEXT CHECK (scope_type IN ('private', 'group') OR scope_type IS NULL),
                        context TEXT NOT NULL,
                        occurred_at TIMESTAMPTZ NOT NULL,
                        importance SMALLINT NOT NULL,
                        payload JSONB NOT NULL
                    )
                    "#,
                )
                .execute(&pool)
                .await
                .expect("legacy memory table should exist for reconciliation tests");

                cleanup_scopes(&pool, person_id, conversation_id).await;
                query("INSERT INTO yunxi_persons (id) VALUES ($1)")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should create canonical person owner");
                query("INSERT INTO yunxi_conversations (id, kind) VALUES ($1, 'direct')")
                    .bind(conversation_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should create canonical conversation owner");
                let qq_suffix = (Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
                let qq_user_id = 9_000_000_000_000_i64 + qq_suffix;
                let qq_bot_id = 8_000_000_000_000_i64 + qq_suffix;
                query(
                    "INSERT INTO yunxi_external_identities (platform, external_id, person_id)
                     VALUES ('qq', $1, $2)",
                )
                .bind(qq_user_id.to_string())
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("QQ person mapping should persist");
                query(
                    "INSERT INTO yunxi_external_conversations
                        (platform, external_id, conversation_id)
                     VALUES ('qq', $1, $2)",
                )
                .bind(format!("direct:{qq_bot_id}:{qq_user_id}"))
                .bind(conversation_id.into_uuid())
                .execute(&pool)
                .await
                .expect("QQ conversation mapping should persist");

                let person_content = format!("person-memory-v2-{suffix}");
                let person_draft = MemoryDraft::new(
                    person_scope,
                    MemoryKind::Fact,
                    person_content.clone(),
                    chrono::Utc::now(),
                )
                .expect("should build person memory")
                .with_importance(77)
                .and_then(|draft| draft.with_tags(["v2", "person"]))
                .expect("should enrich person memory");
                let conversation_content = format!("conversation-memory-v2-{suffix}");
                let conversation_draft = MemoryDraft::new(
                    conversation_scope,
                    MemoryKind::Conversation,
                    conversation_content.clone(),
                    chrono::Utc::now(),
                )
                .expect("should build conversation memory")
                .with_importance(61)
                .and_then(|draft| draft.with_tags(["v2", "conversation"]))
                .expect("should enrich conversation memory");

                let person_memory = store
                    .remember(&person_draft)
                    .await
                    .expect("person memory should persist with a legacy identity mapping");
                let conversation_memory = store.remember(&conversation_draft).await.expect(
                    "conversation memory should persist with a legacy conversation mapping",
                );

                for (memory, context, subject_id) in [
                    (
                        &person_memory,
                        format!("{PRIVATE_CONTEXT}{FACT_CONTEXT_SUFFIX}"),
                        Some(qq_user_id),
                    ),
                    (
                        &conversation_memory,
                        format!("{DIRECT_CONTEXT_PREFIX}{conversation_id}"),
                        None,
                    ),
                ] {
                    let legacy_row = query(
                        "SELECT context, subject_id FROM kovi_bot_memories WHERE id = $1",
                    )
                    .bind(memory.id().to_string())
                    .fetch_one(&pool)
                    .await
                    .expect("legacy projection should be written atomically");
                    assert_eq!(legacy_row.try_get::<String, _>("context").expect("context"), context);
                    assert_eq!(legacy_row.try_get::<Option<i64>, _>("subject_id").expect("subject"), subject_id);
                }

                let person_row =
                    query("SELECT scope_kind, scope_id, content FROM yunxi_memories WHERE id = $1")
                        .bind(person_memory.id().into_uuid())
                        .fetch_one(&pool)
                        .await
                        .expect("person memory should be written to yunxi_memories");
                assert_eq!(
                    person_row
                        .try_get::<String, _>("scope_kind")
                        .expect("scope kind should decode"),
                    "person"
                );
                assert_eq!(
                    person_row
                        .try_get::<Uuid, _>("scope_id")
                        .expect("scope id should decode"),
                    person_id.into_uuid()
                );
                assert_eq!(
                    person_row
                        .try_get::<String, _>("content")
                        .expect("content should decode"),
                    person_content
                );
                let conversation_row =
                    query("SELECT scope_kind, scope_id, content FROM yunxi_memories WHERE id = $1")
                        .bind(conversation_memory.id().into_uuid())
                        .fetch_one(&pool)
                        .await
                        .expect("conversation memory should be written to yunxi_memories");
                assert_eq!(
                    conversation_row
                        .try_get::<String, _>("scope_kind")
                        .expect("scope kind should decode"),
                    "conversation"
                );
                assert_eq!(
                    conversation_row
                        .try_get::<Uuid, _>("scope_id")
                        .expect("scope id should decode"),
                    conversation_id.into_uuid()
                );
                assert_eq!(
                    conversation_row
                        .try_get::<String, _>("content")
                        .expect("content should decode"),
                    conversation_content
                );

                let restarted_legacy_path = std::env::temp_dir()
                    .join(format!("yunxi-memory-v2-restarted-{suffix}.json"))
                    .to_string_lossy()
                    .into_owned();
                let restarted = PostgresMemoryStore::new(
                    Arc::new(MemoryManager::new(&restarted_legacy_path)),
                    Arc::new(PostgresIdentityStore::new(pool.clone())),
                    pool.clone(),
                );
                let person_query = MemoryQuery::new(person_scope, person_content.clone(), 4)
                    .expect("should build person query");
                let recalled_person = restarted
                    .recall(&person_query)
                    .await
                    .expect("new store instance should recall person memory");
                assert_eq!(recalled_person.len(), 1);
                assert_eq!(recalled_person[0].id(), person_memory.id());
                assert_eq!(recalled_person[0].scope(), person_scope);
                assert_eq!(recalled_person[0].kind(), MemoryKind::Fact);
                assert_eq!(recalled_person[0].importance(), 77);
                assert_eq!(recalled_person[0].tags(), ["v2", "person"]);

                let conversation_query =
                    MemoryQuery::new(conversation_scope, conversation_content.clone(), 4)
                        .expect("should build conversation query");
                let recalled_conversation = restarted
                    .recall(&conversation_query)
                    .await
                    .expect("new store instance should recall conversation memory");
                assert_eq!(recalled_conversation.len(), 1);
                assert_eq!(recalled_conversation[0].id(), conversation_memory.id());
                assert_eq!(recalled_conversation[0].scope(), conversation_scope);
                assert_eq!(recalled_conversation[0].kind(), MemoryKind::Conversation);
                assert_eq!(recalled_conversation[0].importance(), 61);
                assert_eq!(recalled_conversation[0].tags(), ["v2", "conversation"]);

                let cjk_memory_id = MemoryId::new();
                query(
                    "INSERT INTO yunxi_memories
                        (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                     VALUES ($1, 'person', $2, 'preference', $3, 80, $4, NOW())",
                )
                .bind(cjk_memory_id.into_uuid())
                .bind(person_id.into_uuid())
                .bind("用户非常喜欢爵士音乐")
                .bind(serde_json::json!(["深夜电台"]))
                .execute(&pool)
                .await
                .expect("CJK retrieval fixture should persist");
                let cjk_query = MemoryQuery::new(person_scope, "喜欢音乐", 4)
                    .expect("CJK memory query should validate");
                assert!(
                    restarted
                        .recall(&cjk_query)
                        .await
                        .expect("CJK memory query should succeed")
                        .iter()
                        .any(|memory| memory.id() == cjk_memory_id),
                    "a Chinese query without spaces should match lexical bigrams"
                );
                let tag_query = MemoryQuery::new(person_scope, "深夜电台", 4)
                    .expect("tag-only memory query should validate");
                assert!(
                    restarted
                        .recall(&tag_query)
                        .await
                        .expect("tag-only memory query should succeed")
                        .iter()
                        .any(|memory| memory.id() == cjk_memory_id),
                    "Core tags should participate in candidate selection"
                );
                let malformed_tags_id = MemoryId::new();
                query(
                    "INSERT INTO yunxi_memories
                        (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                     VALUES ($1, 'person', $2, 'event', 'text survives malformed tags', 80, $3, NOW())",
                )
                .bind(malformed_tags_id.into_uuid())
                .bind(person_id.into_uuid())
                .bind(serde_json::json!({"not": "an array"}))
                .execute(&pool)
                .await
                .expect("malformed tag fixture should persist");
                let malformed_tags_query =
                    MemoryQuery::new(person_scope, "text survives", 4).expect("valid query");
                assert!(
                    restarted
                        .recall(&malformed_tags_query)
                        .await
                        .expect("malformed tags must not fail the whole recall")
                        .iter()
                        .any(|memory| memory.id() == malformed_tags_id)
                );
                let wildcard_query = MemoryQuery::new(person_scope, "%_", 4)
                    .expect("literal wildcard memory query should validate");
                assert!(
                    restarted
                        .recall(&wildcard_query)
                        .await
                        .expect("literal wildcard memory query should succeed")
                        .is_empty(),
                    "SQL wildcard characters must be treated as ordinary text"
                );

                // A canonical ID is globally unique, but the MemoryStore
                // contract is still scope-bound. Asking the person scope to
                // forget a conversation ID must leave both the Core row and
                // its legacy projection untouched.
                assert!(
                    !restarted
                        .forget(person_scope, conversation_memory.id())
                        .await
                        .expect("wrong-scope forget should be harmless")
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = $1",
                    )
                    .bind(conversation_memory.id().to_string())
                    .fetch_one(&pool)
                    .await
                    .expect("wrong-scope legacy row count should decode"),
                    1
                );

                assert!(
                    restarted
                        .forget(person_scope, person_memory.id())
                        .await
                        .expect("person memory should be forgotten")
                );
                assert!(
                    restarted
                        .forget(conversation_scope, conversation_memory.id())
                        .await
                        .expect("conversation memory should be forgotten")
                );
                assert!(
                    restarted
                        .recall(&person_query)
                        .await
                        .expect("person scope should remain readable")
                        .is_empty()
                );
                assert!(
                    restarted
                        .recall(&conversation_query)
                        .await
                        .expect("conversation scope should remain readable")
                        .is_empty()
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_memories WHERE id = $1 OR id = $2",
                    )
                    .bind(person_memory.id().into_uuid())
                    .bind(conversation_memory.id().into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("should verify memory v2 deletion"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = $1 OR id = $2",
                    )
                    .bind(person_memory.id().to_string())
                    .bind(conversation_memory.id().to_string())
                    .fetch_one(&pool)
                    .await
                    .expect("should verify legacy memory deletion"),
                    0
                );

                let cleanup_now = Utc::now();
                let cleanup_cutoff =
                    cleanup_now - chrono::Duration::days(crate::config::get().memory().retention_days());
                let expired_low_id = MemoryId::new();
                let expired_high_id = MemoryId::new();
                for (id, importance) in [(expired_low_id, 10_i16), (expired_high_id, 90_i16)] {
                    query(
                        "INSERT INTO yunxi_memories
                            (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                         VALUES ($1, 'person', $2, 'fact', $3, $4, '[]', $5)",
                    )
                    .bind(id.into_uuid())
                    .bind(person_id.into_uuid())
                    .bind(format!("retention-fixture-{id}"))
                    .bind(importance)
                    .bind(cleanup_cutoff - chrono::Duration::hours(1))
                    .execute(&pool)
                    .await
                    .expect("retention fixture should persist");
                }
                assert!(store.cleanup(cleanup_now).await.expect("cleanup should run") >= 1);
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_memories WHERE id = $1",
                    )
                    .bind(expired_low_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("low retention fixture count should decode"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_memories WHERE id = $1",
                    )
                    .bind(expired_high_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("high retention fixture count should decode"),
                    1
                );

                // A canonical row that has already been projected to the
                // compatibility manager must remove its database twin and
                // cached entry in the same cleanup transaction.
                let legacy_cleanup_id = MemoryId::new();
                let legacy_cleanup_entry = MemoryEntry {
                    id: legacy_cleanup_id.to_string(),
                    content: format!("legacy-reconciliation-fixture-{suffix}"),
                    timestamp: (cleanup_cutoff - chrono::Duration::hours(1))
                        .with_timezone(&chrono::Local),
                    memory_type: MemoryType::Event,
                    importance: 1,
                    tags: Vec::new(),
                    context: GLOBAL_CONTEXT.to_owned(),
                    subject_id: None,
                };
                manager
                    .add_memory(legacy_cleanup_entry.clone())
                    .await
                    .expect("legacy reconciliation fixture should enter the cache");
                query(
                    "INSERT INTO kovi_bot_memories
                        (id, subject_id, context, occurred_at, importance, payload)
                     VALUES ($1, NULL, $2, $3, $4, $5)",
                )
                .bind(&legacy_cleanup_entry.id)
                .bind(&legacy_cleanup_entry.context)
                .bind(legacy_cleanup_entry.timestamp)
                .bind(i16::from(legacy_cleanup_entry.importance))
                .bind(serde_json::to_value(&legacy_cleanup_entry).expect("legacy payload should serialize"))
                .execute(&pool)
                .await
                .expect("legacy reconciliation row should persist");
                query(
                    "INSERT INTO yunxi_memories
                        (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                     VALUES ($1, 'global', NULL, 'event', $2, 10, '[]', $3)",
                )
                .bind(legacy_cleanup_id.into_uuid())
                .bind(&legacy_cleanup_entry.content)
                .bind(legacy_cleanup_entry.timestamp)
                .execute(&pool)
                .await
                .expect("canonical reconciliation row should persist");
                assert!(store.cleanup(cleanup_now).await.expect("reconciliation cleanup should run") >= 1);
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_memories WHERE id = $1",
                    )
                    .bind(legacy_cleanup_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("canonical reconciliation count should decode"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = $1",
                    )
                    .bind(&legacy_cleanup_entry.id)
                    .fetch_one(&pool)
                    .await
                    .expect("legacy reconciliation count should decode"),
                    0
                );
                assert!(
                    manager
                        .get_recent_memories_for_domain_scope(None, GLOBAL_CONTEXT, 128)
                        .await
                        .into_iter()
                        .all(|memory| memory.id != legacy_cleanup_entry.id),
                    "the committed cleanup must evict the compatibility cache entry"
                );

                let malformed_conversation_id = ConversationId::new();
                let malformed_scope = MemoryScope::Conversation(malformed_conversation_id);
                query("INSERT INTO yunxi_conversations (id, kind) VALUES ($1, 'direct')")
                    .bind(malformed_conversation_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should create malformed mapping owner");
                query(
                    "INSERT INTO yunxi_external_conversations
                        (platform, external_id, conversation_id)
                     VALUES ('qq', $1, $2)",
                )
                .bind(format!("malformed:{suffix}"))
                .bind(malformed_conversation_id.into_uuid())
                .execute(&pool)
                .await
                .expect("should create malformed QQ mapping");
                let malformed_draft = MemoryDraft::new(
                    malformed_scope,
                    MemoryKind::Fact,
                    "must not silently downgrade mapping errors",
                    chrono::Utc::now(),
                )
                .expect("should build malformed-scope memory");
                assert!(matches!(
                    store.remember(&malformed_draft).await,
                    Err(MemoryStoreError::InvalidRequest { .. })
                ));
                let retained_id = MemoryId::new();
                query(
                    "INSERT INTO yunxi_memories
                        (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                     VALUES ($1, 'conversation', $2, 'fact', 'retained on lookup error', 50, '[]', NOW())",
                )
                .bind(retained_id.into_uuid())
                .bind(malformed_conversation_id.into_uuid())
                .execute(&pool)
                .await
                .expect("should seed core memory for forget ordering");
                let malformed_query =
                    MemoryQuery::new(malformed_scope, "retained", 4).expect("valid query");
                assert!(matches!(
                    store.recall(&malformed_query).await,
                    Err(MemoryStoreError::InvalidRequest { .. })
                ));
                assert!(matches!(
                    store.forget(malformed_scope, retained_id).await,
                    Err(MemoryStoreError::InvalidRequest { .. })
                ));
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_memories WHERE id = $1",
                    )
                    .bind(retained_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("should verify core memory was retained"),
                    1
                );
                cleanup_scopes(&pool, person_id, malformed_conversation_id).await;
                query("DELETE FROM yunxi_conversations WHERE id = $1")
                    .bind(malformed_conversation_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean malformed mapping owner");

                cleanup_scopes(&pool, person_id, conversation_id).await;
                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean canonical person owner");
                query("DELETE FROM yunxi_conversations WHERE id = $1")
                    .bind(conversation_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean canonical conversation owner");
                query("DELETE FROM kovi_bot_memories WHERE id = $1")
                    .bind(&legacy_cleanup_entry.id)
                    .execute(&pool)
                    .await
                    .expect("should clean legacy reconciliation fixture");
                let _ = std::fs::remove_file(&legacy_path);
                let _ = std::fs::remove_file(&restarted_legacy_path);
            });
    }

    async fn cleanup_scopes(
        pool: &sqlx_postgres::PgPool,
        person_id: PersonId,
        conversation_id: ConversationId,
    ) {
        query(
            "DELETE FROM yunxi_memories
             WHERE (scope_kind = 'person' AND scope_id = $1)
                OR (scope_kind = 'conversation' AND scope_id = $2)",
        )
        .bind(person_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .execute(pool)
        .await
        .expect("should clean isolated memory v2 rows");
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_cleanup_uses_migration_proof_when_identity_mapping_is_gone() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("should connect to PostgreSQL");
                let suffix = Uuid::new_v4().simple().to_string();
                let legacy_path =
                    std::env::temp_dir().join(format!("yunxi-memory-ledger-proof-{suffix}.json"));
                let manager = Arc::new(MemoryManager::new(
                    legacy_path
                        .to_str()
                        .expect("temporary path should be UTF-8"),
                ));
                let store = PostgresMemoryStore::new(
                    Arc::clone(&manager),
                    Arc::new(PostgresIdentityStore::new(pool.clone())),
                    pool.clone(),
                );
                store
                    .identities
                    .initialize_schema()
                    .await
                    .expect("identity schema should initialize");
                store
                    .initialize_schema()
                    .await
                    .expect("memory schema should initialize");
                let migration = MemoryMigrationService::new(pool.clone());
                migration
                    .initialize_schema()
                    .await
                    .expect("migration schema should initialize");

                let core_id = MemoryId::new();
                let orphan_person = PersonId::new();
                let ledger_id = format!("legacy-ledger-proof-{suffix}");
                let canonical_entry = MemoryEntry {
                    id: core_id.to_string(),
                    content: format!("canonical wrong-scope fixture {suffix}"),
                    timestamp: chrono::Local::now() - chrono::Duration::days(2),
                    memory_type: MemoryType::GroupInfo,
                    importance: 1,
                    tags: Vec::new(),
                    context: "group_chat".to_owned(),
                    subject_id: Some(7_654_321),
                };
                let ledger_entry = MemoryEntry {
                    id: ledger_id.clone(),
                    content: format!("ledger fixture {suffix}"),
                    timestamp: canonical_entry.timestamp,
                    memory_type: MemoryType::UserProfile,
                    importance: 1,
                    tags: Vec::new(),
                    context: "private_chat".to_owned(),
                    subject_id: Some(8_765_432),
                };
                for entry in [&canonical_entry, &ledger_entry] {
                    query(
                        "INSERT INTO kovi_bot_memories
                            (id, subject_id, scope_type, context, occurred_at, importance, payload)
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    )
                    .bind(&entry.id)
                    .bind(entry.subject_id)
                    .bind(if entry.context == "group_chat" {
                        Some("group")
                    } else {
                        Some("private")
                    })
                    .bind(&entry.context)
                    .bind(entry.timestamp)
                    .bind(i16::from(entry.importance))
                    .bind(serde_json::to_value(entry).expect("legacy payload should serialize"))
                    .execute(&pool)
                    .await
                    .expect("legacy fixture should persist");
                }
                query(
                    "INSERT INTO yunxi_memories
                        (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                     VALUES ($1, 'person', $2, 'fact', $3, 1, '[]', $4)",
                )
                .bind(core_id.into_uuid())
                .bind(orphan_person.into_uuid())
                .bind(format!("core orphan fixture {suffix}"))
                .bind(canonical_entry.timestamp)
                .execute(&pool)
                .await
                .expect("orphan Core fixture should persist");

                let batch_id = Uuid::new_v4();
                query(
                    "INSERT INTO yunxi_memory_migration_batches
                        (id, migration_version, mode, status, batch_size)
                     VALUES ($1, 'test', 'backfill', 'completed', 1)",
                )
                .bind(batch_id)
                .execute(&pool)
                .await
                .expect("migration batch fixture should persist");
                query(
                    "INSERT INTO yunxi_memory_migration_items
                        (batch_id, legacy_id, target_id, source_hash, target_hash, action, inserted)
                     VALUES ($1, $2, $3, 'source', 'target', 'inserted', TRUE)",
                )
                .bind(batch_id)
                .bind(&ledger_id)
                .bind(core_id.into_uuid())
                .execute(&pool)
                .await
                .expect("migration ledger fixture should persist");

                store
                    .cleanup_with_limits(Utc::now(), 1, 1_000_000)
                    .await
                    .expect("cleanup should use the durable ledger");
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_memories WHERE id = $1",
                    )
                    .bind(core_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("Core fixture count should decode"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = $1",
                    )
                    .bind(&ledger_id)
                    .fetch_one(&pool)
                    .await
                    .expect("ledger fixture count should decode"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = $1",
                    )
                    .bind(core_id.to_string())
                    .fetch_one(&pool)
                    .await
                    .expect("canonical fixture count should decode"),
                    1,
                    "an unproven same-ID row in another scope must survive"
                );

                query("DELETE FROM yunxi_memory_migration_batches WHERE id = $1")
                    .bind(batch_id)
                    .execute(&pool)
                    .await
                    .expect("migration fixture should clean up");
                query("DELETE FROM kovi_bot_memories WHERE id = $1")
                    .bind(core_id.to_string())
                    .execute(&pool)
                    .await
                    .expect("canonical fixture should clean up");
                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(orphan_person.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("orphan person fixture should clean up");
                let _ = std::fs::remove_file(legacy_path);
            });
    }
}
