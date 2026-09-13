//! 群级降温压力的 Postgres 存储。
//!
//! 为什么单独一张表，而不是塞进 `yunxi_relations`：关系表的每一行是"某个人"
//! （主键是 `person_id`，随人删除），而降温是"某个群"的聚合状态，主键是 QQ
//! 群号、随群数据删除整行清掉，成员退群不影响它。键与生命周期都不同，硬塞
//! 进关系表会让两边都别扭。
//!
//! 读写风格对齐 [`super::relation_store`]：`initialize_schema` + `schema::lock`
//! 拿建表锁（`CREATE TABLE IF NOT EXISTS` 在多进程/多线程下不是无竞争的），
//! 读取时按流逝时间衰减但不写回，由下一次记账把衰减后的值带进库里。

use crate::group_cooling::{
    GroupCoolingSignal, apply_group_cooling_pressure, drift_group_cooling_pressure,
};
use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::time::Duration;

/// 一个群的群级降温状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GroupCoolingState {
    /// 降温压力 ∈ [0, 1]；0 表示"照常"。
    pub(crate) pressure: f32,
    /// 上一条"驱赶"证据来自谁——同一个人重复驱赶要打折（见 `group_cooling`）。
    pub(crate) last_push_out_sender: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresGroupCoolingStore {
    pool: PgPool,
}

impl PostgresGroupCoolingStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_group_cooling (
                group_id BIGINT PRIMARY KEY,
                pressure DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (pressure BETWEEN 0 AND 1),
                last_push_out_sender BIGINT,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// 读一个群的降温压力，按流逝时间衰减。
    ///
    /// 与 `PostgresRelationStore::get` 一样只算不写：衰减是时间的函数，
    /// 每次读都落一次库没有意义；下一次记账会带上衰减后的值。
    pub(crate) async fn load(&self, group_id: i64) -> anyhow::Result<Option<GroupCoolingState>> {
        let row = query(
            "SELECT pressure, last_push_out_sender, updated_at
             FROM yunxi_group_cooling WHERE group_id = $1",
        )
        .bind(group_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored_pressure = row.try_get::<f64, _>("pressure")? as f32;
        let updated_at = row.try_get::<DateTime<Utc>, _>("updated_at")?;
        Ok(Some(GroupCoolingState {
            pressure: drift_group_cooling_pressure(
                stored_pressure,
                elapsed_since(updated_at, Utc::now()),
            ),
            last_push_out_sender: row.try_get::<Option<i64>, _>("last_push_out_sender")?,
        }))
    }

    /// 记一条群级证据，返回记账后的状态。
    ///
    /// `None` 表示这条证据什么也没改变（回暖证据遇到本来就是 0 的压力）——
    /// 群里的日常对话因此不会每条都产生一次 upsert，调用方也不必打日志。
    ///
    /// 与 `nudge_tension` 一样不做事务：证据是连续事件流，并发下丢一次加法只
    /// 让压力升得慢一点，而这条路本身有阈值与半衰期兜着，为它上锁不划算。
    pub(crate) async fn nudge(
        &self,
        group_id: i64,
        signal: GroupCoolingSignal,
        sender: Option<i64>,
    ) -> anyhow::Result<Option<GroupCoolingState>> {
        let current = self.load(group_id).await?;
        let pressure = current.map(|state| state.pressure).unwrap_or(0.0);
        if !signal.is_push_out() && pressure == 0.0 {
            return Ok(None);
        }
        // 只有"当面对她"的驱赶要记住是谁给的，下一条同人的证据才打得了折；
        // 窗口内的插嘴指责不打折（它挂在"她刚插过话"这个时刻上），回暖与
        // "无人应答"也不改变这个标记（后者根本没有发送者）。
        let last_push_out_sender = current.and_then(|state| state.last_push_out_sender);
        let next_push_out_sender = if signal == GroupCoolingSignal::DirectedPushOut {
            sender.or(last_push_out_sender)
        } else {
            last_push_out_sender
        };
        let pressure = apply_group_cooling_pressure(pressure, signal, sender, last_push_out_sender);
        query(
            "INSERT INTO yunxi_group_cooling (group_id, pressure, last_push_out_sender)
             VALUES ($1, $2, $3)
             ON CONFLICT (group_id) DO UPDATE SET
                pressure = EXCLUDED.pressure,
                last_push_out_sender = EXCLUDED.last_push_out_sender,
                updated_at = NOW()",
        )
        .bind(group_id)
        .bind(f64::from(pressure))
        .bind(next_push_out_sender)
        .execute(&self.pool)
        .await?;
        Ok(Some(GroupCoolingState {
            pressure,
            last_push_out_sender: next_push_out_sender,
        }))
    }

    /// 群数据删除：这个群的降温状态必须一起消失，否则删完数据她还带着旧账。
    pub(crate) async fn delete_group(&self, group_id: i64) -> anyhow::Result<u64> {
        let result = query("DELETE FROM yunxi_group_cooling WHERE group_id = $1")
            .bind(group_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

/// 从库里那行的时间戳算流逝。时钟回拨（负时长）按 0 处理，不让衰减反向。
fn elapsed_since(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    now.signed_duration_since(updated_at)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::{PostgresGroupCoolingStore, elapsed_since};
    use crate::group_cooling::GroupCoolingSignal;
    use chrono::{Duration as ChronoDuration, Utc};

    #[test]
    fn elapsed_since_ignores_clock_skew() {
        let now = Utc::now();
        assert_eq!(
            elapsed_since(now + ChronoDuration::hours(1), now),
            std::time::Duration::ZERO
        );
        assert_eq!(
            elapsed_since(now - ChronoDuration::seconds(90), now),
            std::time::Duration::from_secs(90)
        );
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_group_cooling_store_accumulates_decays_and_deletes() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
                let pool = sqlx_postgres::PgPoolOptions::new()
                    .max_connections(2)
                    .connect(&database_url)
                    .await
                    .expect("should connect to PostgreSQL");
                let store = PostgresGroupCoolingStore::new(pool.clone());
                store
                    .initialize_schema()
                    .await
                    .expect("should initialize group cooling schema");

                // 用一个不会撞上真实群的负群号，测试结束自己清理。
                let group_id = -9_000_000_000_i64 - (std::process::id() as i64 % 1_000);
                assert_eq!(
                    store.load(group_id).await.expect("should read missing row"),
                    None
                );

                let state = store
                    .nudge(group_id, GroupCoolingSignal::DirectedPushOut, Some(42))
                    .await
                    .expect("should record push-out evidence")
                    .expect("push-out evidence always changes state");
                assert!(state.pressure > 0.0);
                assert_eq!(state.last_push_out_sender, Some(42));

                // 同一个人再来一次：打折不改变"仍然在累积"的方向。
                let repeated = store
                    .nudge(group_id, GroupCoolingSignal::DirectedPushOut, Some(42))
                    .await
                    .expect("should record repeated evidence")
                    .expect("push-out evidence always changes state");
                assert!(repeated.pressure > state.pressure);

                // 自然恢复是"读取时按流逝时间衰减"：把时间戳推回一天，
                // 不写任何新证据，压力就应该掉到一半左右。
                sqlx_core::query::query(
                    "UPDATE yunxi_group_cooling SET updated_at = NOW() - INTERVAL '1 day'
                     WHERE group_id = $1",
                )
                .bind(group_id)
                .execute(&pool)
                .await
                .expect("should age the stored pressure");
                let decayed = store
                    .load(group_id)
                    .await
                    .expect("should read aged pressure")
                    .expect("row should still exist");
                assert!(
                    (decayed.pressure - repeated.pressure / 2.0).abs() < 0.01,
                    "一天应该衰减一半: {} -> {}",
                    repeated.pressure,
                    decayed.pressure
                );

                // 回暖证据一路把压力拉回 0；到 0 之后连库都不写（Some → None）。
                let mut recovered = decayed;
                let mut no_op_reached = false;
                for _ in 0..8 {
                    match store
                        .nudge(group_id, GroupCoolingSignal::Warm, None)
                        .await
                        .expect("should record warm evidence")
                    {
                        Some(next) => {
                            assert!(
                                next.pressure <= recovered.pressure,
                                "回暖不能让压力上升: {} -> {}",
                                recovered.pressure,
                                next.pressure
                            );
                            recovered = next;
                        }
                        None => {
                            no_op_reached = true;
                            break;
                        }
                    }
                }
                assert_eq!(recovered.pressure, 0.0, "回暖必须最终把压力拉回 0");
                assert!(no_op_reached, "压力为 0 之后回暖证据不再写库");

                assert_eq!(
                    store
                        .delete_group(group_id)
                        .await
                        .expect("should delete group"),
                    1
                );
                assert_eq!(
                    store.load(group_id).await.expect("should read deleted row"),
                    None
                );
            });
    }
}
