use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use yunxi_core::{AffectState, AffectStore, AffectStoreError, AffectStoreFuture, PersonId};

#[derive(Debug, Clone)]
pub(crate) struct PostgresAffectStore {
    pool: PgPool,
}

impl PostgresAffectStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_affect_states (
                person_id UUID PRIMARY KEY REFERENCES yunxi_persons(id) ON DELETE CASCADE,
                valence DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (valence BETWEEN -1 AND 1),
                arousal DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (arousal BETWEEN -1 AND 1),
                social_energy DOUBLE PRECISION NOT NULL DEFAULT 1 CHECK (social_energy BETWEEN 0 AND 1),
                curiosity DOUBLE PRECISION NOT NULL DEFAULT 0.5 CHECK (curiosity BETWEEN 0 AND 1),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

impl AffectStore for PostgresAffectStore {
    fn get<'a>(&'a self, person_id: PersonId) -> AffectStoreFuture<'a, AffectState> {
        Box::pin(async move {
            let row = query(
                "SELECT valence, arousal, social_energy, curiosity
                 FROM yunxi_affect_states WHERE person_id = $1",
            )
            .bind(person_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(AffectStoreError::storage)?;
            let Some(row) = row else {
                return Ok(AffectState::default());
            };
            let state = AffectState {
                valence: row
                    .try_get::<f64, _>("valence")
                    .map_err(AffectStoreError::storage)? as f32,
                arousal: row
                    .try_get::<f64, _>("arousal")
                    .map_err(AffectStoreError::storage)? as f32,
                social_energy: row
                    .try_get::<f64, _>("social_energy")
                    .map_err(AffectStoreError::storage)? as f32,
                curiosity: row
                    .try_get::<f64, _>("curiosity")
                    .map_err(AffectStoreError::storage)? as f32,
            };
            state
                .validate()
                .map_err(|_| AffectStoreError::InvalidState)?;
            Ok(state)
        })
    }

    fn set<'a>(
        &'a self,
        person_id: PersonId,
        state: AffectState,
    ) -> AffectStoreFuture<'a, AffectState> {
        Box::pin(async move {
            state
                .validate()
                .map_err(|_| AffectStoreError::InvalidState)?;
            query(
                "INSERT INTO yunxi_affect_states
                    (person_id, valence, arousal, social_energy, curiosity)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (person_id) DO UPDATE SET
                    valence = EXCLUDED.valence,
                    arousal = EXCLUDED.arousal,
                    social_energy = EXCLUDED.social_energy,
                    curiosity = EXCLUDED.curiosity,
                    updated_at = NOW()",
            )
            .bind(person_id.into_uuid())
            .bind(f64::from(state.valence))
            .bind(f64::from(state.arousal))
            .bind(f64::from(state.social_energy))
            .bind(f64::from(state.curiosity))
            .execute(&self.pool)
            .await
            .map_err(AffectStoreError::storage)?;
            Ok(state)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::PostgresAffectStore;
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use sqlx_core::query::query;
    use sqlx_postgres::PgPoolOptions;
    use yunxi_core::{AffectState, AffectStore, PersonId};

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_affect_store_round_trips_state() {
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
                let store = PostgresAffectStore::new(pool.clone());
                store
                    .initialize_schema()
                    .await
                    .expect("should initialize affect schema");

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
                        .expect("should read default state"),
                    AffectState::default()
                );

                let expected = AffectState {
                    valence: -0.5,
                    arousal: 0.25,
                    social_energy: 0.75,
                    curiosity: 1.0,
                };
                assert_eq!(
                    store
                        .set(person_id, expected)
                        .await
                        .expect("should set state"),
                    expected
                );
                assert_eq!(
                    store.get(person_id).await.expect("should reload state"),
                    expected
                );

                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean up isolated person");
            });
    }
}
