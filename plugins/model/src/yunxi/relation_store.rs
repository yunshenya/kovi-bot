use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::time::Duration;
use yunxi_core::{
    PersonId, RelationState, RelationStore, RelationStoreError, RelationStoreFuture,
    adjust_relation_tension, drift_relation_state,
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

    /// 把一条明确的相处证据记到关系张力上（正=更紧张，负=回暖）。
    ///
    /// 为什么要这个方法而不是让调用方 `get` + `set`：张力是**累积**量，而
    /// `set` 会整行覆盖。调用方只该表达"我看到了多强的一条证据"，加多少、
    /// 上限在哪、怎么衰减由 Core 的 [`adjust_relation_tension`] 决定，两处
    /// 判据（字面命中与模型分类）因此落在同一个刻度上。
    ///
    /// 读与写不是原子的：并发下可能丢一次加法。这里刻意不做事务——证据是
    /// 连续事件流，丢一次加法只让张力升得慢一点，而门控本身有阈值与半衰期
    /// 兜着；为此上锁的代价大于收益。
    pub(crate) async fn nudge_tension(
        &self,
        person_id: PersonId,
        signed_strength: f32,
    ) -> Result<Option<RelationState>, RelationStoreError> {
        let Some(current) = self.get(person_id).await? else {
            return Ok(None);
        };
        let adjusted = adjust_relation_tension(current, signed_strength);
        // 只写 tension 一列：与 `set` 的分工见那里的注释——这条通道是 tension 的
        // 唯一写者，改动落在 delta 上，因此不会被别处的整行回写覆盖。
        query("UPDATE yunxi_relations SET tension = $2, updated_at = NOW() WHERE person_id = $1")
            .bind(person_id.into_uuid())
            .bind(f64::from(adjusted.tension))
            .execute(&self.pool)
            .await
            .map_err(RelationStoreError::storage)?;
        Ok(Some(adjusted))
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

    /// 写回"这个人跟她处得怎么样"的**非张力**维度（familiarity/affinity/trust/comfort）。
    ///
    /// **`tension` 不在这条路径里写。** 它是证据累积量，只有一个写者：
    /// [`PostgresRelationStore::nudge_tension`]。原因是一次线上事故：Core 每回合收尾
    /// 会用**回合开始时的快照**整行回写关系，而相处证据是在回合进行中到账的——
    /// 证据写 0.0256，回合收尾写回 0，静默门控因此永远越不了线（2026-09-14）。
    ///
    /// 返回值里的 tension 是**库里当前的值**（`RETURNING`），不是入参里那个快照值，
    /// 免得调用方拿着过期值再写一遍。
    fn set<'a>(&'a self, state: RelationState) -> RelationStoreFuture<'a, RelationState> {
        Box::pin(async move {
            state
                .validate()
                .map_err(|_| RelationStoreError::InvalidState)?;
            let row = query(
                "INSERT INTO yunxi_relations
                    (person_id, familiarity, affinity, trust, comfort)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (person_id) DO UPDATE SET
                    familiarity = EXCLUDED.familiarity,
                    affinity = EXCLUDED.affinity,
                    trust = EXCLUDED.trust,
                    comfort = EXCLUDED.comfort,
                    updated_at = NOW()
                 RETURNING tension",
            )
            .bind(state.person_id.into_uuid())
            .bind(f64::from(state.familiarity))
            .bind(f64::from(state.affinity))
            .bind(f64::from(state.trust))
            .bind(f64::from(state.comfort))
            .fetch_one(&self.pool)
            .await
            .map_err(RelationStoreError::storage)?;
            let tension = row
                .try_get::<f64, _>("tension")
                .map_err(RelationStoreError::storage)?;
            Ok(RelationState {
                tension: tension as f32,
                ..state
            })
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
                // `set` 只写非张力维度：入参里的 tension 是回合开始时的快照，
                // 写进去就会覆盖掉回合期间到账的相处证据（2026-09-14 事故）。
                let persisted = store.set(expected).await.expect("should persist relation");
                assert_eq!(persisted.familiarity, expected.familiarity);
                assert_eq!(persisted.affinity, expected.affinity);
                assert_eq!(persisted.trust, expected.trust);
                assert_eq!(persisted.comfort, expected.comfort);
                assert_eq!(
                    persisted.tension, 0.0,
                    "整行回写不得改动 tension（快照里写 -0.5 也不行）"
                );
                assert_eq!(
                    store.get(person_id).await.expect("should reload relation"),
                    Some(RelationState {
                        tension: 0.0,
                        ..expected
                    })
                );

                // 相反方向：相处证据的 delta 通道必须真的写进去，并且是累加。
                let nudged = store
                    .nudge_tension(person_id, 0.15)
                    .await
                    .expect("should nudge tension")
                    .expect("relation exists");
                // `adjust_relation_tension` 的混合率是 0.2：tension=0 时一条
                // 0.15 强度的证据给出 0.2×0.15 = 0.03。
                assert!(
                    (nudged.tension - 0.2 * 0.15).abs() < 1e-6,
                    "首条证据应当按 (1-tension)×0.2×强度 累积：{}",
                    nudged.tension
                );
                assert_eq!(
                    store
                        .get(person_id)
                        .await
                        .expect("should reload relation")
                        .expect("relation exists")
                        .tension,
                    nudged.tension,
                    "delta 通道的写入必须落库"
                );
                // 紧跟一次整行回写：证据不得被抹掉。
                let after_writeback = store
                    .set(RelationState {
                        tension: 0.0,
                        ..expected
                    })
                    .await
                    .expect("should persist relation");
                assert_eq!(
                    after_writeback.tension, nudged.tension,
                    "回合收尾的整行回写不得抹掉刚记账的证据"
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
                // legacy 建档只能补空行；张力那一路是证据通道的值，不该被它改动。
                let current = RelationState {
                    tension: after_writeback.tension,
                    ..expected
                };
                assert_eq!(
                    store
                        .get(person_id)
                        .await
                        .expect("legacy seed must preserve Core relation"),
                    Some(current)
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
                // 张力那一行现在是证据通道写的值，漂移只让它朝 0 回落。
                assert!(drifted.tension.abs() < after_writeback.tension.abs());
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
