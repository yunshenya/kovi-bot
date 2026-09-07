//! Durable idempotency boundary for Core-owned QQ actions.
//!
//! The in-memory outgoing lifecycle protects one running process. This ledger
//! closes the remaining crash/restart gap by recording the exact platform
//! envelope before transport is allowed to start. Message bodies are not
//! retained: a SHA-256 of the complete envelope is stored alongside explicit
//! target context so a reused key with different semantics fails closed.

use super::owner_lock::{self, DurableOwner};
use anyhow::{Context, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, Postgres};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;
use yunxi_core::{ConversationId, MessageContent, MessageId, PersonId};

const DELIVERY_LEDGER_TABLE: &str = "yunxi_action_delivery_ledger";
const MAX_FAILURE_CATEGORY_BYTES: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryActionKind {
    SendMessage,
    ReachOut,
}

impl DeliveryActionKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SendMessage => "send_message",
            Self::ReachOut => "reach_out",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub(crate) enum DeliveryTarget {
    Conversation(ConversationId),
    Person(PersonId),
}

impl DeliveryTarget {
    const fn kind(self) -> &'static str {
        match self {
            Self::Conversation(_) => "conversation",
            Self::Person(_) => "person",
        }
    }

    const fn id(self) -> Uuid {
        match self {
            Self::Conversation(id) => id.into_uuid(),
            Self::Person(id) => id.into_uuid(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryDestinationKind {
    Group,
    Private,
}

impl DeliveryDestinationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Private => "private",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryStatus {
    Prepared,
    Committed,
    Sent,
    Unknown,
    Failed,
}

impl DeliveryStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Sent => "sent",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "committed" => Ok(Self::Committed),
            "sent" => Ok(Self::Sent),
            "unknown" => Ok(Self::Unknown),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("unknown durable delivery status {other}"),
        }
    }
}

impl fmt::Display for DeliveryStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Fingerprinted QQ envelope plus the target columns retained for audit and
/// collision diagnosis. The plaintext content is deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryAttempt {
    fingerprint: [u8; 32],
    action_kind: DeliveryActionKind,
    target: DeliveryTarget,
    conversation_id: ConversationId,
    destination_kind: DeliveryDestinationKind,
    destination_id: i64,
    core_reply_to: Option<MessageId>,
    external_reply_to: Option<i64>,
}

#[derive(Serialize)]
struct FingerprintEnvelope<'a> {
    schema: &'static str,
    action_kind: DeliveryActionKind,
    target: DeliveryTarget,
    conversation_id: ConversationId,
    destination_kind: DeliveryDestinationKind,
    destination_id: i64,
    content: &'a MessageContent,
    core_reply_to: Option<MessageId>,
    external_reply_to: Option<i64>,
}

impl DeliveryAttempt {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        action_kind: DeliveryActionKind,
        target: DeliveryTarget,
        conversation_id: ConversationId,
        destination_kind: DeliveryDestinationKind,
        destination_id: i64,
        content: &MessageContent,
        core_reply_to: Option<MessageId>,
        external_reply_to: Option<i64>,
    ) -> anyhow::Result<Self> {
        ensure!(
            destination_id > 0,
            "QQ delivery destination must be positive"
        );
        ensure!(
            external_reply_to.is_none_or(|message_id| message_id > 0),
            "QQ reply message id must be positive"
        );
        ensure!(
            matches!(
                (action_kind, target),
                (
                    DeliveryActionKind::SendMessage,
                    DeliveryTarget::Conversation(_)
                ) | (DeliveryActionKind::ReachOut, DeliveryTarget::Person(_))
            ),
            "delivery action kind and canonical target do not match"
        );

        let encoded = serde_json::to_vec(&FingerprintEnvelope {
            schema: "yunxi-qq-delivery-envelope-v1",
            action_kind,
            target,
            conversation_id,
            destination_kind,
            destination_id,
            content,
            core_reply_to,
            external_reply_to,
        })
        .context("serialize durable QQ delivery envelope")?;
        let fingerprint: [u8; 32] = Sha256::digest(encoded).into();
        Ok(Self {
            fingerprint,
            action_kind,
            target,
            conversation_id,
            destination_kind,
            destination_id,
            core_reply_to,
            external_reply_to,
        })
    }
}

#[derive(Debug)]
pub(crate) enum DeliveryCommitOutcome {
    Acquired(DurableCommittedDelivery),
    AlreadyRecorded {
        status: DeliveryStatus,
        external_message_id: Option<i64>,
    },
    EnvelopeConflict,
}

#[derive(Debug, Error)]
pub(crate) enum DeliveryCommitError {
    #[error("durable delivery {owner_kind} target no longer exists")]
    OwnerMissing { owner_kind: &'static str },
    #[error(transparent)]
    Ledger(#[from] anyhow::Error),
}

impl From<sqlx_core::error::Error> for DeliveryCommitError {
    fn from(error: sqlx_core::error::Error) -> Self {
        Self::Ledger(error.into())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresDeliveryLedger {
    pool: PgPool,
}

impl PostgresDeliveryLedger {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {DELIVERY_LEDGER_TABLE} (
                delivery_key TEXT COLLATE "C" PRIMARY KEY
                    CHECK (octet_length(delivery_key) BETWEEN 1 AND 256
                       AND char_length(delivery_key) BETWEEN 1 AND 128
                       AND btrim(delivery_key) <> ''),
                envelope_fingerprint BYTEA NOT NULL
                    CHECK (octet_length(envelope_fingerprint) = 32),
                action_kind TEXT NOT NULL
                    CHECK (action_kind IN ('send_message', 'reach_out')),
                target_kind TEXT NOT NULL
                    CHECK (target_kind IN ('conversation', 'person')),
                target_id UUID NOT NULL,
                conversation_id UUID NOT NULL,
                destination_kind TEXT NOT NULL
                    CHECK (destination_kind IN ('group', 'private')),
                destination_id BIGINT NOT NULL CHECK (destination_id > 0),
                core_reply_to UUID,
                external_reply_to BIGINT CHECK (
                    external_reply_to IS NULL OR external_reply_to > 0
                ),
                status TEXT NOT NULL
                    CHECK (status IN ('prepared', 'committed', 'sent', 'unknown', 'failed')),
                attempt_count INTEGER NOT NULL DEFAULT 1 CHECK (attempt_count > 0),
                external_message_id BIGINT CHECK (
                    external_message_id IS NULL OR external_message_id > 0
                ),
                last_error TEXT CHECK (
                    last_error IS NULL OR octet_length(last_error) <= 1024
                ),
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                committed_at TIMESTAMPTZ,
                sent_at TIMESTAMPTZ,
                failed_at TIMESTAMPTZ,
                unknown_at TIMESTAMPTZ,
                CHECK (
                    (action_kind = 'send_message' AND target_kind = 'conversation')
                    OR (action_kind = 'reach_out' AND target_kind = 'person')
                )
            )
            "#
        ))
        .execute(&mut *transaction)
        .await?;
        query(&format!(
            "CREATE INDEX IF NOT EXISTS yunxi_action_delivery_ledger_status_idx \
             ON {DELIVERY_LEDGER_TABLE} (status, updated_at)"
        ))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Atomically reserve an idempotency key and publish `committed` before
    /// the caller can cross the network boundary. Existing uncertain or sent
    /// rows are permanent replay barriers. A definite `failed` row can be
    /// retried only with the identical envelope fingerprint and target.
    pub(crate) async fn commit_attempt(
        &self,
        delivery_key: &str,
        attempt: &DeliveryAttempt,
    ) -> Result<DeliveryCommitOutcome, DeliveryCommitError> {
        let mut transaction = self.pool.begin().await?;
        if let DeliveryTarget::Person(person_id) = attempt.target {
            let exists = owner_lock::lock_and_owner_exists(
                &mut transaction,
                DurableOwner::Person(person_id.into_uuid()),
            )
            .await?;
            if !exists {
                return Err(DeliveryCommitError::OwnerMissing {
                    owner_kind: "person",
                });
            }
        }
        let mut conversation_ids = vec![attempt.conversation_id.into_uuid()];
        if let DeliveryTarget::Conversation(conversation_id) = attempt.target {
            conversation_ids.push(conversation_id.into_uuid());
        }
        conversation_ids.sort_unstable();
        conversation_ids.dedup();
        for conversation_id in conversation_ids {
            let exists = owner_lock::lock_and_owner_exists(
                &mut transaction,
                DurableOwner::Conversation(conversation_id),
            )
            .await?;
            if !exists {
                return Err(DeliveryCommitError::OwnerMissing {
                    owner_kind: "conversation",
                });
            }
        }
        let inserted = query(&format!(
            r#"
            INSERT INTO {DELIVERY_LEDGER_TABLE} (
                delivery_key, envelope_fingerprint, action_kind, target_kind,
                target_id, conversation_id, destination_kind, destination_id,
                core_reply_to, external_reply_to, status
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'prepared')
            ON CONFLICT (delivery_key) DO NOTHING
            "#
        ))
        .bind(delivery_key)
        .bind(attempt.fingerprint.as_slice())
        .bind(attempt.action_kind.as_str())
        .bind(attempt.target.kind())
        .bind(attempt.target.id())
        .bind(attempt.conversation_id.into_uuid())
        .bind(attempt.destination_kind.as_str())
        .bind(attempt.destination_id)
        .bind(attempt.core_reply_to.map(MessageId::into_uuid))
        .bind(attempt.external_reply_to)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        let row = query(&format!(
            r#"
            SELECT envelope_fingerprint, action_kind, target_kind, target_id,
                   conversation_id, destination_kind, destination_id,
                   core_reply_to, external_reply_to, status, external_message_id
            FROM {DELIVERY_LEDGER_TABLE}
            WHERE delivery_key = $1
            FOR UPDATE
            "#
        ))
        .bind(delivery_key)
        .fetch_one(&mut *transaction)
        .await?;

        if !row_matches_attempt(&row, attempt)? {
            transaction.rollback().await?;
            return Ok(DeliveryCommitOutcome::EnvelopeConflict);
        }

        let status = DeliveryStatus::parse(row.try_get::<&str, _>("status")?)?;
        if !inserted && status != DeliveryStatus::Failed {
            let external_message_id = row.try_get("external_message_id")?;
            transaction.rollback().await?;
            return Ok(DeliveryCommitOutcome::AlreadyRecorded {
                status,
                external_message_id,
            });
        }

        let transitioned = if inserted {
            query(&format!(
                r#"
                UPDATE {DELIVERY_LEDGER_TABLE}
                SET status = 'committed', committed_at = NOW(), updated_at = NOW()
                WHERE delivery_key = $1
                  AND envelope_fingerprint = $2
                  AND status = 'prepared'
                "#
            ))
            .bind(delivery_key)
            .bind(attempt.fingerprint.as_slice())
            .execute(&mut *transaction)
            .await?
        } else {
            query(&format!(
                r#"
                UPDATE {DELIVERY_LEDGER_TABLE}
                SET status = 'committed', attempt_count = attempt_count + 1,
                    external_message_id = NULL, last_error = NULL,
                    committed_at = NOW(), sent_at = NULL, failed_at = NULL,
                    unknown_at = NULL, updated_at = NOW()
                WHERE delivery_key = $1
                  AND envelope_fingerprint = $2
                  AND status = 'failed'
                "#
            ))
            .bind(delivery_key)
            .bind(attempt.fingerprint.as_slice())
            .execute(&mut *transaction)
            .await?
        };
        if transitioned.rows_affected() != 1 {
            return Err(
                anyhow::anyhow!("durable delivery commit transition was not exclusive").into(),
            );
        }
        transaction.commit().await?;

        Ok(DeliveryCommitOutcome::Acquired(DurableCommittedDelivery {
            pool: self.pool.clone(),
            delivery_key: delivery_key.to_owned(),
            fingerprint: attempt.fingerprint,
            armed: true,
        }))
    }
}

/// Remove delivery audit rows attributable to a deleted person without
/// treating shared group conversations as person-owned data. The caller owns
/// the surrounding identity-deletion transaction and its owner locks.
pub(crate) async fn delete_person_domain_rows(
    transaction: &mut Transaction<'_, Postgres>,
    person_id: Option<Uuid>,
    direct_conversation_ids: &[Uuid],
    qq_user_ids: &[i64],
) -> Result<u64, sqlx_core::error::Error> {
    query(&format!(
        r#"
        DELETE FROM {DELIVERY_LEDGER_TABLE}
        WHERE ($1::uuid IS NOT NULL
               AND target_kind = 'person' AND target_id = $1)
           OR (target_kind = 'conversation' AND target_id = ANY($2::uuid[]))
           OR conversation_id = ANY($2::uuid[])
           OR (destination_kind = 'private'
               AND destination_id = ANY($3::bigint[]))
        "#
    ))
    .bind(person_id)
    .bind(direct_conversation_ids)
    .bind(qq_user_ids)
    .execute(&mut **transaction)
    .await
    .map(|result| result.rows_affected())
}

/// Remove delivery rows attributable to one QQ group. Person-target/private
/// rows are intentionally outside this boundary.
pub(crate) async fn delete_group_domain_rows(
    transaction: &mut Transaction<'_, Postgres>,
    conversation_id: Option<Uuid>,
    qq_group_id: i64,
) -> Result<u64, sqlx_core::error::Error> {
    query(&format!(
        r#"
        DELETE FROM {DELIVERY_LEDGER_TABLE}
        WHERE ($1::uuid IS NOT NULL AND (
                  (target_kind = 'conversation' AND target_id = $1)
                  OR conversation_id = $1
              ))
           OR (destination_kind = 'group' AND destination_id = $2)
        "#
    ))
    .bind(conversation_id)
    .bind(qq_group_id)
    .execute(&mut **transaction)
    .await
    .map(|result| result.rows_affected())
}

fn row_matches_attempt(
    row: &sqlx_postgres::PgRow,
    attempt: &DeliveryAttempt,
) -> anyhow::Result<bool> {
    Ok(
        row.try_get::<Vec<u8>, _>("envelope_fingerprint")? == attempt.fingerprint.as_slice()
            && row.try_get::<&str, _>("action_kind")? == attempt.action_kind.as_str()
            && row.try_get::<&str, _>("target_kind")? == attempt.target.kind()
            && row.try_get::<Uuid, _>("target_id")? == attempt.target.id()
            && row.try_get::<Uuid, _>("conversation_id")? == attempt.conversation_id.into_uuid()
            && row.try_get::<&str, _>("destination_kind")? == attempt.destination_kind.as_str()
            && row.try_get::<i64, _>("destination_id")? == attempt.destination_id
            && row.try_get::<Option<Uuid>, _>("core_reply_to")?
                == attempt.core_reply_to.map(MessageId::into_uuid)
            && row.try_get::<Option<i64>, _>("external_reply_to")? == attempt.external_reply_to,
    )
}

/// Owns one durable `committed` row until transport reports a terminal state.
/// Dropping the send future attempts to record `unknown`; if the runtime or DB
/// is already gone, `committed` itself remains a permanent replay barrier.
#[derive(Debug)]
pub(crate) struct DurableCommittedDelivery {
    pool: PgPool,
    delivery_key: String,
    fingerprint: [u8; 32],
    armed: bool,
}

impl DurableCommittedDelivery {
    pub(crate) async fn mark_sent(mut self, external_message_id: i64) -> anyhow::Result<()> {
        ensure!(external_message_id > 0, "QQ message id must be positive");
        self.transition(DeliveryStatus::Sent, Some(external_message_id), None)
            .await
    }

    pub(crate) async fn mark_failed(mut self, category: &str) -> anyhow::Result<()> {
        self.transition(
            DeliveryStatus::Failed,
            None,
            Some(bounded_failure_category(category)),
        )
        .await
    }

    pub(crate) async fn mark_unknown(mut self) -> anyhow::Result<()> {
        self.transition(DeliveryStatus::Unknown, None, None).await
    }

    async fn transition(
        &mut self,
        status: DeliveryStatus,
        external_message_id: Option<i64>,
        last_error: Option<String>,
    ) -> anyhow::Result<()> {
        let timestamp_column = match status {
            DeliveryStatus::Sent => "sent_at",
            DeliveryStatus::Unknown => "unknown_at",
            DeliveryStatus::Failed => "failed_at",
            DeliveryStatus::Prepared | DeliveryStatus::Committed => {
                anyhow::bail!("invalid terminal durable delivery transition")
            }
        };
        let result = query(&format!(
            r#"
            UPDATE {DELIVERY_LEDGER_TABLE}
            SET status = $3, external_message_id = $4, last_error = $5,
                {timestamp_column} = NOW(), updated_at = NOW()
            WHERE delivery_key = $1
              AND envelope_fingerprint = $2
              AND status = 'committed'
            "#
        ))
        .bind(&self.delivery_key)
        .bind(self.fingerprint.as_slice())
        .bind(status.as_str())
        .bind(external_message_id)
        .bind(last_error)
        .execute(&self.pool)
        .await?;
        ensure!(
            result.rows_affected() == 1,
            "durable delivery terminal transition was rejected"
        );
        self.armed = false;
        Ok(())
    }
}

impl Drop for DurableCommittedDelivery {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = kovi::tokio::runtime::Handle::try_current() else {
            return;
        };
        let pool = self.pool.clone();
        let delivery_key = self.delivery_key.clone();
        let fingerprint = self.fingerprint;
        runtime.spawn(async move {
            if let Err(error) = query(&format!(
                r#"
                UPDATE {DELIVERY_LEDGER_TABLE}
                SET status = 'unknown', unknown_at = NOW(), updated_at = NOW()
                WHERE delivery_key = $1
                  AND envelope_fingerprint = $2
                  AND status = 'committed'
                "#
            ))
            .bind(&delivery_key)
            .bind(fingerprint.as_slice())
            .execute(&pool)
            .await
            {
                kovi::log::warn!("could not persist unknown durable QQ delivery outcome: {error}");
            }
        });
    }
}

fn bounded_failure_category(value: &str) -> String {
    let mut bounded = String::with_capacity(value.len().min(MAX_FAILURE_CATEGORY_BYTES));
    for character in value.chars() {
        if bounded.len().saturating_add(character.len_utf8()) > MAX_FAILURE_CATEGORY_BYTES {
            break;
        }
        bounded.push(character);
    }
    bounded
}

#[cfg(test)]
mod tests {
    use super::{
        DELIVERY_LEDGER_TABLE, DeliveryActionKind, DeliveryAttempt, DeliveryCommitError,
        DeliveryCommitOutcome, DeliveryDestinationKind, DeliveryStatus, DeliveryTarget,
        PostgresDeliveryLedger,
    };
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use crate::yunxi::owner_lock::{self, DurableOwner};
    use sqlx_core::query::query;
    use sqlx_core::query_scalar::query_scalar;
    use sqlx_postgres::{PgPool, PgPoolOptions, Postgres};
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;
    use yunxi_core::{ConversationId, MessageContent, PersonId};

    fn attempt(content: &str) -> DeliveryAttempt {
        attempt_for(
            PersonId::from_uuid(Uuid::from_u128(11)),
            ConversationId::from_uuid(Uuid::from_u128(22)),
            33,
            content,
        )
    }

    fn attempt_for(
        person_id: PersonId,
        conversation_id: ConversationId,
        destination_id: i64,
        content: &str,
    ) -> DeliveryAttempt {
        DeliveryAttempt::new(
            DeliveryActionKind::ReachOut,
            DeliveryTarget::Person(person_id),
            conversation_id,
            DeliveryDestinationKind::Private,
            destination_id,
            &MessageContent::text(content),
            None,
            None,
        )
        .expect("attempt should be valid")
    }

    fn same_route_attempt(attempt: &DeliveryAttempt, content: &str) -> DeliveryAttempt {
        DeliveryAttempt::new(
            attempt.action_kind,
            attempt.target,
            attempt.conversation_id,
            attempt.destination_kind,
            attempt.destination_id,
            &MessageContent::text(content),
            attempt.core_reply_to,
            attempt.external_reply_to,
        )
        .expect("same route attempt should be valid")
    }

    #[test]
    fn complete_envelope_fingerprint_is_stable_and_contextual() {
        let first = attempt("hello");
        assert_eq!(first, attempt("hello"));
        assert_ne!(first.fingerprint, attempt("different").fingerprint);

        let other_target = DeliveryAttempt::new(
            DeliveryActionKind::ReachOut,
            DeliveryTarget::Person(PersonId::from_uuid(Uuid::from_u128(44))),
            first.conversation_id,
            first.destination_kind,
            first.destination_id,
            &MessageContent::text("hello"),
            None,
            None,
        )
        .expect("attempt should be valid");
        assert_ne!(first.fingerprint, other_target.fingerprint);
    }

    async fn postgres_store() -> (PgPool, PostgresDeliveryLedger) {
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
        let store = PostgresDeliveryLedger::new(pool.clone());
        store
            .initialize_schema()
            .await
            .expect("should initialize delivery ledger schema");
        (pool, store)
    }

    async fn postgres_attempt(pool: &PgPool, content: &str) -> DeliveryAttempt {
        let person_id = PersonId::new();
        let conversation_id = ConversationId::new();
        query("INSERT INTO yunxi_persons (id) VALUES ($1)")
            .bind(person_id.into_uuid())
            .execute(pool)
            .await
            .expect("should create delivery person");
        query("INSERT INTO yunxi_conversations (id, kind) VALUES ($1, 'direct')")
            .bind(conversation_id.into_uuid())
            .execute(pool)
            .await
            .expect("should create delivery conversation");
        let destination_id = (Uuid::new_v4().as_u128() % 8_000_000_000 + 1) as i64;
        attempt_for(person_id, conversation_id, destination_id, content)
    }

    async fn postgres_conversation_attempt(pool: &PgPool, content: &str) -> DeliveryAttempt {
        let conversation_id = ConversationId::new();
        query("INSERT INTO yunxi_conversations (id, kind) VALUES ($1, 'group')")
            .bind(conversation_id.into_uuid())
            .execute(pool)
            .await
            .expect("should create group delivery conversation");
        let destination_id = (Uuid::new_v4().as_u128() % 8_000_000_000 + 1) as i64;
        DeliveryAttempt::new(
            DeliveryActionKind::SendMessage,
            DeliveryTarget::Conversation(conversation_id),
            conversation_id,
            DeliveryDestinationKind::Group,
            destination_id,
            &MessageContent::text(content),
            None,
            None,
        )
        .expect("conversation attempt should be valid")
    }

    async fn cleanup(pool: &PgPool, key: &str, attempt: &DeliveryAttempt) {
        query(&format!(
            "DELETE FROM {DELIVERY_LEDGER_TABLE} WHERE delivery_key = $1"
        ))
        .bind(key)
        .execute(pool)
        .await
        .expect("should clean delivery ledger row");
        query("DELETE FROM yunxi_conversations WHERE id = $1")
            .bind(attempt.conversation_id.into_uuid())
            .execute(pool)
            .await
            .expect("should clean delivery conversation");
        let DeliveryTarget::Person(person_id) = attempt.target else {
            return;
        };
        query("DELETE FROM yunxi_persons WHERE id = $1")
            .bind(person_id.into_uuid())
            .execute(pool)
            .await
            .expect("should clean delivery person");
    }

    async fn concurrent_commit_pair(
        store: &PostgresDeliveryLedger,
        key: &str,
        first: &DeliveryAttempt,
        second: &DeliveryAttempt,
    ) -> (DeliveryCommitOutcome, DeliveryCommitOutcome) {
        let start = Arc::new(kovi::tokio::sync::Barrier::new(3));
        let first_store = store.clone();
        let first_key = key.to_owned();
        let first_attempt = first.clone();
        let first_start = Arc::clone(&start);
        let first_task = kovi::tokio::spawn(async move {
            first_start.wait().await;
            first_store.commit_attempt(&first_key, &first_attempt).await
        });
        let second_store = store.clone();
        let second_key = key.to_owned();
        let second_attempt = second.clone();
        let second_start = Arc::clone(&start);
        let second_task = kovi::tokio::spawn(async move {
            second_start.wait().await;
            second_store
                .commit_attempt(&second_key, &second_attempt)
                .await
        });
        start.wait().await;
        let (first_result, second_result) = kovi::tokio::join!(first_task, second_task);
        (
            first_result
                .expect("first concurrent delivery task should join")
                .expect("first concurrent delivery commit should not fail"),
            second_result
                .expect("second concurrent delivery task should join")
                .expect("second concurrent delivery commit should not fail"),
        )
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_restart_blocks_a_committed_delivery() {
        crate::database_test_support::block_on(async {
            let (pool, first_process) = postgres_store().await;
            let key = format!("delivery-restart:{}", Uuid::new_v4());
            let envelope = postgres_attempt(&pool, "restart gap").await;
            let DeliveryCommitOutcome::Acquired(committed) = first_process
                .commit_attempt(&key, &envelope)
                .await
                .expect("first process should commit")
            else {
                panic!("first process should acquire delivery")
            };
            // Simulate abrupt process death: no Drop task runs, so PostgreSQL
            // retains the conservative Committed state.
            std::mem::forget(committed);

            let restarted_process = PostgresDeliveryLedger::new(pool.clone());
            assert!(matches!(
                restarted_process
                    .commit_attempt(&key, &envelope)
                    .await
                    .expect("restart lookup should succeed"),
                DeliveryCommitOutcome::AlreadyRecorded {
                    status: DeliveryStatus::Committed,
                    ..
                }
            ));
            cleanup(&pool, &key, &envelope).await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_reused_key_with_different_envelope_fails_closed() {
        crate::database_test_support::block_on(async {
            let (pool, store) = postgres_store().await;
            let key = format!("delivery-conflict:{}", Uuid::new_v4());
            let envelope = postgres_attempt(&pool, "original").await;
            let DeliveryCommitOutcome::Acquired(committed) = store
                .commit_attempt(&key, &envelope)
                .await
                .expect("original should commit")
            else {
                panic!("original should acquire delivery")
            };
            committed
                .mark_sent(77)
                .await
                .expect("original should be marked sent");

            assert!(matches!(
                store
                    .commit_attempt(&key, &same_route_attempt(&envelope, "changed"))
                    .await
                    .expect("collision lookup should succeed"),
                DeliveryCommitOutcome::EnvelopeConflict
            ));
            cleanup(&pool, &key, &envelope).await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_definite_failure_retries_only_the_same_envelope() {
        crate::database_test_support::block_on(async {
            let (pool, store) = postgres_store().await;
            let key = format!("delivery-failed:{}", Uuid::new_v4());
            let envelope = postgres_attempt(&pool, "retry me").await;
            let DeliveryCommitOutcome::Acquired(committed) = store
                .commit_attempt(&key, &envelope)
                .await
                .expect("first attempt should commit")
            else {
                panic!("first attempt should acquire delivery")
            };
            committed
                .mark_failed("qq_rejected")
                .await
                .expect("definite failure should be persisted");

            let restarted_process = PostgresDeliveryLedger::new(pool.clone());
            assert!(matches!(
                restarted_process
                    .commit_attempt(&key, &same_route_attempt(&envelope, "not the same"))
                    .await
                    .expect("changed failed-row lookup should succeed"),
                DeliveryCommitOutcome::EnvelopeConflict
            ));
            let DeliveryCommitOutcome::Acquired(retry) = restarted_process
                .commit_attempt(&key, &envelope)
                .await
                .expect("identical failed attempt should retry")
            else {
                panic!("identical failed attempt should be acquired")
            };
            assert_eq!(
                query_scalar::<Postgres, i32>(&format!(
                    "SELECT attempt_count FROM {DELIVERY_LEDGER_TABLE} WHERE delivery_key = $1"
                ))
                .bind(&key)
                .fetch_one(&pool)
                .await
                .expect("should read retry attempt count"),
                2
            );
            retry
                .mark_sent(88)
                .await
                .expect("retry should be marked sent");
            let second_restart = PostgresDeliveryLedger::new(pool.clone());
            assert!(matches!(
                second_restart
                    .commit_attempt(&key, &envelope)
                    .await
                    .expect("sent lookup should succeed"),
                DeliveryCommitOutcome::AlreadyRecorded {
                    status: DeliveryStatus::Sent,
                    external_message_id: Some(88),
                }
            ));
            assert!(matches!(
                store
                    .commit_attempt(&key, &same_route_attempt(&envelope, "not the same"))
                    .await
                    .expect("changed retry lookup should succeed"),
                DeliveryCommitOutcome::EnvelopeConflict
            ));
            cleanup(&pool, &key, &envelope).await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_concurrent_identical_key_has_one_acquirer_and_one_replay_barrier() {
        crate::database_test_support::block_on(async {
            let (pool, store) = postgres_store().await;
            let key = format!("delivery-concurrent-identical:{}", Uuid::new_v4());
            let envelope = postgres_attempt(&pool, "one network side effect").await;
            let (first, second) = concurrent_commit_pair(&store, &key, &envelope, &envelope).await;
            let (committed, replay) = match (first, second) {
                (DeliveryCommitOutcome::Acquired(committed), replay)
                | (replay, DeliveryCommitOutcome::Acquired(committed)) => (committed, replay),
                _ => panic!("exactly one concurrent contender should acquire the key"),
            };
            assert!(matches!(
                replay,
                DeliveryCommitOutcome::AlreadyRecorded {
                    status: DeliveryStatus::Committed,
                    external_message_id: None,
                }
            ));
            committed
                .mark_sent(99)
                .await
                .expect("winning concurrent delivery should become Sent");

            let restarted_process = PostgresDeliveryLedger::new(pool.clone());
            assert!(matches!(
                restarted_process
                    .commit_attempt(&key, &envelope)
                    .await
                    .expect("restart lookup should succeed"),
                DeliveryCommitOutcome::AlreadyRecorded {
                    status: DeliveryStatus::Sent,
                    external_message_id: Some(99),
                }
            ));
            cleanup(&pool, &key, &envelope).await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_concurrent_changed_envelopes_have_one_acquirer_and_one_conflict() {
        crate::database_test_support::block_on(async {
            let (pool, store) = postgres_store().await;
            let key = format!("delivery-concurrent-conflict:{}", Uuid::new_v4());
            let envelope = postgres_attempt(&pool, "first candidate").await;
            let changed = same_route_attempt(&envelope, "second candidate");
            let (first, second) = concurrent_commit_pair(&store, &key, &envelope, &changed).await;
            let committed = match (first, second) {
                (
                    DeliveryCommitOutcome::Acquired(committed),
                    DeliveryCommitOutcome::EnvelopeConflict,
                )
                | (
                    DeliveryCommitOutcome::EnvelopeConflict,
                    DeliveryCommitOutcome::Acquired(committed),
                ) => committed,
                _ => panic!("changed concurrent envelopes must fail closed for one contender"),
            };
            committed
                .mark_failed("test_cleanup")
                .await
                .expect("winning concurrent delivery should be disarmed");
            cleanup(&pool, &key, &envelope).await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_restart_blocks_an_unknown_delivery() {
        crate::database_test_support::block_on(async {
            let (pool, first_process) = postgres_store().await;
            let key = format!("delivery-unknown:{}", Uuid::new_v4());
            let envelope = postgres_attempt(&pool, "unknown outcome").await;
            let DeliveryCommitOutcome::Acquired(committed) = first_process
                .commit_attempt(&key, &envelope)
                .await
                .expect("first process should commit")
            else {
                panic!("first process should acquire delivery")
            };
            committed
                .mark_unknown()
                .await
                .expect("indeterminate transport should persist Unknown");

            let restarted_process = PostgresDeliveryLedger::new(pool.clone());
            assert!(matches!(
                restarted_process
                    .commit_attempt(&key, &envelope)
                    .await
                    .expect("restart lookup should succeed"),
                DeliveryCommitOutcome::AlreadyRecorded {
                    status: DeliveryStatus::Unknown,
                    external_message_id: None,
                }
            ));
            cleanup(&pool, &key, &envelope).await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_owner_deletion_blocks_then_rejects_a_delivery_commit() {
        crate::database_test_support::block_on(async {
            let (pool, store) = postgres_store().await;
            let key = format!("delivery-delete-race:{}", Uuid::new_v4());
            let envelope = postgres_attempt(&pool, "must not survive purge").await;
            let DeliveryTarget::Person(person_id) = envelope.target else {
                panic!("race fixture must target a person")
            };

            let mut purge = pool.begin().await.expect("should begin purge transaction");
            owner_lock::lock_owner(&mut purge, DurableOwner::Person(person_id.into_uuid()))
                .await
                .expect("should lock person owner");
            owner_lock::lock_owner(
                &mut purge,
                DurableOwner::Conversation(envelope.conversation_id.into_uuid()),
            )
            .await
            .expect("should lock conversation owner");

            let commit_key = key.clone();
            let commit_envelope = envelope.clone();
            let mut commit_task = kovi::tokio::spawn(async move {
                store.commit_attempt(&commit_key, &commit_envelope).await
            });
            assert!(
                kovi::tokio::time::timeout(Duration::from_millis(100), &mut commit_task)
                    .await
                    .is_err(),
                "delivery commit must wait for the deletion owner locks"
            );

            query("DELETE FROM yunxi_persons WHERE id = $1")
                .bind(person_id.into_uuid())
                .execute(&mut *purge)
                .await
                .expect("should delete person while locks are held");
            query("DELETE FROM yunxi_conversations WHERE id = $1")
                .bind(envelope.conversation_id.into_uuid())
                .execute(&mut *purge)
                .await
                .expect("should delete conversation while locks are held");
            purge.commit().await.expect("should commit owner deletion");

            let error = commit_task
                .await
                .expect("delivery task should join")
                .expect_err("deleted owner must reject the waiting commit");
            let DeliveryCommitError::OwnerMissing { owner_kind } = error else {
                panic!("deleted person should return a structured owner-missing error")
            };
            assert_eq!(owner_kind, "person");
            assert_eq!(
                query_scalar::<Postgres, i64>(&format!(
                    "SELECT COUNT(*) FROM {DELIVERY_LEDGER_TABLE} WHERE delivery_key = $1"
                ))
                .bind(&key)
                .fetch_one(&pool)
                .await
                .expect("should count delivery rows"),
                0
            );
            cleanup(&pool, &key, &envelope).await;
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_conversation_deletion_blocks_then_rejects_a_send_commit() {
        crate::database_test_support::block_on(async {
            let (pool, store) = postgres_store().await;
            let key = format!("delivery-group-delete-race:{}", Uuid::new_v4());
            let envelope =
                postgres_conversation_attempt(&pool, "must not survive group purge").await;

            let mut purge = pool.begin().await.expect("should begin purge transaction");
            owner_lock::lock_owner(
                &mut purge,
                DurableOwner::Conversation(envelope.conversation_id.into_uuid()),
            )
            .await
            .expect("should lock conversation owner");

            let commit_key = key.clone();
            let commit_envelope = envelope.clone();
            let mut commit_task = kovi::tokio::spawn(async move {
                store.commit_attempt(&commit_key, &commit_envelope).await
            });
            assert!(
                kovi::tokio::time::timeout(Duration::from_millis(100), &mut commit_task)
                    .await
                    .is_err(),
                "SendMessage commit must wait for the conversation deletion lock"
            );

            query("DELETE FROM yunxi_conversations WHERE id = $1")
                .bind(envelope.conversation_id.into_uuid())
                .execute(&mut *purge)
                .await
                .expect("should delete group conversation while its lock is held");
            purge
                .commit()
                .await
                .expect("should commit conversation deletion");

            let error = commit_task
                .await
                .expect("delivery task should join")
                .expect_err("deleted conversation must reject the waiting SendMessage commit");
            let DeliveryCommitError::OwnerMissing { owner_kind } = error else {
                panic!("deleted conversation should return a structured owner-missing error")
            };
            assert_eq!(owner_kind, "conversation");
            assert_eq!(
                query_scalar::<Postgres, i64>(&format!(
                    "SELECT COUNT(*) FROM {DELIVERY_LEDGER_TABLE} WHERE delivery_key = $1"
                ))
                .bind(&key)
                .fetch_one(&pool)
                .await
                .expect("should count group delivery rows"),
                0
            );
            cleanup(&pool, &key, &envelope).await;
        });
    }
}
