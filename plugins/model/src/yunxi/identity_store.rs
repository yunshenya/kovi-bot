use super::delivery_ledger::{delete_group_domain_rows, delete_person_domain_rows};
use super::owner_lock::{self, DurableOwner};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, Postgres};
use std::collections::HashSet;
use std::str::FromStr;
use uuid::Uuid;
use yunxi_core::{
    AffectState, ConversationId, ConversationKind, ConversationMember, ConversationMemberStore,
    ConversationMemberStoreError, ConversationMemberStoreFuture, ExternalConversation,
    ExternalIdentity, Goal, GoalKind, GoalOwner, GoalState, IdentityStore, IdentityStoreError,
    IdentityStoreFuture, Memory, MemoryDraft, MemoryId, MemoryKind, MemoryScope, MessageId,
    OpenLoop, OpenLoopKind, OpenLoopOwner, OpenLoopStatus, PersonId, PlatformId, RelationState,
};

#[derive(Debug, Clone)]
pub(crate) struct PostgresIdentityStore {
    pool: PgPool,
}

/// Rows removed by a confirmed person-domain deletion. Keeping the counts
/// explicit lets the host report partial failures without claiming that Core
/// data was erased when only the legacy QQ tables were touched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PersonDomainDeletion {
    pub(crate) persons: u64,
    pub(crate) conversations: u64,
    pub(crate) external_identities: u64,
    pub(crate) external_conversations: u64,
    pub(crate) message_mappings: u64,
    pub(crate) delivery_ledger: u64,
    pub(crate) memories: u64,
    pub(crate) open_loops: u64,
    pub(crate) affect_states: u64,
    pub(crate) relations: u64,
    pub(crate) goals: u64,
    pub(crate) world_model: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QqPersonDomainTargets {
    pub(crate) person_id: Option<PersonId>,
    pub(crate) qq_user_ids: Vec<i64>,
    pub(crate) direct_conversation_ids: Vec<ConversationId>,
}

const MAX_QQ_PERSON_ALIASES: usize = 256;
const MAX_QQ_PERSON_DIRECT_CONVERSATIONS: usize = 256;
const MAX_PERSON_DIRECT_CONVERSATIONS: usize = 256;
const MAX_PERSON_DIRECT_CONVERSATION_ROUTES: usize = 4_096;
const PORTABLE_PERSON_EXPORT_VERSION: u16 = 1;
const MAX_PORTABLE_EXTERNAL_IDENTITIES: usize = 256;
const MAX_PORTABLE_MEMORIES: usize = 512;
const MAX_PORTABLE_OPEN_LOOPS: usize = 512;
const MAX_PORTABLE_GOALS: usize = 512;
const DIRECT_MEMORY_CONTEXT_PREFIX: &str = "yunxi_direct_chat:";
const FACT_MEMORY_CONTEXT_SUFFIX: &str = "|yunxi_kind=fact";

/// Return a positive numeric QQ id when an external identity can own rows in
/// the legacy per-user memory table.  Other QQ-shaped identifiers are kept out
/// of this cleanup path deliberately; deleting by a non-canonical string
/// would make an unlink operation broader than the identity it removes.
fn qq_numeric_user_id(external: &ExternalIdentity) -> Option<i64> {
    if external.platform().as_str() != "qq"
        || external.external_id().is_empty()
        || !external
            .external_id()
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let user_id = external.external_id().parse::<i64>().ok()?;
    (user_id > 0 && user_id.to_string() == external.external_id()).then_some(user_id)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PortableExternalIdentity {
    pub(crate) platform: String,
    pub(crate) external_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PortablePersonExport {
    pub(crate) version: u16,
    pub(crate) person_id: PersonId,
    #[serde(default)]
    pub(crate) external_identities: Vec<PortableExternalIdentity>,
    #[serde(default)]
    pub(crate) memories: Vec<Memory>,
    #[serde(default)]
    pub(crate) relation: Option<RelationState>,
    #[serde(default)]
    pub(crate) affect: Option<AffectState>,
    #[serde(default)]
    pub(crate) open_loops: Vec<OpenLoop>,
    #[serde(default)]
    pub(crate) goals: Vec<Goal>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExternalConversationRoute {
    platform: String,
    external_id: String,
}

impl ExternalConversationRoute {
    fn from_external(external: &ExternalConversation) -> Self {
        Self {
            platform: external.platform().as_str().to_owned(),
            external_id: external.external_id().to_owned(),
        }
    }

    fn advisory_lock_key(&self) -> String {
        format!(
            "yunxi-route:conversation:{}:{}",
            self.platform, self.external_id
        )
    }
}

async fn lock_conversation_route(
    transaction: &mut Transaction<'_, Postgres>,
    route: &ExternalConversationRoute,
) -> Result<(), sqlx_core::error::Error> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(route.advisory_lock_key())
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

impl PersonDomainDeletion {
    #[must_use]
    pub(crate) const fn total(self) -> u64 {
        self.persons
            + self.conversations
            + self.external_identities
            + self.external_conversations
            + self.message_mappings
            + self.delivery_ledger
            + self.memories
            + self.open_loops
            + self.affect_states
            + self.relations
            + self.goals
    }
}

impl PostgresIdentityStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Remove only one external identity mapping. Canonical Person data is
    /// deliberately retained so unlinking a platform does not erase memory or
    /// relations; full deletion uses `delete_person_domain_data` instead.
    pub(crate) async fn unlink_external_identity(
        &self,
        external: &ExternalIdentity,
    ) -> Result<bool, IdentityStoreError> {
        // Keep identity unlink ordered with every Core/legacy memory writer.
        // The mapping is re-read after the Person owner lock so a concurrent
        // delete/re-link cannot make us purge a different user's projection.
        let _legacy_save_guard = crate::memory::MEMORY_MANAGER.acquire_save_lock().await;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(IdentityStoreError::storage)?;

        let mapped_person = query_scalar::<Postgres, Uuid>(
            "SELECT person_id FROM yunxi_external_identities
             WHERE platform = $1 AND external_id = $2",
        )
        .bind(external.platform().as_str())
        .bind(external.external_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        if let Some(person_id) = mapped_person {
            if !owner_lock::lock_and_owner_exists(&mut transaction, DurableOwner::Person(person_id))
                .await
                .map_err(IdentityStoreError::storage)?
            {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "external identity points to a missing canonical person",
                )));
            }
            let locked_person = query_scalar::<Postgres, Uuid>(
                "SELECT person_id FROM yunxi_external_identities
                 WHERE platform = $1 AND external_id = $2
                 FOR UPDATE",
            )
            .bind(external.platform().as_str())
            .bind(external.external_id())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            if locked_person != Some(person_id) {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "external identity changed while unlink was acquiring its owner lock",
                )));
            }
        }

        // A numeric QQ id is also the subject key used by the compatibility
        // memory table. Purge every private-shaped row, plus exact direct-chat
        // contexts owned by this canonical person, in the same transaction as
        // the unlink. This prevents a later identity reuse from recalling
        // plaintext that belongs to the previous person.
        let numeric_user_id = qq_numeric_user_id(external);
        let mut purged_legacy_ids = if mapped_person.is_some() {
            if let Some(user_id) = numeric_user_id {
                Self::purge_qq_private_legacy_rows(&mut transaction, user_id).await?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        if mapped_person.is_some()
            && let Some(user_id) = numeric_user_id
        {
            let mut direct_ids = query_scalar::<Postgres, Uuid>(
                "SELECT DISTINCT external.conversation_id
                 FROM yunxi_external_conversations AS external
                 JOIN yunxi_conversations AS conversation
                   ON conversation.id = external.conversation_id
                 WHERE external.platform = 'qq'
                   AND conversation.kind = 'direct'
                   AND external.external_id ~ '^direct:[1-9][0-9]*:[1-9][0-9]*$'
                   AND split_part(external.external_id, ':', 3) = $1
                 ORDER BY external.conversation_id LIMIT $2",
            )
            .bind(user_id.to_string())
            .bind(i64::try_from(MAX_PERSON_DIRECT_CONVERSATIONS + 1).unwrap_or(i64::MAX))
            .fetch_all(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            direct_ids.sort_unstable();
            direct_ids.dedup();
            if direct_ids.len() > MAX_PERSON_DIRECT_CONVERSATIONS {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "identity has too many direct conversations to purge safely on unlink",
                )));
            }
            purged_legacy_ids
                .extend(Self::purge_direct_legacy_rows(&mut transaction, &direct_ids).await?);
        }
        let unlinked = query(
            "DELETE FROM yunxi_external_identities
             WHERE platform = $1 AND external_id = $2",
        )
        .bind(external.platform().as_str())
        .bind(external.external_id())
        .execute(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        .rows_affected()
            != 0;
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;

        // SQL was authoritative; remove only the rows deleted above from the
        // process cache. Calling the narrow invalidator avoids deleting new
        // rows if another process reuses the QQ id immediately after commit.
        for id in purged_legacy_ids {
            crate::memory::MEMORY_MANAGER
                .remove_cached_memory(&id)
                .await;
        }
        Ok(unlinked)
    }

    async fn purge_qq_private_legacy_rows(
        transaction: &mut Transaction<'_, Postgres>,
        user_id: i64,
    ) -> Result<Vec<String>, IdentityStoreError> {
        let rows = query(
            r#"
            DELETE FROM kovi_bot_memories
            WHERE subject_id = $1
              AND (
                    scope_type = 'private'
                    OR context = 'private'
                    OR context LIKE 'private\_%' ESCAPE '\'
                    OR context LIKE 'proactive\_private\_%' ESCAPE '\'
                    OR context = 'proactive_main_admin_decision'
                  )
            RETURNING id
            "#,
        )
        .bind(user_id)
        .fetch_all(&mut **transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<String, _>("id")
                    .map_err(IdentityStoreError::storage)
            })
            .collect()
    }

    async fn purge_direct_legacy_rows(
        transaction: &mut Transaction<'_, Postgres>,
        conversation_ids: &[Uuid],
    ) -> Result<Vec<String>, IdentityStoreError> {
        if conversation_ids.is_empty() {
            return Ok(Vec::new());
        }
        let prefixes = conversation_ids
            .iter()
            .map(|id| format!("{DIRECT_MEMORY_CONTEXT_PREFIX}{id}"))
            .collect::<Vec<_>>();
        let rows = query(
            r#"
            DELETE FROM kovi_bot_memories
            WHERE subject_id IS NULL
              AND EXISTS (
                  SELECT 1
                  FROM UNNEST($1::TEXT[]) AS prefix(value)
                  WHERE context = prefix.value
                     OR context = prefix.value || $2
              )
            RETURNING id
            "#,
        )
        .bind(&prefixes)
        .bind(FACT_MEMORY_CONTEXT_SUFFIX)
        .fetch_all(&mut **transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<String, _>("id")
                    .map_err(IdentityStoreError::storage)
            })
            .collect()
    }

    pub(crate) async fn export_person(
        &self,
        person_id: PersonId,
    ) -> Result<PortablePersonExport, IdentityStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        let person_exists = query_scalar::<Postgres, bool>(
            "SELECT EXISTS (SELECT 1 FROM yunxi_persons WHERE id = $1)",
        )
        .bind(person_id.into_uuid())
        .fetch_one(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        if !person_exists {
            return Err(portable_error("cannot export a missing Yunxi person"));
        }

        let identities = query(
            "SELECT platform, external_id FROM yunxi_external_identities
             WHERE person_id = $1 ORDER BY platform, external_id LIMIT $2",
        )
        .bind(person_id.into_uuid())
        .bind(portable_probe_limit(MAX_PORTABLE_EXTERNAL_IDENTITIES))
        .fetch_all(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        .into_iter()
        .map(|row| {
            Ok(PortableExternalIdentity {
                platform: row
                    .try_get("platform")
                    .map_err(IdentityStoreError::storage)?,
                external_id: row
                    .try_get("external_id")
                    .map_err(IdentityStoreError::storage)?,
            })
        })
        .collect::<Result<Vec<_>, IdentityStoreError>>()?;
        ensure_portable_bound(
            "external identities",
            identities.len(),
            MAX_PORTABLE_EXTERNAL_IDENTITIES,
        )?;
        let rows = query(
            "SELECT id, scope_kind, scope_id, kind, content, importance, tags,
                    occurred_at, created_at
             FROM yunxi_memories WHERE scope_kind = 'person' AND scope_id = $1
             ORDER BY occurred_at, id LIMIT $2",
        )
        .bind(person_id.into_uuid())
        .bind(portable_probe_limit(MAX_PORTABLE_MEMORIES))
        .fetch_all(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        let memories = rows
            .into_iter()
            .map(|row| row_to_portable_memory(&row))
            .collect::<Result<Vec<_>, IdentityStoreError>>()?;
        ensure_portable_bound("memories", memories.len(), MAX_PORTABLE_MEMORIES)?;
        if memories
            .iter()
            .any(|memory| memory.scope() != MemoryScope::Person(person_id))
        {
            return Err(portable_error(
                "stored portable memory scope does not match exported person",
            ));
        }
        let relation = query(
            "SELECT familiarity, affinity, trust, comfort, tension
             FROM yunxi_relations WHERE person_id = $1",
        )
        .bind(person_id.into_uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        .map(|row| row_to_portable_relation(&row, person_id))
        .transpose()?;
        let affect = query(
            "SELECT valence, arousal, social_energy, curiosity
             FROM yunxi_affect_states WHERE person_id = $1",
        )
        .bind(person_id.into_uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        .map(|row| row_to_portable_affect(&row))
        .transpose()?;
        let open_loops = query(
            "SELECT * FROM yunxi_open_loops
             WHERE owner_kind = 'person' AND owner_id = $1
             ORDER BY created_at, id LIMIT $2",
        )
        .bind(person_id.into_uuid())
        .bind(portable_probe_limit(MAX_PORTABLE_OPEN_LOOPS))
        .fetch_all(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        .into_iter()
        .map(|row| row_to_portable_open_loop(&row))
        .collect::<Result<Vec<_>, IdentityStoreError>>()?;
        ensure_portable_bound("open loops", open_loops.len(), MAX_PORTABLE_OPEN_LOOPS)?;
        if open_loops
            .iter()
            .any(|item| item.owner() != OpenLoopOwner::Person(person_id))
        {
            return Err(portable_error(
                "stored open-loop owner does not match exported person",
            ));
        }
        let goals = query(
            "SELECT * FROM yunxi_goals
             WHERE owner_kind = 'person' AND owner_id = $1
             ORDER BY created_at, id LIMIT $2",
        )
        .bind(person_id.into_uuid())
        .bind(portable_probe_limit(MAX_PORTABLE_GOALS))
        .fetch_all(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        .into_iter()
        .map(|row| row_to_portable_goal(&row))
        .collect::<Result<Vec<_>, IdentityStoreError>>()?;
        ensure_portable_bound("goals", goals.len(), MAX_PORTABLE_GOALS)?;
        if goals
            .iter()
            .any(|goal| goal.owner() != GoalOwner::Person(person_id))
        {
            return Err(portable_error(
                "stored goal owner does not match exported person",
            ));
        }
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(PortablePersonExport {
            version: PORTABLE_PERSON_EXPORT_VERSION,
            person_id,
            external_identities: identities,
            memories,
            relation,
            affect,
            open_loops,
            goals,
        })
    }

    pub(crate) async fn import_person(
        &self,
        export: &PortablePersonExport,
    ) -> Result<PersonId, IdentityStoreError> {
        validate_portable_export(export)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        owner_lock::lock_owner(
            &mut transaction,
            DurableOwner::Person(export.person_id.into_uuid()),
        )
        .await
        .map_err(IdentityStoreError::storage)?;
        query("INSERT INTO yunxi_persons (id) VALUES ($1) ON CONFLICT (id) DO NOTHING")
            .bind(export.person_id.into_uuid())
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        let locked_person =
            query_scalar::<Postgres, Uuid>("SELECT id FROM yunxi_persons WHERE id = $1 FOR UPDATE")
                .bind(export.person_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?;
        if locked_person != Some(export.person_id.into_uuid()) {
            return Err(portable_error(
                "portable person disappeared while import was starting",
            ));
        }
        for external in &export.external_identities {
            let platform = PlatformId::new(external.platform.clone())
                .map_err(|error| IdentityStoreError::storage(std::io::Error::other(error)))?;
            let identity = ExternalIdentity::new(platform, external.external_id.clone())
                .map_err(|error| IdentityStoreError::storage(std::io::Error::other(error)))?;
            let mapped_person = query_scalar::<Postgres, Uuid>(
                "INSERT INTO yunxi_external_identities AS stored
                    (platform, external_id, person_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (platform, external_id) DO UPDATE
                    SET person_id = stored.person_id
                 RETURNING person_id",
            )
            .bind(identity.platform().as_str())
            .bind(identity.external_id())
            .bind(export.person_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            if mapped_person != export.person_id.into_uuid() {
                return Err(portable_error(format!(
                    "external identity {}/{} already belongs to another person",
                    identity.platform(),
                    identity.external_id()
                )));
            }
        }
        for memory in &export.memories {
            let stored = query(
                "INSERT INTO yunxi_memories AS current_memory
                    (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at, created_at)
                 VALUES ($1, 'person', $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (id) DO UPDATE SET id = current_memory.id
                 RETURNING *, (xmax = 0) AS portable_was_inserted",
            )
            .bind(memory.id().into_uuid())
            .bind(export.person_id.into_uuid())
            .bind(memory.kind().as_str())
            .bind(memory.content())
            .bind(i16::from(memory.importance()))
            .bind(serde_json::to_value(memory.tags()).map_err(IdentityStoreError::storage)?)
            .bind(memory.occurred_at())
            .bind(memory.created_at())
            .fetch_one(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            let was_inserted = stored
                .try_get::<bool, _>("portable_was_inserted")
                .map_err(IdentityStoreError::storage)?;
            if !was_inserted && row_to_portable_memory(&stored)? != *memory {
                return Err(portable_error(format!(
                    "memory ID {} conflicts with an existing memory",
                    memory.id()
                )));
            }
        }
        for item in &export.open_loops {
            let stored = query(
                "INSERT INTO yunxi_open_loops AS current_item
                    (id, owner_kind, owner_id, kind, summary, source_message_id, due_at,
                     expires_at, salience, status, dedupe_key, created_at, updated_at,
                     resolved_at, triggered_at, version)
                 VALUES ($1, 'person', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                         $13, $14, $15)
                 ON CONFLICT (id) DO UPDATE SET id = current_item.id
                 RETURNING *, (xmax = 0) AS portable_was_inserted",
            )
            .bind(item.id().into_uuid())
            .bind(export.person_id.into_uuid())
            .bind(item.kind().as_str())
            .bind(item.summary())
            .bind(item.source_message_id().map(MessageId::into_uuid))
            .bind(item.due_at())
            .bind(item.expires_at())
            .bind(i16::from(item.salience()))
            .bind(item.status().as_str())
            .bind(item.dedupe_key())
            .bind(item.created_at())
            .bind(item.updated_at())
            .bind(item.resolved_at())
            .bind(item.triggered_at())
            .bind(i64::try_from(item.version()).map_err(|_| {
                portable_error("portable open-loop version exceeds PostgreSQL BIGINT")
            })?)
            .fetch_one(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            let was_inserted = stored
                .try_get::<bool, _>("portable_was_inserted")
                .map_err(IdentityStoreError::storage)?;
            if !was_inserted && row_to_portable_open_loop(&stored)? != *item {
                return Err(portable_error(format!(
                    "open-loop ID {} conflicts with an existing item",
                    item.id()
                )));
            }
        }
        for goal in &export.goals {
            let stored = query(
                "INSERT INTO yunxi_goals AS current_goal
                    (id, owner_kind, owner_id, kind, title, details, state, due_at,
                     created_at, updated_at, completed_at)
                 VALUES ($1, 'person', $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (id) DO UPDATE SET id = current_goal.id
                 RETURNING *, (xmax = 0) AS portable_was_inserted",
            )
            .bind(goal.id().into_uuid())
            .bind(export.person_id.into_uuid())
            .bind(portable_goal_kind_name(goal.kind()))
            .bind(goal.title())
            .bind(goal.details())
            .bind(portable_goal_state_name(goal.state()))
            .bind(goal.due_at())
            .bind(goal.created_at())
            .bind(goal.updated_at())
            .bind(goal.completed_at())
            .fetch_one(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            let was_inserted = stored
                .try_get::<bool, _>("portable_was_inserted")
                .map_err(IdentityStoreError::storage)?;
            if !was_inserted && row_to_portable_goal(&stored)? != *goal {
                return Err(portable_error(format!(
                    "goal ID {} conflicts with an existing goal",
                    goal.id()
                )));
            }
        }
        if let Some(relation) = export.relation {
            query(
                "INSERT INTO yunxi_relations
                    (person_id, familiarity, affinity, trust, comfort, tension)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (person_id) DO UPDATE SET
                    familiarity = EXCLUDED.familiarity, affinity = EXCLUDED.affinity,
                    trust = EXCLUDED.trust, comfort = EXCLUDED.comfort,
                    tension = EXCLUDED.tension, updated_at = NOW()",
            )
            .bind(export.person_id.into_uuid())
            .bind(f64::from(relation.familiarity))
            .bind(f64::from(relation.affinity))
            .bind(f64::from(relation.trust))
            .bind(f64::from(relation.comfort))
            .bind(f64::from(relation.tension))
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        }
        if let Some(affect) = export.affect {
            query(
                "INSERT INTO yunxi_affect_states
                    (person_id, valence, arousal, social_energy, curiosity)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (person_id) DO UPDATE SET
                    valence = EXCLUDED.valence, arousal = EXCLUDED.arousal,
                    social_energy = EXCLUDED.social_energy, curiosity = EXCLUDED.curiosity,
                    updated_at = NOW()",
            )
            .bind(export.person_id.into_uuid())
            .bind(f64::from(affect.valence))
            .bind(f64::from(affect.arousal))
            .bind(f64::from(affect.social_energy))
            .bind(f64::from(affect.curiosity))
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        }
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(export.person_id)
    }

    /// Read the existing Core scopes for one QQ peer without resolving or
    /// creating any identity. The extra row distinguishes the supported bound
    /// from an incomplete result, which must abort erasure rather than leave
    /// stale runtime state behind.
    pub(crate) async fn qq_person_domain_targets(
        &self,
        user_id: i64,
    ) -> Result<QqPersonDomainTargets, IdentityStoreError> {
        if user_id <= 0 {
            return Ok(QqPersonDomainTargets::default());
        }
        let external_id = user_id.to_string();
        let person_id = query_scalar::<Postgres, Uuid>(
            "SELECT person_id FROM yunxi_external_identities
             WHERE platform = 'qq' AND external_id = $1",
        )
        .bind(&external_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)?
        .map(PersonId::from_uuid);
        let alias_external_ids = if let Some(person_id) = person_id {
            let aliases = query_scalar::<Postgres, String>(
                "SELECT external_id FROM yunxi_external_identities
                 WHERE platform = 'qq' AND person_id = $1
                   AND external_id ~ '^[1-9][0-9]*$'
                 ORDER BY external_id
                 LIMIT $2",
            )
            .bind(person_id.into_uuid())
            .bind(i64::try_from(MAX_QQ_PERSON_ALIASES + 1).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(IdentityStoreError::storage)?;
            ensure_bounded_qq_aliases(aliases)?
        } else {
            // A legacy deletion may already have removed the Person/external
            // identity while leaving direct-conversation mappings behind.
            vec![external_id.clone()]
        };
        let mut direct_conversation_ids = query_scalar::<Postgres, Uuid>(
            r#"
            SELECT DISTINCT external.conversation_id
            FROM yunxi_external_conversations AS external
            JOIN yunxi_conversations AS conversation
              ON conversation.id = external.conversation_id
            WHERE external.platform = 'qq'
              AND conversation.kind = 'direct'
              AND external.external_id ~ '^direct:[1-9][0-9]*:[1-9][0-9]*$'
              AND split_part(external.external_id, ':', 3) = ANY($1)
            ORDER BY external.conversation_id
            LIMIT $2
            "#,
        )
        .bind(&alias_external_ids)
        .bind(i64::try_from(MAX_QQ_PERSON_DIRECT_CONVERSATIONS + 1).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)?;
        if direct_conversation_ids.len() > MAX_QQ_PERSON_DIRECT_CONVERSATIONS {
            return Err(IdentityStoreError::storage(std::io::Error::other(
                "QQ person has too many direct conversations to erase safely",
            )));
        }
        direct_conversation_ids.sort_unstable();
        direct_conversation_ids.dedup();
        let qq_user_ids = alias_external_ids
            .iter()
            .map(|external_id| {
                external_id.parse::<i64>().map_err(|error| {
                    IdentityStoreError::storage(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error,
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(QqPersonDomainTargets {
            person_id,
            qq_user_ids,
            direct_conversation_ids: direct_conversation_ids
                .into_iter()
                .map(ConversationId::from_uuid)
                .collect(),
        })
    }

    /// Delete all Core data attributable to one canonical person. For a QQ
    /// identity, every direct conversation with that peer is removed across
    /// bot accounts; group conversations are deliberately excluded because
    /// they are shared resources and have a separate host deletion command.
    ///
    /// The operation is transactional and only looks up existing mappings; it
    /// never creates a PersonId merely because a deletion was requested.
    pub(crate) async fn delete_person_domain_data(
        &self,
        external_identity: &ExternalIdentity,
        direct_conversation: &ExternalConversation,
    ) -> Result<PersonDomainDeletion, IdentityStoreError> {
        if direct_conversation.kind() != ConversationKind::Direct {
            return Err(IdentityStoreError::ConversationKindMismatch {
                requested: ConversationKind::Direct,
                stored: direct_conversation.kind(),
            });
        }

        // Match the lock order used by Core memory writes and cleanup. Holding
        // the legacy manager guard before opening the transaction prevents a
        // writer from publishing a compatibility row after this owner purge
        // has taken its snapshot but before the host's final cache pass.
        let _legacy_save_guard = crate::memory::MEMORY_MANAGER.acquire_save_lock().await;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        // Memory writers and retention use the same lock so domain erasure
        // cannot delete Core data between a dual-write's two projections.
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        let person_id = query_scalar::<Postgres, Uuid>(
            "SELECT person_id FROM yunxi_external_identities
             WHERE platform = $1 AND external_id = $2",
        )
        .bind(external_identity.platform().as_str())
        .bind(external_identity.external_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        if let Some(person_id) = person_id {
            owner_lock::lock_owner(&mut transaction, DurableOwner::Person(person_id))
                .await
                .map_err(IdentityStoreError::storage)?;
            let locked_person_id = query_scalar::<Postgres, Uuid>(
                "SELECT person_id FROM yunxi_external_identities
                 WHERE platform = $1 AND external_id = $2
                 FOR UPDATE",
            )
            .bind(external_identity.platform().as_str())
            .bind(external_identity.external_id())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            if locked_person_id != Some(person_id) {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "external identity changed while person deletion was acquiring its owner lock",
                )));
            }
        }
        let qq_alias_external_ids =
            if is_qq_direct_for_identity(external_identity, direct_conversation) {
                if let Some(person_id) = person_id {
                    let aliases = query_scalar::<Postgres, String>(
                        "SELECT external_id FROM yunxi_external_identities
                     WHERE platform = 'qq' AND person_id = $1
                       AND external_id ~ '^[1-9][0-9]*$'
                     ORDER BY external_id
                     LIMIT $2",
                    )
                    .bind(person_id)
                    .bind(i64::try_from(MAX_QQ_PERSON_ALIASES + 1).unwrap_or(i64::MAX))
                    .fetch_all(&mut *transaction)
                    .await
                    .map_err(IdentityStoreError::storage)?;
                    Some(ensure_bounded_qq_aliases(aliases)?)
                } else {
                    Some(vec![external_identity.external_id().to_owned()])
                }
            } else {
                None
            };
        let mut conversation_ids = if let Some(person_id) = person_id {
            query_scalar::<Postgres, Uuid>(
                "SELECT DISTINCT member.conversation_id
                 FROM yunxi_conversation_members AS member
                 JOIN yunxi_conversations AS conversation
                   ON conversation.id = member.conversation_id
                 WHERE member.person_id = $1 AND conversation.kind = 'direct'
                 ORDER BY member.conversation_id
                 LIMIT $2",
            )
            .bind(person_id)
            .bind(i64::try_from(MAX_PERSON_DIRECT_CONVERSATIONS + 1).unwrap_or(i64::MAX))
            .fetch_all(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
        } else {
            Vec::new()
        };
        if let Some(qq_alias_external_ids) = &qq_alias_external_ids {
            conversation_ids.extend(
                query_scalar::<Postgres, Uuid>(
                    r#"
                SELECT DISTINCT external.conversation_id
                FROM yunxi_external_conversations AS external
                JOIN yunxi_conversations AS conversation
                  ON conversation.id = external.conversation_id
                WHERE external.platform = 'qq'
                  AND conversation.kind = 'direct'
                  AND external.external_id ~ '^direct:[1-9][0-9]*:[1-9][0-9]*$'
                  AND split_part(external.external_id, ':', 3) = ANY($1)
                ORDER BY external.conversation_id
                LIMIT $2
                "#,
                )
                .bind(qq_alias_external_ids)
                .bind(i64::try_from(MAX_PERSON_DIRECT_CONVERSATIONS + 1).unwrap_or(i64::MAX))
                .fetch_all(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?,
            );
        }
        conversation_ids.extend(
            query_scalar::<Postgres, Uuid>(
                "SELECT external.conversation_id
                 FROM yunxi_external_conversations AS external
                 JOIN yunxi_conversations AS conversation
                   ON conversation.id = external.conversation_id
                 WHERE external.platform = $1 AND external.external_id = $2
                   AND conversation.kind = 'direct'",
            )
            .bind(direct_conversation.platform().as_str())
            .bind(direct_conversation.external_id())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?,
        );
        conversation_ids.sort_unstable();
        conversation_ids.dedup();
        if conversation_ids.len() > MAX_PERSON_DIRECT_CONVERSATIONS {
            return Err(IdentityStoreError::storage(std::io::Error::other(
                "person has too many direct conversations to erase safely",
            )));
        }
        let qq_user_ids = qq_alias_external_ids
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|external_id| {
                external_id.parse::<i64>().map_err(|error| {
                    IdentityStoreError::storage(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error,
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut conversation_routes = if conversation_ids.is_empty() {
            Vec::new()
        } else {
            query(
                "SELECT platform, external_id
                 FROM yunxi_external_conversations
                 WHERE conversation_id = ANY($1)
                 ORDER BY platform, external_id
                 LIMIT $2",
            )
            .bind(&conversation_ids)
            .bind(i64::try_from(MAX_PERSON_DIRECT_CONVERSATION_ROUTES + 1).unwrap_or(i64::MAX))
            .fetch_all(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
            .into_iter()
            .map(|row| {
                Ok(ExternalConversationRoute {
                    platform: row
                        .try_get::<String, _>("platform")
                        .map_err(IdentityStoreError::storage)?,
                    external_id: row
                        .try_get::<String, _>("external_id")
                        .map_err(IdentityStoreError::storage)?,
                })
            })
            .collect::<Result<Vec<_>, IdentityStoreError>>()?
        };
        conversation_routes.push(ExternalConversationRoute::from_external(
            direct_conversation,
        ));
        conversation_routes.sort_unstable();
        conversation_routes.dedup();
        if conversation_routes.len() > MAX_PERSON_DIRECT_CONVERSATION_ROUTES {
            return Err(IdentityStoreError::storage(std::io::Error::other(
                "person has too many external conversation routes to erase safely",
            )));
        }
        for route in &conversation_routes {
            lock_conversation_route(&mut transaction, route)
                .await
                .map_err(IdentityStoreError::storage)?;
        }
        for conversation_id in &conversation_ids {
            owner_lock::lock_owner(
                &mut transaction,
                DurableOwner::Conversation(*conversation_id),
            )
            .await
            .map_err(IdentityStoreError::storage)?;
        }
        if !conversation_ids.is_empty() {
            let locked_direct_ids = query_scalar::<Postgres, Uuid>(
                "SELECT id FROM yunxi_conversations
                 WHERE id = ANY($1) AND kind = 'direct'
                 ORDER BY id
                 FOR UPDATE",
            )
            .bind(&conversation_ids)
            .fetch_all(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            if locked_direct_ids != conversation_ids {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "direct conversation ownership changed during person deletion",
                )));
            }
            let has_conflicting_members = query_scalar::<Postgres, bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM yunxi_conversation_members
                     WHERE conversation_id = ANY($1)
                       AND ($2::uuid IS NULL OR person_id <> $2)
                 )",
            )
            .bind(&conversation_ids)
            .bind(person_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            if has_conflicting_members {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "direct conversation belongs to another canonical person",
                )));
            }
        }
        if person_id.is_none() {
            let appeared_person = query_scalar::<Postgres, Uuid>(
                "SELECT person_id FROM yunxi_external_identities
                 WHERE platform = $1 AND external_id = $2",
            )
            .bind(external_identity.platform().as_str())
            .bind(external_identity.external_id())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            if appeared_person.is_some() {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "external identity appeared while legacy person deletion was running",
                )));
            }
        }

        let mut deleted = PersonDomainDeletion::default();
        deleted.delivery_ledger +=
            delete_person_domain_rows(&mut transaction, person_id, &conversation_ids, &qq_user_ids)
                .await
                .map_err(IdentityStoreError::storage)?;
        // World Model v4 data follows the same erasure boundary (v4 §242);
        // best-effort: a world-store failure must not abort the erasure.
        if let Ok(rows) =
            super::world_model_store::PostgresWorldModelStore::delete_person_domain_rows(
                &mut transaction,
                person_id,
                &conversation_ids,
            )
            .await
        {
            deleted.world_model = rows;
        }
        if let Some(person_id) = person_id {
            deleted.memories +=
                query("DELETE FROM yunxi_memories WHERE scope_kind = 'person' AND scope_id = $1")
                    .bind(person_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(IdentityStoreError::storage)?
                    .rows_affected();
            deleted.open_loops +=
                query("DELETE FROM yunxi_open_loops WHERE owner_kind = 'person' AND owner_id = $1")
                    .bind(person_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(IdentityStoreError::storage)?
                    .rows_affected();
            deleted.goals +=
                query("DELETE FROM yunxi_goals WHERE owner_kind = 'person' AND owner_id = $1")
                    .bind(person_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(IdentityStoreError::storage)?
                    .rows_affected();
            deleted.affect_states += query("DELETE FROM yunxi_affect_states WHERE person_id = $1")
                .bind(person_id)
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?
                .rows_affected();
            deleted.relations += query("DELETE FROM yunxi_relations WHERE person_id = $1")
                .bind(person_id)
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?
                .rows_affected();
            deleted.external_identities +=
                query("DELETE FROM yunxi_external_identities WHERE person_id = $1")
                    .bind(person_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(IdentityStoreError::storage)?
                    .rows_affected();
            deleted.persons += query("DELETE FROM yunxi_persons WHERE id = $1")
                .bind(person_id)
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?
                .rows_affected();
        }

        for conversation_id in conversation_ids {
            deleted.memories += query(
                "DELETE FROM yunxi_memories
                 WHERE scope_kind = 'conversation' AND scope_id = $1",
            )
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
            .rows_affected();
            deleted.open_loops += query(
                "DELETE FROM yunxi_open_loops
                 WHERE owner_kind = 'conversation' AND owner_id = $1",
            )
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
            .rows_affected();
            deleted.goals += query(
                "DELETE FROM yunxi_goals WHERE owner_kind = 'conversation' AND owner_id = $1",
            )
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
            .rows_affected();
            deleted.message_mappings +=
                query("DELETE FROM yunxi_message_mappings WHERE conversation_id = $1")
                    .bind(conversation_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(IdentityStoreError::storage)?
                    .rows_affected();
            deleted.external_conversations +=
                query("DELETE FROM yunxi_external_conversations WHERE conversation_id = $1")
                    .bind(conversation_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(IdentityStoreError::storage)?
                    .rows_affected();
            deleted.conversations += query("DELETE FROM yunxi_conversations WHERE id = $1")
                .bind(conversation_id)
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?
                .rows_affected();
        }

        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(deleted)
    }

    /// Looks up an existing canonical QQ group without creating a replacement
    /// mapping while a deletion barrier is being established.
    pub(crate) async fn qq_group_conversation_id(
        &self,
        group_id: i64,
    ) -> Result<Option<ConversationId>, IdentityStoreError> {
        if group_id <= 0 {
            return Err(IdentityStoreError::storage(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "QQ group id must be positive",
            )));
        }
        query_scalar::<Postgres, Uuid>(
            "SELECT external.conversation_id
             FROM yunxi_external_conversations AS external
             JOIN yunxi_conversations AS conversation
               ON conversation.id = external.conversation_id
             WHERE external.platform = 'qq' AND external.external_id = $1
               AND conversation.kind = 'group'",
        )
        .bind(format!("group:{group_id}"))
        .fetch_optional(&self.pool)
        .await
        .map(|conversation_id| conversation_id.map(ConversationId::from_uuid))
        .map_err(IdentityStoreError::storage)
    }

    /// Delete the canonical Core conversation domains attributable to one QQ
    /// group. Removing the canonical conversation makes a cross-process send
    /// waiting on its owner lock fail existence revalidation after the purge.
    pub(crate) async fn delete_qq_group_domain_data(
        &self,
        group_id: i64,
    ) -> Result<u64, IdentityStoreError> {
        if group_id <= 0 {
            return Err(IdentityStoreError::storage(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "QQ group id must be positive",
            )));
        }
        // Keep the compatibility table mutation order identical to the Core
        // memory writer/cleanup path while the canonical group owner is
        // removed.
        let _legacy_save_guard = crate::memory::MEMORY_MANAGER.acquire_save_lock().await;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        let conversation_id = query_scalar::<Postgres, Uuid>(
            "SELECT external.conversation_id
             FROM yunxi_external_conversations AS external
             JOIN yunxi_conversations AS conversation
               ON conversation.id = external.conversation_id
             WHERE external.platform = 'qq' AND external.external_id = $1
               AND conversation.kind = 'group'
             FOR UPDATE OF external, conversation",
        )
        .bind(format!("group:{group_id}"))
        .fetch_optional(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        if let Some(conversation_id) = conversation_id {
            owner_lock::lock_owner(
                &mut transaction,
                DurableOwner::Conversation(conversation_id),
            )
            .await
            .map_err(IdentityStoreError::storage)?;
        }
        let mut deleted = delete_group_domain_rows(&mut transaction, conversation_id, group_id)
            .await
            .map_err(IdentityStoreError::storage)?;
        // World Model v4 conversation rows follow the same group erasure
        // boundary (v4 §242); best-effort.
        if let Some(conversation_id) = conversation_id
            && let Ok(rows) =
                super::world_model_store::PostgresWorldModelStore::delete_conversation_domain_rows(
                    &mut transaction,
                    conversation_id,
                )
                .await
        {
            deleted += rows;
        }
        if let Some(conversation_id) = conversation_id {
            deleted += query(
                "DELETE FROM yunxi_memories
                 WHERE scope_kind = 'conversation' AND scope_id = $1",
            )
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
            .rows_affected();
            deleted += query(
                "DELETE FROM yunxi_open_loops
                 WHERE owner_kind = 'conversation' AND owner_id = $1",
            )
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
            .rows_affected();
            deleted += query(
                "DELETE FROM yunxi_goals
                 WHERE owner_kind = 'conversation' AND owner_id = $1",
            )
            .bind(conversation_id)
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
            .rows_affected();
            deleted += query("DELETE FROM yunxi_message_mappings WHERE conversation_id = $1")
                .bind(conversation_id)
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?
                .rows_affected();
            deleted += query("DELETE FROM yunxi_external_conversations WHERE conversation_id = $1")
                .bind(conversation_id)
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?
                .rows_affected();
            deleted += query("DELETE FROM yunxi_conversations WHERE id = $1")
                .bind(conversation_id)
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?
                .rows_affected();
        }
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(deleted)
    }

    /// Resolve the only safe QQ ReachOut route while serializing against
    /// person-domain deletion. Unlike the general ingress resolver, this
    /// rechecks canonical identity ownership before it may create a direct
    /// conversation.
    pub(crate) async fn resolve_qq_direct_for_person_delivery(
        &self,
        person_id: PersonId,
        self_id: i64,
    ) -> Result<Option<(ConversationId, i64)>, IdentityStoreError> {
        if self_id <= 0 {
            return Ok(None);
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        if !owner_lock::lock_and_owner_exists(
            &mut transaction,
            DurableOwner::Person(person_id.into_uuid()),
        )
        .await
        .map_err(IdentityStoreError::storage)?
        {
            transaction
                .rollback()
                .await
                .map_err(IdentityStoreError::storage)?;
            return Ok(None);
        }
        let external_ids = query_scalar::<Postgres, String>(
            "SELECT external_id FROM yunxi_external_identities
             WHERE platform = 'qq' AND person_id = $1
             ORDER BY external_id LIMIT 2",
        )
        .bind(person_id.into_uuid())
        .fetch_all(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        let [external_id] = external_ids.as_slice() else {
            transaction
                .rollback()
                .await
                .map_err(IdentityStoreError::storage)?;
            return Ok(None);
        };
        let Ok(user_id) = external_id.parse::<i64>() else {
            transaction
                .rollback()
                .await
                .map_err(IdentityStoreError::storage)?;
            return Ok(None);
        };
        if user_id <= 0 {
            transaction
                .rollback()
                .await
                .map_err(IdentityStoreError::storage)?;
            return Ok(None);
        }

        let direct_external_id = format!("direct:{self_id}:{user_id}");
        lock_conversation_route(
            &mut transaction,
            &ExternalConversationRoute {
                platform: "qq".to_owned(),
                external_id: direct_external_id.clone(),
            },
        )
        .await
        .map_err(IdentityStoreError::storage)?;
        if let Some(row) = query(
            "SELECT conversation.id, conversation.kind
             FROM yunxi_external_conversations AS external
             JOIN yunxi_conversations AS conversation
               ON conversation.id = external.conversation_id
             WHERE external.platform = 'qq' AND external.external_id = $1",
        )
        .bind(&direct_external_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        {
            let conversation_id = row
                .try_get::<Uuid, _>("id")
                .map_err(IdentityStoreError::storage)?;
            let kind = parse_stored_kind(
                &row.try_get::<String, _>("kind")
                    .map_err(IdentityStoreError::storage)?,
            )?;
            ensure_kind(ConversationKind::Direct, kind)?;
            owner_lock::lock_owner(
                &mut transaction,
                DurableOwner::Conversation(conversation_id),
            )
            .await
            .map_err(IdentityStoreError::storage)?;
            transaction
                .commit()
                .await
                .map_err(IdentityStoreError::storage)?;
            return Ok(Some((ConversationId::from_uuid(conversation_id), user_id)));
        }

        let candidate = ConversationId::new();
        query("INSERT INTO yunxi_conversations (id, kind) VALUES ($1, 'direct')")
            .bind(candidate.into_uuid())
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        let winner = query_scalar::<Postgres, Uuid>(
            "INSERT INTO yunxi_external_conversations AS current_mapping
                (platform, external_id, conversation_id)
             VALUES ('qq', $1, $2)
             ON CONFLICT (platform, external_id) DO UPDATE
             SET conversation_id = current_mapping.conversation_id
             RETURNING current_mapping.conversation_id",
        )
        .bind(&direct_external_id)
        .bind(candidate.into_uuid())
        .fetch_one(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        if winner != candidate.into_uuid() {
            query("DELETE FROM yunxi_conversations WHERE id = $1")
                .bind(candidate.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?;
        }
        owner_lock::lock_owner(&mut transaction, DurableOwner::Conversation(winner))
            .await
            .map_err(IdentityStoreError::storage)?;
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(Some((ConversationId::from_uuid(winner), user_id)))
    }

    pub(crate) async fn qq_external_identities_for_person(
        &self,
        person_id: PersonId,
    ) -> Result<Vec<String>, IdentityStoreError> {
        query_scalar::<Postgres, String>(
            "SELECT external_id FROM yunxi_external_identities WHERE platform = 'qq' AND person_id = $1 ORDER BY external_id LIMIT 32",
        )
        .bind(person_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)
    }

    /// Resolve a delivery target without guessing when a person has changed
    /// accounts. `LIMIT 2` distinguishes zero, exactly one, and ambiguous
    /// mappings while keeping the query bounded.
    pub(crate) async fn qq_external_identity_for_delivery(
        &self,
        person_id: PersonId,
    ) -> Result<Option<String>, IdentityStoreError> {
        let ids = query_scalar::<Postgres, String>(
            "SELECT external_id FROM yunxi_external_identities WHERE platform = 'qq' AND person_id = $1 ORDER BY external_id LIMIT 2",
        )
        .bind(person_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)?;
        Ok((ids.len() == 1).then(|| ids.into_iter().next()).flatten())
    }

    pub(crate) async fn qq_external_conversations_for_id(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<(String, ConversationKind)>, IdentityStoreError> {
        let rows = query(
            "SELECT external.external_id, conversation.kind FROM yunxi_external_conversations AS external JOIN yunxi_conversations AS conversation ON conversation.id = external.conversation_id WHERE external.platform = 'qq' AND external.conversation_id = $1 ORDER BY external.external_id LIMIT 2",
        )
        .bind(conversation_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)?;
        rows.into_iter()
            .map(|row| {
                let external_id = row
                    .try_get::<String, _>("external_id")
                    .map_err(IdentityStoreError::storage)?;
                let kind = row
                    .try_get::<String, _>("kind")
                    .map_err(IdentityStoreError::storage)?;
                let kind =
                    ConversationKind::from_str(&kind).map_err(IdentityStoreError::storage)?;
                Ok((external_id, kind))
            })
            .collect()
    }

    pub(crate) async fn record_qq_message_mapping(
        &self,
        message_id: MessageId,
        conversation_id: ConversationId,
        external_message_id: i64,
        direction: &'static str,
    ) -> Result<(), IdentityStoreError> {
        query(
            "INSERT INTO yunxi_message_mappings
                (message_id, conversation_id, platform, external_message_id, direction)
             VALUES ($1, $2, 'qq', $3, $4)
             ON CONFLICT (message_id) DO UPDATE
             SET external_message_id = EXCLUDED.external_message_id,
                 direction = EXCLUDED.direction",
        )
        .bind(message_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(external_message_id)
        .bind(direction)
        .execute(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)?;
        Ok(())
    }

    pub(crate) async fn qq_message_id_for_core(
        &self,
        message_id: MessageId,
        conversation_id: ConversationId,
    ) -> Result<Option<i64>, IdentityStoreError> {
        query_scalar::<Postgres, i64>(
            "SELECT external_message_id FROM yunxi_message_mappings
             WHERE message_id = $1 AND conversation_id = $2 AND platform = 'qq' LIMIT 1",
        )
        .bind(message_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)
    }

    /// Return whether a QQ message id was durably emitted by Yunxi in this
    /// conversation. Unlike the Redis recall window, this survives process and
    /// cache restarts and can therefore classify a late quote reliably.
    pub(crate) async fn qq_outbound_message_exists(
        &self,
        conversation_id: ConversationId,
        external_message_id: i64,
    ) -> Result<bool, IdentityStoreError> {
        query_scalar::<Postgres, bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM yunxi_message_mappings
                 WHERE conversation_id = $1
                   AND platform = 'qq'
                   AND external_message_id = $2
                   AND direction = 'outbound'
             )",
        )
        .bind(conversation_id.into_uuid())
        .bind(external_message_id)
        .fetch_one(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)
    }

    /// Prune message id mappings older than the given retention. These rows are
    /// only needed to cross-reference recently sent/received OneBot message IDs
    /// back to Core identifiers (recall, quoting, dedupe). Without a retention
    /// the table grows by one row per message forever on a long-running deploy;
    /// a rolling window keeps it bounded while never dropping a mapping that is
    /// still within the recall horizon.
    pub(crate) async fn cleanup_expired_message_mappings(
        &self,
        now: DateTime<Utc>,
        retention_days: i64,
    ) -> Result<u64, IdentityStoreError> {
        let cutoff = now - chrono::Duration::days(retention_days);
        let deleted = query("DELETE FROM yunxi_message_mappings WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(IdentityStoreError::storage)?
            .rows_affected();
        Ok(deleted)
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        for statement in [
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_persons (
                id UUID PRIMARY KEY,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_external_identities (
                platform TEXT COLLATE "C" NOT NULL
                    CHECK (platform ~ '^[a-z][a-z0-9_-]{0,63}$'),
                external_id TEXT COLLATE "C" NOT NULL
                    CHECK (octet_length(external_id) BETWEEN 1 AND 512),
                person_id UUID NOT NULL
                    REFERENCES yunxi_persons(id) ON DELETE CASCADE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (platform, external_id)
            )
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_external_identities_person_idx
                ON yunxi_external_identities (person_id)
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_conversations (
                id UUID PRIMARY KEY,
                kind TEXT NOT NULL CHECK (kind IN ('direct', 'group', 'system')),
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_external_conversations (
                platform TEXT COLLATE "C" NOT NULL
                    CHECK (platform ~ '^[a-z][a-z0-9_-]{0,63}$'),
                external_id TEXT COLLATE "C" NOT NULL
                    CHECK (octet_length(external_id) BETWEEN 1 AND 512),
                conversation_id UUID NOT NULL
                    REFERENCES yunxi_conversations(id) ON DELETE CASCADE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (platform, external_id)
            )
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_external_conversations_conversation_idx
                ON yunxi_external_conversations (conversation_id)
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_message_mappings (
                message_id UUID PRIMARY KEY,
                conversation_id UUID NOT NULL
                    REFERENCES yunxi_conversations(id) ON DELETE CASCADE,
                platform TEXT COLLATE "C" NOT NULL
                    CHECK (platform ~ '^[a-z][a-z0-9_-]{0,63}$'),
                external_message_id BIGINT NOT NULL CHECK (external_message_id > 0),
                direction TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_conversation_members (
                conversation_id UUID NOT NULL
                    REFERENCES yunxi_conversations(id) ON DELETE CASCADE,
                person_id UUID NOT NULL
                    REFERENCES yunxi_persons(id) ON DELETE CASCADE,
                role TEXT CHECK (role IS NULL OR (octet_length(role) BETWEEN 1 AND 256
                    AND char_length(role) BETWEEN 1 AND 128 AND btrim(role) <> '')),
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (conversation_id, person_id)
            )
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_message_mappings_conversation_idx
                ON yunxi_message_mappings (conversation_id, platform, external_message_id)
            "#,
            r#"
            CREATE INDEX IF NOT EXISTS yunxi_conversation_members_person_idx
                ON yunxi_conversation_members (person_id, conversation_id)
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
            ("yunxi_persons", "id", "uuid"),
            ("yunxi_persons", "created_at", "timestamptz"),
            ("yunxi_external_identities", "platform", "text"),
            ("yunxi_external_identities", "external_id", "text"),
            ("yunxi_external_identities", "person_id", "uuid"),
            ("yunxi_external_identities", "created_at", "timestamptz"),
            ("yunxi_conversations", "id", "uuid"),
            ("yunxi_conversations", "kind", "text"),
            ("yunxi_conversations", "created_at", "timestamptz"),
            ("yunxi_external_conversations", "platform", "text"),
            ("yunxi_external_conversations", "external_id", "text"),
            ("yunxi_external_conversations", "conversation_id", "uuid"),
            ("yunxi_external_conversations", "created_at", "timestamptz"),
            ("yunxi_message_mappings", "message_id", "uuid"),
            ("yunxi_message_mappings", "conversation_id", "uuid"),
            ("yunxi_message_mappings", "platform", "text"),
            ("yunxi_message_mappings", "external_message_id", "int8"),
            ("yunxi_message_mappings", "direction", "text"),
            ("yunxi_message_mappings", "created_at", "timestamptz"),
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
                anyhow::bail!("Yunxi identity schema is missing column {table}.{column}");
            };
            let stored_type = stored.try_get::<String, _>("udt_name")?;
            let nullable = stored.try_get::<String, _>("is_nullable")?;
            if stored_type != udt_name || nullable != "NO" {
                anyhow::bail!(
                    "Yunxi identity schema column {table}.{column} has type {stored_type} and nullability {nullable}, expected {udt_name} NOT NULL"
                );
            }
        }

        for (table, expected_definition) in [
            ("yunxi_persons", "PRIMARY KEY (id)"),
            (
                "yunxi_external_identities",
                "PRIMARY KEY (platform, external_id)",
            ),
            ("yunxi_conversations", "PRIMARY KEY (id)"),
            (
                "yunxi_external_conversations",
                "PRIMARY KEY (platform, external_id)",
            ),
        ] {
            let primary_key_definition = query_scalar::<Postgres, Option<String>>(
                r#"
                SELECT pg_get_constraintdef(constraint_row.oid)
                FROM pg_constraint AS constraint_row
                JOIN pg_class AS table_row
                  ON table_row.oid = constraint_row.conrelid
                JOIN pg_namespace AS namespace_row
                  ON namespace_row.oid = table_row.relnamespace
                WHERE namespace_row.nspname = current_schema()
                  AND table_row.relname = $1
                  AND constraint_row.contype = 'p'
                "#,
            )
            .bind(table)
            .fetch_one(&self.pool)
            .await?;
            if primary_key_definition.as_deref() != Some(expected_definition) {
                anyhow::bail!(
                    "Yunxi identity schema table {table} has primary key {:?}, expected {expected_definition}",
                    primary_key_definition
                );
            }
        }

        for (table, column) in [
            ("yunxi_external_identities", "platform"),
            ("yunxi_external_identities", "external_id"),
            ("yunxi_external_conversations", "platform"),
            ("yunxi_external_conversations", "external_id"),
        ] {
            let collation = query_scalar::<Postgres, String>(
                r#"
                SELECT collation_name
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
            if collation.as_deref() != Some("C") {
                anyhow::bail!(
                    "Yunxi identity schema column {table}.{column} uses collation {:?}, expected C",
                    collation
                );
            }
        }

        for (table, parent_table, column) in [
            ("yunxi_external_identities", "yunxi_persons", "person_id"),
            (
                "yunxi_external_conversations",
                "yunxi_conversations",
                "conversation_id",
            ),
        ] {
            let foreign_key = query_scalar::<Postgres, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM pg_constraint AS constraint_row
                    JOIN pg_class AS child_table
                      ON child_table.oid = constraint_row.conrelid
                    JOIN pg_namespace AS child_namespace
                      ON child_namespace.oid = child_table.relnamespace
                    JOIN pg_class AS parent_table
                      ON parent_table.oid = constraint_row.confrelid
                    JOIN pg_namespace AS parent_namespace
                      ON parent_namespace.oid = parent_table.relnamespace
                    JOIN pg_attribute AS child_column
                      ON child_column.attrelid = constraint_row.conrelid
                     AND child_column.attname = $3
                     AND child_column.attnum = constraint_row.conkey[1]
                    JOIN pg_attribute AS parent_column
                      ON parent_column.attrelid = constraint_row.confrelid
                     AND parent_column.attname = 'id'
                     AND parent_column.attnum = constraint_row.confkey[1]
                    WHERE child_namespace.nspname = current_schema()
                      AND parent_namespace.nspname = current_schema()
                      AND child_table.relname = $1
                      AND parent_table.relname = $2
                      AND constraint_row.contype = 'f'
                      AND constraint_row.confdeltype = 'c'
                      AND constraint_row.convalidated
                      AND array_length(constraint_row.conkey, 1) = 1
                      AND array_length(constraint_row.confkey, 1) = 1
                )
                "#,
            )
            .bind(table)
            .bind(parent_table)
            .bind(column)
            .fetch_optional(&self.pool)
            .await?;
            if foreign_key != Some(true) {
                anyhow::bail!(
                    "Yunxi identity schema is missing ON DELETE CASCADE foreign key {table}.{column} -> {parent_table}.id"
                );
            }
        }

        for (table, required_fragments) in [
            (
                "yunxi_external_identities",
                vec!["platform", "[a-z][a-z0-9_-]", "{0,63}"],
            ),
            (
                "yunxi_external_identities",
                vec!["external_id", "octet_length", "1", "512"],
            ),
            (
                "yunxi_conversations",
                vec!["kind", "direct", "group", "system"],
            ),
            (
                "yunxi_external_conversations",
                vec!["platform", "[a-z][a-z0-9_-]", "{0,63}"],
            ),
            (
                "yunxi_external_conversations",
                vec!["external_id", "octet_length", "1", "512"],
            ),
        ] {
            let definitions = query_scalar::<Postgres, String>(
                r#"
                SELECT pg_get_constraintdef(constraint_row.oid)
                FROM pg_constraint AS constraint_row
                JOIN pg_class AS table_row
                  ON table_row.oid = constraint_row.conrelid
                JOIN pg_namespace AS namespace_row
                  ON namespace_row.oid = table_row.relnamespace
                WHERE namespace_row.nspname = current_schema()
                  AND table_row.relname = $1
                  AND constraint_row.contype = 'c'
                  AND constraint_row.convalidated
                "#,
            )
            .bind(table)
            .fetch_all(&self.pool)
            .await?;
            let valid = definitions.iter().any(|definition| {
                let definition = definition.to_ascii_lowercase();
                required_fragments
                    .iter()
                    .all(|fragment| definition.contains(fragment))
            });
            if !valid {
                anyhow::bail!(
                    "Yunxi identity schema table {table} is missing a validated CHECK constraint containing {required_fragments:?}"
                );
            }
        }

        for (table, index, column) in [
            (
                "yunxi_external_identities",
                "yunxi_external_identities_person_idx",
                "person_id",
            ),
            (
                "yunxi_external_conversations",
                "yunxi_external_conversations_conversation_idx",
                "conversation_id",
            ),
        ] {
            let exists = query_scalar::<Postgres, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM pg_index AS index_row
                    JOIN pg_class AS table_row
                      ON table_row.oid = index_row.indrelid
                    JOIN pg_namespace AS namespace_row
                      ON namespace_row.oid = table_row.relnamespace
                    JOIN pg_class AS index_table
                      ON index_table.oid = index_row.indexrelid
                    JOIN pg_attribute AS column_row
                      ON column_row.attrelid = index_row.indrelid
                     AND column_row.attname = $3
                    WHERE namespace_row.nspname = current_schema()
                      AND table_row.relname = $1
                      AND index_table.relname = $2
                      AND index_row.indnatts = 1
                      AND index_row.indnkeyatts = 1
                      AND column_row.attnum = ANY(index_row.indkey)
                      AND index_row.indpred IS NULL
                      AND index_row.indexprs IS NULL
                      AND index_row.indisvalid
                      AND index_row.indisready
                      AND NOT index_row.indisunique
                )
                "#,
            )
            .bind(table)
            .bind(index)
            .bind(column)
            .fetch_one(&self.pool)
            .await?;
            if !exists {
                anyhow::bail!("Yunxi identity schema is missing index {index}");
            }
        }
        Ok(())
    }

    pub(crate) async fn resolve_identity(
        &self,
        external: &ExternalIdentity,
    ) -> Result<PersonId, IdentityStoreError> {
        let candidate = PersonId::new();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        // Identity resolution can create the subject key used by legacy
        // memory projections.  Serialize it with memory maintenance first,
        // then use the canonical Person lock, matching unlink/erasure order.
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(IdentityStoreError::storage)?;

        // Do not hold a mapping row lock while acquiring the owner lock.  The
        // deletion path follows this same read -> owner lock -> FOR UPDATE
        // sequence, so unlink and resolve cannot deadlock each other.
        if let Some(existing) = query_scalar::<Postgres, Uuid>(
            "SELECT person_id FROM yunxi_external_identities
             WHERE platform = $1 AND external_id = $2",
        )
        .bind(external.platform().as_str())
        .bind(external.external_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        {
            if !owner_lock::lock_and_owner_exists(&mut transaction, DurableOwner::Person(existing))
                .await
                .map_err(IdentityStoreError::storage)?
            {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "external identity points to a missing canonical person",
                )));
            }
            let locked_person = query_scalar::<Postgres, Uuid>(
                "SELECT person_id FROM yunxi_external_identities
                 WHERE platform = $1 AND external_id = $2
                 FOR UPDATE",
            )
            .bind(external.platform().as_str())
            .bind(external.external_id())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            if locked_person != Some(existing) {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "external identity changed while resolution was acquiring its owner lock",
                )));
            }
            transaction
                .commit()
                .await
                .map_err(IdentityStoreError::storage)?;
            return Ok(PersonId::from_uuid(existing));
        }

        query("INSERT INTO yunxi_persons (id) VALUES ($1)")
            .bind(candidate.into_uuid())
            .execute(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        let winner = query_scalar::<Postgres, Uuid>(
            r#"
            INSERT INTO yunxi_external_identities AS current_mapping
                (platform, external_id, person_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (platform, external_id) DO UPDATE
            SET person_id = current_mapping.person_id
            RETURNING current_mapping.person_id
            "#,
        )
        .bind(external.platform().as_str())
        .bind(external.external_id())
        .bind(candidate.into_uuid())
        .fetch_one(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        if winner != candidate.into_uuid() {
            query("DELETE FROM yunxi_persons WHERE id = $1")
                .bind(candidate.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?;
        }
        if !owner_lock::lock_and_owner_exists(&mut transaction, DurableOwner::Person(winner))
            .await
            .map_err(IdentityStoreError::storage)?
        {
            return Err(IdentityStoreError::storage(std::io::Error::other(
                "resolved external identity points to a missing canonical person",
            )));
        }
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(PersonId::from_uuid(winner))
    }

    async fn resolve_conversation_in_transaction(
        transaction: &mut Transaction<'_, Postgres>,
        external: &ExternalConversation,
    ) -> Result<ConversationId, IdentityStoreError> {
        let route = ExternalConversationRoute::from_external(external);
        lock_conversation_route(transaction, &route)
            .await
            .map_err(IdentityStoreError::storage)?;
        if let Some(conversation_id) = query_scalar::<Postgres, Uuid>(
            "SELECT conversation_id FROM yunxi_external_conversations
             WHERE platform = $1 AND external_id = $2",
        )
        .bind(external.platform().as_str())
        .bind(external.external_id())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(IdentityStoreError::storage)?
        {
            if !owner_lock::lock_and_owner_exists(
                transaction,
                DurableOwner::Conversation(conversation_id),
            )
            .await
            .map_err(IdentityStoreError::storage)?
            {
                return Err(IdentityStoreError::storage(std::io::Error::other(
                    "canonical conversation disappeared during route resolution",
                )));
            }
            let stored_kind = query_scalar::<Postgres, String>(
                "SELECT kind FROM yunxi_conversations WHERE id = $1",
            )
            .bind(conversation_id)
            .fetch_one(&mut **transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
            ensure_kind(external.kind(), parse_stored_kind(&stored_kind)?)?;
            return Ok(ConversationId::from_uuid(conversation_id));
        }

        let candidate = ConversationId::new();
        query("INSERT INTO yunxi_conversations (id, kind) VALUES ($1, $2)")
            .bind(candidate.into_uuid())
            .bind(external.kind().as_str())
            .execute(&mut **transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        let winner = query_scalar::<Postgres, Uuid>(
            r#"
            INSERT INTO yunxi_external_conversations AS current_mapping
                (platform, external_id, conversation_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (platform, external_id) DO UPDATE
            SET conversation_id = current_mapping.conversation_id
            RETURNING current_mapping.conversation_id
            "#,
        )
        .bind(external.platform().as_str())
        .bind(external.external_id())
        .bind(candidate.into_uuid())
        .fetch_one(&mut **transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        if winner != candidate.into_uuid() {
            query("DELETE FROM yunxi_conversations WHERE id = $1")
                .bind(candidate.into_uuid())
                .execute(&mut **transaction)
                .await
                .map_err(IdentityStoreError::storage)?;
        }
        if !owner_lock::lock_and_owner_exists(transaction, DurableOwner::Conversation(winner))
            .await
            .map_err(IdentityStoreError::storage)?
        {
            return Err(IdentityStoreError::storage(std::io::Error::other(
                "canonical conversation disappeared while its route was being created",
            )));
        }
        let stored_kind =
            query_scalar::<Postgres, String>("SELECT kind FROM yunxi_conversations WHERE id = $1")
                .bind(winner)
                .fetch_one(&mut **transaction)
                .await
                .map_err(IdentityStoreError::storage)?;
        ensure_kind(external.kind(), parse_stored_kind(&stored_kind)?)?;
        Ok(ConversationId::from_uuid(winner))
    }

    /// Resolve a direct conversation only while its canonical Person still
    /// exists, and persist membership in the same lock-ordered transaction.
    pub(crate) async fn resolve_direct_for_person(
        &self,
        person_id: PersonId,
        external: &ExternalConversation,
    ) -> Result<ConversationId, IdentityStoreError> {
        if external.kind() != ConversationKind::Direct {
            return Err(IdentityStoreError::ConversationKindMismatch {
                requested: ConversationKind::Direct,
                stored: external.kind(),
            });
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        if !owner_lock::lock_and_owner_exists(
            &mut transaction,
            DurableOwner::Person(person_id.into_uuid()),
        )
        .await
        .map_err(IdentityStoreError::storage)?
        {
            return Err(IdentityStoreError::storage(std::io::Error::other(
                "canonical person disappeared before direct conversation resolution",
            )));
        }
        let conversation_id =
            Self::resolve_conversation_in_transaction(&mut transaction, external).await?;
        let has_other_member = query_scalar::<Postgres, bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM yunxi_conversation_members
                 WHERE conversation_id = $1 AND person_id <> $2
             )",
        )
        .bind(conversation_id.into_uuid())
        .bind(person_id.into_uuid())
        .fetch_one(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        if has_other_member {
            return Err(IdentityStoreError::storage(std::io::Error::other(
                "direct conversation already belongs to another canonical person",
            )));
        }
        query(
            "INSERT INTO yunxi_conversation_members (conversation_id, person_id)
             VALUES ($1, $2)
             ON CONFLICT (conversation_id, person_id) DO UPDATE SET updated_at = NOW()",
        )
        .bind(conversation_id.into_uuid())
        .bind(person_id.into_uuid())
        .execute(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(conversation_id)
    }

    /// Resolve a shared conversation route. Direct routes require
    /// [`Self::resolve_direct_for_person`] so they cannot bypass Person-domain
    /// deletion and membership persistence.
    pub(crate) async fn resolve_conversation(
        &self,
        external: &ExternalConversation,
    ) -> Result<ConversationId, IdentityStoreError> {
        if external.kind() == ConversationKind::Direct {
            return Err(IdentityStoreError::storage(std::io::Error::other(
                "direct conversation resolution requires a canonical Person owner",
            )));
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        owner_lock::lock_memory_maintenance(&mut transaction)
            .await
            .map_err(IdentityStoreError::storage)?;
        let conversation_id =
            Self::resolve_conversation_in_transaction(&mut transaction, external).await?;
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(conversation_id)
    }
}

impl IdentityStore for PostgresIdentityStore {
    fn resolve_external_identity<'a>(
        &'a self,
        external: &'a ExternalIdentity,
    ) -> IdentityStoreFuture<'a, PersonId> {
        Box::pin(async move { self.resolve_identity(external).await })
    }

    fn resolve_external_conversation<'a>(
        &'a self,
        external: &'a ExternalConversation,
    ) -> IdentityStoreFuture<'a, ConversationId> {
        Box::pin(async move { self.resolve_conversation(external).await })
    }
}

impl ConversationMemberStore for PostgresIdentityStore {
    fn upsert<'a>(
        &'a self,
        member: &'a ConversationMember,
    ) -> ConversationMemberStoreFuture<'a, ConversationMember> {
        Box::pin(async move {
            member
                .validate()
                .map_err(|error| ConversationMemberStoreError::InvalidRequest {
                    reason: error.to_string(),
                })?;
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(ConversationMemberStoreError::storage)?;
            if !owner_lock::lock_and_owner_exists(
                &mut transaction,
                DurableOwner::Person(member.person_id().into_uuid()),
            )
            .await
            .map_err(ConversationMemberStoreError::storage)?
            {
                return Err(ConversationMemberStoreError::InvalidRequest {
                    reason: "canonical person does not exist".to_owned(),
                });
            }
            if !owner_lock::lock_and_owner_exists(
                &mut transaction,
                DurableOwner::Conversation(member.conversation_id().into_uuid()),
            )
            .await
            .map_err(ConversationMemberStoreError::storage)?
            {
                return Err(ConversationMemberStoreError::InvalidRequest {
                    reason: "canonical conversation does not exist".to_owned(),
                });
            }
            query(
                "INSERT INTO yunxi_conversation_members
                    (conversation_id, person_id, role)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (conversation_id, person_id) DO UPDATE SET
                    role = EXCLUDED.role, updated_at = NOW()",
            )
            .bind(member.conversation_id().into_uuid())
            .bind(member.person_id().into_uuid())
            .bind(member.role())
            .execute(&mut *transaction)
            .await
            .map_err(ConversationMemberStoreError::storage)?;
            transaction
                .commit()
                .await
                .map_err(ConversationMemberStoreError::storage)?;
            Ok(member.clone())
        })
    }

    fn get(
        &self,
        conversation_id: ConversationId,
        person_id: PersonId,
    ) -> ConversationMemberStoreFuture<'_, Option<ConversationMember>> {
        Box::pin(async move {
            let row = query(
                "SELECT role FROM yunxi_conversation_members
                 WHERE conversation_id = $1 AND person_id = $2",
            )
            .bind(conversation_id.into_uuid())
            .bind(person_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(ConversationMemberStoreError::storage)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let role = row
                .try_get::<Option<String>, _>("role")
                .map_err(ConversationMemberStoreError::storage)?;
            let member = ConversationMember::new(conversation_id, person_id)
                .with_role(role)
                .map_err(|error| ConversationMemberStoreError::InvalidRequest {
                    reason: error.to_string(),
                })?;
            Ok(Some(member))
        })
    }

    fn list(
        &self,
        conversation_id: ConversationId,
        limit: usize,
    ) -> ConversationMemberStoreFuture<'_, Vec<ConversationMember>> {
        Box::pin(async move {
            let limit = limit.min(256);
            if limit == 0 {
                return Ok(Vec::new());
            }
            let rows = query(
                "SELECT person_id, role FROM yunxi_conversation_members
                 WHERE conversation_id = $1 ORDER BY updated_at DESC, person_id LIMIT $2",
            )
            .bind(conversation_id.into_uuid())
            .bind(i64::try_from(limit).unwrap_or(256))
            .fetch_all(&self.pool)
            .await
            .map_err(ConversationMemberStoreError::storage)?;
            rows.into_iter()
                .map(|row| {
                    let person_id = row
                        .try_get::<Uuid, _>("person_id")
                        .map_err(ConversationMemberStoreError::storage)?;
                    let role = row
                        .try_get::<Option<String>, _>("role")
                        .map_err(ConversationMemberStoreError::storage)?;
                    ConversationMember::new(conversation_id, PersonId::from_uuid(person_id))
                        .with_role(role)
                        .map_err(|error| ConversationMemberStoreError::InvalidRequest {
                            reason: error.to_string(),
                        })
                })
                .collect()
        })
    }

    fn remove(
        &self,
        conversation_id: ConversationId,
        person_id: PersonId,
    ) -> ConversationMemberStoreFuture<'_, bool> {
        Box::pin(async move {
            let result = query(
                "DELETE FROM yunxi_conversation_members
                 WHERE conversation_id = $1 AND person_id = $2",
            )
            .bind(conversation_id.into_uuid())
            .bind(person_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(ConversationMemberStoreError::storage)?;
            Ok(result.rows_affected() != 0)
        })
    }
}

fn portable_error(message: impl Into<String>) -> IdentityStoreError {
    IdentityStoreError::storage(std::io::Error::other(message.into()))
}

fn portable_probe_limit(maximum: usize) -> i64 {
    i64::try_from(maximum.saturating_add(1)).unwrap_or(i64::MAX)
}

fn ensure_portable_bound(
    field: &str,
    actual: usize,
    maximum: usize,
) -> Result<(), IdentityStoreError> {
    if actual > maximum {
        return Err(portable_error(format!(
            "portable person has {actual} {field}, above maximum {maximum}"
        )));
    }
    Ok(())
}

fn validate_portable_export(export: &PortablePersonExport) -> Result<(), IdentityStoreError> {
    if export.version != PORTABLE_PERSON_EXPORT_VERSION {
        return Err(portable_error(format!(
            "unsupported Yunxi person export version {}",
            export.version
        )));
    }
    ensure_portable_bound(
        "external identities",
        export.external_identities.len(),
        MAX_PORTABLE_EXTERNAL_IDENTITIES,
    )?;
    ensure_portable_bound("memories", export.memories.len(), MAX_PORTABLE_MEMORIES)?;
    ensure_portable_bound(
        "open loops",
        export.open_loops.len(),
        MAX_PORTABLE_OPEN_LOOPS,
    )?;
    ensure_portable_bound("goals", export.goals.len(), MAX_PORTABLE_GOALS)?;

    let mut external_identities = HashSet::with_capacity(export.external_identities.len());
    for external in &export.external_identities {
        let platform = PlatformId::new(external.platform.clone())
            .map_err(|error| portable_error(format!("invalid portable platform: {error}")))?;
        ExternalIdentity::new(platform, external.external_id.clone())
            .map_err(|error| portable_error(format!("invalid portable identity: {error}")))?;
        if !external_identities.insert((&external.platform, &external.external_id)) {
            return Err(portable_error(
                "portable person contains a duplicate external identity",
            ));
        }
    }

    let mut memory_ids = HashSet::with_capacity(export.memories.len());
    for memory in &export.memories {
        if memory.scope() != MemoryScope::Person(export.person_id) {
            return Err(portable_error(
                "portable memory scope does not match exported person",
            ));
        }
        let draft = MemoryDraft::new(
            memory.scope(),
            memory.kind(),
            memory.content(),
            memory.occurred_at(),
        )
        .and_then(|draft| draft.with_importance(memory.importance()))
        .and_then(|draft| draft.with_tags(memory.tags().iter().cloned()))
        .map_err(|error| portable_error(format!("portable memory is invalid: {error}")))?;
        let validated = Memory::from_draft(memory.id(), &draft, memory.created_at())
            .map_err(|error| portable_error(format!("portable memory is invalid: {error}")))?;
        if validated != *memory {
            return Err(portable_error("portable memory is not in canonical form"));
        }
        if !memory_ids.insert(memory.id()) {
            return Err(portable_error(
                "portable person contains a duplicate memory ID",
            ));
        }
    }

    if let Some(relation) = export.relation {
        if relation.person_id != export.person_id {
            return Err(portable_error(
                "portable relation person does not match export",
            ));
        }
        relation
            .validate()
            .map_err(|error| portable_error(format!("portable relation is invalid: {error}")))?;
    }
    if let Some(affect) = export.affect {
        affect
            .validate()
            .map_err(|error| portable_error(format!("portable affect is invalid: {error}")))?;
    }

    let mut open_loop_ids = HashSet::with_capacity(export.open_loops.len());
    for item in &export.open_loops {
        if item.owner() != OpenLoopOwner::Person(export.person_id) {
            return Err(portable_error(
                "portable open-loop owner does not match exported person",
            ));
        }
        validate_portable_open_loop(item)?;
        if !open_loop_ids.insert(item.id()) {
            return Err(portable_error(
                "portable person contains a duplicate open-loop ID",
            ));
        }
    }

    let mut goal_ids = HashSet::with_capacity(export.goals.len());
    for goal in &export.goals {
        if goal.owner() != GoalOwner::Person(export.person_id) {
            return Err(portable_error(
                "portable goal owner does not match exported person",
            ));
        }
        validate_portable_goal(goal)?;
        if !goal_ids.insert(goal.id()) {
            return Err(portable_error(
                "portable person contains a duplicate goal ID",
            ));
        }
    }
    Ok(())
}

fn row_to_portable_memory(row: &sqlx_postgres::PgRow) -> Result<Memory, IdentityStoreError> {
    let scope_kind = row
        .try_get::<String, _>("scope_kind")
        .map_err(IdentityStoreError::storage)?;
    let scope_id = row
        .try_get::<Option<Uuid>, _>("scope_id")
        .map_err(IdentityStoreError::storage)?;
    let scope = match (scope_kind.as_str(), scope_id) {
        ("person", Some(id)) => MemoryScope::Person(PersonId::from_uuid(id)),
        ("conversation", Some(id)) => MemoryScope::Conversation(ConversationId::from_uuid(id)),
        ("global", None) => MemoryScope::Global,
        _ => return Err(portable_error("stored memory scope is invalid")),
    };
    let kind = MemoryKind::from_str(
        &row.try_get::<String, _>("kind")
            .map_err(IdentityStoreError::storage)?,
    )
    .map_err(|error| portable_error(format!("stored memory kind is invalid: {error}")))?;
    let tags = serde_json::from_value::<Vec<String>>(
        row.try_get::<serde_json::Value, _>("tags")
            .map_err(IdentityStoreError::storage)?,
    )
    .map_err(|error| portable_error(format!("stored memory tags are invalid: {error}")))?;
    let importance = row
        .try_get::<i16, _>("importance")
        .map_err(IdentityStoreError::storage)?;
    let importance = u8::try_from(importance)
        .map_err(|_| portable_error("stored memory importance is invalid"))?;
    let draft = MemoryDraft::new(
        scope,
        kind,
        row.try_get::<String, _>("content")
            .map_err(IdentityStoreError::storage)?,
        row.try_get::<DateTime<Utc>, _>("occurred_at")
            .map_err(IdentityStoreError::storage)?,
    )
    .and_then(|draft| draft.with_importance(importance))
    .and_then(|draft| draft.with_tags(tags))
    .map_err(|error| portable_error(format!("stored memory is invalid: {error}")))?;
    Memory::from_draft(
        MemoryId::from_uuid(
            row.try_get::<Uuid, _>("id")
                .map_err(IdentityStoreError::storage)?,
        ),
        &draft,
        row.try_get::<DateTime<Utc>, _>("created_at")
            .map_err(IdentityStoreError::storage)?,
    )
    .map_err(|error| portable_error(format!("stored memory is invalid: {error}")))
}

fn row_to_portable_relation(
    row: &sqlx_postgres::PgRow,
    person_id: PersonId,
) -> Result<RelationState, IdentityStoreError> {
    let state = RelationState {
        person_id,
        familiarity: row
            .try_get::<f64, _>("familiarity")
            .map_err(IdentityStoreError::storage)? as f32,
        affinity: row
            .try_get::<f64, _>("affinity")
            .map_err(IdentityStoreError::storage)? as f32,
        trust: row
            .try_get::<f64, _>("trust")
            .map_err(IdentityStoreError::storage)? as f32,
        comfort: row
            .try_get::<f64, _>("comfort")
            .map_err(IdentityStoreError::storage)? as f32,
        tension: row
            .try_get::<f64, _>("tension")
            .map_err(IdentityStoreError::storage)? as f32,
    };
    state
        .validate()
        .map_err(|error| portable_error(format!("stored relation is invalid: {error}")))?;
    Ok(state)
}

fn row_to_portable_affect(row: &sqlx_postgres::PgRow) -> Result<AffectState, IdentityStoreError> {
    let state = AffectState {
        valence: row
            .try_get::<f64, _>("valence")
            .map_err(IdentityStoreError::storage)? as f32,
        arousal: row
            .try_get::<f64, _>("arousal")
            .map_err(IdentityStoreError::storage)? as f32,
        social_energy: row
            .try_get::<f64, _>("social_energy")
            .map_err(IdentityStoreError::storage)? as f32,
        curiosity: row
            .try_get::<f64, _>("curiosity")
            .map_err(IdentityStoreError::storage)? as f32,
    };
    state
        .validate()
        .map_err(|error| portable_error(format!("stored affect is invalid: {error}")))?;
    Ok(state)
}

fn row_to_portable_open_loop(row: &sqlx_postgres::PgRow) -> Result<OpenLoop, IdentityStoreError> {
    let owner_kind = row
        .try_get::<String, _>("owner_kind")
        .map_err(IdentityStoreError::storage)?;
    let owner_id = row
        .try_get::<Option<Uuid>, _>("owner_id")
        .map_err(IdentityStoreError::storage)?;
    let owner = match (owner_kind.as_str(), owner_id) {
        ("person", Some(id)) => OpenLoopOwner::Person(PersonId::from_uuid(id)),
        ("conversation", Some(id)) => OpenLoopOwner::Conversation(ConversationId::from_uuid(id)),
        ("global", None) => OpenLoopOwner::Global,
        _ => return Err(portable_error("stored open-loop owner is invalid")),
    };
    let kind = OpenLoopKind::from_str(
        &row.try_get::<String, _>("kind")
            .map_err(IdentityStoreError::storage)?,
    )
    .map_err(|error| portable_error(format!("stored open-loop kind is invalid: {error}")))?;
    let status = OpenLoopStatus::from_str(
        &row.try_get::<String, _>("status")
            .map_err(IdentityStoreError::storage)?,
    )
    .map_err(|error| portable_error(format!("stored open-loop status is invalid: {error}")))?;
    let salience = row
        .try_get::<i16, _>("salience")
        .map_err(IdentityStoreError::storage)?;
    let salience = u8::try_from(salience)
        .map_err(|_| portable_error("stored open-loop salience is invalid"))?;
    let version = row
        .try_get::<i64, _>("version")
        .map_err(IdentityStoreError::storage)?;
    let version = u64::try_from(version)
        .map_err(|_| portable_error("stored open-loop version is invalid"))?;
    let item = OpenLoop::restore(
        row.try_get::<Uuid, _>("id")
            .map_err(IdentityStoreError::storage)?
            .into(),
        owner,
        kind,
        row.try_get::<String, _>("summary")
            .map_err(IdentityStoreError::storage)?,
        row.try_get::<Option<Uuid>, _>("source_message_id")
            .map_err(IdentityStoreError::storage)?
            .map(Into::into),
        row.try_get::<Option<DateTime<Utc>>, _>("due_at")
            .map_err(IdentityStoreError::storage)?,
        row.try_get::<Option<DateTime<Utc>>, _>("expires_at")
            .map_err(IdentityStoreError::storage)?,
        salience,
        status,
        row.try_get::<DateTime<Utc>, _>("created_at")
            .map_err(IdentityStoreError::storage)?,
        row.try_get::<DateTime<Utc>, _>("updated_at")
            .map_err(IdentityStoreError::storage)?,
        row.try_get::<Option<DateTime<Utc>>, _>("resolved_at")
            .map_err(IdentityStoreError::storage)?,
        row.try_get::<Option<DateTime<Utc>>, _>("triggered_at")
            .map_err(IdentityStoreError::storage)?,
        version,
        row.try_get::<Option<String>, _>("dedupe_key")
            .map_err(IdentityStoreError::storage)?,
    )
    .map_err(|error| portable_error(format!("stored open loop is invalid: {error}")))?;
    validate_portable_open_loop(&item)?;
    Ok(item)
}

fn validate_portable_open_loop(item: &OpenLoop) -> Result<(), IdentityStoreError> {
    let restored = OpenLoop::restore(
        item.id(),
        item.owner(),
        item.kind(),
        item.summary(),
        item.source_message_id(),
        item.due_at(),
        item.expires_at(),
        item.salience(),
        item.status(),
        item.created_at(),
        item.updated_at(),
        item.resolved_at(),
        item.triggered_at(),
        item.version(),
        item.dedupe_key().map(str::to_owned),
    )
    .map_err(|error| portable_error(format!("portable open loop is invalid: {error}")))?;
    if restored != *item
        || item.updated_at() < item.created_at()
        || item
            .resolved_at()
            .is_some_and(|at| at < item.created_at() || at > item.updated_at())
        || item
            .triggered_at()
            .is_some_and(|at| at < item.created_at() || at > item.updated_at())
        || item.version() > i64::MAX as u64
    {
        return Err(portable_error(
            "portable open-loop lifecycle state is invalid",
        ));
    }
    Ok(())
}

fn portable_goal_kind_name(kind: GoalKind) -> &'static str {
    match kind {
        GoalKind::Personal => "personal",
        GoalKind::Conversation => "conversation",
        GoalKind::FollowUp => "follow_up",
        GoalKind::Project => "project",
        GoalKind::System => "system",
    }
}

fn parse_portable_goal_kind(value: &str) -> Result<GoalKind, IdentityStoreError> {
    match value {
        "personal" => Ok(GoalKind::Personal),
        "conversation" => Ok(GoalKind::Conversation),
        "follow_up" => Ok(GoalKind::FollowUp),
        "project" => Ok(GoalKind::Project),
        "system" => Ok(GoalKind::System),
        _ => Err(portable_error("stored goal kind is invalid")),
    }
}

fn portable_goal_state_name(state: GoalState) -> &'static str {
    match state {
        GoalState::Active => "active",
        GoalState::Paused => "paused",
        GoalState::Completed => "completed",
        GoalState::Cancelled => "cancelled",
    }
}

fn parse_portable_goal_state(value: &str) -> Result<GoalState, IdentityStoreError> {
    match value {
        "active" => Ok(GoalState::Active),
        "paused" => Ok(GoalState::Paused),
        "completed" => Ok(GoalState::Completed),
        "cancelled" => Ok(GoalState::Cancelled),
        _ => Err(portable_error("stored goal state is invalid")),
    }
}

fn row_to_portable_goal(row: &sqlx_postgres::PgRow) -> Result<Goal, IdentityStoreError> {
    let owner_kind = row
        .try_get::<String, _>("owner_kind")
        .map_err(IdentityStoreError::storage)?;
    let owner_id = row
        .try_get::<Option<Uuid>, _>("owner_id")
        .map_err(IdentityStoreError::storage)?;
    let owner = match (owner_kind.as_str(), owner_id) {
        ("person", Some(id)) => GoalOwner::Person(PersonId::from_uuid(id)),
        ("conversation", Some(id)) => GoalOwner::Conversation(ConversationId::from_uuid(id)),
        ("global", None) => GoalOwner::Global,
        _ => return Err(portable_error("stored goal owner is invalid")),
    };
    let kind = parse_portable_goal_kind(
        &row.try_get::<String, _>("kind")
            .map_err(IdentityStoreError::storage)?,
    )?;
    let state = parse_portable_goal_state(
        &row.try_get::<String, _>("state")
            .map_err(IdentityStoreError::storage)?,
    )?;
    let goal = serde_json::from_value::<Goal>(serde_json::json!({
        "id": row.try_get::<Uuid, _>("id").map_err(IdentityStoreError::storage)?,
        "owner": owner,
        "kind": kind,
        "title": row.try_get::<String, _>("title").map_err(IdentityStoreError::storage)?,
        "details": row.try_get::<Option<String>, _>("details").map_err(IdentityStoreError::storage)?,
        "state": state,
        "due_at": row.try_get::<Option<DateTime<Utc>>, _>("due_at").map_err(IdentityStoreError::storage)?,
        "created_at": row.try_get::<DateTime<Utc>, _>("created_at").map_err(IdentityStoreError::storage)?,
        "updated_at": row.try_get::<DateTime<Utc>, _>("updated_at").map_err(IdentityStoreError::storage)?,
        "completed_at": row.try_get::<Option<DateTime<Utc>>, _>("completed_at").map_err(IdentityStoreError::storage)?,
    }))
    .map_err(|error| portable_error(format!("stored goal is invalid: {error}")))?;
    validate_portable_goal(&goal)?;
    Ok(goal)
}

fn validate_portable_goal(goal: &Goal) -> Result<(), IdentityStoreError> {
    goal.validate()
        .map_err(|error| portable_error(format!("portable goal is invalid: {error}")))?;
    if goal.updated_at() < goal.created_at()
        || (goal.state() == GoalState::Completed) != goal.completed_at().is_some()
        || goal
            .completed_at()
            .is_some_and(|at| at < goal.created_at() || at > goal.updated_at())
    {
        return Err(portable_error("portable goal lifecycle state is invalid"));
    }
    Ok(())
}

fn parse_stored_kind(value: &str) -> Result<ConversationKind, IdentityStoreError> {
    ConversationKind::from_str(value).map_err(IdentityStoreError::storage)
}

fn ensure_kind(
    requested: ConversationKind,
    stored: ConversationKind,
) -> Result<(), IdentityStoreError> {
    if requested != stored {
        return Err(IdentityStoreError::ConversationKindMismatch { requested, stored });
    }
    Ok(())
}

fn ensure_bounded_qq_aliases(mut aliases: Vec<String>) -> Result<Vec<String>, IdentityStoreError> {
    if aliases.len() > MAX_QQ_PERSON_ALIASES {
        return Err(IdentityStoreError::storage(std::io::Error::other(
            "QQ person has too many external identity aliases to erase safely",
        )));
    }
    aliases.sort_unstable();
    aliases.dedup();
    Ok(aliases)
}

fn is_qq_direct_for_identity(
    external_identity: &ExternalIdentity,
    direct_conversation: &ExternalConversation,
) -> bool {
    if external_identity.platform().as_str() != "qq"
        || direct_conversation.platform().as_str() != "qq"
        || direct_conversation.kind() != ConversationKind::Direct
    {
        return false;
    }
    let mut parts = direct_conversation.external_id().split(':');
    let valid = matches!(parts.next(), Some("direct"))
        && parts
            .next()
            .and_then(|value| value.parse::<i64>().ok())
            .is_some_and(|value| value > 0)
        && parts.next() == Some(external_identity.external_id())
        && parts.next().is_none();
    valid
        && external_identity
            .external_id()
            .parse::<i64>()
            .is_ok_and(|value| value > 0)
}

#[cfg(test)]
mod tests {
    use super::{
        ExternalConversationRoute, PortableExternalIdentity, PortablePersonExport,
        PostgresIdentityStore,
    };
    use crate::memory::MemoryManager;
    use crate::yunxi::affect_store::PostgresAffectStore;
    use crate::yunxi::delivery_ledger::PostgresDeliveryLedger;
    use crate::yunxi::goal_store::PostgresGoalStore;
    use crate::yunxi::memory_store::PostgresMemoryStore;
    use crate::yunxi::open_loop_store::PostgresOpenLoopStore;
    use crate::yunxi::qq;
    use crate::yunxi::relation_store::PostgresRelationStore;
    use chrono::Utc;
    use sqlx_core::error::DatabaseError;
    use sqlx_core::query::query;
    use sqlx_core::query_scalar::query_scalar;
    use sqlx_core::row::Row;
    use sqlx_postgres::{PgPool, PgPoolOptions, Postgres};
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;
    use yunxi_core::{
        ConversationKind, ExternalConversation, ExternalIdentity, GoalDraft, GoalKind, GoalOwner,
        GoalStore, GoalStoreError, IdentityStoreError, MemoryDraft, MemoryKind, MemoryScope,
        MemoryStore, MemoryStoreError, OpenLoopDraft, OpenLoopKind, OpenLoopOwner, OpenLoopStore,
        OpenLoopStoreError, PersonId, PlatformId,
    };

    async fn initialize_portable_schema(pool: &PgPool) -> PostgresIdentityStore {
        let identities = PostgresIdentityStore::new(pool.clone());
        identities
            .initialize_schema()
            .await
            .expect("应初始化 identity schema");
        PostgresMemoryStore::new(
            Arc::clone(&crate::memory::MEMORY_MANAGER),
            Arc::new(identities.clone()),
            pool.clone(),
        )
        .initialize_schema()
        .await
        .expect("应初始化 memory schema");
        PostgresRelationStore::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 relation schema");
        PostgresAffectStore::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 affect schema");
        PostgresOpenLoopStore::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 open-loop schema");
        PostgresGoalStore::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 goal schema");
        identities
    }

    async fn initialize_delivery_ledger(pool: &PgPool) {
        PostgresDeliveryLedger::new(pool.clone())
            .initialize_schema()
            .await
            .expect("应初始化 delivery ledger schema");
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_delivery_fixture(
        pool: &PgPool,
        delivery_key: &str,
        action_kind: &str,
        target_kind: &str,
        target_id: Uuid,
        conversation_id: Uuid,
        destination_kind: &str,
        destination_id: i64,
    ) {
        query(
            "INSERT INTO yunxi_action_delivery_ledger
                (delivery_key, envelope_fingerprint, action_kind, target_kind, target_id,
                 conversation_id, destination_kind, destination_id, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'sent')",
        )
        .bind(delivery_key)
        .bind(vec![7_u8; 32])
        .bind(action_kind)
        .bind(target_kind)
        .bind(target_id)
        .bind(conversation_id)
        .bind(destination_kind)
        .bind(destination_id)
        .execute(pool)
        .await
        .expect("应创建 delivery ledger fixture");
    }

    #[test]
    fn portable_person_old_json_defaults_new_state_domains() {
        let person_id = PersonId::new();
        let export: PortablePersonExport = serde_json::from_value(serde_json::json!({
            "version": 1,
            "person_id": person_id,
        }))
        .expect("旧快照缺少新增字段时应使用空状态");

        assert!(export.external_identities.is_empty());
        assert!(export.memories.is_empty());
        assert!(export.relation.is_none());
        assert!(export.affect.is_none());
        assert!(export.open_loops.is_empty());
        assert!(export.goals.is_empty());
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_identity_mapping_is_stable_and_race_safe() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(16)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = Arc::new(PostgresIdentityStore::new(pool.clone()));
                store.initialize_schema().await.expect("应初始化 schema");
                store
                    .initialize_schema()
                    .await
                    .expect("重复初始化应安全");

                let platform = PlatformId::new("phase1test").expect("valid platform");
                let suffix = Uuid::new_v4().to_string();
                let identity = ExternalIdentity::new(
                    platform.clone(),
                    format!("race-person:{suffix}"),
                )
                .expect("valid identity");
                let mut tasks = Vec::new();
                for _ in 0..16 {
                    let store = Arc::clone(&store);
                    let identity = identity.clone();
                    tasks.push(kovi::tokio::spawn(async move {
                        store.resolve_identity(&identity).await
                    }));
                }
                let mut winners = Vec::new();
                for task in tasks {
                    winners.push(
                        task.await
                            .expect("resolver task should join")
                            .expect("resolution should succeed"),
                    );
                }
                assert!(winners.iter().all(|winner| *winner == winners[0]));
                // Only inspect this test's identity mapping. A global person count
                // is inherently racy when PostgreSQL ignored tests run in parallel.
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_external_identities
                         WHERE platform = $1 AND external_id = $2",
                    )
                    .bind(platform.as_str())
                    .bind(identity.external_id())
                    .fetch_one(&pool)
                    .await
                    .expect("应读取 identity mapping 数量"),
                    1,
                    "并发 resolver 只能保留一个 identity mapping"
                );

                let other_identity = ExternalIdentity::new(
                    platform.clone(),
                    format!("other-person:{suffix}"),
                )
                .expect("valid identity");
                let other_person = store
                    .resolve_identity(&other_identity)
                    .await
                    .expect("other identity should resolve");
                assert_ne!(winners[0], other_person);
                let restarted = PostgresIdentityStore::new(pool.clone());
                assert_eq!(
                    restarted
                        .resolve_identity(&identity)
                        .await
                        .expect("mapping should survive a new store"),
                    winners[0]
                );

                let group = ExternalConversation::new(
                    platform.clone(),
                    format!("group:{suffix}"),
                    ConversationKind::Group,
                )
                .expect("valid group");
                let race_conversation = ExternalConversation::new(
                    platform.clone(),
                    format!("race-conversation:{suffix}"),
                    ConversationKind::Group,
                )
                .expect("valid race conversation");
                let mut conversation_tasks = Vec::new();
                for _ in 0..16 {
                    let store = Arc::clone(&store);
                    let race_conversation = race_conversation.clone();
                    conversation_tasks.push(kovi::tokio::spawn(async move {
                        store.resolve_conversation(&race_conversation).await
                    }));
                }
                let mut conversation_winners = Vec::new();
                for task in conversation_tasks {
                    conversation_winners.push(
                        task.await
                            .expect("conversation resolver task should join")
                            .expect("conversation resolution should succeed"),
                    );
                }
                assert!(
                    conversation_winners
                        .iter()
                        .all(|winner| *winner == conversation_winners[0])
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_external_conversations
                         WHERE platform = $1 AND external_id = $2",
                    )
                    .bind(platform.as_str())
                    .bind(race_conversation.external_id())
                    .fetch_one(&pool)
                    .await
                    .expect("应读取 conversation mapping 数量"),
                    1,
                    "并发 resolver 只能保留一个 conversation mapping"
                );
                let direct = ExternalConversation::new(
                    platform.clone(),
                    format!("direct:{suffix}"),
                    ConversationKind::Direct,
                )
                .expect("valid direct");
                let group_id = store
                    .resolve_conversation(&group)
                    .await
                    .expect("group should resolve");
                assert_eq!(
                    restarted
                        .resolve_conversation(&group)
                        .await
                        .expect("group mapping should survive a new store"),
                    group_id
                );
                let direct_id = store
                    .resolve_direct_for_person(winners[0], &direct)
                    .await
                    .expect("direct should resolve");
                assert_ne!(group_id, direct_id);
                let wrong_kind = ExternalConversation::new(
                    platform.clone(),
                    group.external_id(),
                    ConversationKind::Direct,
                )
                .expect("valid conflicting reference");
                assert!(matches!(
                    store.resolve_conversation(&wrong_kind).await,
                    Err(IdentityStoreError::Storage { .. })
                ));
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_external_conversations
                         WHERE platform = $1 AND external_id = ANY($2)",
                    )
                    .bind(platform.as_str())
                    .bind(vec![
                        group.external_id(),
                        direct.external_id(),
                        race_conversation.external_id(),
                    ])
                    .fetch_one(&pool)
                    .await
                    .expect("应读取 conversation mapping 数量"),
                    3
                );

                let mut transaction = pool.begin().await.expect("应开始唯一约束事务");
                let first = Uuid::new_v4();
                let second = Uuid::new_v4();
                query("INSERT INTO yunxi_persons (id) VALUES ($1), ($2)")
                    .bind(first)
                    .bind(second)
                    .execute(&mut *transaction)
                    .await
                    .expect("应创建约束测试 person");
                let duplicate_key = format!("duplicate:{suffix}");
                query(
                    "INSERT INTO yunxi_external_identities (platform, external_id, person_id) VALUES ($1, $2, $3)",
                )
                .bind(platform.as_str())
                .bind(&duplicate_key)
                .bind(first)
                .execute(&mut *transaction)
                .await
                .expect("第一次映射应成功");
                let duplicate_error = query(
                    "INSERT INTO yunxi_external_identities (platform, external_id, person_id) VALUES ($1, $2, $3)",
                )
                .bind(platform.as_str())
                .bind(&duplicate_key)
                .bind(second)
                .execute(&mut *transaction)
                .await
                .expect_err("重复 external identity 必须触发唯一约束");
                assert_eq!(
                    duplicate_error
                        .as_database_error()
                        .and_then(DatabaseError::code)
                        .as_deref(),
                    Some("23505")
                );
                transaction.rollback().await.expect("应回滚约束测试");

                query(
                    "DELETE FROM yunxi_external_identities WHERE platform = $1 AND external_id = ANY($2)",
                )
                .bind(platform.as_str())
                .bind(vec![identity.external_id(), other_identity.external_id()])
                .execute(&pool)
                .await
                .expect("应清理 identity mappings");
                query("DELETE FROM yunxi_persons WHERE id = ANY($1)")
                    .bind(vec![winners[0].into_uuid(), other_person.into_uuid()])
                    .execute(&pool)
                    .await
                    .expect("应清理 persons");
                query(
                    "DELETE FROM yunxi_external_conversations WHERE platform = $1 AND external_id = ANY($2)",
                )
                .bind(platform.as_str())
                .bind(vec![
                    group.external_id(),
                    direct.external_id(),
                    race_conversation.external_id(),
                ])
                .execute(&pool)
                .await
                .expect("应清理 conversation mappings");
                query("DELETE FROM yunxi_conversations WHERE id = ANY($1)")
                    .bind(vec![
                        group_id.into_uuid(),
                        direct_id.into_uuid(),
                        conversation_winners[0].into_uuid(),
                    ])
                    .execute(&pool)
                    .await
                    .expect("应清理 conversations");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_person_domain_deletion_removes_core_data_transactionally() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = Arc::new(PostgresIdentityStore::new(pool.clone()));
                store
                    .initialize_schema()
                    .await
                    .expect("应初始化身份 schema");
                initialize_delivery_ledger(&pool).await;
                PostgresOpenLoopStore::new(pool.clone())
                    .initialize_schema()
                    .await
                    .expect("应初始化 open-loop schema");
                PostgresMemoryStore::new(
                    Arc::clone(&crate::memory::MEMORY_MANAGER),
                    Arc::clone(&store),
                    pool.clone(),
                )
                .initialize_schema()
                .await
                .expect("应初始化 memory schema");
                PostgresAffectStore::new(pool.clone())
                    .initialize_schema()
                    .await
                    .expect("应初始化 affect schema");
                PostgresRelationStore::new(pool.clone())
                    .initialize_schema()
                    .await
                    .expect("应初始化 relation schema");
                crate::yunxi::affect_store::PostgresAffectStore::new(pool.clone())
                    .initialize_schema()
                    .await
                    .expect("应初始化 affect schema");
                PostgresGoalStore::new(pool.clone())
                    .initialize_schema()
                    .await
                    .expect("应初始化 goal schema");

                let suffix = (Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
                let user_id = 1_000_000_000_000_i64 + suffix;
                let alias_user_id = 1_500_000_000_000_i64 + suffix;
                let first_bot_id = 2_000_000_000_000_i64 + suffix;
                let second_bot_id = 3_000_000_000_000_i64 + suffix;
                let group_external_id = 4_000_000_000_000_i64 + suffix;
                let identity = qq::person(user_id).expect("valid deletion identity");
                let alias_identity = qq::person(alias_user_id).expect("valid alias identity");
                let first_direct =
                    qq::direct(first_bot_id, user_id).expect("valid first direct conversation");
                let second_direct =
                    qq::direct(second_bot_id, user_id).expect("valid second direct conversation");
                let alias_direct = qq::direct(first_bot_id, alias_user_id)
                    .expect("valid alias direct conversation");
                let group = qq::group(group_external_id).expect("valid group conversation");
                let matrix_direct = ExternalConversation::new(
                    PlatformId::new("matrix-test").expect("valid matrix platform"),
                    format!("direct:portable:{suffix}"),
                    ConversationKind::Direct,
                )
                .expect("valid cross-platform direct conversation");
                let person_id = store
                    .resolve_identity(&identity)
                    .await
                    .expect("identity should resolve");
                query(
                    "INSERT INTO yunxi_external_identities (platform, external_id, person_id)
                     VALUES ('qq', $1, $2)",
                )
                .bind(alias_identity.external_id())
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("alias should link to the canonical person");
                let first_conversation_id = store
                    .resolve_direct_for_person(person_id, &first_direct)
                    .await
                    .expect("first conversation should resolve");
                let second_conversation_id = store
                    .resolve_direct_for_person(person_id, &second_direct)
                    .await
                    .expect("second conversation should resolve");
                let alias_conversation_id = store
                    .resolve_direct_for_person(person_id, &alias_direct)
                    .await
                    .expect("alias conversation should resolve");
                let matrix_conversation_id = store
                    .resolve_direct_for_person(person_id, &matrix_direct)
                    .await
                    .expect("cross-platform direct conversation should resolve");
                let group_conversation_id = store
                    .resolve_conversation(&group)
                    .await
                    .expect("group conversation should resolve");
                let person_uuid = person_id.into_uuid();
                let first_conversation_uuid = first_conversation_id.into_uuid();
                let second_conversation_uuid = second_conversation_id.into_uuid();
                let alias_conversation_uuid = alias_conversation_id.into_uuid();
                let matrix_conversation_uuid = matrix_conversation_id.into_uuid();
                let group_conversation_uuid = group_conversation_id.into_uuid();

                for (scope_kind, scope_id) in [
                    ("person", person_uuid),
                    ("conversation", first_conversation_uuid),
                    ("conversation", second_conversation_uuid),
                    ("conversation", alias_conversation_uuid),
                    ("conversation", matrix_conversation_uuid),
                    ("conversation", group_conversation_uuid),
                ] {
                    query(
                        "INSERT INTO yunxi_memories
                            (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                         VALUES ($1, $2, $3, 'fact', 'deletion test', 50, '[]', NOW())",
                    )
                    .bind(Uuid::new_v4())
                    .bind(scope_kind)
                    .bind(scope_id)
                    .execute(&pool)
                    .await
                    .expect("应创建测试 memory");
                }
                for (owner_kind, owner_id) in [
                    ("person", person_uuid),
                    ("conversation", first_conversation_uuid),
                    ("conversation", second_conversation_uuid),
                    ("conversation", alias_conversation_uuid),
                    ("conversation", matrix_conversation_uuid),
                    ("conversation", group_conversation_uuid),
                ] {
                    query(
                        "INSERT INTO yunxi_open_loops
                            (id, owner_kind, owner_id, kind, summary, status)
                         VALUES ($1, $2, $3, 'follow_up', 'deletion test', 'open')",
                    )
                    .bind(Uuid::new_v4())
                    .bind(owner_kind)
                    .bind(owner_id)
                    .execute(&pool)
                    .await
                    .expect("应创建测试 open-loop");
                    query(
                        "INSERT INTO yunxi_goals
                            (id, owner_kind, owner_id, kind, title, state, created_at, updated_at)
                         VALUES ($1, $2, $3, 'personal', 'deletion test', 'active', NOW(), NOW())",
                    )
                    .bind(Uuid::new_v4())
                    .bind(owner_kind)
                    .bind(owner_id)
                    .execute(&pool)
                    .await
                    .expect("应创建测试 goal");
                }
                query("INSERT INTO yunxi_affect_states (person_id) VALUES ($1)")
                    .bind(person_uuid)
                    .execute(&pool)
                    .await
                    .expect("应创建测试 affect");
                query("INSERT INTO yunxi_relations (person_id) VALUES ($1)")
                    .bind(person_uuid)
                    .execute(&pool)
                    .await
                    .expect("应创建测试 relation");
                for (conversation_id, external_message_id) in [
                    (first_conversation_uuid, 1_i64),
                    (second_conversation_uuid, 2_i64),
                    (alias_conversation_uuid, 3_i64),
                    (matrix_conversation_uuid, 4_i64),
                    (group_conversation_uuid, 5_i64),
                ] {
                    query(
                        "INSERT INTO yunxi_message_mappings
                            (message_id, conversation_id, platform, external_message_id, direction)
                         VALUES ($1, $2, 'qq', $3, 'inbound')",
                    )
                    .bind(Uuid::new_v4())
                    .bind(conversation_id)
                    .bind(external_message_id)
                    .execute(&pool)
                    .await
                    .expect("应创建测试 message mapping");
                }
                let matrix_delivery_key = format!("cross-platform-delete:{suffix}");
                insert_delivery_fixture(
                    &pool,
                    &matrix_delivery_key,
                    "send_message",
                    "conversation",
                    matrix_conversation_uuid,
                    matrix_conversation_uuid,
                    "private",
                    user_id,
                )
                .await;

                let targets = store
                    .qq_person_domain_targets(user_id)
                    .await
                    .expect("alias erasure targets should resolve without creating data");
                assert_eq!(targets.person_id, Some(person_id));
                assert_eq!(targets.qq_user_ids, vec![user_id, alias_user_id]);
                let mut expected_conversation_ids = vec![
                    first_conversation_id,
                    second_conversation_id,
                    alias_conversation_id,
                ];
                expected_conversation_ids.sort_unstable();
                assert_eq!(targets.direct_conversation_ids, expected_conversation_ids);

                let deleted = store
                    .delete_person_domain_data(&identity, &first_direct)
                    .await
                    .expect("domain deletion should succeed");
                assert_eq!(deleted.persons, 1);
                assert_eq!(deleted.conversations, 4);
                assert_eq!(deleted.external_identities, 2);
                assert_eq!(deleted.external_conversations, 4);
                assert_eq!(deleted.message_mappings, 4);
                assert_eq!(deleted.delivery_ledger, 1);
                assert_eq!(deleted.memories, 5);
                assert_eq!(deleted.open_loops, 5);
                assert_eq!(deleted.affect_states, 1);
                assert_eq!(deleted.relations, 1);
                assert_eq!(deleted.goals, 5);
                assert_eq!(deleted.total(), 33);

                for (table, column, id) in [
                    ("yunxi_persons", "id", person_uuid),
                    ("yunxi_conversations", "id", first_conversation_uuid),
                    ("yunxi_conversations", "id", second_conversation_uuid),
                    ("yunxi_conversations", "id", alias_conversation_uuid),
                    ("yunxi_conversations", "id", matrix_conversation_uuid),
                ] {
                    let remaining = query_scalar::<Postgres, i64>(&format!(
                        "SELECT COUNT(*) FROM {table} WHERE {column} = $1"
                    ))
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .expect("应核对删除结果");
                    assert_eq!(remaining, 0);
                }

                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_conversations WHERE id = $1",
                    )
                    .bind(group_conversation_uuid)
                    .fetch_one(&pool)
                    .await
                    .expect("应保留群聊 conversation"),
                    1
                );
                for (table, owner_column) in [
                    ("yunxi_memories", "scope_id"),
                    ("yunxi_open_loops", "owner_id"),
                    ("yunxi_goals", "owner_id"),
                    ("yunxi_message_mappings", "conversation_id"),
                    ("yunxi_external_conversations", "conversation_id"),
                ] {
                    let remaining = query_scalar::<Postgres, i64>(&format!(
                        "SELECT COUNT(*) FROM {table} WHERE {owner_column} = $1"
                    ))
                    .bind(group_conversation_uuid)
                    .fetch_one(&pool)
                    .await
                    .expect("应核对群聊数据保留结果");
                    assert_eq!(remaining, 1, "{table}");
                }

                for (table, owner_column) in [
                    ("yunxi_memories", "scope_id"),
                    ("yunxi_open_loops", "owner_id"),
                    ("yunxi_goals", "owner_id"),
                    ("yunxi_message_mappings", "conversation_id"),
                    ("yunxi_external_conversations", "conversation_id"),
                ] {
                    query(&format!("DELETE FROM {table} WHERE {owner_column} = $1"))
                        .bind(group_conversation_uuid)
                        .execute(&pool)
                        .await
                        .expect("应清理群聊测试数据");
                }
                query("DELETE FROM yunxi_conversations WHERE id = $1")
                    .bind(group_conversation_uuid)
                    .execute(&pool)
                    .await
                    .expect("应清理群聊 conversation");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_person_domain_deletion_purges_only_attributable_delivery_rows() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(8)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_portable_schema(&pool).await;
                initialize_delivery_ledger(&pool).await;

                let suffix = (Uuid::new_v4().as_u128() % 900_000_000) as i64;
                let user_id = 1_100_000_000_000_i64 + suffix;
                let alias_user_id = 1_200_000_000_000_i64 + suffix;
                let other_user_id = 1_300_000_000_000_i64 + suffix;
                let bot_id = 2_100_000_000_000_i64 + suffix;
                let group_id = 3_100_000_000_000_i64 + suffix;
                let identity = qq::person(user_id).expect("valid person identity");
                let alias = qq::person(alias_user_id).expect("valid alias identity");
                let other_identity = qq::person(other_user_id).expect("valid other identity");
                let direct = qq::direct(bot_id, user_id).expect("valid direct conversation");
                let other_direct =
                    qq::direct(bot_id, other_user_id).expect("valid other direct conversation");
                let group = qq::group(group_id).expect("valid group conversation");
                let person_id = store
                    .resolve_identity(&identity)
                    .await
                    .expect("person identity should resolve");
                query(
                    "INSERT INTO yunxi_external_identities (platform, external_id, person_id)
                     VALUES ('qq', $1, $2)",
                )
                .bind(alias.external_id())
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("alias should link to person");
                let other_person_id = store
                    .resolve_identity(&other_identity)
                    .await
                    .expect("other identity should resolve");
                let direct_id = store
                    .resolve_direct_for_person(person_id, &direct)
                    .await
                    .expect("direct conversation should resolve");
                let other_direct_id = store
                    .resolve_direct_for_person(other_person_id, &other_direct)
                    .await
                    .expect("other direct conversation should resolve");
                let group_conversation_id = store
                    .resolve_conversation(&group)
                    .await
                    .expect("group conversation should resolve");
                let prefix = format!("person-ledger:{}", Uuid::new_v4());
                let person_key = format!("{prefix}:person");
                let direct_key = format!("{prefix}:direct");
                let qq_destination_key = format!("{prefix}:qq-destination");
                let shared_group_key = format!("{prefix}:shared-group");
                let other_person_key = format!("{prefix}:other-person");

                insert_delivery_fixture(
                    &pool,
                    &person_key,
                    "reach_out",
                    "person",
                    person_id.into_uuid(),
                    direct_id.into_uuid(),
                    "private",
                    user_id,
                )
                .await;
                insert_delivery_fixture(
                    &pool,
                    &direct_key,
                    "send_message",
                    "conversation",
                    direct_id.into_uuid(),
                    direct_id.into_uuid(),
                    "private",
                    user_id,
                )
                .await;
                insert_delivery_fixture(
                    &pool,
                    &qq_destination_key,
                    "send_message",
                    "conversation",
                    other_direct_id.into_uuid(),
                    other_direct_id.into_uuid(),
                    "private",
                    alias_user_id,
                )
                .await;
                insert_delivery_fixture(
                    &pool,
                    &shared_group_key,
                    "send_message",
                    "conversation",
                    group_conversation_id.into_uuid(),
                    group_conversation_id.into_uuid(),
                    "group",
                    user_id,
                )
                .await;
                insert_delivery_fixture(
                    &pool,
                    &other_person_key,
                    "reach_out",
                    "person",
                    other_person_id.into_uuid(),
                    other_direct_id.into_uuid(),
                    "private",
                    other_user_id,
                )
                .await;

                let deleted = store
                    .delete_person_domain_data(&identity, &direct)
                    .await
                    .expect("person domain deletion should succeed");
                assert_eq!(deleted.delivery_ledger, 3);
                let remaining = query_scalar::<Postgres, String>(
                    "SELECT delivery_key FROM yunxi_action_delivery_ledger
                     WHERE delivery_key LIKE $1 ORDER BY delivery_key",
                )
                .bind(format!("{prefix}%"))
                .fetch_all(&pool)
                .await
                .expect("should read remaining delivery fixtures");
                let mut expected = vec![shared_group_key.clone(), other_person_key.clone()];
                expected.sort_unstable();
                assert_eq!(remaining, expected);
                assert!(
                    store
                        .resolve_qq_direct_for_person_delivery(person_id, bot_id)
                        .await
                        .expect("deleted person route lookup should not fail")
                        .is_none(),
                    "ReachOut must not recreate a direct conversation after person deletion"
                );

                store
                    .delete_person_domain_data(&other_identity, &other_direct)
                    .await
                    .expect("other person cleanup should succeed");
                store
                    .delete_qq_group_domain_data(group_id)
                    .await
                    .expect("group cleanup should succeed");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_group_domain_deletion_removes_canonical_and_delivery_rows() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(8)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_portable_schema(&pool).await;
                initialize_delivery_ledger(&pool).await;
                let suffix = (Uuid::new_v4().as_u128() % 900_000_000) as i64;
                let group_id = 3_200_000_000_000_i64 + suffix;
                let other_group_id = 3_300_000_000_000_i64 + suffix;
                let group = qq::group(group_id).expect("valid group");
                let other_group = qq::group(other_group_id).expect("valid other group");
                let conversation_id = store
                    .resolve_conversation(&group)
                    .await
                    .expect("group should resolve");
                let other_conversation_id = store
                    .resolve_conversation(&other_group)
                    .await
                    .expect("other group should resolve");
                let prefix = format!("group-ledger:{}", Uuid::new_v4());
                let conversation_key = format!("{prefix}:conversation");
                let destination_key = format!("{prefix}:destination");
                let other_key = format!("{prefix}:other");
                insert_delivery_fixture(
                    &pool,
                    &conversation_key,
                    "send_message",
                    "conversation",
                    conversation_id.into_uuid(),
                    conversation_id.into_uuid(),
                    "group",
                    group_id,
                )
                .await;
                insert_delivery_fixture(
                    &pool,
                    &destination_key,
                    "send_message",
                    "conversation",
                    other_conversation_id.into_uuid(),
                    other_conversation_id.into_uuid(),
                    "group",
                    group_id,
                )
                .await;
                insert_delivery_fixture(
                    &pool,
                    &other_key,
                    "send_message",
                    "conversation",
                    other_conversation_id.into_uuid(),
                    other_conversation_id.into_uuid(),
                    "group",
                    other_group_id,
                )
                .await;

                let deleted = store
                    .delete_qq_group_domain_data(group_id)
                    .await
                    .expect("group Core deletion should succeed");
                assert!(
                    deleted >= 4,
                    "ledger, mapping, and conversation must be deleted"
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_conversations WHERE id = $1",
                    )
                    .bind(conversation_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("should count deleted conversation"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, String>(
                        "SELECT delivery_key FROM yunxi_action_delivery_ledger
                         WHERE delivery_key LIKE $1",
                    )
                    .bind(format!("{prefix}%"))
                    .fetch_all(&pool)
                    .await
                    .expect("should read remaining group ledger rows"),
                    vec![other_key]
                );
                store
                    .delete_qq_group_domain_data(other_group_id)
                    .await
                    .expect("other group cleanup should succeed");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_missing_person_deletion_rejects_another_person_direct_member() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_portable_schema(&pool).await;
                initialize_delivery_ledger(&pool).await;
                let suffix = (Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
                let missing_user_id = 6_100_000_000_000_i64 + suffix;
                let other_user_id = 6_200_000_000_000_i64 + suffix;
                let bot_id = 6_300_000_000_000_i64 + suffix;
                let missing_identity = qq::person(missing_user_id).expect("valid missing identity");
                let other_identity = qq::person(other_user_id).expect("valid other identity");
                let direct =
                    qq::direct(bot_id, missing_user_id).expect("valid conflicting direct route");
                let other_person_id = store
                    .resolve_identity(&other_identity)
                    .await
                    .expect("other identity should resolve");
                let conversation_id = store
                    .resolve_direct_for_person(other_person_id, &direct)
                    .await
                    .expect("other Person direct route should resolve");

                assert!(matches!(
                    store
                        .delete_person_domain_data(&missing_identity, &direct)
                        .await,
                    Err(IdentityStoreError::Storage { .. })
                ));
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_persons WHERE id = $1",
                    )
                    .bind(other_person_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("应核对其他 Person 保留"),
                    1
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_conversations WHERE id = $1",
                    )
                    .bind(conversation_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("应核对其他 Person 的 direct conversation 保留"),
                    1
                );

                store
                    .delete_person_domain_data(&other_identity, &direct)
                    .await
                    .expect("应清理其他 Person fixture");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_person_import_waits_for_owner_lock() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_portable_schema(&pool).await;
                let person_id = PersonId::new();
                let export = PortablePersonExport {
                    version: 1,
                    person_id,
                    external_identities: Vec::new(),
                    memories: Vec::new(),
                    relation: None,
                    affect: None,
                    open_loops: Vec::new(),
                    goals: Vec::new(),
                };

                let mut blocker = pool.begin().await.expect("应开始 owner lock 事务");
                crate::yunxi::owner_lock::lock_owner(
                    &mut blocker,
                    crate::yunxi::owner_lock::DurableOwner::Person(person_id.into_uuid()),
                )
                .await
                .expect("应锁定 Person owner");
                let import_store = store.clone();
                let import_task =
                    kovi::tokio::spawn(async move { import_store.import_person(&export).await });
                kovi::tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(
                    !import_task.is_finished(),
                    "portable import must wait for the Person owner lock"
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_persons WHERE id = $1",
                    )
                    .bind(person_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("应检查锁等待期间的 Person 可见性"),
                    0
                );

                blocker.commit().await.expect("应释放 Person owner lock");
                assert_eq!(
                    import_task
                        .await
                        .expect("import task should join")
                        .expect("import should complete after owner unlock"),
                    person_id
                );
                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应清理 import owner-lock fixture");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_person_deletion_blocks_then_rejects_missing_direct_route_creation() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(8)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = Arc::new(initialize_portable_schema(&pool).await);
                initialize_delivery_ledger(&pool).await;
                let suffix = (Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
                let user_id = 7_100_000_000_000_i64 + suffix;
                let first_bot_id = 7_200_000_000_000_i64 + suffix;
                let second_bot_id = 7_300_000_000_000_i64 + suffix;
                let identity = qq::person(user_id).expect("valid race identity");
                let existing_direct =
                    qq::direct(first_bot_id, user_id).expect("valid existing direct route");
                let missing_direct =
                    qq::direct(second_bot_id, user_id).expect("valid missing direct route");
                let person_id = store
                    .resolve_identity(&identity)
                    .await
                    .expect("identity should resolve");
                let existing_conversation_id = store
                    .resolve_direct_for_person(person_id, &existing_direct)
                    .await
                    .expect("existing direct route should resolve");

                let mut blocker = pool.begin().await.expect("应开始 Conversation lock 事务");
                crate::yunxi::owner_lock::lock_owner(
                    &mut blocker,
                    crate::yunxi::owner_lock::DurableOwner::Conversation(
                        existing_conversation_id.into_uuid(),
                    ),
                )
                .await
                .expect("应锁定 existing Conversation owner");

                let delete_store = Arc::clone(&store);
                let delete_identity = identity.clone();
                let delete_direct = existing_direct.clone();
                let delete_task = kovi::tokio::spawn(async move {
                    delete_store
                        .delete_person_domain_data(&delete_identity, &delete_direct)
                        .await
                });

                let route_key = ExternalConversationRoute::from_external(&existing_direct)
                    .advisory_lock_key();
                let mut deletion_holds_route = false;
                for _ in 0..100 {
                    let mut probe = pool.begin().await.expect("应开始 route lock probe");
                    let acquired = query_scalar::<Postgres, bool>(
                        "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))",
                    )
                    .bind(&route_key)
                    .fetch_one(&mut *probe)
                    .await
                    .expect("应探测删除事务的 route lock");
                    probe.rollback().await.expect("应释放 route lock probe");
                    if !acquired {
                        deletion_holds_route = true;
                        break;
                    }
                    kovi::tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(
                    deletion_holds_route,
                    "production deletion should hold the existing route before waiting on Conversation"
                );

                let resolve_store = Arc::clone(&store);
                let resolve_task = kovi::tokio::spawn(async move {
                    resolve_store
                        .resolve_direct_for_person(person_id, &missing_direct)
                        .await
                });
                kovi::tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(
                    !resolve_task.is_finished(),
                    "new direct route resolution must wait behind Person deletion"
                );

                blocker
                    .commit()
                    .await
                    .expect("应释放 existing Conversation owner");
                delete_task
                    .await
                    .expect("delete task should join")
                    .expect("production Person deletion should succeed");
                resolve_task
                    .await
                    .expect("resolve task should join")
                    .expect_err("stale Person route resolution must fail after deletion");
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_external_conversations
                         WHERE platform = $1 AND external_id = $2",
                    )
                    .bind(existing_direct.platform().as_str())
                    .bind(format!("direct:{second_bot_id}:{user_id}"))
                    .fetch_one(&pool)
                    .await
                    .expect("应核对 missing route 未被创建"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_conversations WHERE id = $1",
                    )
                    .bind(existing_conversation_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("应核对已有 direct conversation 已删除"),
                    0
                );
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_person_domain_deletion_serializes_concurrent_owner_writes() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(16)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let identities = Arc::new(PostgresIdentityStore::new(pool.clone()));
                identities
                    .initialize_schema()
                    .await
                    .expect("应初始化身份 schema");
                initialize_delivery_ledger(&pool).await;
                let open_loops = Arc::new(PostgresOpenLoopStore::new(pool.clone()));
                open_loops
                    .initialize_schema()
                    .await
                    .expect("应初始化 open-loop schema");
                let suffix = Uuid::new_v4().simple().to_string();
                let legacy_path = std::env::temp_dir()
                    .join(format!("yunxi-owner-lock-{suffix}.json"))
                    .to_string_lossy()
                    .into_owned();
                let memories = Arc::new(PostgresMemoryStore::new(
                    Arc::new(MemoryManager::new(&legacy_path)),
                    Arc::clone(&identities),
                    pool.clone(),
                ));
                memories
                    .initialize_schema()
                    .await
                    .expect("应初始化 memory schema");
                let goals = Arc::new(PostgresGoalStore::new(pool.clone()));
                goals
                    .initialize_schema()
                    .await
                    .expect("应初始化 goal schema");
                PostgresAffectStore::new(pool.clone())
                    .initialize_schema()
                    .await
                    .expect("应初始化 affect schema");
                PostgresRelationStore::new(pool.clone())
                    .initialize_schema()
                    .await
                    .expect("应初始化 relation schema");

                let numeric_suffix = (Uuid::new_v4().as_u128() % 1_000_000_000) as i64;
                let user_id = 5_000_000_000_000_i64 + numeric_suffix;
                let bot_id = 6_000_000_000_000_i64 + numeric_suffix;
                let identity = qq::person(user_id).expect("valid concurrent identity");
                let direct =
                    qq::direct(bot_id, user_id).expect("valid concurrent direct conversation");
                let person_id = identities
                    .resolve_identity(&identity)
                    .await
                    .expect("identity should resolve");
                let conversation_id = identities
                    .resolve_direct_for_person(person_id, &direct)
                    .await
                    .expect("conversation should resolve");

                let person_memory = MemoryDraft::new(
                    MemoryScope::Person(person_id),
                    MemoryKind::Fact,
                    "concurrent person memory",
                    chrono::Utc::now(),
                )
                .expect("valid person memory");
                let conversation_memory = MemoryDraft::new(
                    MemoryScope::Conversation(conversation_id),
                    MemoryKind::Conversation,
                    "concurrent conversation memory",
                    chrono::Utc::now(),
                )
                .expect("valid conversation memory");
                let person_open_loop = OpenLoopDraft::new(
                    OpenLoopOwner::Person(person_id),
                    OpenLoopKind::FollowUp,
                    "concurrent person open loop",
                )
                .expect("valid person open loop");
                let conversation_open_loop = OpenLoopDraft::new(
                    OpenLoopOwner::Conversation(conversation_id),
                    OpenLoopKind::FollowUp,
                    "concurrent conversation open loop",
                )
                .expect("valid conversation open loop");
                let person_goal = GoalDraft::new(
                    GoalOwner::Person(person_id),
                    GoalKind::Personal,
                    "concurrent person goal",
                )
                .expect("valid person goal");
                let conversation_goal = GoalDraft::new(
                    GoalOwner::Conversation(conversation_id),
                    GoalKind::Conversation,
                    "concurrent conversation goal",
                )
                .expect("valid conversation goal");

                let (
                    deleted,
                    person_memory_result,
                    conversation_memory_result,
                    person_loop_result,
                    conversation_loop_result,
                    person_goal_result,
                    conversation_goal_result,
                ) = kovi::tokio::join!(
                    identities.delete_person_domain_data(&identity, &direct),
                    memories.remember(&person_memory),
                    memories.remember(&conversation_memory),
                    open_loops.create(&person_open_loop),
                    open_loops.create(&conversation_open_loop),
                    goals.create(&person_goal),
                    goals.create(&conversation_goal),
                );
                deleted.expect("concurrent domain deletion should succeed");
                for result in [person_memory_result, conversation_memory_result] {
                    if let Err(error) = result {
                        assert!(
                            matches!(error, MemoryStoreError::InvalidRequest { .. }),
                            "unexpected memory writer error: {error}"
                        );
                    }
                }
                for result in [person_loop_result, conversation_loop_result] {
                    if let Err(error) = result {
                        assert!(
                            matches!(error, OpenLoopStoreError::InvalidRequest { .. }),
                            "unexpected open-loop writer error: {error}"
                        );
                    }
                }
                for result in [person_goal_result, conversation_goal_result] {
                    if let Err(error) = result {
                        assert!(
                            matches!(error, GoalStoreError::InvalidRequest { .. }),
                            "unexpected goal writer error: {error}"
                        );
                    }
                }
                assert!(matches!(
                    memories.remember(&person_memory).await,
                    Err(MemoryStoreError::InvalidRequest { .. })
                ));
                assert!(matches!(
                    open_loops.create(&person_open_loop).await,
                    Err(OpenLoopStoreError::InvalidRequest { .. })
                ));
                assert!(matches!(
                    goals.create(&person_goal).await,
                    Err(GoalStoreError::InvalidRequest { .. })
                ));

                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_persons WHERE id = $1",
                    )
                    .bind(person_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("应核对 person 删除"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_conversations WHERE id = $1",
                    )
                    .bind(conversation_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("应核对 conversation 删除"),
                    0
                );
                for (table, owner_column, owner_id) in [
                    ("yunxi_memories", "scope_id", person_id.into_uuid()),
                    ("yunxi_memories", "scope_id", conversation_id.into_uuid()),
                    ("yunxi_open_loops", "owner_id", person_id.into_uuid()),
                    ("yunxi_open_loops", "owner_id", conversation_id.into_uuid()),
                    ("yunxi_goals", "owner_id", person_id.into_uuid()),
                    ("yunxi_goals", "owner_id", conversation_id.into_uuid()),
                ] {
                    let remaining = query_scalar::<Postgres, i64>(&format!(
                        "SELECT COUNT(*) FROM {table} WHERE {owner_column} = $1"
                    ))
                    .bind(owner_id)
                    .fetch_one(&pool)
                    .await
                    .expect("应核对并发 owner 数据删除");
                    assert_eq!(remaining, 0, "{table} retained owner {owner_id}");
                }
                let _ = std::fs::remove_file(legacy_path);
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_person_export_import_and_unlink_round_trip() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(8)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_portable_schema(&pool).await;
                let open_loops = PostgresOpenLoopStore::new(pool.clone());
                let goals = PostgresGoalStore::new(pool.clone());

                let suffix = Uuid::new_v4().to_string();
                let person_id = PersonId::new();
                let platform = PlatformId::new("portabletest").expect("valid platform");
                let primary = ExternalIdentity::new(platform.clone(), format!("primary:{suffix}"))
                    .expect("valid primary identity");
                let secondary = ExternalIdentity::new(platform, format!("secondary:{suffix}"))
                    .expect("valid secondary identity");
                let memory_id = Uuid::new_v4();
                let occurred_at = Utc::now();

                query("INSERT INTO yunxi_persons (id) VALUES ($1)")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应创建 portable person");
                query(
                    "INSERT INTO yunxi_external_identities (platform, external_id, person_id)
                     VALUES ($1, $2, $3), ($1, $4, $3)",
                )
                .bind(primary.platform().as_str())
                .bind(primary.external_id())
                .bind(person_id.into_uuid())
                .bind(secondary.external_id())
                .execute(&pool)
                .await
                .expect("应创建 portable identities");
                query(
                    "INSERT INTO yunxi_memories
                        (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                     VALUES ($1, 'person', $2, 'fact', 'portable memory', 80, $3, $4)",
                )
                .bind(memory_id)
                .bind(person_id.into_uuid())
                .bind(serde_json::json!(["portable", "round-trip"]))
                .bind(occurred_at)
                .execute(&pool)
                .await
                .expect("应创建 portable memory");
                query(
                    "INSERT INTO yunxi_relations
                        (person_id, familiarity, affinity, trust, comfort, tension)
                     VALUES ($1, 0.25, -0.5, 0.75, 0.5, -0.25)",
                )
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("应创建 portable relation");
                query(
                    "INSERT INTO yunxi_affect_states
                        (person_id, valence, arousal, social_energy, curiosity)
                     VALUES ($1, -0.25, 0.5, 0.75, 0.9)",
                )
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("应创建 portable affect");
                let open_loop = open_loops
                    .create(
                        &OpenLoopDraft::new(
                            OpenLoopOwner::Person(person_id),
                            OpenLoopKind::AwaitingOutcome,
                            "portable open loop",
                        )
                        .expect("应创建合法 open-loop draft")
                        .with_due_at(Some(occurred_at + chrono::Duration::hours(2)))
                        .with_salience(73)
                        .expect("应设置合法 salience")
                        .with_dedupe_key(Some(format!("portable:{suffix}")))
                        .expect("应设置合法 dedupe key"),
                    )
                    .await
                    .expect("应创建 portable open loop");
                let goal = goals
                    .create(
                        &GoalDraft::new(
                            GoalOwner::Person(person_id),
                            GoalKind::Project,
                            "portable goal",
                        )
                        .expect("应创建合法 goal draft")
                        .with_details("portable goal details")
                        .expect("应设置合法 goal details")
                        .with_due_at(Some(occurred_at + chrono::Duration::days(3))),
                    )
                    .await
                    .expect("应创建 portable goal");

                let exported = store
                    .export_person(person_id)
                    .await
                    .expect("应导出 person snapshot");
                assert_eq!(exported.version, 1);
                assert_eq!(exported.person_id, person_id);
                assert_eq!(exported.external_identities.len(), 2);
                assert_eq!(exported.memories.len(), 1);
                assert_eq!(exported.memories[0].content(), "portable memory");
                assert_eq!(exported.memories[0].importance(), 80);
                assert_eq!(exported.memories[0].tags(), ["portable", "round-trip"]);
                let relation = exported.relation.expect("relation should be exported");
                assert_eq!(relation.person_id, person_id);
                assert_eq!(relation.affinity, -0.5);
                let affect = exported.affect.expect("affect should be exported");
                assert_eq!(affect.valence, -0.25);
                assert_eq!(affect.curiosity, 0.9);
                assert_eq!(exported.open_loops, [open_loop]);
                assert_eq!(exported.goals, [goal]);

                assert!(
                    store
                        .unlink_external_identity(&primary)
                        .await
                        .expect("unlink should succeed")
                );
                assert!(
                    !store
                        .unlink_external_identity(&primary)
                        .await
                        .expect("second unlink should be idempotent")
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_external_identities
                         WHERE person_id = $1",
                    )
                    .bind(person_id.into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("应读取剩余 identity"),
                    1
                );

                assert_eq!(
                    store
                        .import_person(&exported)
                        .await
                        .expect("import should restore the unlinked identity"),
                    person_id
                );
                assert_eq!(
                    store
                        .export_person(person_id)
                        .await
                        .expect("re-export should round-trip"),
                    exported
                );

                for statement in [
                    "DELETE FROM yunxi_memories WHERE scope_kind = 'person' AND scope_id = $1",
                    "DELETE FROM yunxi_open_loops WHERE owner_kind = 'person' AND owner_id = $1",
                    "DELETE FROM yunxi_goals WHERE owner_kind = 'person' AND owner_id = $1",
                ] {
                    query(statement)
                        .bind(person_id.into_uuid())
                        .execute(&pool)
                        .await
                        .expect("应清理 portable person owner state");
                }
                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应清理 portable person root");
                assert_eq!(
                    store
                        .import_person(&exported)
                        .await
                        .expect("应向空目标恢复完整 person snapshot"),
                    person_id
                );
                assert_eq!(
                    store
                        .export_person(person_id)
                        .await
                        .expect("完整恢复后应可等价导出"),
                    exported
                );

                for statement in [
                    "DELETE FROM yunxi_memories WHERE scope_kind = 'person' AND scope_id = $1",
                    "DELETE FROM yunxi_open_loops WHERE owner_kind = 'person' AND owner_id = $1",
                    "DELETE FROM yunxi_goals WHERE owner_kind = 'person' AND owner_id = $1",
                ] {
                    query(statement)
                        .bind(person_id.into_uuid())
                        .execute(&pool)
                        .await
                        .expect("应清理 round-trip owner state");
                }
                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应清理 round-trip person");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_qq_unlink_purges_private_legacy_rows_before_identity_reuse() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(8)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_portable_schema(&pool).await;

                let suffix = (Uuid::new_v4().as_u128() % 900_000_000) as i64;
                let user_id = 8_400_000_000_000_i64 + suffix;
                let other_user_id = user_id + 1;
                let identity = qq::person(user_id).expect("valid QQ identity");
                let person_id = store
                    .resolve_identity(&identity)
                    .await
                    .expect("identity should resolve");
                let direct_conversation_id = Uuid::new_v4();
                let direct_bot_id = user_id + 10;
                query("INSERT INTO yunxi_conversations (id, kind) VALUES ($1, 'direct')")
                    .bind(direct_conversation_id)
                    .execute(&pool)
                    .await
                    .expect("应创建直聊会话 fixture");
                query(
                    "INSERT INTO yunxi_conversation_members (conversation_id, person_id)
                     VALUES ($1, $2)",
                )
                .bind(direct_conversation_id)
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("应创建直聊成员 fixture");
                query(
                    "INSERT INTO yunxi_external_conversations
                        (platform, external_id, conversation_id)
                     VALUES ('qq', $1, $2)",
                )
                .bind(format!("direct:{direct_bot_id}:{user_id}"))
                .bind(direct_conversation_id)
                .execute(&pool)
                .await
                .expect("应创建直聊外部映射 fixture");
                let private_ids = vec![
                    format!("unlink-private-{suffix}"),
                    format!("unlink-private-batch-{suffix}"),
                    format!("unlink-proactive-private-{suffix}"),
                    format!("unlink-main-admin-{suffix}"),
                ];
                let group_id = format!("unlink-group-{suffix}");
                let other_id = format!("unlink-other-{suffix}");
                let direct_legacy_id = format!("unlink-direct-{suffix}");
                for (id, scope_type, context, subject_id) in [
                    (
                        private_ids[0].as_str(),
                        Some("private"),
                        "private_chat",
                        Some(user_id),
                    ),
                    (
                        private_ids[1].as_str(),
                        None,
                        "private_chat_batch",
                        Some(user_id),
                    ),
                    (
                        private_ids[2].as_str(),
                        None,
                        "proactive_private_chat",
                        Some(user_id),
                    ),
                    (
                        private_ids[3].as_str(),
                        None,
                        "proactive_main_admin_decision",
                        Some(user_id),
                    ),
                    (
                        group_id.as_str(),
                        Some("group"),
                        "group_chat",
                        Some(user_id),
                    ),
                    (
                        other_id.as_str(),
                        Some("private"),
                        "private_chat",
                        Some(other_user_id),
                    ),
                ] {
                    query(
                        "INSERT INTO kovi_bot_memories
                            (id, subject_id, scope_type, context, occurred_at, importance, payload)
                         VALUES ($1, $2, $3, $4, NOW(), 50, $5)",
                    )
                    .bind(id)
                    .bind(subject_id)
                    .bind(scope_type)
                    .bind(context)
                    .bind(serde_json::json!({"id": id, "content": "unlink fixture"}))
                    .execute(&pool)
                    .await
                    .expect("应创建 legacy unlink fixture");
                }
                query(
                    "INSERT INTO kovi_bot_memories
                        (id, subject_id, context, occurred_at, importance, payload)
                     VALUES ($1, NULL, $2, NOW(), 50, $3)",
                )
                .bind(&direct_legacy_id)
                .bind(format!("yunxi_direct_chat:{direct_conversation_id}"))
                .bind(serde_json::json!({
                    "id": direct_legacy_id,
                    "content": "unlink direct fixture"
                }))
                .execute(&pool)
                .await
                .expect("应创建直聊 legacy fixture");

                assert!(
                    store
                        .unlink_external_identity(&identity)
                        .await
                        .expect("QQ identity unlink should succeed")
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_external_identities
                         WHERE platform = 'qq' AND external_id = $1",
                    )
                    .bind(identity.external_id())
                    .fetch_one(&pool)
                    .await
                    .expect("应检查 QQ mapping"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = ANY($1)",
                    )
                    .bind(&private_ids)
                    .fetch_one(&pool)
                    .await
                    .expect("应检查私聊 legacy rows"),
                    0,
                    "解绑必须清理所有私聊形态的兼容记忆"
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = ANY($1)",
                    )
                    .bind(vec![group_id.clone(), other_id.clone()])
                    .fetch_one(&pool)
                    .await
                    .expect("应检查不相关 legacy rows"),
                    2,
                    "解绑不能误删群记忆或其他 QQ 用户记忆"
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = $1",
                    )
                    .bind(&direct_legacy_id)
                    .fetch_one(&pool)
                    .await
                    .expect("应检查直聊 legacy row"),
                    0,
                    "解绑必须清理旧直聊上下文投影"
                );

                let reused_person = store
                    .resolve_identity(&identity)
                    .await
                    .expect("解绑后的 QQ identity 应可重新绑定");
                assert_ne!(reused_person, person_id);

                query("DELETE FROM kovi_bot_memories WHERE id = ANY($1)")
                    .bind(vec![group_id, other_id])
                    .execute(&pool)
                    .await
                    .expect("应清理 legacy fixtures");
                query("DELETE FROM yunxi_external_conversations WHERE conversation_id = $1")
                    .bind(direct_conversation_id)
                    .execute(&pool)
                    .await
                    .expect("应清理直聊外部映射 fixture");
                query("DELETE FROM yunxi_conversation_members WHERE conversation_id = $1")
                    .bind(direct_conversation_id)
                    .execute(&pool)
                    .await
                    .expect("应清理直聊成员 fixture");
                query("DELETE FROM yunxi_conversations WHERE id = $1")
                    .bind(direct_conversation_id)
                    .execute(&pool)
                    .await
                    .expect("应清理直聊会话 fixture");
                query("DELETE FROM yunxi_persons WHERE id = ANY($1)")
                    .bind(vec![person_id.into_uuid(), reused_person.into_uuid()])
                    .execute(&pool)
                    .await
                    .expect("应清理 identity reuse fixtures");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_person_import_rejects_identity_and_memory_conflicts_transactionally() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(8)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_portable_schema(&pool).await;
                let source_person = PersonId::new();
                let other_person = PersonId::new();
                let suffix = Uuid::new_v4().to_string();
                let memory_id = Uuid::new_v4();
                query("INSERT INTO yunxi_persons (id) VALUES ($1), ($2)")
                    .bind(source_person.into_uuid())
                    .bind(other_person.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应创建冲突测试 persons");
                query(
                    "INSERT INTO yunxi_external_identities (platform, external_id, person_id)
                     VALUES ('portabletest', $1, $2), ('portabletest', $3, $4)",
                )
                .bind(format!("source:{suffix}"))
                .bind(source_person.into_uuid())
                .bind(format!("occupied:{suffix}"))
                .bind(other_person.into_uuid())
                .execute(&pool)
                .await
                .expect("应创建冲突测试 identities");
                query(
                    "INSERT INTO yunxi_memories
                        (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
                     VALUES ($1, 'person', $2, 'fact', 'source memory', 60, '[]', NOW())",
                )
                .bind(memory_id)
                .bind(source_person.into_uuid())
                .execute(&pool)
                .await
                .expect("应创建冲突测试 memory");

                let mut exported = store
                    .export_person(source_person)
                    .await
                    .expect("应导出冲突测试 snapshot");
                let fresh_identity = format!("fresh-before-conflict:{suffix}");
                exported.external_identities.push(PortableExternalIdentity {
                    platform: "portabletest".to_owned(),
                    external_id: fresh_identity.clone(),
                });
                exported.external_identities.push(PortableExternalIdentity {
                    platform: "portabletest".to_owned(),
                    external_id: format!("occupied:{suffix}"),
                });
                let error = store
                    .import_person(&exported)
                    .await
                    .expect_err("被占用 identity 必须拒绝导入");
                assert!(
                    format!("{error:?}").contains("another person"),
                    "unexpected identity conflict error: {error:?}"
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_external_identities
                         WHERE platform = 'portabletest' AND external_id = $1",
                    )
                    .bind(&fresh_identity)
                    .fetch_one(&pool)
                    .await
                    .expect("应核对 identity 冲突事务回滚"),
                    0
                );
                assert_eq!(
                    query_scalar::<Postgres, Uuid>(
                        "SELECT person_id FROM yunxi_external_identities
                         WHERE platform = 'portabletest' AND external_id = $1",
                    )
                    .bind(format!("occupied:{suffix}"))
                    .fetch_one(&pool)
                    .await
                    .expect("应核对被占用 identity 未被劫持"),
                    other_person.into_uuid()
                );

                exported.external_identities.truncate(1);
                let fresh_before_memory_conflict = format!("fresh-before-memory:{suffix}");
                exported.external_identities.push(PortableExternalIdentity {
                    platform: "portabletest".to_owned(),
                    external_id: fresh_before_memory_conflict.clone(),
                });
                query(
                    "UPDATE yunxi_memories
                     SET scope_id = $2, content = 'conflicting memory'
                     WHERE id = $1",
                )
                .bind(memory_id)
                .bind(other_person.into_uuid())
                .execute(&pool)
                .await
                .expect("应制造 memory ID 冲突");
                let error = store
                    .import_person(&exported)
                    .await
                    .expect_err("异内容 memory ID 必须拒绝导入");
                assert!(
                    format!("{error:?}").contains("memory ID"),
                    "unexpected memory conflict error: {error:?}"
                );
                assert_eq!(
                    query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM yunxi_external_identities
                         WHERE platform = 'portabletest' AND external_id = $1",
                    )
                    .bind(&fresh_before_memory_conflict)
                    .fetch_one(&pool)
                    .await
                    .expect("应核对 memory 冲突事务回滚"),
                    0
                );
                let stored_memory =
                    query("SELECT scope_id, content FROM yunxi_memories WHERE id = $1")
                        .bind(memory_id)
                        .fetch_one(&pool)
                        .await
                        .expect("应读取冲突 memory");
                assert_eq!(
                    stored_memory
                        .try_get::<Uuid, _>("scope_id")
                        .expect("应读取 memory scope"),
                    other_person.into_uuid()
                );
                assert_eq!(
                    stored_memory
                        .try_get::<String, _>("content")
                        .expect("应读取 memory content"),
                    "conflicting memory"
                );

                query("DELETE FROM yunxi_memories WHERE id = $1")
                    .bind(memory_id)
                    .execute(&pool)
                    .await
                    .expect("应清理冲突 memory");
                query("DELETE FROM yunxi_persons WHERE id = $1 OR id = $2")
                    .bind(source_person.into_uuid())
                    .bind(other_person.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应清理冲突测试 persons");
            });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_person_export_fails_closed_above_collection_limits() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("需要 DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(8)
                    .connect(&database_url)
                    .await
                    .expect("应连接 PostgreSQL");
                let store = initialize_portable_schema(&pool).await;
                let person_id = PersonId::new();
                let suffix = Uuid::new_v4().to_string();
                query("INSERT INTO yunxi_persons (id) VALUES ($1)")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应创建上限测试 person");
                query(
                    "INSERT INTO yunxi_external_identities (platform, external_id, person_id)
                     SELECT 'portablelimit', $1 || ':' || sequence::text, $2
                     FROM generate_series(1, 257) AS sequence",
                )
                .bind(&suffix)
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("应创建超限 identities");
                let error = store
                    .export_person(person_id)
                    .await
                    .expect_err("257 个 identities 必须拒绝导出");
                assert!(format!("{error:?}").contains("above maximum 256"));

                query("DELETE FROM yunxi_external_identities WHERE person_id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应清理超限 identities");
                let memory_ids = (0..513).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
                query(
                    "INSERT INTO yunxi_memories
                        (id, scope_kind, scope_id, kind, content, importance, tags,
                         occurred_at, created_at)
                     SELECT imported.id, 'person', $1, 'fact', 'portable limit memory', 50,
                            '[]'::jsonb, NOW(), NOW()
                     FROM UNNEST($2::uuid[]) AS imported(id)",
                )
                .bind(person_id.into_uuid())
                .bind(&memory_ids)
                .execute(&pool)
                .await
                .expect("应创建超限 memories");
                let error = store
                    .export_person(person_id)
                    .await
                    .expect_err("513 条 memories 必须拒绝导出");
                assert!(format!("{error:?}").contains("above maximum 512"));

                query("DELETE FROM yunxi_memories WHERE scope_id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应清理超限 memories");
                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("应清理上限测试 person");
            });
    }
}
