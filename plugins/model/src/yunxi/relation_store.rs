use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::time::Duration;
use yunxi_core::{
    PersonId, RelationState, RelationStore, RelationStoreError, RelationStoreFuture,
    drift_relation_state,
};

const MINIMUM_DRIFT_ELAPSED: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub(crate) struct PostgresRelationStore {
    pool: PgPool,
}

impl PostgresRelationStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
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
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Bootstrap a canonical row from legacy state without racing with or
    /// replacing a Core-owned evolution that already exists.
    pub(crate) async fn seed_if_absent(
        &self,
        state: RelationState,
    ) -> Result<bool, RelationStoreError> {
        state
            .validate()
            .map_err(|_| RelationStoreError::InvalidState)?;
        let result = query(
            "INSERT INTO yunxi_relations
                (person_id, familiarity, affinity, trust, comfort, tension)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (person_id) DO NOTHING",
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
        Ok(result.rows_affected() == 1)
    }
}

impl RelationStore for PostgresRelationStore {
    fn get<'a>(&'a self, person_id: PersonId) -> RelationStoreFuture<'a, Option<RelationState>> {
        Box::pin(async move {
            let row = query(
                "SELECT familiarity, affinity, trust, comfort, tension, updated_at
                 FROM yunxi_relations WHERE person_id = $1",
            )
            .bind(person_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(RelationStoreError::storage)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let stored_state = RelationState {
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
            stored_state.validate().map_err(|error| {
                RelationStoreError::storage(std::io::Error::other(error.to_string()))
            })?;
            let updated_at = row
                .try_get::<DateTime<Utc>, _>("updated_at")
                .map_err(RelationStoreError::storage)?;
            let state =
                drift_relation_state(stored_state, elapsed_for_drift(updated_at, Utc::now()));
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

fn elapsed_for_drift(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    let Ok(elapsed) = now.signed_duration_since(updated_at).to_std() else {
        return Duration::ZERO;
    };
    if elapsed < MINIMUM_DRIFT_ELAPSED {
        Duration::ZERO
    } else {
        elapsed
    }
}

#[cfg(test)]
mod tests {
    use super::{PostgresRelationStore, elapsed_for_drift};
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use chrono::{Duration as ChronoDuration, Utc};
    use sqlx_core::query::query;
    use sqlx_postgres::PgPoolOptions;
    use yunxi_core::{PersonId, RelationState, RelationStore};

    #[test]
    fn relation_drift_elapsed_ignores_clock_skew_and_subminute_jitter() {
        let now = Utc::now();
        assert_eq!(
            elapsed_for_drift(now + ChronoDuration::hours(1), now),
            std::time::Duration::ZERO
        );
        assert_eq!(
            elapsed_for_drift(now - ChronoDuration::seconds(59), now),
            std::time::Duration::ZERO
        );
        assert_eq!(
            elapsed_for_drift(now - ChronoDuration::seconds(61), now),
            std::time::Duration::from_secs(61)
        );
    }

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

                let bootstrap = RelationState {
                    person_id,
                    familiarity: 0.1,
                    affinity: 0.2,
                    trust: 0.3,
                    comfort: 0.4,
                    tension: 0.0,
                };
                assert!(
                    store
                        .seed_if_absent(bootstrap)
                        .await
                        .expect("first legacy seed should insert")
                );
                assert_eq!(
                    store
                        .get(person_id)
                        .await
                        .expect("bootstrap should be readable"),
                    Some(bootstrap)
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

                let legacy_seed = RelationState {
                    person_id,
                    familiarity: 1.0,
                    affinity: -1.0,
                    trust: 1.0,
                    comfort: -1.0,
                    tension: 1.0,
                };
                assert!(
                    !store
                        .seed_if_absent(legacy_seed)
                        .await
                        .expect("legacy seed should be accepted")
                );
                assert_eq!(
                    store
                        .get(person_id)
                        .await
                        .expect("legacy seed must preserve Core relation"),
                    Some(expected)
                );

                query(
                    "UPDATE yunxi_relations
                     SET updated_at = NOW() - INTERVAL '365 days'
                     WHERE person_id = $1",
                )
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("should age stored relation state");
                let drifted = store
                    .get(person_id)
                    .await
                    .expect("should apply elapsed relation drift")
                    .expect("relation should still exist");
                assert_eq!(drifted.person_id, person_id);
                assert!(drifted.familiarity.abs() < expected.familiarity.abs());
                assert!(drifted.affinity.abs() < expected.affinity.abs());
                assert!(drifted.trust.abs() < expected.trust.abs());
                assert!(drifted.comfort.abs() < expected.comfort.abs());
                assert!(drifted.tension.abs() < expected.tension.abs());
                drifted
                    .validate()
                    .expect("drifted relation should be valid");

                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean up isolated person");
            });
    }
}
