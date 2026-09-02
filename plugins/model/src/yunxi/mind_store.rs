use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, Postgres};
use uuid::Uuid;
use yunxi_core::{
    AgendaItem, AgendaItemId, AgendaStatus, AgendaStore, Belief, BeliefId, BeliefStore,
    ConsolidationConfig, ConsolidationPlan, ConsolidationResult, CuriosityId, CuriosityItem,
    CuriosityStatus, CuriosityStore, Episode, EpisodeStore, Interest, InterestId, InterestStore,
    MindConsolidationStore, MindDataErasure, MindDataErasureError, MindDataErasureFuture,
    MindScope, MindServices, MindStoreError, MindStoreFuture, OpenQuestion, OpenQuestionId,
    OpenQuestionStatus, OpenQuestionStore, Preference, PreferenceId, PreferenceStore, SelfModel,
    SelfModelStore, lexical_terms,
};

const MAX_LIST_LIMIT: usize = 128;
const GLOBAL_SCOPE_KEY: &str = "global";

#[derive(Debug, Clone)]
pub(crate) struct PostgresMindStore {
    pool: PgPool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MindStoreStatus {
    pub version: u64,
    pub beliefs: u64,
    pub preferences: u64,
    pub interests: u64,
    pub open_questions: u64,
    pub active_agenda: u64,
}

#[derive(Debug, Clone, Copy)]
enum RecordTable {
    Beliefs,
    Preferences,
    Interests,
    Curiosities,
    OpenQuestions,
    Agenda,
    Episodes,
}

impl RecordTable {
    const fn name(self) -> &'static str {
        match self {
            Self::Beliefs => "yunxi_beliefs",
            Self::Preferences => "yunxi_preferences",
            Self::Interests => "yunxi_interests",
            Self::Curiosities => "yunxi_curiosities",
            Self::OpenQuestions => "yunxi_open_questions",
            Self::Agenda => "yunxi_agenda_items",
            Self::Episodes => "yunxi_episodes",
        }
    }

    const fn kind(self) -> &'static str {
        match self {
            Self::Beliefs => "belief",
            Self::Preferences => "preference",
            Self::Interests => "interest",
            Self::Curiosities => "curiosity",
            Self::OpenQuestions => "open_question",
            Self::Agenda => "agenda",
            Self::Episodes => "episode",
        }
    }
}

#[derive(Debug)]
struct StoredRecord<'a, T> {
    id: Uuid,
    scope: MindScope,
    person_id: Option<Uuid>,
    conversation_id: Option<Uuid>,
    participant_ids: Vec<Uuid>,
    dedupe_key: String,
    status: &'static str,
    primary_score: f64,
    secondary_score: f64,
    expires_at: Option<DateTime<Utc>>,
    occurred_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    version: u64,
    value: &'a T,
}

impl PostgresMindStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        query(
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_mind_meta (
                singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
                version BIGINT NOT NULL DEFAULT 0 CHECK (version >= 0),
                schema_version SMALLINT NOT NULL DEFAULT 1 CHECK (schema_version >= 1),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )
            "#,
        )
        .execute(&mut *transaction)
        .await?;
        query("INSERT INTO yunxi_mind_meta (singleton) VALUES (TRUE) ON CONFLICT DO NOTHING")
            .execute(&mut *transaction)
            .await?;
        query(
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_self_model (
                singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
                payload JSONB NOT NULL,
                version BIGINT NOT NULL CHECK (version > 0),
                schema_version SMALLINT NOT NULL DEFAULT 1 CHECK (schema_version >= 1),
                updated_at TIMESTAMPTZ NOT NULL
            )
            "#,
        )
        .execute(&mut *transaction)
        .await?;
        for table in [
            RecordTable::Beliefs,
            RecordTable::Preferences,
            RecordTable::Interests,
            RecordTable::Curiosities,
            RecordTable::OpenQuestions,
            RecordTable::Agenda,
            RecordTable::Episodes,
        ] {
            let statement = format!(
                r#"
                CREATE TABLE IF NOT EXISTS {} (
                    id UUID PRIMARY KEY,
                    scope_kind TEXT NOT NULL
                        CHECK (scope_kind IN ('global', 'person', 'conversation')),
                    scope_id UUID,
                    scope_key TEXT NOT NULL,
                    person_id UUID,
                    conversation_id UUID,
                    participant_ids UUID[] NOT NULL DEFAULT '{{}}',
                    dedupe_key TEXT NOT NULL
                        CHECK (octet_length(dedupe_key) BETWEEN 1 AND 4096),
                    status TEXT NOT NULL CHECK (octet_length(status) BETWEEN 1 AND 64),
                    primary_score DOUBLE PRECISION NOT NULL DEFAULT 0,
                    secondary_score DOUBLE PRECISION NOT NULL DEFAULT 0,
                    expires_at TIMESTAMPTZ,
                    occurred_at TIMESTAMPTZ NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL,
                    version BIGINT NOT NULL CHECK (version > 0),
                    schema_version SMALLINT NOT NULL DEFAULT 1 CHECK (schema_version >= 1),
                    payload JSONB NOT NULL,
                    CHECK (
                        (scope_kind = 'global' AND scope_id IS NULL)
                        OR (scope_kind = 'person' AND scope_id = person_id AND scope_id IS NOT NULL)
                        OR (scope_kind = 'conversation' AND scope_id = conversation_id AND scope_id IS NOT NULL)
                    )
                )
                "#,
                table.name()
            );
            query(&statement).execute(&mut *transaction).await?;
            let unique_predicate = match table {
                RecordTable::Curiosities => " WHERE status IN ('open', 'asked')",
                RecordTable::OpenQuestions => " WHERE status = 'open'",
                RecordTable::Agenda => " WHERE status IN ('active', 'deferred')",
                RecordTable::Beliefs
                | RecordTable::Preferences
                | RecordTable::Interests
                | RecordTable::Episodes => "",
            };
            let unique_index = format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {}_active_scope_dedupe_idx ON {} (scope_key, dedupe_key){}",
                table.name(),
                table.name(),
                unique_predicate,
            );
            query(&unique_index).execute(&mut *transaction).await?;
            let retrieval_index = format!(
                "CREATE INDEX IF NOT EXISTS {}_retrieval_idx ON {} (scope_key, status, primary_score DESC, secondary_score DESC, updated_at DESC, id)",
                table.name(),
                table.name()
            );
            query(&retrieval_index).execute(&mut *transaction).await?;
            let person_index = format!(
                "CREATE INDEX IF NOT EXISTS {}_person_idx ON {} (person_id) WHERE person_id IS NOT NULL",
                table.name(),
                table.name()
            );
            query(&person_index).execute(&mut *transaction).await?;
            let conversation_index = format!(
                "CREATE INDEX IF NOT EXISTS {}_conversation_idx ON {} (conversation_id) WHERE conversation_id IS NOT NULL",
                table.name(),
                table.name()
            );
            query(&conversation_index)
                .execute(&mut *transaction)
                .await?;
            let expiry_index = format!(
                "CREATE INDEX IF NOT EXISTS {}_expiry_idx ON {} (expires_at, id) WHERE expires_at IS NOT NULL",
                table.name(),
                table.name()
            );
            query(&expiry_index).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn seed_self_model_if_absent(&self) -> anyhow::Result<()> {
        if SelfModelStore::get(self)
            .await
            .map_err(anyhow::Error::from)?
            .is_none()
        {
            let seed = SelfModel::seed_yunxi(Utc::now());
            match SelfModelStore::put(self, &seed, None).await {
                Ok(_) | Err(MindStoreError::VersionConflict { .. }) => {}
                Err(error) => return Err(anyhow::Error::from(error)),
            }
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn services(self: &std::sync::Arc<Self>) -> MindServices {
        MindServices::from_store(std::sync::Arc::clone(self))
    }

    pub(crate) async fn status(&self) -> Result<MindStoreStatus, MindStoreError> {
        let row = query(
            r#"
            SELECT
                (SELECT version FROM yunxi_mind_meta WHERE singleton = TRUE) AS version,
                (SELECT COUNT(*) FROM yunxi_beliefs) AS beliefs,
                (SELECT COUNT(*) FROM yunxi_preferences) AS preferences,
                (SELECT COUNT(*) FROM yunxi_interests) AS interests,
                (SELECT COUNT(*) FROM yunxi_open_questions WHERE status = 'open') AS open_questions,
                (SELECT COUNT(*) FROM yunxi_agenda_items WHERE status = 'active') AS active_agenda
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(MindStoreError::storage)?;
        Ok(MindStoreStatus {
            version: database_count(&row, "version")?,
            beliefs: database_count(&row, "beliefs")?,
            preferences: database_count(&row, "preferences")?,
            interests: database_count(&row, "interests")?,
            open_questions: database_count(&row, "open_questions")?,
            active_agenda: database_count(&row, "active_agenda")?,
        })
    }

    pub(crate) async fn cleanup(&self, now: DateTime<Utc>) -> Result<u64, MindStoreError> {
        let mut transaction = self.pool.begin().await.map_err(MindStoreError::storage)?;
        lock_meta(&mut transaction).await?;
        let model_config = crate::config::get();
        let memory_config = model_config.memory();
        let episode_retention_days = memory_config.episode_retention_days();
        let episode_cutoff = now - chrono::Duration::days(episode_retention_days);
        let episode_protected_salience = f64::from(memory_config.episode_protected_salience());
        let episode_max_per_scope =
            i64::try_from(memory_config.episode_max_per_scope()).map_err(|_| {
                MindStoreError::InvalidRequest {
                    reason: "episode scope capacity exceeds PostgreSQL BIGINT",
                }
            })?;
        let orphaned_agenda = query(
            r#"
            DELETE FROM yunxi_agenda_items AS agenda
            WHERE agenda.status IN ('active', 'deferred')
              AND (
                    (
                        agenda.dedupe_key LIKE 'curiosity:%'
                        AND NOT EXISTS (
                            SELECT 1
                            FROM yunxi_curiosities AS curiosity
                            WHERE agenda.dedupe_key = 'curiosity:' || curiosity.id::text
                              AND curiosity.status IN ('open', 'asked')
                              AND (curiosity.expires_at IS NULL OR curiosity.expires_at > $1)
                        )
                    )
                    OR
                    (
                        agenda.dedupe_key LIKE 'open_question:%'
                        AND NOT EXISTS (
                            SELECT 1
                            FROM yunxi_open_questions AS question
                            WHERE agenda.dedupe_key = 'open_question:' || question.id::text
                              AND question.status = 'open'
                        )
                    )
              )
            "#,
        )
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(MindStoreError::storage)?
        .rows_affected();
        let curiosities = query(
            "DELETE FROM yunxi_curiosities WHERE expires_at IS NOT NULL AND expires_at <= $1",
        )
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(MindStoreError::storage)?
        .rows_affected();
        let agenda = query(
            "DELETE FROM yunxi_agenda_items WHERE status IN ('resolved', 'dropped') AND updated_at < $1",
        )
        .bind(now - chrono::Duration::days(7))
        .execute(&mut *transaction)
        .await
        .map_err(MindStoreError::storage)?
        .rows_affected();
        // Episodes are intentionally retained longer than the V1 memory cache,
        // but known statuses still need a deterministic per-scope bound. Age
        // cleanup only removes resolved, low-value rows. Capacity cleanup
        // ranks known rows by protection priority; unknown statuses are
        // deliberately excluded so a future status can never be deleted by
        // an older binary (fail closed).
        let expired_episodes = query(
            r#"
            DELETE FROM yunxi_episodes
            WHERE occurred_at < $1
              AND status = 'resolved'
              AND primary_score < $2
              AND secondary_score < $2
            "#,
        )
        .bind(episode_cutoff)
        .bind(episode_protected_salience)
        .execute(&mut *transaction)
        .await
        .map_err(MindStoreError::storage)?
        .rows_affected();
        let capped_episodes = query(
            r#"
            WITH ranked AS (
                SELECT
                    id,
                    primary_score,
                    secondary_score,
                    ROW_NUMBER() OVER (
                        PARTITION BY scope_kind, scope_id
                        ORDER BY
                            CASE
                                WHEN status = 'unresolved' THEN 2
                                WHEN primary_score >= $2
                                  OR secondary_score >= $2 THEN 1
                                ELSE 0
                            END DESC,
                            primary_score DESC,
                            secondary_score DESC,
                            occurred_at DESC,
                            id
                    ) AS retention_rank
                FROM yunxi_episodes
                WHERE status IN ('resolved', 'unresolved')
            ), evictable AS (
                SELECT id
                FROM ranked
                WHERE retention_rank > $1
            )
            DELETE FROM yunxi_episodes AS episode
            USING evictable
            WHERE episode.id = evictable.id
            "#,
        )
        .bind(episode_max_per_scope)
        .bind(episode_protected_salience)
        .execute(&mut *transaction)
        .await
        .map_err(MindStoreError::storage)?
        .rows_affected();
        let removed = orphaned_agenda
            .saturating_add(curiosities)
            .saturating_add(agenda)
            .saturating_add(expired_episodes)
            .saturating_add(capped_episodes);
        if removed > 0 {
            bump_meta(&mut transaction).await?;
        }
        transaction
            .commit()
            .await
            .map_err(MindStoreError::storage)?;
        Ok(removed)
    }
}

fn database_count(row: &sqlx_postgres::PgRow, column: &str) -> Result<u64, MindStoreError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(MindStoreError::storage)?;
    u64::try_from(value).map_err(MindStoreError::storage)
}

fn scope_columns(scope: MindScope) -> (&'static str, Option<Uuid>, String) {
    match scope {
        MindScope::Global => ("global", None, GLOBAL_SCOPE_KEY.to_owned()),
        MindScope::Person { person_id } => {
            let id = person_id.into_uuid();
            ("person", Some(id), format!("person:{id}"))
        }
        MindScope::Conversation { conversation_id } => {
            let id = conversation_id.into_uuid();
            ("conversation", Some(id), format!("conversation:{id}"))
        }
    }
}

fn scope_keys(scopes: &[MindScope]) -> Vec<String> {
    scopes
        .iter()
        .copied()
        .map(|scope| scope_columns(scope).2)
        .collect()
}

fn validate_limit(limit: usize) -> Result<i64, MindStoreError> {
    if limit > MAX_LIST_LIMIT {
        return Err(MindStoreError::InvalidRequest {
            reason: "mind query limit exceeds 128",
        });
    }
    Ok(limit as i64)
}

fn storage_validation_config() -> ConsolidationConfig {
    ConsolidationConfig {
        max_belief_delta: 1.0,
        max_preference_delta: 1.0,
        max_interest_affinity_delta: 1.0,
        max_updates_per_reflection: 128,
    }
}

async fn lock_meta(transaction: &mut Transaction<'_, Postgres>) -> Result<u64, MindStoreError> {
    let row = query("SELECT version FROM yunxi_mind_meta WHERE singleton = TRUE FOR UPDATE")
        .fetch_one(&mut **transaction)
        .await
        .map_err(MindStoreError::storage)?;
    let version: i64 = row.try_get("version").map_err(MindStoreError::storage)?;
    u64::try_from(version).map_err(|_| MindStoreError::InvalidRequest {
        reason: "stored mind version is negative",
    })
}

async fn bump_meta(transaction: &mut Transaction<'_, Postgres>) -> Result<u64, MindStoreError> {
    let row = query(
        "UPDATE yunxi_mind_meta SET version = version + 1, updated_at = now() WHERE singleton = TRUE RETURNING version",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(MindStoreError::storage)?;
    let version: i64 = row.try_get("version").map_err(MindStoreError::storage)?;
    u64::try_from(version).map_err(|_| MindStoreError::InvalidRequest {
        reason: "stored mind version is negative",
    })
}

async fn payload_by_id<T: DeserializeOwned>(
    pool: &PgPool,
    table: RecordTable,
    id: Uuid,
) -> Result<Option<T>, MindStoreError> {
    let statement = format!("SELECT payload FROM {} WHERE id = $1", table.name());
    let row = query(&statement)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MindStoreError::storage)?;
    row.map(|row| decode_row(&row)).transpose()
}

async fn payload_by_id_tx<T: DeserializeOwned>(
    transaction: &mut Transaction<'_, Postgres>,
    table: RecordTable,
    id: Uuid,
) -> Result<Option<T>, MindStoreError> {
    let statement = format!("SELECT payload FROM {} WHERE id = $1", table.name());
    let row = query(&statement)
        .bind(id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(MindStoreError::storage)?;
    row.map(|row| decode_row(&row)).transpose()
}

async fn payload_by_key<T: DeserializeOwned>(
    pool: &PgPool,
    table: RecordTable,
    scope: MindScope,
    dedupe_key: &str,
    active_statuses: Option<&[&str]>,
) -> Result<Option<T>, MindStoreError> {
    let (_, _, scope_key) = scope_columns(scope);
    let statement = if active_statuses.is_some() {
        format!(
            "SELECT payload FROM {} WHERE scope_key = $1 AND dedupe_key = $2 AND status = ANY($3) LIMIT 1",
            table.name()
        )
    } else {
        format!(
            "SELECT payload FROM {} WHERE scope_key = $1 AND dedupe_key = $2 LIMIT 1",
            table.name()
        )
    };
    let mut query = query(&statement).bind(scope_key).bind(dedupe_key);
    if let Some(statuses) = active_statuses {
        query = query.bind(statuses);
    }
    let row = query
        .fetch_optional(pool)
        .await
        .map_err(MindStoreError::storage)?;
    row.map(|row| decode_row(&row)).transpose()
}

async fn list_payloads<T: DeserializeOwned>(
    pool: &PgPool,
    table: RecordTable,
    scopes: &[MindScope],
    search: &str,
    statuses: Option<&[&str]>,
    now: Option<DateTime<Utc>>,
    limit: usize,
) -> Result<Vec<T>, MindStoreError> {
    let limit = validate_limit(limit)?;
    if limit == 0 || scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scope_keys = scope_keys(scopes);
    let search = search.trim();
    let search_enabled = !search.is_empty();
    let search_terms = lexical_terms(search);
    if search_enabled && search_terms.is_empty() {
        return Ok(Vec::new());
    }
    let statement = format!(
        r#"
        SELECT payload
        FROM {}
        WHERE scope_key = ANY($1)
          AND ($2::TEXT[] IS NULL OR status = ANY($2))
          AND ($3::TIMESTAMPTZ IS NULL OR expires_at IS NULL OR expires_at > $3)
          AND (
            NOT $4::BOOLEAN
            OR EXISTS (
              SELECT 1
              FROM unnest($5::TEXT[]) AS search_term
              WHERE CASE
                WHEN search_term ~ '^[a-z0-9]+$' THEN
                  search_term = ANY(
                    regexp_split_to_array(lower(dedupe_key), '[^a-z0-9]+')
                  )
                ELSE strpos(lower(dedupe_key), search_term) > 0
              END
            )
          )
        ORDER BY
          CASE WHEN NOT $4::BOOLEAN THEN 0 ELSE (
            SELECT COUNT(*)
            FROM unnest($5::TEXT[]) AS search_term
            WHERE CASE
              WHEN search_term ~ '^[a-z0-9]+$' THEN
                search_term = ANY(
                  regexp_split_to_array(lower(dedupe_key), '[^a-z0-9]+')
                )
              ELSE strpos(lower(dedupe_key), search_term) > 0
            END
          ) END DESC,
          primary_score DESC,
          secondary_score DESC,
          updated_at DESC,
          id
        LIMIT $6
        "#,
        table.name()
    );
    let status_values = statuses.map(|values| values.to_vec());
    let rows = query(&statement)
        .bind(scope_keys)
        .bind(status_values)
        .bind(now)
        .bind(search_enabled)
        .bind(search_terms)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(MindStoreError::storage)?;
    rows.iter().map(decode_row).collect()
}

fn decode_row<T: DeserializeOwned>(row: &sqlx_postgres::PgRow) -> Result<T, MindStoreError> {
    let value: Value = row.try_get("payload").map_err(MindStoreError::storage)?;
    serde_json::from_value(value).map_err(MindStoreError::storage)
}

async fn put_record<T: Serialize>(
    pool: &PgPool,
    table: RecordTable,
    record: StoredRecord<'_, T>,
    expected_version: Option<u64>,
) -> Result<Value, MindStoreError> {
    let mut transaction = pool.begin().await.map_err(MindStoreError::storage)?;
    lock_meta(&mut transaction).await?;
    let payload = put_record_tx(&mut transaction, table, record, expected_version).await?;
    bump_meta(&mut transaction).await?;
    transaction
        .commit()
        .await
        .map_err(MindStoreError::storage)?;
    Ok(payload)
}

async fn put_record_tx<T: Serialize>(
    transaction: &mut Transaction<'_, Postgres>,
    table: RecordTable,
    record: StoredRecord<'_, T>,
    expected_version: Option<u64>,
) -> Result<Value, MindStoreError> {
    match expected_version {
        None if record.version != 1 => {
            return Err(MindStoreError::InvalidRequest {
                reason: "new mind records must start at version 1",
            });
        }
        Some(expected) if record.version != expected.saturating_add(1) => {
            return Err(MindStoreError::VersionConflict {
                kind: table.kind(),
                id: record.id.to_string(),
                expected: expected.saturating_add(1),
                actual: record.version,
            });
        }
        _ => {}
    }
    let (scope_kind, scope_id, scope_key) = scope_columns(record.scope);
    let payload = serde_json::to_value(record.value).map_err(MindStoreError::storage)?;
    let version = i64::try_from(record.version).map_err(|_| MindStoreError::InvalidRequest {
        reason: "mind record version exceeds PostgreSQL BIGINT",
    })?;
    let returned = if let Some(expected) = expected_version {
        let statement = format!(
            r#"
            UPDATE {}
            SET scope_kind = $2, scope_id = $3, scope_key = $4,
                person_id = $5, conversation_id = $6, participant_ids = $7,
                dedupe_key = $8, status = $9, primary_score = $10,
                secondary_score = $11, expires_at = $12, occurred_at = $13,
                updated_at = $14, version = $15, payload = $16
            WHERE id = $1 AND version = $17
            RETURNING payload
            "#,
            table.name()
        );
        query(&statement)
            .bind(record.id)
            .bind(scope_kind)
            .bind(scope_id)
            .bind(scope_key)
            .bind(record.person_id)
            .bind(record.conversation_id)
            .bind(record.participant_ids)
            .bind(&record.dedupe_key)
            .bind(record.status)
            .bind(record.primary_score)
            .bind(record.secondary_score)
            .bind(record.expires_at)
            .bind(record.occurred_at)
            .bind(record.updated_at)
            .bind(version)
            .bind(payload)
            .bind(
                i64::try_from(expected).map_err(|_| MindStoreError::InvalidRequest {
                    reason: "expected mind version exceeds PostgreSQL BIGINT",
                })?,
            )
            .fetch_optional(&mut **transaction)
            .await
            .map_err(MindStoreError::storage)?
    } else {
        let statement = format!(
            r#"
            INSERT INTO {} (
                id, scope_kind, scope_id, scope_key, person_id, conversation_id,
                participant_ids, dedupe_key, status, primary_score, secondary_score,
                expires_at, occurred_at, updated_at, version, payload
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
            ON CONFLICT DO NOTHING
            RETURNING payload
            "#,
            table.name()
        );
        query(&statement)
            .bind(record.id)
            .bind(scope_kind)
            .bind(scope_id)
            .bind(scope_key)
            .bind(record.person_id)
            .bind(record.conversation_id)
            .bind(record.participant_ids)
            .bind(&record.dedupe_key)
            .bind(record.status)
            .bind(record.primary_score)
            .bind(record.secondary_score)
            .bind(record.expires_at)
            .bind(record.occurred_at)
            .bind(record.updated_at)
            .bind(version)
            .bind(payload)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(MindStoreError::storage)?
    };
    if let Some(row) = returned {
        return row.try_get("payload").map_err(MindStoreError::storage);
    }
    let statement = format!(
        "SELECT id, version FROM {} WHERE id = $1 OR (scope_key = $2 AND dedupe_key = $3) LIMIT 1",
        table.name()
    );
    let conflict = query(&statement)
        .bind(record.id)
        .bind(scope_columns(record.scope).2)
        .bind(&record.dedupe_key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(MindStoreError::storage)?;
    let (id, actual) = conflict.map_or((record.id, 0), |row| {
        (
            row.try_get::<Uuid, _>("id").unwrap_or(record.id),
            row.try_get::<i64, _>("version").unwrap_or(0).max(0) as u64,
        )
    });
    Err(MindStoreError::VersionConflict {
        kind: table.kind(),
        id: id.to_string(),
        expected: expected_version.unwrap_or(0),
        actual,
    })
}

fn decode_and_validate<T>(
    payload: Value,
    validate: impl FnOnce(&T) -> Result<(), yunxi_core::MindValidationError>,
) -> Result<T, MindStoreError>
where
    T: DeserializeOwned,
{
    let value: T = serde_json::from_value(payload).map_err(MindStoreError::storage)?;
    validate(&value)?;
    Ok(value)
}

fn scope_ids(scope: MindScope) -> (Option<Uuid>, Option<Uuid>) {
    match scope {
        MindScope::Global => (None, None),
        MindScope::Person { person_id } => (Some(person_id.into_uuid()), None),
        MindScope::Conversation { conversation_id } => (None, Some(conversation_id.into_uuid())),
    }
}

fn curiosity_status(status: CuriosityStatus) -> &'static str {
    match status {
        CuriosityStatus::Open => "open",
        CuriosityStatus::Asked => "asked",
        CuriosityStatus::Resolved => "resolved",
        CuriosityStatus::Dropped => "dropped",
        CuriosityStatus::Expired => "expired",
    }
}

fn open_question_status(status: OpenQuestionStatus) -> &'static str {
    match status {
        OpenQuestionStatus::Open => "open",
        OpenQuestionStatus::Resolved => "resolved",
        OpenQuestionStatus::Dropped => "dropped",
    }
}

fn agenda_status(status: AgendaStatus) -> &'static str {
    match status {
        AgendaStatus::Active => "active",
        AgendaStatus::Deferred => "deferred",
        AgendaStatus::Resolved => "resolved",
        AgendaStatus::Dropped => "dropped",
    }
}

fn belief_record(belief: &Belief) -> StoredRecord<'_, Belief> {
    let (person_id, conversation_id) = scope_ids(belief.scope());
    StoredRecord {
        id: belief.id().into_uuid(),
        scope: belief.scope(),
        person_id,
        conversation_id,
        participant_ids: Vec::new(),
        dedupe_key: belief.proposition_key().to_owned(),
        status: "active",
        primary_score: f64::from(belief.confidence()),
        secondary_score: f64::from(belief.stability()),
        expires_at: belief.valid_until(),
        occurred_at: belief.created_at(),
        updated_at: belief.updated_at(),
        version: belief.version(),
        value: belief,
    }
}

fn preference_record(preference: &Preference) -> StoredRecord<'_, Preference> {
    StoredRecord {
        id: preference.id().into_uuid(),
        scope: MindScope::Global,
        person_id: None,
        conversation_id: None,
        participant_ids: Vec::new(),
        dedupe_key: preference.subject_key().to_owned(),
        status: "active",
        primary_score: f64::from(preference.intensity() * preference.confidence()),
        secondary_score: f64::from(preference.valence().abs()),
        expires_at: None,
        occurred_at: preference.updated_at(),
        updated_at: preference.updated_at(),
        version: preference.version(),
        value: preference,
    }
}

fn interest_record(interest: &Interest) -> StoredRecord<'_, Interest> {
    StoredRecord {
        id: interest.id().into_uuid(),
        scope: MindScope::Global,
        person_id: None,
        conversation_id: None,
        participant_ids: Vec::new(),
        dedupe_key: interest.topic_key().to_owned(),
        status: "active",
        primary_score: f64::from(interest.activation()),
        secondary_score: f64::from(interest.long_term_affinity()),
        expires_at: None,
        occurred_at: interest.updated_at(),
        updated_at: interest.updated_at(),
        version: interest.version(),
        value: interest,
    }
}

fn curiosity_record(curiosity: &CuriosityItem) -> StoredRecord<'_, CuriosityItem> {
    let (_, scope_conversation) = scope_ids(curiosity.scope());
    StoredRecord {
        id: curiosity.id().into_uuid(),
        scope: curiosity.scope(),
        person_id: curiosity.subject().map(yunxi_core::PersonId::into_uuid),
        conversation_id: curiosity
            .conversation_id()
            .map(yunxi_core::ConversationId::into_uuid)
            .or(scope_conversation),
        participant_ids: curiosity
            .subject()
            .map(yunxi_core::PersonId::into_uuid)
            .into_iter()
            .collect(),
        dedupe_key: curiosity.question_key().to_owned(),
        status: curiosity_status(curiosity.status()),
        primary_score: f64::from(curiosity.salience()),
        secondary_score: 0.0,
        expires_at: curiosity.expires_at(),
        occurred_at: curiosity.updated_at(),
        updated_at: curiosity.updated_at(),
        version: curiosity.version(),
        value: curiosity,
    }
}

fn open_question_record(question: &OpenQuestion) -> StoredRecord<'_, OpenQuestion> {
    let (person_id, conversation_id) = scope_ids(question.scope());
    StoredRecord {
        id: question.id().into_uuid(),
        scope: question.scope(),
        person_id,
        conversation_id,
        participant_ids: Vec::new(),
        dedupe_key: question.question_key().to_owned(),
        status: open_question_status(question.status()),
        primary_score: f64::from(question.salience()),
        secondary_score: 0.0,
        expires_at: None,
        occurred_at: question.updated_at(),
        updated_at: question.updated_at(),
        version: question.version(),
        value: question,
    }
}

fn agenda_record(item: &AgendaItem) -> StoredRecord<'_, AgendaItem> {
    let (person_id, conversation_id) = scope_ids(item.scope());
    StoredRecord {
        id: item.id().into_uuid(),
        scope: item.scope(),
        person_id,
        conversation_id,
        participant_ids: Vec::new(),
        dedupe_key: item.subject().dedupe_key(),
        status: agenda_status(item.status()),
        primary_score: f64::from(item.salience()),
        secondary_score: f64::from(item.activation()),
        expires_at: item.cooldown_until(),
        occurred_at: item.updated_at(),
        updated_at: item.updated_at(),
        version: item.version(),
        value: item,
    }
}

fn episode_record(episode: &Episode) -> StoredRecord<'_, Episode> {
    let (person_id, conversation_id) = scope_ids(episode.scope());
    StoredRecord {
        id: episode.id().into_uuid(),
        scope: episode.scope(),
        person_id,
        conversation_id,
        participant_ids: episode
            .participants()
            .iter()
            .copied()
            .map(yunxi_core::PersonId::into_uuid)
            .collect(),
        dedupe_key: episode.id().to_string(),
        status: if episode.unresolved() {
            "unresolved"
        } else {
            "resolved"
        },
        primary_score: f64::from(episode.salience()),
        secondary_score: f64::from(episode.emotional_weight().abs()),
        expires_at: None,
        occurred_at: episode.occurred_at(),
        updated_at: episode.occurred_at(),
        version: episode.version(),
        value: episode,
    }
}

impl SelfModelStore for PostgresMindStore {
    fn get(&self) -> MindStoreFuture<'_, Option<SelfModel>> {
        Box::pin(async move {
            let row = query("SELECT payload FROM yunxi_self_model WHERE singleton = TRUE")
                .fetch_optional(&self.pool)
                .await
                .map_err(MindStoreError::storage)?;
            row.map(|row| {
                let value = decode_row::<SelfModel>(&row)?;
                value.validate()?;
                Ok(value)
            })
            .transpose()
        })
    }

    fn put<'a>(
        &'a self,
        model: &'a SelfModel,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, SelfModel> {
        Box::pin(async move {
            model.validate()?;
            match expected_version {
                None if model.version() != 1 => {
                    return Err(MindStoreError::InvalidRequest {
                        reason: "new self model must start at version 1",
                    });
                }
                Some(expected) if model.version() != expected.saturating_add(1) => {
                    return Err(MindStoreError::VersionConflict {
                        kind: "self_model",
                        id: "singleton".to_owned(),
                        expected: expected.saturating_add(1),
                        actual: model.version(),
                    });
                }
                _ => {}
            }
            let payload = serde_json::to_value(model).map_err(MindStoreError::storage)?;
            let mut transaction = self.pool.begin().await.map_err(MindStoreError::storage)?;
            lock_meta(&mut transaction).await?;
            let returned = if let Some(expected) = expected_version {
                query(
                    r#"
                    UPDATE yunxi_self_model
                    SET payload = $1, version = $2, updated_at = $3
                    WHERE singleton = TRUE AND version = $4
                    RETURNING payload
                    "#,
                )
                .bind(payload)
                .bind(i64::try_from(model.version()).map_err(|_| {
                    MindStoreError::InvalidRequest {
                        reason: "self-model version exceeds PostgreSQL BIGINT",
                    }
                })?)
                .bind(model.updated_at())
                .bind(
                    i64::try_from(expected).map_err(|_| MindStoreError::InvalidRequest {
                        reason: "expected self-model version exceeds PostgreSQL BIGINT",
                    })?,
                )
                .fetch_optional(&mut *transaction)
                .await
                .map_err(MindStoreError::storage)?
            } else {
                query(
                    r#"
                    INSERT INTO yunxi_self_model (singleton, payload, version, updated_at)
                    VALUES (TRUE, $1, 1, $2)
                    ON CONFLICT DO NOTHING
                    RETURNING payload
                    "#,
                )
                .bind(payload)
                .bind(model.updated_at())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(MindStoreError::storage)?
            };
            let Some(row) = returned else {
                let row = query("SELECT version FROM yunxi_self_model WHERE singleton = TRUE")
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(MindStoreError::storage)?;
                let actual = row
                    .and_then(|row| row.try_get::<i64, _>("version").ok())
                    .unwrap_or(0)
                    .max(0) as u64;
                return Err(MindStoreError::VersionConflict {
                    kind: "self_model",
                    id: "singleton".to_owned(),
                    expected: expected_version.unwrap_or(0),
                    actual,
                });
            };
            bump_meta(&mut transaction).await?;
            transaction
                .commit()
                .await
                .map_err(MindStoreError::storage)?;
            let payload: Value = row.try_get("payload").map_err(MindStoreError::storage)?;
            decode_and_validate(payload, SelfModel::validate)
        })
    }
}

impl BeliefStore for PostgresMindStore {
    fn get(&self, id: BeliefId) -> MindStoreFuture<'_, Option<Belief>> {
        Box::pin(async move {
            let value = payload_by_id(&self.pool, RecordTable::Beliefs, id.into_uuid()).await?;
            if let Some(value) = &value {
                Belief::validate(value)?;
            }
            Ok(value)
        })
    }

    fn find_by_key<'a>(
        &'a self,
        scope: MindScope,
        proposition_key: &'a str,
    ) -> MindStoreFuture<'a, Option<Belief>> {
        Box::pin(async move {
            let value = payload_by_key(
                &self.pool,
                RecordTable::Beliefs,
                scope,
                proposition_key,
                None,
            )
            .await?;
            if let Some(value) = &value {
                Belief::validate(value)?;
            }
            Ok(value)
        })
    }

    fn put<'a>(
        &'a self,
        belief: &'a Belief,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Belief> {
        Box::pin(async move {
            belief.validate()?;
            let payload = put_record(
                &self.pool,
                RecordTable::Beliefs,
                belief_record(belief),
                expected_version,
            )
            .await?;
            decode_and_validate(payload, Belief::validate)
        })
    }

    fn relevant<'a>(
        &'a self,
        scopes: &'a [MindScope],
        query_text: &'a str,
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Belief>> {
        Box::pin(async move {
            let values: Vec<Belief> = list_payloads(
                &self.pool,
                RecordTable::Beliefs,
                scopes,
                query_text,
                Some(&["active"]),
                Some(now),
                limit,
            )
            .await?;
            for value in &values {
                value.validate()?;
            }
            Ok(values)
        })
    }
}

impl PreferenceStore for PostgresMindStore {
    fn get(&self, id: PreferenceId) -> MindStoreFuture<'_, Option<Preference>> {
        Box::pin(async move {
            let value = payload_by_id(&self.pool, RecordTable::Preferences, id.into_uuid()).await?;
            if let Some(value) = &value {
                Preference::validate(value)?;
            }
            Ok(value)
        })
    }

    fn find_by_key<'a>(&'a self, subject_key: &'a str) -> MindStoreFuture<'a, Option<Preference>> {
        Box::pin(async move {
            let value = payload_by_key(
                &self.pool,
                RecordTable::Preferences,
                MindScope::Global,
                subject_key,
                None,
            )
            .await?;
            if let Some(value) = &value {
                Preference::validate(value)?;
            }
            Ok(value)
        })
    }

    fn put<'a>(
        &'a self,
        preference: &'a Preference,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Preference> {
        Box::pin(async move {
            preference.validate()?;
            let payload = put_record(
                &self.pool,
                RecordTable::Preferences,
                preference_record(preference),
                expected_version,
            )
            .await?;
            decode_and_validate(payload, Preference::validate)
        })
    }

    fn relevant<'a>(
        &'a self,
        query_text: &'a str,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Preference>> {
        Box::pin(async move {
            let values: Vec<Preference> = list_payloads(
                &self.pool,
                RecordTable::Preferences,
                &[MindScope::Global],
                query_text,
                Some(&["active"]),
                None,
                limit,
            )
            .await?;
            for value in &values {
                value.validate()?;
            }
            Ok(values)
        })
    }
}

impl InterestStore for PostgresMindStore {
    fn get(&self, id: InterestId) -> MindStoreFuture<'_, Option<Interest>> {
        Box::pin(async move {
            let value = payload_by_id(&self.pool, RecordTable::Interests, id.into_uuid()).await?;
            if let Some(value) = &value {
                Interest::validate(value)?;
            }
            Ok(value)
        })
    }

    fn find_by_key<'a>(&'a self, topic_key: &'a str) -> MindStoreFuture<'a, Option<Interest>> {
        Box::pin(async move {
            let value = payload_by_key(
                &self.pool,
                RecordTable::Interests,
                MindScope::Global,
                topic_key,
                None,
            )
            .await?;
            if let Some(value) = &value {
                Interest::validate(value)?;
            }
            Ok(value)
        })
    }

    fn put<'a>(
        &'a self,
        interest: &'a Interest,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Interest> {
        Box::pin(async move {
            interest.validate()?;
            let payload = put_record(
                &self.pool,
                RecordTable::Interests,
                interest_record(interest),
                expected_version,
            )
            .await?;
            decode_and_validate(payload, Interest::validate)
        })
    }

    fn relevant<'a>(
        &'a self,
        query_text: &'a str,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Interest>> {
        Box::pin(async move {
            let values: Vec<Interest> = list_payloads(
                &self.pool,
                RecordTable::Interests,
                &[MindScope::Global],
                query_text,
                Some(&["active"]),
                None,
                limit,
            )
            .await?;
            for value in &values {
                value.validate()?;
            }
            Ok(values)
        })
    }
}

impl CuriosityStore for PostgresMindStore {
    fn get(&self, id: CuriosityId) -> MindStoreFuture<'_, Option<CuriosityItem>> {
        Box::pin(async move {
            let value = payload_by_id(&self.pool, RecordTable::Curiosities, id.into_uuid()).await?;
            if let Some(value) = &value {
                CuriosityItem::validate(value)?;
            }
            Ok(value)
        })
    }

    fn find_open_by_key<'a>(
        &'a self,
        scope: MindScope,
        question_key: &'a str,
    ) -> MindStoreFuture<'a, Option<CuriosityItem>> {
        Box::pin(async move {
            let value = payload_by_key(
                &self.pool,
                RecordTable::Curiosities,
                scope,
                question_key,
                Some(&["open", "asked"]),
            )
            .await?;
            if let Some(value) = &value {
                CuriosityItem::validate(value)?;
            }
            Ok(value)
        })
    }

    fn put<'a>(
        &'a self,
        curiosity: &'a CuriosityItem,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, CuriosityItem> {
        Box::pin(async move {
            curiosity.validate()?;
            let payload = put_record(
                &self.pool,
                RecordTable::Curiosities,
                curiosity_record(curiosity),
                expected_version,
            )
            .await?;
            decode_and_validate(payload, CuriosityItem::validate)
        })
    }

    fn list_open<'a>(
        &'a self,
        scopes: &'a [MindScope],
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<CuriosityItem>> {
        Box::pin(async move {
            let values: Vec<CuriosityItem> = list_payloads(
                &self.pool,
                RecordTable::Curiosities,
                scopes,
                "",
                Some(&["open", "asked"]),
                Some(now),
                limit,
            )
            .await?;
            for value in &values {
                value.validate()?;
            }
            Ok(values)
        })
    }
}

impl OpenQuestionStore for PostgresMindStore {
    fn get(&self, id: OpenQuestionId) -> MindStoreFuture<'_, Option<OpenQuestion>> {
        Box::pin(async move {
            let value =
                payload_by_id(&self.pool, RecordTable::OpenQuestions, id.into_uuid()).await?;
            if let Some(value) = &value {
                OpenQuestion::validate(value)?;
            }
            Ok(value)
        })
    }

    fn find_open_by_key<'a>(
        &'a self,
        scope: MindScope,
        question_key: &'a str,
    ) -> MindStoreFuture<'a, Option<OpenQuestion>> {
        Box::pin(async move {
            let value = payload_by_key(
                &self.pool,
                RecordTable::OpenQuestions,
                scope,
                question_key,
                Some(&["open"]),
            )
            .await?;
            if let Some(value) = &value {
                OpenQuestion::validate(value)?;
            }
            Ok(value)
        })
    }

    fn put<'a>(
        &'a self,
        question: &'a OpenQuestion,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, OpenQuestion> {
        Box::pin(async move {
            question.validate()?;
            let payload = put_record(
                &self.pool,
                RecordTable::OpenQuestions,
                open_question_record(question),
                expected_version,
            )
            .await?;
            decode_and_validate(payload, OpenQuestion::validate)
        })
    }

    fn list_open<'a>(
        &'a self,
        scopes: &'a [MindScope],
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<OpenQuestion>> {
        Box::pin(async move {
            let values: Vec<OpenQuestion> = list_payloads(
                &self.pool,
                RecordTable::OpenQuestions,
                scopes,
                "",
                Some(&["open"]),
                None,
                limit,
            )
            .await?;
            for value in &values {
                value.validate()?;
            }
            Ok(values)
        })
    }
}

impl AgendaStore for PostgresMindStore {
    fn get(&self, id: AgendaItemId) -> MindStoreFuture<'_, Option<AgendaItem>> {
        Box::pin(async move {
            let value = payload_by_id(&self.pool, RecordTable::Agenda, id.into_uuid()).await?;
            if let Some(value) = &value {
                AgendaItem::validate(value)?;
            }
            Ok(value)
        })
    }

    fn find_active_by_key<'a>(
        &'a self,
        scope: MindScope,
        subject_key: &'a str,
    ) -> MindStoreFuture<'a, Option<AgendaItem>> {
        Box::pin(async move {
            let value = payload_by_key(
                &self.pool,
                RecordTable::Agenda,
                scope,
                subject_key,
                Some(&["active", "deferred"]),
            )
            .await?;
            if let Some(value) = &value {
                AgendaItem::validate(value)?;
            }
            Ok(value)
        })
    }

    fn put<'a>(
        &'a self,
        item: &'a AgendaItem,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, AgendaItem> {
        Box::pin(async move {
            item.validate()?;
            let payload = put_record(
                &self.pool,
                RecordTable::Agenda,
                agenda_record(item),
                expected_version,
            )
            .await?;
            decode_and_validate(payload, AgendaItem::validate)
        })
    }

    fn list_active<'a>(
        &'a self,
        scopes: &'a [MindScope],
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<AgendaItem>> {
        Box::pin(async move {
            let limit = validate_limit(limit)?;
            if limit == 0 || scopes.is_empty() {
                return Ok(Vec::new());
            }
            // For agenda rows `expires_at` carries cooldown_until, so an item
            // becomes eligible after that timestamp rather than before it.
            let rows = query(
                r#"
                SELECT payload
                FROM yunxi_agenda_items
                WHERE scope_key = ANY($1)
                  AND status = 'active'
                  AND (expires_at IS NULL OR expires_at <= $2)
                ORDER BY primary_score DESC, secondary_score DESC, updated_at DESC, id
                LIMIT $3
                "#,
            )
            .bind(scope_keys(scopes))
            .bind(now)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(MindStoreError::storage)?;
            let values: Vec<AgendaItem> =
                rows.iter().map(decode_row).collect::<Result<Vec<_>, _>>()?;
            for value in &values {
                value.validate()?;
            }
            Ok(values)
        })
    }
}

impl EpisodeStore for PostgresMindStore {
    fn put<'a>(&'a self, episode: &'a Episode) -> MindStoreFuture<'a, Episode> {
        Box::pin(async move {
            episode.validate()?;
            // The read, idempotence check, and insert must share the same
            // metadata row lock. Otherwise cleanup/erasure can delete a row
            // after the preflight read and the stale writer can resurrect it.
            let mut transaction = self.pool.begin().await.map_err(MindStoreError::storage)?;
            lock_meta(&mut transaction).await?;
            if let Some(existing) = payload_by_id_tx::<Episode>(
                &mut transaction,
                RecordTable::Episodes,
                episode.id().into_uuid(),
            )
            .await?
            {
                existing.validate()?;
                if existing == *episode {
                    transaction
                        .commit()
                        .await
                        .map_err(MindStoreError::storage)?;
                    return Ok(existing);
                }
                return Err(MindStoreError::VersionConflict {
                    kind: "episode",
                    id: episode.id().to_string(),
                    expected: 0,
                    actual: existing.version(),
                });
            }
            let payload = put_record_tx(
                &mut transaction,
                RecordTable::Episodes,
                episode_record(episode),
                None,
            )
            .await?;
            bump_meta(&mut transaction).await?;
            transaction
                .commit()
                .await
                .map_err(MindStoreError::storage)?;
            decode_and_validate(payload, Episode::validate)
        })
    }

    fn list_recent<'a>(
        &'a self,
        scopes: &'a [MindScope],
        since: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Episode>> {
        Box::pin(async move {
            let limit = validate_limit(limit)?;
            if limit == 0 || scopes.is_empty() {
                return Ok(Vec::new());
            }
            let rows = query(
                r#"
                SELECT payload
                FROM yunxi_episodes
                WHERE scope_key = ANY($1) AND occurred_at >= $2
                ORDER BY occurred_at DESC, primary_score DESC, id
                LIMIT $3
                "#,
            )
            .bind(scope_keys(scopes))
            .bind(since)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(MindStoreError::storage)?;
            let values = rows.iter().map(decode_row).collect::<Result<Vec<_>, _>>()?;
            for value in &values {
                Episode::validate(value)?;
            }
            Ok(values)
        })
    }
}

impl MindConsolidationStore for PostgresMindStore {
    fn apply<'a>(
        &'a self,
        plan: &'a ConsolidationPlan,
    ) -> MindStoreFuture<'a, ConsolidationResult> {
        Box::pin(async move {
            plan.validate(storage_validation_config())?;
            let mut transaction = self.pool.begin().await.map_err(MindStoreError::storage)?;
            let actual_version = lock_meta(&mut transaction).await?;
            if actual_version != plan.base_mind_version {
                return Err(MindStoreError::VersionConflict {
                    kind: "mind",
                    id: "global".to_owned(),
                    expected: plan.base_mind_version,
                    actual: actual_version,
                });
            }
            let mut applied_updates = 0usize;
            for upsert in &plan.beliefs {
                put_record_tx(
                    &mut transaction,
                    RecordTable::Beliefs,
                    belief_record(&upsert.value),
                    upsert.expected_version,
                )
                .await?;
                applied_updates += 1;
            }
            for upsert in &plan.preferences {
                put_record_tx(
                    &mut transaction,
                    RecordTable::Preferences,
                    preference_record(&upsert.value),
                    upsert.expected_version,
                )
                .await?;
                applied_updates += 1;
            }
            for upsert in &plan.interests {
                put_record_tx(
                    &mut transaction,
                    RecordTable::Interests,
                    interest_record(&upsert.value),
                    upsert.expected_version,
                )
                .await?;
                applied_updates += 1;
            }
            for upsert in &plan.open_questions {
                put_record_tx(
                    &mut transaction,
                    RecordTable::OpenQuestions,
                    open_question_record(&upsert.value),
                    upsert.expected_version,
                )
                .await?;
                applied_updates += 1;
            }
            for upsert in &plan.agenda {
                put_record_tx(
                    &mut transaction,
                    RecordTable::Agenda,
                    agenda_record(&upsert.value),
                    upsert.expected_version,
                )
                .await?;
                applied_updates += 1;
            }
            for episode in &plan.episodes {
                put_record_tx(
                    &mut transaction,
                    RecordTable::Episodes,
                    episode_record(episode),
                    None,
                )
                .await?;
                applied_updates += 1;
            }
            let new_mind_version = bump_meta(&mut transaction).await?;
            transaction
                .commit()
                .await
                .map_err(MindStoreError::storage)?;
            Ok(ConsolidationResult {
                applied_updates,
                new_mind_version,
            })
        })
    }

    fn current_version(&self) -> MindStoreFuture<'_, u64> {
        Box::pin(async move {
            let row = query("SELECT version FROM yunxi_mind_meta WHERE singleton = TRUE")
                .fetch_one(&self.pool)
                .await
                .map_err(MindStoreError::storage)?;
            let version: i64 = row.try_get("version").map_err(MindStoreError::storage)?;
            u64::try_from(version).map_err(|_| MindStoreError::InvalidRequest {
                reason: "stored mind version is negative",
            })
        })
    }
}

impl MindDataErasure for PostgresMindStore {
    fn erase_person(&self, person_id: yunxi_core::PersonId) -> MindDataErasureFuture<'_> {
        Box::pin(async move {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(MindDataErasureError::storage)?;
            lock_meta(&mut transaction)
                .await
                .map_err(mind_erasure_store_error)?;
            let person_id = person_id.into_uuid();
            for table in [
                RecordTable::Beliefs,
                RecordTable::Curiosities,
                RecordTable::OpenQuestions,
                RecordTable::Agenda,
                RecordTable::Episodes,
            ] {
                let statement = format!(
                    "DELETE FROM {} WHERE person_id = $1 OR $1 = ANY(participant_ids)",
                    table.name()
                );
                query(&statement)
                    .bind(person_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(MindDataErasureError::storage)?;
            }
            bump_meta(&mut transaction)
                .await
                .map_err(mind_erasure_store_error)?;
            transaction
                .commit()
                .await
                .map_err(MindDataErasureError::storage)?;
            Ok(())
        })
    }

    fn erase_conversation(
        &self,
        conversation_id: yunxi_core::ConversationId,
    ) -> MindDataErasureFuture<'_> {
        Box::pin(async move {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(MindDataErasureError::storage)?;
            lock_meta(&mut transaction)
                .await
                .map_err(mind_erasure_store_error)?;
            let conversation_id = conversation_id.into_uuid();
            for table in [
                RecordTable::Beliefs,
                RecordTable::Curiosities,
                RecordTable::OpenQuestions,
                RecordTable::Agenda,
                RecordTable::Episodes,
            ] {
                let statement = format!("DELETE FROM {} WHERE conversation_id = $1", table.name());
                query(&statement)
                    .bind(conversation_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(MindDataErasureError::storage)?;
            }
            bump_meta(&mut transaction)
                .await
                .map_err(mind_erasure_store_error)?;
            transaction
                .commit()
                .await
                .map_err(MindDataErasureError::storage)?;
            Ok(())
        })
    }
}

fn mind_erasure_store_error(error: MindStoreError) -> MindDataErasureError {
    MindDataErasureError::storage(error)
}

#[cfg(test)]
mod tests {
    use super::{
        PostgresMindStore, RecordTable, bump_meta, episode_record, lock_meta, put_record_tx,
    };
    use chrono::{Duration, Utc};
    use sqlx_core::query::query;
    use sqlx_core::row::Row;
    use sqlx_postgres::PgPoolOptions;
    use uuid::Uuid;
    use yunxi_core::{
        AgendaItem, AgendaItemId, AgendaSource, AgendaStore, AgendaSubject, Belief, BeliefId,
        BeliefSource, BeliefStore, ConsolidationConfig, ConsolidationPlan, ConversationId,
        CuriosityId, CuriosityItem, CuriosityStore, Episode, EpisodeId, EpisodeStore, EventId,
        Interest, InterestId, InterestStore, MindConsolidationStore, MindDataErasure, MindScope,
        MindSource, MindStoreError, MindUpsert, PersonId, TraceContext,
    };

    fn belief(
        id: BeliefId,
        scope: MindScope,
        proposition: String,
        now: chrono::DateTime<Utc>,
    ) -> Belief {
        Belief::new(
            id,
            scope,
            proposition,
            0.7,
            0.5,
            BeliefSource::Experience,
            Vec::new(),
            None,
            now,
        )
        .expect("test belief should be valid")
    }

    fn episode(
        id: EpisodeId,
        scope: MindScope,
        summary: String,
        salience: f32,
        emotional_weight: f32,
        unresolved: bool,
        occurred_at: chrono::DateTime<Utc>,
    ) -> Episode {
        Episode::new(
            id,
            scope,
            Vec::new(),
            Vec::new(),
            summary,
            salience,
            emotional_weight,
            unresolved,
            MindSource::Reflection,
            occurred_at,
            occurred_at,
        )
        .expect("test episode should be valid")
    }

    async fn delete_test_rows(pool: &sqlx_postgres::PgPool, ids: &[Uuid]) {
        for table in [
            "yunxi_beliefs",
            "yunxi_preferences",
            "yunxi_interests",
            "yunxi_curiosities",
            "yunxi_open_questions",
            "yunxi_agenda_items",
            "yunxi_episodes",
        ] {
            query(&format!("DELETE FROM {table} WHERE id = ANY($1)"))
                .bind(ids)
                .execute(pool)
                .await
                .expect("test rows should be removable");
        }
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_mind_store_contracts_are_durable_bounded_and_atomic() {
        crate::database_test_support::block_on(async {
            let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .connect(&database_url)
                .await
                .expect("should connect to PostgreSQL");
            let store = PostgresMindStore::new(pool.clone());
            store
                .initialize_schema()
                .await
                .expect("first migration should succeed");
            store
                .initialize_schema()
                .await
                .expect("migration should be idempotent");
            store
                .seed_self_model_if_absent()
                .await
                .expect("self model seed should be restart-safe");

            let marker = Uuid::new_v4();
            let now = Utc::now();
            let person_id = PersonId::new();
            let conversation_id = ConversationId::new();
            let mut cleanup_ids = Vec::new();

            let mut global_beliefs = Vec::new();
            for index in 0..3 {
                let value = belief(
                    BeliefId::new(),
                    MindScope::Global,
                    format!("mind postgres test {marker} belief {index}"),
                    now,
                );
                cleanup_ids.push(value.id().into_uuid());
                BeliefStore::put(&store, &value, None)
                    .await
                    .expect("belief should persist");
                global_beliefs.push(value);
            }

            let restarted = PostgresMindStore::new(pool.clone());
            assert_eq!(
                BeliefStore::get(&restarted, global_beliefs[0].id())
                    .await
                    .expect("restart read should succeed"),
                Some(global_beliefs[0].clone())
            );
            let bounded = BeliefStore::relevant(
                &restarted,
                &[MindScope::Global],
                &marker.to_string(),
                now,
                2,
            )
            .await
            .expect("bounded retrieval should succeed");
            assert_eq!(bounded.len(), 2);
            let no_opinion = BeliefStore::relevant(
                &restarted,
                &[MindScope::Global],
                &format!("absent-topic-{}", Uuid::new_v4()),
                now,
                2,
            )
            .await
            .expect("irrelevant retrieval should succeed");
            assert!(
                no_opinion.is_empty(),
                "a nonempty query without topic overlap must represent NoOpinion"
            );

            let duplicate = belief(
                BeliefId::new(),
                MindScope::Global,
                global_beliefs[0].proposition().to_owned(),
                now,
            );
            cleanup_ids.push(duplicate.id().into_uuid());
            assert!(matches!(
                BeliefStore::put(&store, &duplicate, None).await,
                Err(MindStoreError::VersionConflict { .. })
            ));

            let first_atomic = belief(
                BeliefId::new(),
                MindScope::Global,
                format!("mind atomic {marker}"),
                now,
            );
            let second_atomic = belief(
                BeliefId::new(),
                MindScope::Global,
                format!("mind atomic {marker}"),
                now,
            );
            cleanup_ids.extend([
                first_atomic.id().into_uuid(),
                second_atomic.id().into_uuid(),
            ]);
            let atomic_base = MindConsolidationStore::current_version(&store)
                .await
                .expect("mind version should load");
            let atomic_plan = ConsolidationPlan {
                base_mind_version: atomic_base,
                beliefs: vec![
                    MindUpsert {
                        value: first_atomic.clone(),
                        expected_version: None,
                    },
                    MindUpsert {
                        value: second_atomic,
                        expected_version: None,
                    },
                ],
                preferences: Vec::new(),
                interests: Vec::new(),
                open_questions: Vec::new(),
                agenda: Vec::new(),
                episodes: Vec::new(),
                reason_tags: Vec::new(),
                created_at: now,
                trace: TraceContext::root(EventId::new()),
            };
            assert!(matches!(
                MindConsolidationStore::apply(&store, &atomic_plan).await,
                Err(MindStoreError::VersionConflict { .. })
            ));
            assert!(
                BeliefStore::get(&store, first_atomic.id())
                    .await
                    .expect("rolled-back row lookup should succeed")
                    .is_none(),
                "a failed consolidation must roll back earlier writes"
            );

            let interest = Interest::new(
                InterestId::new(),
                format!("mind restart interest {marker}"),
                0.8,
                0.2,
                0.7,
                MindSource::Experience,
                now,
            )
            .expect("test interest should be valid");
            cleanup_ids.push(interest.id().into_uuid());
            let success_base = MindConsolidationStore::current_version(&store)
                .await
                .expect("mind version should load");
            let success_plan = ConsolidationPlan {
                base_mind_version: success_base,
                beliefs: Vec::new(),
                preferences: Vec::new(),
                interests: vec![MindUpsert {
                    value: interest.clone(),
                    expected_version: None,
                }],
                open_questions: Vec::new(),
                agenda: Vec::new(),
                episodes: Vec::new(),
                reason_tags: Vec::new(),
                created_at: now,
                trace: TraceContext::root(EventId::new()),
            };
            let result = MindConsolidationStore::apply(&store, &success_plan)
                .await
                .expect("valid consolidation should commit");
            assert_eq!(result.applied_updates, 1);
            assert!(matches!(
                MindConsolidationStore::apply(&store, &success_plan).await,
                Err(MindStoreError::VersionConflict { kind: "mind", .. })
            ));
            assert_eq!(
                InterestStore::get(&restarted, interest.id())
                    .await
                    .expect("restart interest read should succeed"),
                Some(interest)
            );

            let expired = CuriosityItem::new(
                CuriosityId::new(),
                format!("expired curiosity {marker}"),
                None,
                None,
                0.5,
                now - Duration::hours(2),
                Some(now - Duration::hours(1)),
            )
            .expect("expired test curiosity should be valid");
            cleanup_ids.push(expired.id().into_uuid());
            let expired_agenda = AgendaItem::new(
                AgendaItemId::new(),
                MindScope::Global,
                AgendaSubject::Curiosity(expired.id()),
                0.5,
                0.5,
                0.5,
                AgendaSource::Curiosity,
                now - Duration::hours(2),
            )
            .expect("expired curiosity agenda should be valid");
            cleanup_ids.push(expired_agenda.id().into_uuid());
            CuriosityStore::put(&store, &expired, None)
                .await
                .expect("expired curiosity should persist before cleanup");
            AgendaStore::put(&store, &expired_agenda, None)
                .await
                .expect("expired curiosity agenda should persist before cleanup");
            assert!(store.cleanup(now).await.expect("cleanup should run") >= 2);
            assert!(
                CuriosityStore::get(&store, expired.id())
                    .await
                    .expect("cleaned curiosity lookup should succeed")
                    .is_none()
            );
            assert!(
                AgendaStore::get(&store, expired_agenda.id())
                    .await
                    .expect("cleaned agenda lookup should succeed")
                    .is_none()
            );

            let config = crate::config::get();
            let episode_memory = config.memory();
            let episode_cutoff = now - Duration::days(episode_memory.episode_retention_days());
            let expired_low = episode(
                EpisodeId::new(),
                MindScope::Global,
                format!("expired low episode {marker}"),
                0.1,
                0.1,
                false,
                episode_cutoff - Duration::hours(1),
            );
            let expired_high = episode(
                EpisodeId::new(),
                MindScope::Global,
                format!("expired high episode {marker}"),
                0.9,
                0.1,
                false,
                episode_cutoff - Duration::hours(1),
            );
            let expired_unresolved = episode(
                EpisodeId::new(),
                MindScope::Global,
                format!("expired unresolved episode {marker}"),
                0.1,
                0.1,
                true,
                episode_cutoff - Duration::hours(1),
            );
            let expired_unknown_status = episode(
                EpisodeId::new(),
                MindScope::Global,
                format!("expired unknown-status episode {marker}"),
                0.1,
                0.1,
                false,
                episode_cutoff - Duration::hours(1),
            );
            cleanup_ids.extend([
                expired_low.id().into_uuid(),
                expired_high.id().into_uuid(),
                expired_unresolved.id().into_uuid(),
                expired_unknown_status.id().into_uuid(),
            ]);
            for value in [
                &expired_low,
                &expired_high,
                &expired_unresolved,
                &expired_unknown_status,
            ] {
                EpisodeStore::put(&store, value)
                    .await
                    .expect("episode should persist before cleanup");
            }
            query("UPDATE yunxi_episodes SET status = 'future_state' WHERE id = $1")
                .bind(expired_unknown_status.id().into_uuid())
                .execute(&pool)
                .await
                .expect("unknown episode status should be writable for the fail-closed test");

            let capped_scope_id = ConversationId::new();
            let capped_scope = MindScope::Conversation {
                conversation_id: capped_scope_id,
            };
            let cap = episode_memory.episode_max_per_scope();
            for index in 0..cap.saturating_add(2) {
                let value = episode(
                    EpisodeId::new(),
                    capped_scope,
                    format!("capacity episode {marker} {index}"),
                    0.1,
                    0.1,
                    false,
                    now - Duration::minutes(index as i64),
                );
                cleanup_ids.push(value.id().into_uuid());
                EpisodeStore::put(&store, &value)
                    .await
                    .expect("capacity episode should persist before cleanup");
            }
            let removed = store
                .cleanup(now)
                .await
                .expect("episode cleanup should run");
            assert!(removed >= 1, "the expired low episode should be removed");
            let protected_count_row =
                query("SELECT COUNT(*) AS count FROM yunxi_episodes WHERE id = ANY($1)")
                    .bind(vec![
                        expired_high.id().into_uuid(),
                        expired_unresolved.id().into_uuid(),
                        expired_unknown_status.id().into_uuid(),
                    ])
                    .fetch_one(&pool)
                    .await
                    .expect("protected episode count should be queryable");
            let protected_count: i64 = protected_count_row
                .try_get("count")
                .expect("protected episode count should decode");
            assert_eq!(
                protected_count, 3,
                "high-salience, unresolved, and unknown-status episodes must survive cleanup"
            );
            let expired_low_row =
                query("SELECT COUNT(*) AS count FROM yunxi_episodes WHERE id = $1")
                    .bind(expired_low.id().into_uuid())
                    .fetch_one(&pool)
                    .await
                    .expect("expired episode count should be queryable");
            let expired_low_count: i64 = expired_low_row
                .try_get("count")
                .expect("expired episode count should decode");
            assert_eq!(
                expired_low_count, 0,
                "low-value old episodes should be removed"
            );
            let count_row =
                query("SELECT COUNT(*) AS count FROM yunxi_episodes WHERE scope_key = $1")
                    .bind(format!("conversation:{capped_scope_id}"))
                    .fetch_one(&pool)
                    .await
                    .expect("capped episode count should be queryable");
            let count: i64 = count_row
                .try_get("count")
                .expect("capped episode count should decode");
            assert!(
                count <= cap as i64,
                "low-value episode count should stay within the per-scope cap"
            );

            // The cap is hard for known statuses even when every row is
            // protected. The least valuable protected row (the oldest tie)
            // must be evicted, while an unknown status remains untouched.
            let protected_scope_id = ConversationId::new();
            let protected_scope = MindScope::Conversation {
                conversation_id: protected_scope_id,
            };
            let mut protected_ids = Vec::with_capacity(cap.saturating_add(1));
            for index in 0..cap.saturating_add(1) {
                let value = episode(
                    EpisodeId::new(),
                    protected_scope,
                    format!("protected capacity episode {marker} {index}"),
                    0.1,
                    0.1,
                    true,
                    now - Duration::minutes(index as i64),
                );
                cleanup_ids.push(value.id().into_uuid());
                protected_ids.push(value.id().into_uuid());
                EpisodeStore::put(&store, &value)
                    .await
                    .expect("protected episode should persist before cleanup");
            }
            let unknown_protected = episode(
                EpisodeId::new(),
                protected_scope,
                format!("unknown protected capacity episode {marker}"),
                0.0,
                0.0,
                false,
                now - Duration::days(30),
            );
            cleanup_ids.push(unknown_protected.id().into_uuid());
            EpisodeStore::put(&store, &unknown_protected)
                .await
                .expect("unknown-status episode should persist before cleanup");
            query("UPDATE yunxi_episodes SET status = 'future_state' WHERE id = $1")
                .bind(unknown_protected.id().into_uuid())
                .execute(&pool)
                .await
                .expect("unknown protected episode status should be writable");

            store
                .cleanup(now)
                .await
                .expect("protected episode cap cleanup should run");
            let known_count_row = query(
                "SELECT COUNT(*) AS count FROM yunxi_episodes WHERE scope_key = $1 AND status IN ('resolved', 'unresolved')",
            )
            .bind(format!("conversation:{protected_scope_id}"))
            .fetch_one(&pool)
            .await
            .expect("known protected episode count should be queryable");
            let known_count: i64 = known_count_row
                .try_get("count")
                .expect("known protected episode count should decode");
            assert_eq!(
                known_count, cap as i64,
                "known episode statuses must obey the hard per-scope cap"
            );
            let oldest_protected_count_row =
                query("SELECT COUNT(*) AS count FROM yunxi_episodes WHERE id = $1")
                    .bind(
                        *protected_ids
                            .last()
                            .expect("protected ids should be nonempty"),
                    )
                    .fetch_one(&pool)
                    .await
                    .expect("oldest protected episode count should be queryable");
            let oldest_protected_count: i64 = oldest_protected_count_row
                .try_get("count")
                .expect("oldest protected episode count should decode");
            assert_eq!(
                oldest_protected_count, 0,
                "the lowest-value protected known row should be evicted when necessary"
            );
            let unknown_count_row = query(
                "SELECT COUNT(*) AS count FROM yunxi_episodes WHERE id = $1 AND status = 'future_state'",
            )
            .bind(unknown_protected.id().into_uuid())
            .fetch_one(&pool)
            .await
            .expect("unknown episode count should be queryable");
            let unknown_count: i64 = unknown_count_row
                .try_get("count")
                .expect("unknown episode count should decode");
            assert_eq!(
                unknown_count, 1,
                "unknown episode statuses must be retained fail-closed"
            );

            let person_belief = belief(
                BeliefId::new(),
                MindScope::Person { person_id },
                format!("person-scoped {marker}"),
                now,
            );
            let conversation_belief = belief(
                BeliefId::new(),
                MindScope::Conversation { conversation_id },
                format!("conversation-scoped {marker}"),
                now,
            );
            cleanup_ids.extend([
                person_belief.id().into_uuid(),
                conversation_belief.id().into_uuid(),
            ]);
            BeliefStore::put(&store, &person_belief, None)
                .await
                .expect("person belief should persist");
            BeliefStore::put(&store, &conversation_belief, None)
                .await
                .expect("conversation belief should persist");
            MindDataErasure::erase_person(&store, person_id)
                .await
                .expect("person erasure should succeed");
            assert!(
                BeliefStore::get(&store, person_belief.id())
                    .await
                    .expect("person erasure lookup should succeed")
                    .is_none()
            );
            MindDataErasure::erase_conversation(&store, conversation_id)
                .await
                .expect("conversation erasure should succeed");
            assert!(
                BeliefStore::get(&store, conversation_belief.id())
                    .await
                    .expect("conversation erasure lookup should succeed")
                    .is_none()
            );
            drop(restarted);
            drop(store);
            let restarted_after_erasure = PostgresMindStore::new(pool.clone());
            restarted_after_erasure
                .initialize_schema()
                .await
                .expect("schema initialization after erasure should be restart-safe");
            assert!(
                BeliefStore::get(&restarted_after_erasure, person_belief.id())
                    .await
                    .expect("post-restart person lookup should succeed")
                    .is_none()
            );
            assert!(
                BeliefStore::get(&restarted_after_erasure, conversation_belief.id())
                    .await
                    .expect("post-restart conversation lookup should succeed")
                    .is_none()
            );
            assert!(
                BeliefStore::get(&restarted_after_erasure, global_beliefs[0].id())
                    .await
                    .expect("post-restart global state lookup should succeed")
                    .is_some(),
                "person erasure must survive restart and preserve Yunxi-global state"
            );

            delete_test_rows(&pool, &cleanup_ids).await;
            success_plan
                .validate(ConsolidationConfig {
                    max_belief_delta: 1.0,
                    max_preference_delta: 1.0,
                    max_interest_affinity_delta: 1.0,
                    max_updates_per_reflection: 128,
                })
                .expect("successful plan should remain structurally valid");
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_episode_put_compares_under_meta_lock_after_delete_replace() {
        crate::database_test_support::block_on(async {
            let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .connect(&database_url)
                .await
                .expect("should connect to PostgreSQL");
            let store = PostgresMindStore::new(pool.clone());
            store
                .initialize_schema()
                .await
                .expect("episode schema should initialize");

            let marker = Uuid::new_v4();
            let person_id = PersonId::new();
            let scope = MindScope::Person { person_id };
            let now = Utc::now();
            let id = EpisodeId::new();
            let original = episode(
                id,
                scope,
                format!("episode before erase {marker}"),
                0.4,
                0.1,
                false,
                now,
            );
            let replacement = episode(
                id,
                scope,
                format!("episode after erase {marker}"),
                0.8,
                0.3,
                true,
                now,
            );
            EpisodeStore::put(&store, &original)
                .await
                .expect("original episode should persist");

            // Hold the same row lock used by erasure, remove the old row, and
            // install a changed same-ID version before releasing the lock.
            // The writer below must not observe the pre-delete payload.
            let mut erasure = pool
                .begin()
                .await
                .expect("should begin erasure transaction");
            lock_meta(&mut erasure)
                .await
                .expect("erasure should acquire the mind metadata lock");
            query("DELETE FROM yunxi_episodes WHERE id = $1")
                .bind(id.into_uuid())
                .execute(&mut *erasure)
                .await
                .expect("erasure should remove the old episode");
            put_record_tx(
                &mut erasure,
                RecordTable::Episodes,
                episode_record(&replacement),
                None,
            )
            .await
            .expect("replacement episode should be staged in the same transaction");
            bump_meta(&mut erasure)
                .await
                .expect("replacement should advance the mind version");

            let writer_store = store.clone();
            let writer_episode = original.clone();
            let writer = kovi::tokio::spawn(async move {
                EpisodeStore::put(&writer_store, &writer_episode).await
            });
            kovi::tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                !writer.is_finished(),
                "episode put must wait for the metadata lock before comparing payloads"
            );

            erasure
                .commit()
                .await
                .expect("delete-and-replace transaction should commit");
            let result = writer
                .await
                .expect("episode writer task should join")
                .expect_err("stale pre-delete payload must not be accepted as idempotent");
            assert!(
                matches!(
                    result,
                    MindStoreError::VersionConflict {
                        kind: "episode",
                        actual: 1,
                        ..
                    }
                ),
                "writer should compare against the post-delete replacement: {result:?}"
            );
            assert_eq!(
                EpisodeStore::list_recent(&store, &[scope], now, 4)
                    .await
                    .expect("replacement episode should remain readable"),
                vec![replacement]
            );

            query("DELETE FROM yunxi_episodes WHERE id = $1")
                .bind(id.into_uuid())
                .execute(&pool)
                .await
                .expect("test episode should be removable");
        });
    }
}
