use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::time::Duration;
use yunxi_core::{
    AffectState, AffectStore, AffectStoreError, AffectStoreFuture, PersonId, drift_affect_state,
};

const MINIMUM_DRIFT_ELAPSED: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub(crate) struct PostgresAffectStore {
    pool: PgPool,
}

impl PostgresAffectStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
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
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Bootstrap a canonical row from legacy state without racing with or
    /// replacing a Core-owned evolution that already exists.
    pub(crate) async fn seed_if_absent(
        &self,
        person_id: PersonId,
        state: AffectState,
    ) -> Result<bool, AffectStoreError> {
        state
            .validate()
            .map_err(|_| AffectStoreError::InvalidState)?;
        let result = query(
            "INSERT INTO yunxi_affect_states
                (person_id, valence, arousal, social_energy, curiosity)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (person_id) DO NOTHING",
        )
        .bind(person_id.into_uuid())
        .bind(f64::from(state.valence))
        .bind(f64::from(state.arousal))
        .bind(f64::from(state.social_energy))
        .bind(f64::from(state.curiosity))
        .execute(&self.pool)
        .await
        .map_err(AffectStoreError::storage)?;
        Ok(result.rows_affected() == 1)
    }
}

impl AffectStore for PostgresAffectStore {
    fn get<'a>(&'a self, person_id: PersonId) -> AffectStoreFuture<'a, AffectState> {
        Box::pin(async move {
            let row = query(
                "SELECT valence, arousal, social_energy, curiosity, updated_at
                 FROM yunxi_affect_states WHERE person_id = $1",
            )
            .bind(person_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(AffectStoreError::storage)?;
            let Some(row) = row else {
                return Ok(AffectState::default());
            };
            let stored_state = AffectState {
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
            stored_state
                .validate()
                .map_err(|_| AffectStoreError::InvalidState)?;
            let updated_at = row
                .try_get::<DateTime<Utc>, _>("updated_at")
                .map_err(AffectStoreError::storage)?;
            let state = drift_affect_state(stored_state, elapsed_for_drift(updated_at, Utc::now()));
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
    use super::{PostgresAffectStore, elapsed_for_drift};
    use crate::yunxi::identity_store::PostgresIdentityStore;
    use chrono::{Duration as ChronoDuration, Utc};
    use sqlx_core::query::query;
    use sqlx_postgres::PgPoolOptions;
    use yunxi_core::{AffectState, AffectStore, PersonId};

    #[test]
    fn affect_drift_elapsed_ignores_clock_skew_and_subminute_jitter() {
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

                let bootstrap = AffectState {
                    valence: 0.1,
                    arousal: -0.1,
                    social_energy: 0.9,
                    curiosity: 0.6,
                };
                assert!(
                    store
                        .seed_if_absent(person_id, bootstrap)
                        .await
                        .expect("first legacy seed should insert")
                );
                assert_eq!(
                    store
                        .get(person_id)
                        .await
                        .expect("bootstrap should be readable"),
                    bootstrap
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

                let legacy_seed = AffectState {
                    valence: 1.0,
                    arousal: -1.0,
                    social_energy: 0.0,
                    curiosity: 0.0,
                };
                assert!(
                    !store
                        .seed_if_absent(person_id, legacy_seed)
                        .await
                        .expect("legacy seed should be accepted")
                );
                assert_eq!(
                    store
                        .get(person_id)
                        .await
                        .expect("legacy seed must preserve Core state"),
                    expected
                );

                query(
                    "UPDATE yunxi_affect_states
                     SET updated_at = NOW() - INTERVAL '24 hours'
                     WHERE person_id = $1",
                )
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("should age stored affect state");
                let drifted = store
                    .get(person_id)
                    .await
                    .expect("should apply elapsed affect drift");
                assert!(drifted.valence.abs() < expected.valence.abs());
                assert!(drifted.arousal.abs() < expected.arousal.abs());
                assert!(drifted.social_energy > expected.social_energy);
                assert!((drifted.curiosity - 0.5).abs() < (expected.curiosity - 0.5).abs());
                drifted.validate().expect("drifted affect should be valid");

                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean up isolated person");
            });
    }
}
