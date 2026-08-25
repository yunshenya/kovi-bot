//! Compatibility adapter from the platform-neutral MemoryStore port to the
//! existing bounded MemoryManager. QQ identifiers stay inside this module.

use super::identity_store::PostgresIdentityStore;
use super::owner_lock::{self, DurableOwner};
use crate::memory::{MemoryEntry, MemoryManager, MemoryType};
use chrono::{DateTime, Local, Utc};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, Postgres};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;
use yunxi_core::{
    ConversationKind, Memory, MemoryDraft, MemoryId, MemoryKind, MemoryQuery, MemoryScope,
    MemoryStore, MemoryStoreError, MemoryStoreFuture,
};

const PRIVATE_CONTEXT: &str = "private_chat";
const GROUP_CONTEXT: &str = "group_chat";
const GLOBAL_CONTEXT: &str = "yunxi_global:";
const DIRECT_CONTEXT_PREFIX: &str = "yunxi_direct_chat:";
const FACT_CONTEXT_SUFFIX: &str = "|yunxi_kind=fact";
const MAX_LEGACY_SUBJECTS: usize = 32;

#[derive(Clone)]
pub(crate) struct PostgresMemoryStore {
    manager: Arc<MemoryManager>,
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
        .execute(&self.pool)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_memories_scope_idx
             ON yunxi_memories (scope_kind, scope_id, occurred_at DESC)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

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
        query_input: &MemoryQuery,
    ) -> Result<Vec<Memory>, MemoryStoreError> {
        let (scope_kind, scope_id) = scope_parts(query_input.scope());
        let minimum = i16::from(query_input.min_importance().unwrap_or(0));
        let rows = query(
            "SELECT id, kind, content, importance, tags, occurred_at, created_at
             FROM yunxi_memories
             WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2
               AND importance >= $3
               AND ($4 = '' OR content ILIKE '%' || $4 || '%')
             ORDER BY occurred_at DESC LIMIT $5",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(minimum)
        .bind(query_input.text())
        .bind(i64::try_from(query_input.limit()).unwrap_or(32))
        .fetch_all(&self.pool)
        .await
        .map_err(MemoryStoreError::storage)?;
        rows.into_iter()
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
                let tags = serde_json::from_value::<Vec<String>>(
                    row.try_get("tags").map_err(MemoryStoreError::storage)?,
                )
                .map_err(|error| MemoryStoreError::InvalidRequest {
                    reason: error.to_string(),
                })?;
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
            .collect()
    }
}

impl MemoryStore for PostgresMemoryStore {
    fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
        Box::pin(async move {
            draft
                .validate()
                .map_err(|error| MemoryStoreError::InvalidRequest {
                    reason: error.to_string(),
                })?;
            let legacy_scope = match self.legacy_scope(draft.scope()).await {
                Ok(scope) => Some(scope),
                Err(MemoryStoreError::UnsupportedScope { .. }) => None,
                Err(error) => return Err(error),
            };
            let id = MemoryId::new();
            let mut transaction = self.pool.begin().await.map_err(MemoryStoreError::storage)?;
            let owner = memory_owner(draft.scope());
            if !owner_lock::lock_and_owner_exists(&mut transaction, owner)
                .await
                .map_err(MemoryStoreError::storage)?
            {
                return Err(MemoryStoreError::InvalidRequest {
                    reason: format!("memory owner {owner:?} does not exist"),
                });
            }
            self.write_core_memory(&mut transaction, id, draft).await?;
            transaction
                .commit()
                .await
                .map_err(MemoryStoreError::storage)?;
            let Some(scope) = legacy_scope else {
                return Memory::from_draft(id, draft, Utc::now()).map_err(|error| {
                    MemoryStoreError::InvalidRequest {
                        reason: error.to_string(),
                    }
                });
            };
            let context = if draft.kind() == MemoryKind::Fact {
                format!("{}{FACT_CONTEXT_SUFFIX}", scope.context)
            } else {
                scope.context.clone()
            };
            let entry = MemoryEntry {
                id: id.to_string(),
                content: draft.content().to_string(),
                timestamp: draft.occurred_at().with_timezone(&Local),
                memory_type: legacy_kind(draft.kind()),
                importance: importance_to_legacy(draft.importance()),
                tags: draft.tags().to_vec(),
                context,
                subject_id: scope.subject_ids.first().copied(),
            };
            if let Err(error) = self.manager.add_memory(entry).await {
                eprintln!("[WARN] legacy memory double-write failed: {error}");
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
            let legacy_scope = match self.legacy_scope(query.scope()).await {
                Ok(scope) => Some(scope),
                Err(MemoryStoreError::UnsupportedScope { .. }) => None,
                Err(error) => return Err(error),
            };
            let mut memories = self.read_core_memories(query).await?;
            let Some(scope) = legacy_scope else {
                memories.truncate(query.limit());
                return Ok(memories);
            };
            let fetch_limit = query.limit().saturating_mul(4).min(128);
            let mut entries = Vec::new();
            let subject_ids = if scope.subject_ids.is_empty() {
                vec![None]
            } else {
                scope.subject_ids.iter().copied().map(Some).collect()
            };
            for subject_id in subject_ids {
                entries.extend(
                    self.manager
                        .get_recent_memories_for_domain_scope(
                            subject_id,
                            &scope.context,
                            fetch_limit,
                        )
                        .await,
                );
            }
            let mut seen_ids = HashSet::new();
            entries.retain(|entry| seen_ids.insert(entry.id.clone()));
            let mut matches = entries
                .into_iter()
                .filter(|entry| {
                    query.min_importance().is_none_or(|minimum| {
                        u16::from(entry.importance) * 10 >= u16::from(minimum)
                    })
                })
                .filter(|entry| query.text().trim().is_empty() || matches_text(entry, query.text()))
                .collect::<Vec<_>>();
            matches.sort_by(|left, right| {
                relevance(right, query.text())
                    .cmp(&relevance(left, query.text()))
                    .then_with(|| right.timestamp.cmp(&left.timestamp))
            });
            matches.truncate(query.limit());
            memories.extend(
                matches
                    .into_iter()
                    .filter_map(|entry| to_core_memory(query.scope(), entry).ok())
                    .collect::<Vec<_>>(),
            );
            let mut seen = HashSet::new();
            memories.retain(|memory| seen.insert(memory.id()));
            memories.sort_by_key(|memory| std::cmp::Reverse(memory.occurred_at()));
            memories.truncate(query.limit());
            Ok(memories)
        })
    }

    fn forget(&self, scope: MemoryScope, id: MemoryId) -> MemoryStoreFuture<'_, bool> {
        Box::pin(async move {
            let legacy_scope = match self.legacy_scope(scope).await {
                Ok(scope) => Some(scope),
                Err(MemoryStoreError::UnsupportedScope { .. }) => None,
                Err(error) => return Err(error),
            };
            let (scope_kind, scope_id) = scope_parts(scope);
            let core_deleted = query(
                "DELETE FROM yunxi_memories WHERE id = $1 AND scope_kind = $2
                 AND scope_id IS NOT DISTINCT FROM $3",
            )
            .bind(id.into_uuid())
            .bind(scope_kind)
            .bind(scope_id)
            .execute(&self.pool)
            .await
            .map_err(MemoryStoreError::storage)?
            .rows_affected()
                > 0;
            let Some(scope) = legacy_scope else {
                return Ok(core_deleted);
            };
            let subject_ids = if scope.subject_ids.is_empty() {
                vec![None]
            } else {
                scope.subject_ids.iter().copied().map(Some).collect()
            };
            for subject_id in subject_ids {
                if self
                    .manager
                    .delete_memory_for_domain_scope(&id.to_string(), subject_id, &scope.context)
                    .await
                    .map_err(storage_error)?
                {
                    return Ok(true);
                }
            }
            Ok(core_deleted)
        })
    }
}

#[derive(Debug, Clone)]
struct LegacyScope {
    subject_ids: Vec<i64>,
    context: String,
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
    let value = value.parse::<i64>().ok()?;
    (value > 0).then_some(value)
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

fn matches_text(entry: &MemoryEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let searchable = format!(
        "{} {}",
        entry.content.to_lowercase(),
        entry.tags.join(" ").to_lowercase()
    );
    searchable.contains(&query)
        || query
            .split_whitespace()
            .any(|term| searchable.contains(term))
}

fn relevance(entry: &MemoryEntry, query: &str) -> u8 {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return entry.importance;
    }
    let searchable = format!(
        "{} {}",
        entry.content.to_lowercase(),
        entry.tags.join(" ").to_lowercase()
    );
    let exact = u8::from(searchable.contains(&query)).saturating_mul(10);
    exact.saturating_add(entry.importance)
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
        PostgresMemoryStore, core_kind, importance_to_legacy, parse_direct_external,
        parse_positive_decimal,
    };
    use crate::memory::{MemoryManager, MemoryType};
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use sqlx_core::query::query;
    use sqlx_core::query_scalar::query_scalar;
    use sqlx_core::row::Row;
    use sqlx_postgres::{PgPoolOptions, Postgres};
    use std::sync::Arc;
    use uuid::Uuid;
    use yunxi_core::{
        ConversationId, MemoryDraft, MemoryId, MemoryKind, MemoryQuery, MemoryScope, MemoryStore,
        MemoryStoreError, PersonId,
    };

    #[test]
    fn external_numeric_mapping_never_accepts_non_positive_values() {
        assert_eq!(parse_positive_decimal("123"), Some(123));
        assert_eq!(parse_positive_decimal("0"), None);
        assert_eq!(parse_positive_decimal("-1"), None);
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
                let store = PostgresMemoryStore::new(
                    Arc::new(MemoryManager::new(&legacy_path)),
                    Arc::clone(&identities),
                    pool.clone(),
                );
                store
                    .initialize_schema()
                    .await
                    .expect("should initialize memory v2 schema");

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
                    .expect("person memory should persist without a legacy identity mapping");
                let conversation_memory = store.remember(&conversation_draft).await.expect(
                    "conversation memory should persist without a legacy conversation mapping",
                );

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
}
