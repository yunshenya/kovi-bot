use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::time::Duration;
use yunxi_core::{
    PersonId, RelationNudge, RelationState, RelationStore, RelationStoreError, RelationStoreFuture,
    drift_relation_state, relation_half_lives,
};

const MINIMUM_DRIFT_ELAPSED: Duration = Duration::from_secs(60);

/// 一个维度"按流逝时间朝 0 衰减"的 SQL 表达式，与 `drift_relation_state` 同一口径：
/// 同一个 `2^(-t/half_life)`，同样忽略 60 秒内的抖动与时钟回拨。
///
/// 为什么要写进 SQL 而不是在 Rust 里算好再写：关系行只有一根 `updated_at` 时钟，
/// 而每一列各有自己的写者。一个只写自己那几列的写者如果把 `updated_at` 推到
/// `NOW()` 却不落别列的漂移，那些列攒下的衰减就被永久吞掉（`comfort` 的半衰期只有
/// 30 天，这是实打实的）。把漂移放进同一条 UPDATE 里，任何写者都不会丢别人的衰减。
///
/// 只插值 `'static` 的列名、表限定符与占位符名，没有任何请求数据进入 SQL 文本。
fn decay_sql(
    column: &'static str,
    half_life_param: &'static str,
    qualifier: &'static str,
) -> String {
    format!(
        "CASE WHEN NOW() - {qualifier}.updated_at < INTERVAL '60 seconds' THEN {qualifier}.{column} \
         ELSE LEAST(1.0, GREATEST(-1.0, {qualifier}.{column} * power(0.5::float8, \
              (EXTRACT(EPOCH FROM (NOW() - {qualifier}.updated_at))::float8) / {half_life_param}))) END"
    )
}

/// 把一个"朝 `target` 混合 `rate`"的拉拽写进 SQL；未提供时（绑定为 NULL）保持衰减
/// 后的值不变。
///
/// 用 `COALESCE(目标, 当前值)` 让"这条通道这次不碰这一列"退化成恒等操作，于是同一个
/// 语句模板能服务任意子集的更新，不必为每种组合写一条 SQL。
fn pull_sql(decayed: &str, target_param: &'static str, rate_param: &'static str) -> String {
    format!(
        "LEAST(1.0, GREATEST(-1.0, ({decayed}) + \
         (COALESCE({target_param}::float8, ({decayed})) - ({decayed})) \
         * COALESCE({rate_param}::float8, 0.0)))"
    )
}

fn row_to_relation(
    row: &sqlx_postgres::PgRow,
    person_id: PersonId,
) -> Result<RelationState, RelationStoreError> {
    Ok(RelationState {
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
    })
}

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

    /// 把一条相处证据记进关系：张力按同一刻度累积，好感与信任按同一条证据的
    /// 方向移动。
    ///
    /// 为什么是"一条语句 + 拉拽"而不是"读出来改好再写回去"：关系行只有一根
    /// `updated_at` 时钟，而每一列各有自己的写者。整行回写会把别列在别处攒下的
    /// 变化（以及它们的漂移）一起抹掉——2026-09-14 的张力事故就是这么来的。
    /// 这里把漂移与本列的拉拽放进**同一条 UPDATE**，于是：
    ///
    /// - 本列的变化相对**库里的当前值**计算，并发写者不会互相覆盖；
    /// - 别列的漂移在同一条语句里落地，谁都不会吞掉别人的衰减。
    ///
    /// 行不存在时返回 `Ok(None)`（调用方只记日志）——证据不该凭空造一条关系行，
    /// 建档是 `seed_if_absent` 与回合回写的事。
    pub(crate) async fn nudge(
        &self,
        person_id: PersonId,
        nudge: RelationNudge,
    ) -> Result<Option<RelationState>, RelationStoreError> {
        if !nudge.validate() {
            return Err(RelationStoreError::InvalidState);
        }
        if nudge.is_empty() {
            return self.get(person_id).await;
        }
        let familiarity = decay_sql("familiarity", "$8", "r");
        let comfort = decay_sql("comfort", "$9", "r");
        let affinity = pull_sql(&decay_sql("affinity", "$10", "r"), "$2", "$3");
        let trust = pull_sql(&decay_sql("trust", "$11", "r"), "$4", "$5");
        let tension = pull_sql(&decay_sql("tension", "$12", "r"), "$6", "$7");
        let sql = format!(
            "WITH decayed AS (
                 SELECT r.person_id, {familiarity} AS familiarity, {comfort} AS comfort,
                        {affinity} AS affinity, {trust} AS trust, {tension} AS tension
                 FROM yunxi_relations r WHERE r.person_id = $1
             )
             UPDATE yunxi_relations AS target SET
                 familiarity = decayed.familiarity,
                 comfort = decayed.comfort,
                 affinity = decayed.affinity,
                 trust = decayed.trust,
                 tension = decayed.tension,
                 updated_at = NOW()
             FROM decayed
             WHERE target.person_id = decayed.person_id
             RETURNING target.familiarity, target.affinity, target.trust,
                       target.comfort, target.tension",
            familiarity = familiarity,
            comfort = comfort,
            affinity = affinity,
            trust = trust,
            tension = tension,
        );
        let (affinity_target, affinity_rate) = pull_params(nudge.affinity);
        let (trust_target, trust_rate) = pull_params(nudge.trust);
        let (tension_target, tension_rate) = pull_params(nudge.tension);
        let row = query(&sql)
            .bind(person_id.into_uuid())
            .bind(affinity_target)
            .bind(affinity_rate)
            .bind(trust_target)
            .bind(trust_rate)
            .bind(tension_target)
            .bind(tension_rate)
            .bind(relation_half_lives::FAMILIARITY_SECONDS)
            .bind(relation_half_lives::COMFORT_SECONDS)
            .bind(relation_half_lives::AFFINITY_SECONDS)
            .bind(relation_half_lives::TRUST_SECONDS)
            .bind(relation_half_lives::TENSION_SECONDS)
            .fetch_optional(&self.pool)
            .await
            .map_err(RelationStoreError::storage)?;
        row.as_ref()
            .map(|row| row_to_relation(row, person_id))
            .transpose()
    }
}

fn pull_params(pull: Option<yunxi_core::RelationPull>) -> (Option<f64>, Option<f64>) {
    match pull {
        Some(pull) => (Some(f64::from(pull.target)), Some(f64::from(pull.rate))),
        None => (None, None),
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

    /// 写回"这个人跟她处得怎么样"里**属于这一轮结构演化**的两维：
    /// `familiarity`（这次互动本身让她更熟悉对方）与 `comfort`（这次互动的相处口径）。
    ///
    /// **另外三维不在这条路径里写。** `tension` / `affinity` / `trust` 是相处证据的
    /// 量，只有 delta 通道（[`PostgresRelationStore::nudge`]）能改。原因是一次线上
    /// 事故：Core 每回合收尾会用**回合开始时的快照**整行回写关系，而相处证据是在回合
    /// 进行中到账的——证据写 0.0256，回合收尾写回 0，静默门控因此永远越不了线
    /// （2026-09-14）。同一个坑对好感与信任一样成立，所以三者一起交给 delta 通道。
    ///
    /// 这条 UPDATE 同时把三维的漂移落到库里：它们各自还有自己的写者，而
    /// `updated_at` 是共享的，不在这里落漂移就等于把它们的衰减抹掉。
    ///
    /// 返回值是**库里当前的真实值**（`RETURNING`），不是入参里那份快照，免得调用方
    /// 拿着过期值再写一遍。
    fn set<'a>(&'a self, state: RelationState) -> RelationStoreFuture<'a, RelationState> {
        Box::pin(async move {
            state
                .validate()
                .map_err(|_| RelationStoreError::InvalidState)?;
            let affinity = decay_sql("affinity", "$6", "r");
            let trust = decay_sql("trust", "$7", "r");
            let tension = decay_sql("tension", "$8", "r");
            let sql = format!(
                "INSERT INTO yunxi_relations AS r
                    (person_id, familiarity, affinity, trust, comfort, tension)
                 VALUES ($1, $2, $3, $4, $5, $9)
                 ON CONFLICT (person_id) DO UPDATE SET
                    familiarity = EXCLUDED.familiarity,
                    comfort = EXCLUDED.comfort,
                    affinity = {affinity},
                    trust = {trust},
                    tension = {tension},
                    updated_at = NOW()
                 RETURNING familiarity, affinity, trust, comfort, tension",
                affinity = affinity,
                trust = trust,
                tension = tension,
            );
            let row = query(&sql)
                .bind(state.person_id.into_uuid())
                .bind(f64::from(state.familiarity))
                .bind(f64::from(state.affinity))
                .bind(f64::from(state.trust))
                .bind(f64::from(state.comfort))
                .bind(relation_half_lives::AFFINITY_SECONDS)
                .bind(relation_half_lives::TRUST_SECONDS)
                .bind(relation_half_lives::TENSION_SECONDS)
                .bind(f64::from(state.tension))
                .fetch_one(&self.pool)
                .await
                .map_err(RelationStoreError::storage)?;
            row_to_relation(&row, state.person_id)
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
                // `set` 只写它自己那两维（familiarity / comfort）：入参里的
                // affinity / trust / tension 是回合开始时的快照，写进去就会覆盖掉
                // 回合期间到账的相处证据（2026-09-14 事故，好感与信任同形）。
                let persisted = store.set(expected).await.expect("should persist relation");
                assert_eq!(persisted.familiarity, expected.familiarity);
                assert_eq!(persisted.comfort, expected.comfort);
                assert_eq!(
                    persisted.affinity, bootstrap.affinity,
                    "整行回写不得改动好感（快照里写 0.5 也不行）"
                );
                assert_eq!(
                    persisted.trust, bootstrap.trust,
                    "整行回写不得改动信任（快照里写 -0.25 也不行）"
                );
                assert_eq!(
                    persisted.tension, bootstrap.tension,
                    "整行回写不得改动张力（快照里写 -0.5 也不行）"
                );
                let after_writeback = store
                    .get(person_id)
                    .await
                    .expect("should reload relation")
                    .expect("relation exists");
                assert_eq!(after_writeback.familiarity, expected.familiarity);
                assert_eq!(after_writeback.comfort, expected.comfort);
                assert_eq!(after_writeback.affinity, bootstrap.affinity);
                assert_eq!(after_writeback.trust, bootstrap.trust);
                assert_eq!(after_writeback.tension, bootstrap.tension);

                // 相处证据的 delta 通道：一条 0.15 强度的不友好同时推高张力、压低
                // 好感与信任，且三者的位移都相对**库里的当前值**计算。
                let hostile = yunxi_core::relation_evidence_nudge(0.15);
                let nudged = store
                    .nudge(person_id, hostile)
                    .await
                    .expect("should nudge relation")
                    .expect("relation exists");
                // 张力混合率 0.2：0 时一条 0.15 强度的证据给出 0.2×0.15 = 0.03。
                assert!(
                    (nudged.tension - 0.2 * 0.15).abs() < 1e-6,
                    "首条证据应当按 (1-tension)×0.2×强度 累积：{}",
                    nudged.tension
                );
                assert!(
                    nudged.affinity < bootstrap.affinity,
                    "不友好必须压低好感：{} -> {}",
                    bootstrap.affinity,
                    nudged.affinity
                );
                assert!(nudged.trust < bootstrap.trust);
                assert_eq!(
                    store
                        .get(person_id)
                        .await
                        .expect("should reload relation")
                        .expect("relation exists"),
                    nudged,
                    "delta 通道的写入必须落库"
                );

                // 反方向：善意同时降温、抬高好感与信任。
                let warm = yunxi_core::relation_evidence_nudge(-0.5);
                let warmed = store
                    .nudge(person_id, warm)
                    .await
                    .expect("should nudge relation")
                    .expect("relation exists");
                assert!(warmed.tension < nudged.tension, "善意必须降温");
                assert!(
                    warmed.affinity > nudged.affinity,
                    "善意必须抬高好感：{} -> {}",
                    nudged.affinity,
                    warmed.affinity
                );
                assert!(warmed.trust > nudged.trust);

                // 紧跟一次整行回写：证据（张力/好感/信任）一个都不许被抹掉。
                let after_evidence_writeback = store
                    .set(RelationState {
                        tension: 0.0,
                        affinity: 0.0,
                        trust: 0.0,
                        ..expected
                    })
                    .await
                    .expect("should persist relation");
                assert_eq!(after_evidence_writeback.tension, warmed.tension);
                assert_eq!(after_evidence_writeback.affinity, warmed.affinity);
                assert_eq!(after_evidence_writeback.trust, warmed.trust);

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
                // legacy 建档只能补空行，不能改动证据通道写下的任何一列。
                let current = RelationState {
                    tension: after_evidence_writeback.tension,
                    affinity: after_evidence_writeback.affinity,
                    trust: after_evidence_writeback.trust,
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
                assert!(drifted.affinity.abs() < current.affinity.abs());
                assert!(drifted.trust.abs() < current.trust.abs());
                assert!(drifted.comfort.abs() < expected.comfort.abs());
                assert!(drifted.tension.abs() < current.tension.abs());
                drifted
                    .validate()
                    .expect("drifted relation should be valid");

                // **漂移不会被别的写者吞掉。** 把行的时钟拨老一年，然后让证据通道
                // 记一条：它必须把别列的漂移一起落库，而不是只写自己那一列再顺手把
                // `updated_at` 推到 NOW() —— `comfort` 的半衰期只有 30 天，被吞掉的
                // 衰减是实打实的。
                query(
                    "UPDATE yunxi_relations
                     SET updated_at = NOW() - INTERVAL '365 days'
                     WHERE person_id = $1",
                )
                .bind(person_id.into_uuid())
                .execute(&pool)
                .await
                .expect("should age stored relation state again");
                let aged = store
                    .get(person_id)
                    .await
                    .expect("should apply elapsed relation drift")
                    .expect("relation should still exist");
                let after_nudge = store
                    .nudge(person_id, yunxi_core::relation_evidence_nudge(0.1))
                    .await
                    .expect("should nudge relation")
                    .expect("relation exists");
                // 一年之后 familiarity（半年半衰期）、affinity（一年）、comfort（30 天）
                // 都应当已经明显回落，而不是原封不动地停在旧值上。
                assert!(
                    after_nudge.familiarity < expected.familiarity,
                    "证据通道必须把别列的漂移一起落库：familiarity {} 未衰减",
                    after_nudge.familiarity
                );
                assert!(after_nudge.comfort < expected.comfort);
                assert!(after_nudge.comfort < after_evidence_writeback.comfort);
                assert!(
                    after_nudge.affinity < aged.affinity.max(current.affinity),
                    "好感也应当在同一条语句里落地漂移"
                );

                query("DELETE FROM yunxi_persons WHERE id = $1")
                    .bind(person_id.into_uuid())
                    .execute(&pool)
                    .await
                    .expect("should clean up isolated person");
            });
    }
}
