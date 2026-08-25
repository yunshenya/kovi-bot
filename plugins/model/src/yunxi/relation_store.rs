use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use yunxi_core::{PersonId, RelationState, RelationStore, RelationStoreError, RelationStoreFuture};

#[derive(Debug, Clone)]
pub(crate) struct PostgresRelationStore {
    pool: PgPool,
}

impl PostgresRelationStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_relations (
                person_id UUID PRIMARY KEY REFERENCES yunxi_persons(id) ON DELETE CASCADE,
                familiarity DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (familiarity BETWEEN -1 AND 1),
                affinity DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (affinity BETWEEN -1 AND 1),
                trust DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (trust BETWEEN -1 AND 1),
                comfort DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (comfort BETWEEN -1 AND 1),
                tension DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (tension BETWEEN -1 AND 1),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

impl RelationStore for PostgresRelationStore {
    fn get<'a>(&'a self, person_id: PersonId) -> RelationStoreFuture<'a, Option<RelationState>> {
        Box::pin(async move {
            let row = query(
                "SELECT familiarity, affinity, trust, comfort, tension
                 FROM yunxi_relations WHERE person_id = $1",
            )
            .bind(person_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(RelationStoreError::storage)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let state = RelationState {
                person_id,
                familiarity: row
                    .try_get::<f64, _>("familiarity")
                    .map_err(RelationStoreError::storage)? as f32,
                affinity: row
                    .try_get::<f64, _>("affinity")
                    .map_err(RelationStoreError::storage)? as f32,
                trust: row
                    .try_get::<f64, _>("trust")
                    .map_err(RelationStoreError::storage)? as f32,
                comfort: row
                    .try_get::<f64, _>("comfort")
                    .map_err(RelationStoreError::storage)? as f32,
                tension: row
                    .try_get::<f64, _>("tension")
                    .map_err(RelationStoreError::storage)? as f32,
            };
            state.validate().map_err(|error| {
                RelationStoreError::storage(std::io::Error::other(error.to_string()))
            })?;
            Ok(Some(state))
        })
    }

    fn set<'a>(&'a self, state: RelationState) -> RelationStoreFuture<'a, RelationState> {
        Box::pin(async move {
            state
                .validate()
                .map_err(|_| RelationStoreError::InvalidState)?;
            query(
                "INSERT INTO yunxi_relations
                    (person_id, familiarity, affinity, trust, comfort, tension)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (person_id) DO UPDATE SET
                    familiarity = EXCLUDED.familiarity,
                    affinity = EXCLUDED.affinity,
                    trust = EXCLUDED.trust,
                    comfort = EXCLUDED.comfort,
                    tension = EXCLUDED.tension,
                    updated_at = NOW()",
            )
            .bind(state.person_id.into_uuid())
            .bind(f64::from(state.familiarity))
            .bind(f64::from(state.affinity))
            .bind(f64::from(state.trust))
            .bind(f64::from(state.comfort))
            .bind(f64::from(state.tension))
            .execute(&self.pool)
            .await
            .map_err(RelationStoreError::storage)?;
            Ok(state)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::PostgresRelationStore;
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use sqlx_core::query::query;
    use sqlx_postgres::PgPoolOptions;
    use yunxi_core::{PersonId, RelationState, RelationStore};

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_relation_store_reads_optional_state() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
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
                let store = PostgresRelationStore::new(pool.clone());
                store
                    .initialize_schema()
                    .await
                    .expect("should initialize relation schema");

                let person_id = PersonId::new();
                query("INSERT INTO yunxi_persons (id) VALUES ($1)")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should create isolated person");

                assert_eq!(
                    store
                        .get(person_id)
                        .await
                        .expect("should read missing relation"),
                    None
                );

                let expected = RelationState {
                    person_id,
                    familiarity: 0.25,
                    affinity: 0.5,
                    trust: -0.25,
                    comfort: 0.75,
                    tension: -0.5,
                };
                assert_eq!(
                    store.set(expected).await.expect("should persist relation"),
                    expected
                );

                assert_eq!(
                    store.get(person_id).await.expect("should reload relation"),
                    Some(expected)
                );

                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean up isolated person");
            });
    }
}
