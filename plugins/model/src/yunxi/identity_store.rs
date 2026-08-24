use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, Postgres};
use std::str::FromStr;
use uuid::Uuid;
use yunxi_core::{
    ConversationId, ConversationKind, ExternalConversation, ExternalIdentity, IdentityStore,
    IdentityStoreError, IdentityStoreFuture, PersonId,
};

#[derive(Debug, Clone)]
pub(crate) struct PostgresIdentityStore {
    pool: PgPool,
}

impl PostgresIdentityStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn qq_external_identities_for_person(
        &self,
        person_id: PersonId,
    ) -> Result<Vec<String>, IdentityStoreError> {
        query_scalar::<Postgres, String>(
            "SELECT external_id FROM yunxi_external_identities WHERE platform = 'qq' AND person_id = $1 ORDER BY external_id",
        )
        .bind(person_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(IdentityStoreError::storage)
    }

    pub(crate) async fn qq_external_conversations_for_id(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<(String, ConversationKind)>, IdentityStoreError> {
        let rows = query(
            "SELECT external.external_id, conversation.kind FROM yunxi_external_conversations AS external JOIN yunxi_conversations AS conversation ON conversation.id = external.conversation_id WHERE external.platform = 'qq' AND external.conversation_id = $1 ORDER BY external.external_id",
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

#[cfg(test)]
mod tests {
    use super::PostgresIdentityStore;
    use sqlx_core::error::DatabaseError;
    use sqlx_core::query::query;
    use sqlx_core::query_scalar::query_scalar;
    use sqlx_postgres::{PgPoolOptions, Postgres};
    use std::sync::Arc;
    use uuid::Uuid;
    use yunxi_core::{
        ConversationKind, ExternalConversation, ExternalIdentity, IdentityStoreError, PlatformId,
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
}
