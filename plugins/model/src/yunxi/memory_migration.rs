//! Explicit, non-destructive migration of legacy QQ memories into Memory v2.
//!
//! The runtime keeps the legacy table as a compatibility source. This module
//! is intentionally an offline service: every invocation processes one
//! bounded, auditable batch and never deletes a legacy row.

use super::owner_lock;
use crate::memory::{MemoryEntry, MemoryType};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, PgPoolOptions, Postgres};
use std::env;
use uuid::Uuid;
use yunxi_core::{ConversationId, MemoryDraft, MemoryKind, MemoryScope, PersonId};

pub(crate) const DEFAULT_BATCH_SIZE: i64 = 500;
pub(crate) const MAX_BATCH_SIZE: i64 = 2_000;
const MIGRATION_VERSION: &str = "memory-v2-legacy-qq-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackfillOptions {
    pub(crate) dry_run: bool,
    pub(crate) batch_size: i64,
    pub(crate) cursor: Option<String>,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            batch_size: DEFAULT_BATCH_SIZE,
            cursor: None,
        }
    }
}

impl BackfillOptions {
    fn validate(self) -> Result<Self> {
        ensure!(
            (1..=MAX_BATCH_SIZE).contains(&self.batch_size),
            "batch-size must be between 1 and {MAX_BATCH_SIZE}"
        );
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryMigrationCommand {
    Backfill(BackfillOptions),
    Validate { batch_id: Option<Uuid> },
    Rollback { batch_id: Uuid, dry_run: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BackfillReport {
    pub(crate) batch_id: Uuid,
    pub(crate) dry_run: bool,
    pub(crate) source_rows: u64,
    pub(crate) inserted_rows: u64,
    pub(crate) would_insert_rows: u64,
    pub(crate) already_present_rows: u64,
    pub(crate) mismatched_rows: u64,
    pub(crate) unresolved_rows: u64,
    pub(crate) invalid_rows: u64,
    pub(crate) comparison_rows: u64,
    pub(crate) content_hash: String,
    pub(crate) comparison_hash: String,
    pub(crate) cursor_start: Option<String>,
    pub(crate) cursor_end: Option<String>,
    pub(crate) has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ValidationReport {
    pub(crate) batch_id: Uuid,
    pub(crate) audited_rows: u64,
    pub(crate) matching_rows: u64,
    pub(crate) missing_rows: u64,
    pub(crate) changed_rows: u64,
    pub(crate) content_hash: String,
    pub(crate) comparison_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RollbackReport {
    pub(crate) batch_id: Uuid,
    pub(crate) dry_run: bool,
    pub(crate) deleted_rows: u64,
    pub(crate) skipped_changed_rows: u64,
    pub(crate) already_rolled_back: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryMigrationService {
    pool: PgPool,
}

impl MemoryMigrationService {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        for statement in [
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_memory_migration_batches (
                id UUID PRIMARY KEY,
                migration_version TEXT NOT NULL,
                mode TEXT NOT NULL CHECK (mode IN ('backfill', 'dry_run')),
                status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'rolled_back')),
                batch_size INTEGER NOT NULL CHECK (batch_size > 0),
                cursor_start TEXT,
                cursor_end TEXT,
                source_rows BIGINT NOT NULL DEFAULT 0 CHECK (source_rows >= 0),
                inserted_rows BIGINT NOT NULL DEFAULT 0 CHECK (inserted_rows >= 0),
                mismatched_rows BIGINT NOT NULL DEFAULT 0 CHECK (mismatched_rows >= 0),
                unresolved_rows BIGINT NOT NULL DEFAULT 0 CHECK (unresolved_rows >= 0),
                invalid_rows BIGINT NOT NULL DEFAULT 0 CHECK (invalid_rows >= 0),
                content_hash TEXT NOT NULL DEFAULT '',
                comparison_hash TEXT NOT NULL DEFAULT '',
                rolled_back_rows BIGINT NOT NULL DEFAULT 0 CHECK (rolled_back_rows >= 0),
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                completed_at TIMESTAMPTZ
            )
            "#,
            r#"
            CREATE TABLE IF NOT EXISTS yunxi_memory_migration_items (
                batch_id UUID NOT NULL REFERENCES yunxi_memory_migration_batches(id) ON DELETE CASCADE,
                legacy_id TEXT NOT NULL,
                target_id UUID,
                source_hash TEXT NOT NULL,
                target_hash TEXT,
                action TEXT NOT NULL CHECK (action IN (
                    'inserted', 'would_insert', 'already_present', 'mismatched',
                    'unresolved', 'invalid'
                )),
                inserted BOOLEAN NOT NULL DEFAULT FALSE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (batch_id, legacy_id)
            )
            "#,
            "CREATE INDEX IF NOT EXISTS yunxi_memory_migration_items_target_idx
             ON yunxi_memory_migration_items (target_id)",
        ] {
            query(statement).execute(&mut *transaction).await?;
        }
        // Older installations may have created the ledger before the
        // insertion provenance bit was added. Keep the upgrade additive and
        // default old rows to unproven so cleanup remains fail-closed.
        query(
            "ALTER TABLE yunxi_memory_migration_items
             ADD COLUMN IF NOT EXISTS inserted BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn backfill(&self, options: BackfillOptions) -> Result<BackfillReport> {
        let options = options.validate()?;
        self.initialize_schema().await?;
        let batch_id = Uuid::new_v4();
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        owner_lock::lock_memory_maintenance(&mut transaction).await?;
        let mode = if options.dry_run {
            "dry_run"
        } else {
            "backfill"
        };
        if !options.dry_run {
            query(
                "INSERT INTO yunxi_memory_migration_batches
                    (id, migration_version, mode, status, batch_size, cursor_start)
                 VALUES ($1, $2, $3, 'running', $4, $5)",
            )
            .bind(batch_id)
            .bind(MIGRATION_VERSION)
            .bind(mode)
            .bind(i32::try_from(options.batch_size).unwrap_or(i32::MAX))
            .bind(options.cursor.as_deref())
            .execute(&mut *transaction)
            .await?;
        }

        let rows = query(
            "SELECT id, subject_id, scope_type, context, occurred_at, importance, payload
             FROM kovi_bot_memories
             WHERE ($1::TEXT IS NULL OR id > $1)
             ORDER BY id
             LIMIT $2",
        )
        .bind(options.cursor.as_deref())
        .bind(options.batch_size)
        .fetch_all(&mut *transaction)
        .await
        .context("read legacy kovi_bot_memories")?;

        let mut report = BackfillReport {
            batch_id,
            dry_run: options.dry_run,
            source_rows: 0,
            inserted_rows: 0,
            would_insert_rows: 0,
            already_present_rows: 0,
            mismatched_rows: 0,
            unresolved_rows: 0,
            invalid_rows: 0,
            comparison_rows: 0,
            content_hash: String::new(),
            comparison_hash: String::new(),
            cursor_start: options.cursor.clone(),
            cursor_end: None,
            has_more: rows.len() as i64 == options.batch_size,
        };
        let mut content_parts = Vec::with_capacity(rows.len());
        let mut comparison_parts = Vec::with_capacity(rows.len());

        for row in rows {
            report.source_rows += 1;
            let legacy_id = row.try_get::<String, _>("id")?;
            report.cursor_end = Some(legacy_id.clone());
            let parsed = parse_legacy_row(&row);
            let (source_hash, action, target_id, target_hash, inserted) = match parsed {
                Err(error) => {
                    report.invalid_rows += 1;
                    let source_hash = hash_text(&format!("invalid:{legacy_id}:{error}"));
                    (source_hash, "invalid", None, None, false)
                }
                Ok(legacy) => {
                    let source_hash = source_hash(&legacy);
                    match classify_scope(
                        legacy.scope_type.as_deref(),
                        &legacy.context,
                        legacy.subject_id,
                    )? {
                        None => {
                            report.unresolved_rows += 1;
                            (source_hash, "unresolved", None, None, false)
                        }
                        Some(scope_hint) => match resolve_scope(&mut transaction, scope_hint)
                            .await?
                        {
                            None => {
                                report.unresolved_rows += 1;
                                (source_hash, "unresolved", None, None, false)
                            }
                            Some(scope) => match legacy_draft(&legacy, scope) {
                                None => {
                                    report.invalid_rows += 1;
                                    (source_hash, "invalid", None, None, false)
                                }
                                Some(draft) => {
                                    let target_id = deterministic_target_id(&legacy.id);
                                    let expected_hash = draft_hash(scope, &draft);
                                    let existing =
                                        fetch_target(&mut transaction, target_id).await?;
                                    if let Some(existing) = existing {
                                        let actual_hash = target_hash(&existing);
                                        report.comparison_rows += 1;
                                        if actual_hash == expected_hash {
                                            report.already_present_rows += 1;
                                            (
                                                source_hash,
                                                "already_present",
                                                Some(target_id),
                                                Some(actual_hash),
                                                false,
                                            )
                                        } else {
                                            report.mismatched_rows += 1;
                                            (
                                                source_hash,
                                                "mismatched",
                                                Some(target_id),
                                                Some(actual_hash),
                                                false,
                                            )
                                        }
                                    } else if options.dry_run {
                                        report.would_insert_rows += 1;
                                        (
                                            source_hash,
                                            "would_insert",
                                            Some(target_id),
                                            Some(expected_hash),
                                            false,
                                        )
                                    } else {
                                        insert_target(&mut transaction, target_id, scope, &draft)
                                            .await?;
                                        report.inserted_rows += 1;
                                        report.comparison_rows += 1;
                                        (
                                            source_hash,
                                            "inserted",
                                            Some(target_id),
                                            Some(expected_hash),
                                            true,
                                        )
                                    }
                                }
                            },
                        },
                    }
                }
            };
            content_parts.push(format!("{legacy_id}:{source_hash}"));
            comparison_parts.push(format!(
                "{legacy_id}:{action}:{}",
                target_hash.as_deref().unwrap_or("-")
            ));
            if !options.dry_run {
                query(
                    "INSERT INTO yunxi_memory_migration_items
                        (batch_id, legacy_id, target_id, source_hash, target_hash, action, inserted)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (batch_id, legacy_id) DO UPDATE SET
                        target_id = EXCLUDED.target_id, source_hash = EXCLUDED.source_hash,
                        target_hash = EXCLUDED.target_hash, action = EXCLUDED.action,
                        inserted = EXCLUDED.inserted",
                )
                .bind(batch_id)
                .bind(&legacy_id)
                .bind(target_id)
                .bind(&source_hash)
                .bind(target_hash)
                .bind(action)
                .bind(inserted)
                .execute(&mut *transaction)
                .await?;
            }
        }
        report.content_hash = aggregate_hash(&content_parts);
        report.comparison_hash = aggregate_hash(&comparison_parts);
        if !options.dry_run {
            query(
                "UPDATE yunxi_memory_migration_batches
                 SET status = 'completed', cursor_end = $2, source_rows = $3,
                     inserted_rows = $4, mismatched_rows = $5, unresolved_rows = $6,
                     invalid_rows = $7, content_hash = $8, comparison_hash = $9,
                     completed_at = NOW()
                 WHERE id = $1",
            )
            .bind(batch_id)
            .bind(report.cursor_end.as_deref())
            .bind(i64::try_from(report.source_rows).unwrap_or(i64::MAX))
            .bind(i64::try_from(report.inserted_rows).unwrap_or(i64::MAX))
            .bind(i64::try_from(report.mismatched_rows).unwrap_or(i64::MAX))
            .bind(i64::try_from(report.unresolved_rows).unwrap_or(i64::MAX))
            .bind(i64::try_from(report.invalid_rows).unwrap_or(i64::MAX))
            .bind(&report.content_hash)
            .bind(&report.comparison_hash)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(report)
    }

    pub(crate) async fn validate(&self, batch_id: Option<Uuid>) -> Result<ValidationReport> {
        self.initialize_schema().await?;
        let batch_id = match batch_id {
            Some(batch_id) => batch_id,
            None => query_scalar::<Postgres, Uuid>(
                "SELECT id FROM yunxi_memory_migration_batches
                 WHERE status = 'completed' ORDER BY completed_at DESC, id DESC LIMIT 1",
            )
            .fetch_optional(&self.pool)
            .await?
            .context("no completed memory migration batch")?,
        };
        let rows = query(
            "SELECT legacy_id, target_id, source_hash, target_hash
             FROM yunxi_memory_migration_items WHERE batch_id = $1 ORDER BY legacy_id",
        )
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await?;
        ensure!(
            !rows.is_empty(),
            "memory migration batch has no audited rows"
        );
        let mut report = ValidationReport {
            batch_id,
            audited_rows: 0,
            matching_rows: 0,
            missing_rows: 0,
            changed_rows: 0,
            content_hash: String::new(),
            comparison_hash: String::new(),
        };
        let mut content_parts = Vec::with_capacity(rows.len());
        let mut comparison_parts = Vec::with_capacity(rows.len());
        for row in rows {
            let legacy_id = row.try_get::<String, _>("legacy_id")?;
            let source_hash = row.try_get::<String, _>("source_hash")?;
            let expected_target_hash = row.try_get::<Option<String>, _>("target_hash")?;
            let target_id = row.try_get::<Option<Uuid>, _>("target_id")?;
            let actual = match target_id {
                Some(target_id) => fetch_target_from_pool(&self.pool, target_id).await?,
                None => None,
            };
            let actual_hash = actual.as_ref().map(target_hash);
            report.audited_rows += 1;
            if expected_target_hash.is_some() && actual.is_none() {
                report.missing_rows += 1;
            } else if expected_target_hash == actual_hash {
                report.matching_rows += 1;
            } else {
                report.changed_rows += 1;
            }
            content_parts.push(format!("{legacy_id}:{source_hash}"));
            comparison_parts.push(format!(
                "{legacy_id}:{}:{}",
                expected_target_hash.as_deref().unwrap_or("-"),
                actual_hash.as_deref().unwrap_or("-")
            ));
        }
        report.content_hash = aggregate_hash(&content_parts);
        report.comparison_hash = aggregate_hash(&comparison_parts);
        Ok(report)
    }

    pub(crate) async fn rollback(&self, batch_id: Uuid, dry_run: bool) -> Result<RollbackReport> {
        self.initialize_schema().await?;
        let mut transaction = self.pool.begin().await?;
        owner_lock::lock_memory_maintenance(&mut transaction).await?;
        let status = query_scalar::<Postgres, String>(
            "SELECT status FROM yunxi_memory_migration_batches WHERE id = $1 FOR UPDATE",
        )
        .bind(batch_id)
        .fetch_optional(&mut *transaction)
        .await?
        .context("memory migration batch not found")?;
        if status == "rolled_back" {
            transaction.commit().await?;
            return Ok(RollbackReport {
                batch_id,
                dry_run,
                deleted_rows: 0,
                skipped_changed_rows: 0,
                already_rolled_back: true,
            });
        }
        let rows = query(
            "SELECT target_id, target_hash FROM yunxi_memory_migration_items
             WHERE batch_id = $1 AND inserted = TRUE AND target_id IS NOT NULL
             ORDER BY legacy_id",
        )
        .bind(batch_id)
        .fetch_all(&mut *transaction)
        .await?;
        let mut report = RollbackReport {
            batch_id,
            dry_run,
            deleted_rows: 0,
            skipped_changed_rows: 0,
            already_rolled_back: false,
        };
        for row in rows {
            let target_id = row.try_get::<Uuid, _>("target_id")?;
            let expected = row.try_get::<Option<String>, _>("target_hash")?;
            let Some(actual) = fetch_target(&mut transaction, target_id).await? else {
                continue;
            };
            if expected.as_deref() != Some(target_hash(&actual).as_str()) {
                report.skipped_changed_rows += 1;
                continue;
            }
            if !dry_run {
                query("DELETE FROM yunxi_memories WHERE id = $1")
                    .bind(target_id)
                    .execute(&mut *transaction)
                    .await?;
                report.deleted_rows += 1;
            }
        }
        if !dry_run {
            query(
                "UPDATE yunxi_memory_migration_batches
                 SET status = 'rolled_back', rolled_back_rows = $2, completed_at = NOW()
                 WHERE id = $1",
            )
            .bind(batch_id)
            .bind(i64::try_from(report.deleted_rows).unwrap_or(i64::MAX))
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(report)
    }
}

pub(crate) async fn run_cli(args: Vec<String>) -> Result<String> {
    let command = parse_cli_args(&args)?;
    let database_url =
        env::var("DATABASE_URL").context("DATABASE_URL is required for yunxi-memory-migrate")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let service = MemoryMigrationService::new(pool);
    let value = match command {
        MemoryMigrationCommand::Backfill(options) => {
            serde_json::to_value(service.backfill(options).await?)?
        }
        MemoryMigrationCommand::Validate { batch_id } => {
            serde_json::to_value(service.validate(batch_id).await?)?
        }
        MemoryMigrationCommand::Rollback { batch_id, dry_run } => {
            serde_json::to_value(service.rollback(batch_id, dry_run).await?)?
        }
    };
    Ok(serde_json::to_string_pretty(&value)?)
}

pub(crate) fn parse_cli_args(args: &[String]) -> Result<MemoryMigrationCommand> {
    let command = args.first().map(String::as_str).unwrap_or("backfill");
    let mut dry_run = false;
    let mut batch_size = DEFAULT_BATCH_SIZE;
    let mut cursor = None;
    let mut batch_id = None;
    let mut index = usize::from(!args.is_empty());
    while index < args.len() {
        let argument = &args[index];
        let (key, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(key, value)| (key, Some(value)));
        match key {
            "--dry-run" => dry_run = true,
            "--batch-size" => {
                batch_size = parse_i64(
                    inline.or_else(|| args.get(index + 1).map(String::as_str)),
                    "batch-size",
                )?
            }
            "--cursor" => {
                cursor = Some(parse_string(
                    inline.or_else(|| args.get(index + 1).map(String::as_str)),
                    "cursor",
                )?)
            }
            "--batch-id" => {
                batch_id = Some(parse_uuid(
                    inline.or_else(|| args.get(index + 1).map(String::as_str)),
                    "batch-id",
                )?)
            }
            "--help" | "-h" => bail!(usage()),
            _ => bail!("unknown migration argument {argument}\n{}", usage()),
        }
        if inline.is_none() && matches!(key, "--batch-size" | "--cursor" | "--batch-id") {
            index += 1;
        }
        index += 1;
    }
    match command {
        "backfill" => BackfillOptions {
            dry_run,
            batch_size,
            cursor,
        }
        .validate()
        .map(MemoryMigrationCommand::Backfill),
        "validate" => Ok(MemoryMigrationCommand::Validate { batch_id }),
        "rollback" => Ok(MemoryMigrationCommand::Rollback {
            batch_id: batch_id.context("rollback requires --batch-id")?,
            dry_run,
        }),
        _ => bail!("unknown migration command {command}\n{}", usage()),
    }
}

fn usage() -> &'static str {
    "usage: yunxi-memory-migrate [backfill|validate|rollback] [--dry-run] [--batch-size N] [--cursor LEGACY_ID] [--batch-id UUID]"
}

fn parse_i64(value: Option<&str>, name: &str) -> Result<i64> {
    value
        .context(format!("{name} requires a value"))?
        .parse::<i64>()
        .with_context(|| format!("{name} must be an integer"))
}

fn parse_uuid(value: Option<&str>, name: &str) -> Result<Uuid> {
    Uuid::parse_str(value.context(format!("{name} requires a value"))?)
        .with_context(|| format!("{name} must be a UUID"))
}

fn parse_string(value: Option<&str>, name: &str) -> Result<String> {
    let value = value.context(format!("{name} requires a value"))?;
    ensure!(!value.is_empty(), "{name} must not be empty");
    Ok(value.to_owned())
}

#[derive(Debug, Clone)]
struct LegacyMemory {
    id: String,
    subject_id: Option<i64>,
    scope_type: Option<String>,
    context: String,
    entry: MemoryEntry,
}

#[derive(Debug, Clone, Copy)]
enum LegacyScopeHint {
    Person(i64),
    Group(i64),
    Conversation(ConversationId),
    Global,
}

#[derive(Debug, Clone)]
struct TargetMemory {
    scope_kind: String,
    scope_id: Option<Uuid>,
    kind: String,
    content: String,
    importance: i16,
    tags: Value,
    occurred_at: DateTime<Utc>,
}

fn parse_legacy_row(row: &sqlx_postgres::PgRow) -> Result<LegacyMemory> {
    let id = row.try_get::<String, _>("id")?;
    let payload = row.try_get::<Value, _>("payload")?;
    let entry: MemoryEntry = serde_json::from_value(payload)
        .with_context(|| format!("legacy memory payload {id} is invalid"))?;
    let context = row.try_get::<String, _>("context")?;
    Ok(LegacyMemory {
        id,
        subject_id: row.try_get("subject_id")?,
        scope_type: row.try_get("scope_type")?,
        context,
        entry,
    })
}

fn classify_scope(
    scope_type: Option<&str>,
    context: &str,
    subject_id: Option<i64>,
) -> Result<Option<LegacyScopeHint>> {
    if let Some(value) = context.strip_prefix("yunxi_direct_chat:") {
        return Ok(Some(LegacyScopeHint::Conversation(
            ConversationId::from_uuid(
                Uuid::parse_str(value).context("invalid direct conversation id")?,
            ),
        )));
    }
    if context == "yunxi_global:" || scope_type == Some("global") {
        return Ok(Some(LegacyScopeHint::Global));
    }
    let kind = scope_type.or_else(|| {
        if context == "private"
            || context.starts_with("private_")
            || context.starts_with("proactive_private_")
        {
            Some("private")
        } else if context == "group"
            || context.starts_with("group_")
            || context.starts_with("proactive_group_")
        {
            Some("group")
        } else {
            None
        }
    });
    match kind {
        Some("private") => Ok(subject_id
            .filter(|value| *value > 0)
            .map(LegacyScopeHint::Person)),
        Some("group") => Ok(subject_id
            .filter(|value| *value > 0)
            .map(LegacyScopeHint::Group)),
        Some("global") => Ok(Some(LegacyScopeHint::Global)),
        Some(_) | None => Ok(None),
    }
}

async fn resolve_scope(
    transaction: &mut Transaction<'_, Postgres>,
    hint: LegacyScopeHint,
) -> Result<Option<MemoryScope>> {
    match hint {
        LegacyScopeHint::Global => Ok(Some(MemoryScope::Global)),
        LegacyScopeHint::Conversation(id) => {
            let exists = query_scalar::<Postgres, bool>(
                "SELECT EXISTS(SELECT 1 FROM yunxi_conversations WHERE id = $1)",
            )
            .bind(id.into_uuid())
            .fetch_one(&mut **transaction)
            .await?;
            Ok(exists.then_some(MemoryScope::Conversation(id)))
        }
        LegacyScopeHint::Person(subject_id) => {
            let ids = query_scalar::<Postgres, Uuid>(
                "SELECT person_id FROM yunxi_external_identities WHERE platform = 'qq' AND external_id = $1 LIMIT 2",
            )
            .bind(subject_id.to_string())
            .fetch_all(&mut **transaction)
            .await?;
            Ok((ids.len() == 1).then(|| MemoryScope::Person(PersonId::from_uuid(ids[0]))))
        }
        LegacyScopeHint::Group(group_id) => {
            let ids = query_scalar::<Postgres, Uuid>(
                "SELECT external.conversation_id
                 FROM yunxi_external_conversations AS external
                 JOIN yunxi_conversations AS conversation ON conversation.id = external.conversation_id
                 WHERE external.platform = 'qq' AND external.external_id = $1
                   AND conversation.kind = 'group' LIMIT 2",
            )
            .bind(format!("group:{group_id}"))
            .fetch_all(&mut **transaction)
            .await?;
            Ok((ids.len() == 1)
                .then(|| MemoryScope::Conversation(ConversationId::from_uuid(ids[0]))))
        }
    }
}

fn legacy_draft(legacy: &LegacyMemory, scope: MemoryScope) -> Option<MemoryDraft> {
    let kind = match legacy.entry.memory_type {
        MemoryType::Conversation => MemoryKind::Conversation,
        MemoryType::UserProfile | MemoryType::GroupInfo => MemoryKind::Profile,
        MemoryType::Event if legacy.context.ends_with("|yunxi_kind=fact") => MemoryKind::Fact,
        MemoryType::Event => MemoryKind::Event,
        MemoryType::Preference => MemoryKind::Preference,
        MemoryType::Emotion => MemoryKind::Emotion,
    };
    let importance = u16::from(legacy.entry.importance)
        .saturating_mul(10)
        .min(100) as u8;
    let occurred_at = postgres_timestamp_precision(legacy.entry.timestamp.with_timezone(&Utc));
    MemoryDraft::new(scope, kind, legacy.entry.content.clone(), occurred_at)
        .ok()?
        .with_importance(importance)
        .ok()?
        .with_tags(legacy.entry.tags.clone())
        .ok()
}

fn postgres_timestamp_precision(value: DateTime<Utc>) -> DateTime<Utc> {
    let nanoseconds = value.nanosecond();
    value
        .with_nanosecond(nanoseconds - nanoseconds % 1_000)
        .expect("microsecond-aligned nanoseconds are valid")
}

fn deterministic_target_id(legacy_id: &str) -> Uuid {
    Uuid::parse_str(legacy_id)
        .unwrap_or_else(|_| Uuid::new_v5(&Uuid::NAMESPACE_URL, legacy_id.as_bytes()))
}

fn source_hash(legacy: &LegacyMemory) -> String {
    let entry = serde_json::to_string(&legacy.entry).unwrap_or_default();
    hash_text(&format!(
        "{}|{}|{}|{}|{}",
        legacy.id,
        legacy
            .subject_id
            .map_or_else(|| "-".to_owned(), |id| id.to_string()),
        legacy.scope_type.as_deref().unwrap_or("-"),
        legacy.context,
        entry,
    ))
}

fn draft_hash(scope: MemoryScope, draft: &MemoryDraft) -> String {
    let (kind, scope_id) = scope_kind(scope);
    hash_text(&format!(
        "{}|{}|{}|{}|{}|{}|{}",
        kind,
        scope_id.map_or_else(|| "-".to_owned(), |id| id.to_string()),
        draft.kind(),
        draft.importance(),
        serde_json::to_string(draft.tags()).unwrap_or_default(),
        draft
            .occurred_at()
            .to_rfc3339_opts(SecondsFormat::Nanos, true),
        draft.content(),
    ))
}

fn target_hash(target: &TargetMemory) -> String {
    hash_text(&format!(
        "{}|{}|{}|{}|{}|{}|{}",
        target.scope_kind,
        target
            .scope_id
            .map_or_else(|| "-".to_owned(), |id| id.to_string()),
        target.kind,
        target.importance,
        target.tags,
        target
            .occurred_at
            .to_rfc3339_opts(SecondsFormat::Nanos, true),
        target.content,
    ))
}

fn hash_text(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn aggregate_hash(parts: &[String]) -> String {
    let mut sorted = parts.to_vec();
    sorted.sort_unstable();
    hash_text(&sorted.join("\n"))
}

fn scope_kind(scope: MemoryScope) -> (&'static str, Option<Uuid>) {
    match scope {
        MemoryScope::Person(id) => ("person", Some(id.into_uuid())),
        MemoryScope::Conversation(id) => ("conversation", Some(id.into_uuid())),
        MemoryScope::Global => ("global", None),
    }
}

async fn insert_target(
    transaction: &mut Transaction<'_, Postgres>,
    id: Uuid,
    scope: MemoryScope,
    draft: &MemoryDraft,
) -> Result<()> {
    let (scope_kind, scope_id) = scope_kind(scope);
    query(
        "INSERT INTO yunxi_memories
            (id, scope_kind, scope_id, kind, content, importance, tags, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(scope_kind)
    .bind(scope_id)
    .bind(draft.kind().as_str())
    .bind(draft.content())
    .bind(i16::from(draft.importance()))
    .bind(serde_json::to_value(draft.tags())?)
    .bind(draft.occurred_at())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn fetch_target(
    transaction: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Option<TargetMemory>> {
    let Some(row) = query(
        "SELECT scope_kind, scope_id, kind, content, importance, tags, occurred_at
         FROM yunxi_memories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut **transaction)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(TargetMemory {
        scope_kind: row.try_get("scope_kind")?,
        scope_id: row.try_get("scope_id")?,
        kind: row.try_get("kind")?,
        content: row.try_get("content")?,
        importance: row.try_get("importance")?,
        tags: row.try_get("tags")?,
        occurred_at: row.try_get("occurred_at")?,
    }))
}

async fn fetch_target_from_pool(pool: &PgPool, id: Uuid) -> Result<Option<TargetMemory>> {
    let Some(row) = query(
        "SELECT scope_kind, scope_id, kind, content, importance, tags, occurred_at
         FROM yunxi_memories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(TargetMemory {
        scope_kind: row.try_get("scope_kind")?,
        scope_id: row.try_get("scope_id")?,
        kind: row.try_get("kind")?,
        content: row.try_get("content")?,
        importance: row.try_get("importance")?,
        tags: row.try_get("tags")?,
        occurred_at: row.try_get("occurred_at")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        BackfillOptions, LegacyScopeHint, MemoryMigrationCommand, aggregate_hash, classify_scope,
        deterministic_target_id, parse_cli_args, postgres_timestamp_precision,
    };
    use crate::memory::{MemoryEntry, MemoryType};
    use chrono::Utc;
    use sqlx_core::query::query;
    use sqlx_core::query_scalar::query_scalar;
    use sqlx_postgres::Postgres;
    use uuid::Uuid;

    #[test]
    fn scope_classification_preserves_private_group_and_direct_boundaries() {
        assert!(matches!(
            classify_scope(Some("private"), "private_chat", Some(42)).expect("scope"),
            Some(LegacyScopeHint::Person(42))
        ));
        assert!(matches!(
            classify_scope(Some("group"), "group_chat", Some(43)).expect("scope"),
            Some(LegacyScopeHint::Group(43))
        ));
        let conversation_id = Uuid::new_v4();
        assert!(matches!(
            classify_scope(None, &format!("yunxi_direct_chat:{conversation_id}"), None)
                .expect("scope"),
            Some(LegacyScopeHint::Conversation(_))
        ));
        assert!(matches!(
            classify_scope(Some("global"), "yunxi_global:", None).expect("scope"),
            Some(LegacyScopeHint::Global)
        ));
    }

    #[test]
    fn target_ids_and_aggregate_hashes_are_stable() {
        let id = deterministic_target_id("legacy-1");
        assert_eq!(id, deterministic_target_id("legacy-1"));
        assert_ne!(id, deterministic_target_id("legacy-2"));
        assert_eq!(
            aggregate_hash(&["b:2".to_owned(), "a:1".to_owned()]),
            aggregate_hash(&["a:1".to_owned(), "b:2".to_owned()])
        );
    }

    #[test]
    fn postgres_timestamp_precision_discards_submicrosecond_nanoseconds() {
        let timestamp = chrono::DateTime::<Utc>::from_timestamp(1_700_000_000, 123_456_789)
            .expect("fixed timestamp");

        assert_eq!(
            postgres_timestamp_precision(timestamp).timestamp_subsec_nanos(),
            123_456_000
        );
    }

    #[test]
    fn cli_parser_requires_explicit_rollback_batch_and_bounds_batches() {
        let args = vec![
            "backfill".to_owned(),
            "--dry-run".to_owned(),
            "--batch-size".to_owned(),
            "10".to_owned(),
        ];
        assert!(matches!(
            parse_cli_args(&args).expect("parse"),
            MemoryMigrationCommand::Backfill(BackfillOptions {
                dry_run: true,
                batch_size: 10,
                ..
            })
        ));
        assert!(parse_cli_args(&["rollback".to_owned()]).is_err());
        assert!(parse_cli_args(&["backfill".to_owned(), "--batch-size=2001".to_owned()]).is_err());
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_backfill_is_idempotent_and_rollback_keeps_legacy_rows() {
        kovi::tokio::runtime::Runtime::new()
            .expect("test runtime")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
                let pool = sqlx_postgres::PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("connect postgres");
                let suffix = Uuid::new_v4().simple().to_string();
                let person_id = Uuid::new_v4();
                let subject_id = 9_000_000_000_000_i64
                    + i64::from(Uuid::new_v4().as_bytes()[0]);
                let memory_id = format!("migration-fixture-{suffix}");
                query("CREATE TABLE IF NOT EXISTS kovi_bot_memories (
                    id TEXT PRIMARY KEY, subject_id BIGINT, scope_type TEXT,
                    context TEXT NOT NULL, occurred_at TIMESTAMPTZ NOT NULL,
                    importance SMALLINT NOT NULL, payload JSONB NOT NULL
                )")
                .execute(&pool)
                .await
                .expect("create legacy fixture");
                query("CREATE TABLE IF NOT EXISTS yunxi_persons (
                    id UUID PRIMARY KEY, created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                )")
                .execute(&pool)
                .await
                .expect("create person fixture");
                query("CREATE TABLE IF NOT EXISTS yunxi_external_identities (
                    platform TEXT NOT NULL, external_id TEXT NOT NULL, person_id UUID NOT NULL,
                    PRIMARY KEY(platform, external_id)
                )")
                .execute(&pool)
                .await
                .expect("create identity fixture");
                query("CREATE TABLE IF NOT EXISTS yunxi_memories (
                    id UUID PRIMARY KEY, scope_kind TEXT NOT NULL, scope_id UUID,
                    kind TEXT NOT NULL, content TEXT NOT NULL, importance SMALLINT NOT NULL,
                    tags JSONB NOT NULL, occurred_at TIMESTAMPTZ NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
                )")
                .execute(&pool)
                .await
                .expect("create target fixture");
                query("INSERT INTO yunxi_persons (id) VALUES ($1) ON CONFLICT DO NOTHING")
                    .bind(person_id)
                    .execute(&pool)
                    .await
                    .expect("insert person fixture");
                query("INSERT INTO yunxi_external_identities (platform, external_id, person_id)
                       VALUES ('qq', $1, $2) ON CONFLICT DO NOTHING")
                    .bind(subject_id.to_string())
                    .bind(person_id)
                    .execute(&pool)
                    .await
                    .expect("insert identity fixture");
                let entry = MemoryEntry {
                    id: memory_id.clone(),
                    content: "migration fixture".to_owned(),
                    timestamp: chrono::DateTime::<Utc>::from_timestamp(
                        1_700_000_000,
                        123_456_789,
                    )
                    .expect("fixed submicrosecond timestamp")
                    .with_timezone(&chrono::Local),
                    memory_type: MemoryType::Event,
                    importance: 7,
                    tags: vec!["fixture".to_owned()],
                    context: "private_chat".to_owned(),
                    subject_id: Some(subject_id),
                };
                query("INSERT INTO kovi_bot_memories
                    (id, subject_id, scope_type, context, occurred_at, importance, payload)
                    VALUES ($1, $2, 'private', 'private_chat', $3, 7, $4)
                    ON CONFLICT (id) DO UPDATE SET payload = EXCLUDED.payload")
                    .bind(&memory_id)
                    .bind(entry.subject_id)
                    .bind(entry.timestamp.with_timezone(&Utc))
                    .bind(serde_json::to_value(&entry).expect("encode fixture"))
                    .execute(&pool)
                    .await
                    .expect("insert legacy fixture");
                // The shared integration database can contain unrelated legacy
                // rows. Start just before this fixture and process one row so
                // report counters describe this test's fixture only.
                let fixture_cursor = query_scalar::<Postgres, String>(
                    "SELECT id FROM kovi_bot_memories
                     WHERE id < $1 ORDER BY id DESC LIMIT 1",
                )
                .bind(&memory_id)
                .fetch_optional(&pool)
                .await
                .expect("read fixture cursor");
                let fixture_options = BackfillOptions {
                    batch_size: 1,
                    cursor: fixture_cursor,
                    ..BackfillOptions::default()
                };
                let service = super::MemoryMigrationService::new(pool.clone());
                let report = service
                    .backfill(fixture_options.clone())
                    .await
                    .expect("backfill fixture");
                assert_eq!(report.inserted_rows, 1);
                let repeat = service
                    .backfill(fixture_options)
                    .await
                    .expect("repeat backfill fixture");
                assert_eq!(repeat.already_present_rows, 1);
                assert_eq!(repeat.mismatched_rows, 0);
                let validation = service.validate(Some(report.batch_id)).await.expect("validate fixture");
                assert_eq!(validation.matching_rows, 1);
                assert_eq!(validation.changed_rows, 0);
                let rollback = service.rollback(report.batch_id, false).await.expect("rollback fixture");
                assert_eq!(rollback.deleted_rows, 1);
                assert_eq!(rollback.skipped_changed_rows, 0);
                let legacy_count = query_scalar::<Postgres, i64>(
                    "SELECT COUNT(*) FROM kovi_bot_memories WHERE id = $1",
                )
                .bind(&memory_id)
                .fetch_one(&pool)
                .await
                .expect("legacy row survives");
                assert_eq!(legacy_count, 1);
                query("DELETE FROM kovi_bot_memories WHERE id = $1").bind(&memory_id).execute(&pool).await.ok();
                query("DELETE FROM yunxi_external_identities WHERE platform = 'qq' AND external_id = $1").bind(subject_id.to_string()).execute(&pool).await.ok();
                query("DELETE FROM yunxi_persons WHERE id = $1").bind(person_id).execute(&pool).await.ok();
            });
    }
}
