use super::owner_lock::{self, DurableOwner};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, Postgres};
use std::str::FromStr;
use uuid::Uuid;
use yunxi_core::{
    ConversationId, ConversationKind, ExternalConversation, ExternalIdentity, IdentityStore,
    IdentityStoreError, IdentityStoreFuture, MessageId, PersonId,
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
    pub(crate) memories: u64,
    pub(crate) open_loops: u64,
    pub(crate) affect_states: u64,
    pub(crate) relations: u64,
    pub(crate) goals: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QqPersonDomainTargets {
    pub(crate) person_id: Option<PersonId>,
    pub(crate) qq_user_ids: Vec<i64>,
    pub(crate) direct_conversation_ids: Vec<ConversationId>,
}

const MAX_QQ_PERSON_ALIASES: usize = 256;
const MAX_QQ_PERSON_DIRECT_CONVERSATIONS: usize = 256;

impl PersonDomainDeletion {
    #[must_use]
    pub(crate) const fn total(self) -> u64 {
        self.persons
            + self.conversations
            + self.external_identities
            + self.external_conversations
            + self.message_mappings
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

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        let person_id = query_scalar::<Postgres, Uuid>(
            "SELECT person_id FROM yunxi_external_identities
             WHERE platform = $1 AND external_id = $2
             FOR UPDATE",
        )
        .bind(external_identity.platform().as_str())
        .bind(external_identity.external_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        let qq_alias_external_ids =
            if is_qq_direct_for_identity(external_identity, direct_conversation) {
                if let Some(person_id) = person_id {
                    let aliases = query_scalar::<Postgres, String>(
                        "SELECT external_id FROM yunxi_external_identities
                     WHERE platform = 'qq' AND person_id = $1
                       AND external_id ~ '^[1-9][0-9]*$'
                     ORDER BY external_id
                     LIMIT $2
                     FOR UPDATE",
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
        let mut conversation_ids = if let Some(qq_alias_external_ids) = qq_alias_external_ids {
            query_scalar::<Postgres, Uuid>(
                r#"
                SELECT external.conversation_id
                FROM yunxi_external_conversations AS external
                JOIN yunxi_conversations AS conversation
                  ON conversation.id = external.conversation_id
                WHERE external.platform = 'qq'
                  AND conversation.kind = 'direct'
                  AND external.external_id ~ '^direct:[1-9][0-9]*:[1-9][0-9]*$'
                  AND split_part(external.external_id, ':', 3) = ANY($1)
                ORDER BY external.conversation_id, external.external_id
                FOR UPDATE OF external, conversation
                "#,
            )
            .bind(&qq_alias_external_ids)
            .fetch_all(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
        } else {
            query_scalar::<Postgres, Uuid>(
                "SELECT external.conversation_id
                 FROM yunxi_external_conversations AS external
                 JOIN yunxi_conversations AS conversation
                   ON conversation.id = external.conversation_id
                 WHERE external.platform = $1 AND external.external_id = $2
                   AND conversation.kind = 'direct'
                 FOR UPDATE OF external, conversation",
            )
            .bind(direct_conversation.platform().as_str())
            .bind(direct_conversation.external_id())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(IdentityStoreError::storage)?
            .into_iter()
            .collect()
        };
        conversation_ids.sort_unstable();
        conversation_ids.dedup();

        if let Some(person_id) = person_id {
            owner_lock::lock_owner(&mut transaction, DurableOwner::Person(person_id))
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

        let mut deleted = PersonDomainDeletion::default();
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

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
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
            CREATE INDEX IF NOT EXISTS yunxi_message_mappings_conversation_idx
                ON yunxi_message_mappings (conversation_id, platform, external_message_id)
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
        if let Some(existing) = query_scalar::<Postgres, Uuid>(
            r#"
            SELECT person_id
            FROM yunxi_external_identities
            WHERE platform = $1 AND external_id = $2
            "#,
        )
        .bind(external.platform().as_str())
        .bind(external.external_id())
        .fetch_optional(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)?
        {
            return Ok(PersonId::from_uuid(existing));
        }

        let candidate = PersonId::new();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
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
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(PersonId::from_uuid(winner))
    }

    pub(crate) async fn resolve_conversation(
        &self,
        external: &ExternalConversation,
    ) -> Result<ConversationId, IdentityStoreError> {
        if let Some(row) = query(
            r#"
            SELECT conversation.id, conversation.kind
            FROM yunxi_external_conversations AS external
            JOIN yunxi_conversations AS conversation
              ON conversation.id = external.conversation_id
            WHERE external.platform = $1 AND external.external_id = $2
            "#,
        )
        .bind(external.platform().as_str())
        .bind(external.external_id())
        .fetch_optional(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)?
        {
            let id = row
                .try_get::<Uuid, _>("id")
                .map_err(IdentityStoreError::storage)?;
            let kind = parse_stored_kind(
                &row.try_get::<String, _>("kind")
                    .map_err(IdentityStoreError::storage)?,
            )?;
            ensure_kind(external.kind(), kind)?;
            return Ok(ConversationId::from_uuid(id));
        }

        let candidate = ConversationId::new();
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(IdentityStoreError::storage)?;
        query("INSERT INTO yunxi_conversations (id, kind) VALUES ($1, $2)")
            .bind(candidate.into_uuid())
            .bind(external.kind().as_str())
            .execute(&mut *transaction)
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
        .fetch_one(&mut *transaction)
        .await
        .map_err(IdentityStoreError::storage)?;
        let stored_kind =
            query_scalar::<Postgres, String>("SELECT kind FROM yunxi_conversations WHERE id = $1")
                .bind(winner)
                .fetch_one(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?;
        ensure_kind(external.kind(), parse_stored_kind(&stored_kind)?)?;
        if winner != candidate.into_uuid() {
            query("DELETE FROM yunxi_conversations WHERE id = $1")
                .bind(candidate.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(IdentityStoreError::storage)?;
        }
        transaction
            .commit()
            .await
            .map_err(IdentityStoreError::storage)?;
        Ok(ConversationId::from_uuid(winner))
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
    use super::PostgresIdentityStore;
    use crate::memory::MemoryManager;
    use crate::yunxi::affect_store::PostgresAffectStore;
    use crate::yunxi::goal_store::PostgresGoalStore;
    use crate::yunxi::memory_store::PostgresMemoryStore;
    use crate::yunxi::open_loop_store::PostgresOpenLoopStore;
    use crate::yunxi::qq;
    use crate::yunxi::relation_store::PostgresRelationStore;
    use sqlx_core::error::DatabaseError;
    use sqlx_core::query::query;
    use sqlx_core::query_scalar::query_scalar;
    use sqlx_postgres::{PgPoolOptions, Postgres};
    use std::sync::Arc;
    use uuid::Uuid;
    use yunxi_core::{
        ConversationKind, ExternalConversation, ExternalIdentity, GoalDraft, GoalKind, GoalOwner,
        GoalStore, GoalStoreError, IdentityStoreError, MemoryDraft, MemoryKind, MemoryScope,
        MemoryStore, MemoryStoreError, OpenLoopDraft, OpenLoopKind, OpenLoopOwner, OpenLoopStore,
        OpenLoopStoreError, PlatformId,
    };

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

                let person_count_before = query_scalar::<Postgres, i64>(
                    "SELECT COUNT(*) FROM yunxi_persons",
                )
                .fetch_one(&pool)
                .await
                .expect("应读取 person 数量");
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
                assert_eq!(
                    query_scalar::<Postgres, i64>("SELECT COUNT(*) FROM yunxi_persons")
                        .fetch_one(&pool)
                        .await
                        .expect("应读取 person 数量"),
                    person_count_before + 1,
                    "并发输掉的候选 person 必须删除"
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

                let conversation_count_before = query_scalar::<Postgres, i64>(
                    "SELECT COUNT(*) FROM yunxi_conversations",
                )
                .fetch_one(&pool)
                .await
                .expect("应读取 conversation 数量");
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
                    query_scalar::<Postgres, i64>("SELECT COUNT(*) FROM yunxi_conversations")
                        .fetch_one(&pool)
                        .await
                        .expect("应读取 conversation 数量"),
                    conversation_count_before + 1,
                    "并发输掉的候选 conversation 必须删除"
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
                    .resolve_conversation(&direct)
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
                    Err(IdentityStoreError::ConversationKindMismatch {
                        requested: ConversationKind::Direct,
                        stored: ConversationKind::Group,
                    })
                ));
                assert_eq!(
                    query_scalar::<Postgres, i64>("SELECT COUNT(*) FROM yunxi_conversations")
                        .fetch_one(&pool)
                        .await
                        .expect("应读取 conversation 数量"),
                    conversation_count_before + 3
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
                    .resolve_conversation(&first_direct)
                    .await
                    .expect("first conversation should resolve");
                let second_conversation_id = store
                    .resolve_conversation(&second_direct)
                    .await
                    .expect("second conversation should resolve");
                let alias_conversation_id = store
                    .resolve_conversation(&alias_direct)
                    .await
                    .expect("alias conversation should resolve");
                let group_conversation_id = store
                    .resolve_conversation(&group)
                    .await
                    .expect("group conversation should resolve");
                let person_uuid = person_id.into_uuid();
                let first_conversation_uuid = first_conversation_id.into_uuid();
                let second_conversation_uuid = second_conversation_id.into_uuid();
                let alias_conversation_uuid = alias_conversation_id.into_uuid();
                let group_conversation_uuid = group_conversation_id.into_uuid();

                for (scope_kind, scope_id) in [
                    ("person", person_uuid),
                    ("conversation", first_conversation_uuid),
                    ("conversation", second_conversation_uuid),
                    ("conversation", alias_conversation_uuid),
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
                    (group_conversation_uuid, 4_i64),
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
                assert_eq!(deleted.conversations, 3);
                assert_eq!(deleted.external_identities, 2);
                assert_eq!(deleted.external_conversations, 3);
                assert_eq!(deleted.message_mappings, 3);
                assert_eq!(deleted.memories, 4);
                assert_eq!(deleted.open_loops, 4);
                assert_eq!(deleted.affect_states, 1);
                assert_eq!(deleted.relations, 1);
                assert_eq!(deleted.goals, 4);
                assert_eq!(deleted.total(), 26);

                for (table, column, id) in [
                    ("yunxi_persons", "id", person_uuid),
                    ("yunxi_conversations", "id", first_conversation_uuid),
                    ("yunxi_conversations", "id", second_conversation_uuid),
                    ("yunxi_conversations", "id", alias_conversation_uuid),
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
                    .resolve_conversation(&direct)
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
}
