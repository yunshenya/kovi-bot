//! Structured "gag ledger" store: promises, running gags and grudges that the
//! bot owes or holds, per scope (global / person / conversation), bounded and
//! Postgres-backed. The host records entries (owner commands / extraction) and
//! injects open entries into reply context so she "remembers her debts".

use crate::config::GagLedgerConfig;
use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, Postgres};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Global/Conversation scopes are wired by future host paths.
pub(crate) enum GagScope {
    Global,
    Person(String),
    Conversation(String),
}

impl GagScope {
    fn kind_id(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Global => ("global", None),
            Self::Person(id) => ("person", Some(id)),
            Self::Conversation(id) => ("conversation", Some(id)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GagKind {
    Promise,
    Gag,
    Grudge,
}

impl GagKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Promise => "promise",
            Self::Gag => "gag",
            Self::Grudge => "grudge",
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Extra fields are surfaced by future list/UI paths.
pub(crate) struct GagEntry {
    pub id: Uuid,
    pub kind: GagKind,
    pub text: String,
    pub state: String,
    pub occurrence: i64,
    /// 与列类型一致（`importance INTEGER`）：按 i64 读会被 sqlx 判为类型不匹配。
    pub importance: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresGagStore {
    pool: PgPool,
    config: GagLedgerConfig,
}

impl PostgresGagStore {
    pub(crate) fn new(pool: PgPool, config: GagLedgerConfig) -> Self {
        Self { pool, config }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        // 与另外十几个 store 一样先取建表锁：`CREATE TABLE IF NOT EXISTS` 在
        // PostgreSQL 里并非无竞态，两个进程/两个测试线程同时首次初始化会撞
        // `pg_type_typname_nsp_index` 唯一冲突，而这是启动路径——失败会让整个
        // 插件初始化返回 Err。
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        query(
            "CREATE TABLE IF NOT EXISTS yunxi_gag_entries (
                id UUID PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_id TEXT,
                kind TEXT NOT NULL,
                text TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'open',
                occurrence BIGINT NOT NULL DEFAULT 1,
                importance INTEGER NOT NULL DEFAULT 50,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                last_mentioned_at TIMESTAMPTZ
            )",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_gag_entries_scope_idx
             ON yunxi_gag_entries (scope_kind, scope_id, state)",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Record one entry. Bounded: prunes the oldest open entries of the same
    /// scope when the per-scope/global caps would be exceeded.
    pub(crate) async fn add(
        &self,
        scope: GagScope,
        kind: GagKind,
        text: &str,
        importance: u8,
    ) -> anyhow::Result<Uuid> {
        let (scope_kind, scope_id) = scope.kind_id();
        let now = Utc::now();
        let id = Uuid::new_v4();
        // 「数一数 → 删最旧 → 插入」必须是**一个事务**，而且按作用域串行。以前这里是
        // 三条独立语句：同一用户连发两条 `#记下` 时，两边都读到"还没满"，于是双双跳过
        // 删除、条数突破上限。被撑大的 open 集合不会自愈——`list_open` 按 created_at
        // 取前 N，超出的老条目会长期把新条目挤在 LIMIT 之外。
        let mut transaction = self.pool.begin().await?;
        query(
            "SELECT pg_advisory_xact_lock(
                 hashtextextended('yunxi-gag:' || $1 || ':' || COALESCE($2, ''), 0)
             )",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .execute(&mut *transaction)
        .await?;
        // Bound the scope: drop the oldest open entry of this scope if full.
        let scope_count: i64 = query_scalar(
            "SELECT count(*) FROM yunxi_gag_entries
             WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2 AND state = 'open'",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .fetch_one(&mut *transaction)
        .await?;
        if scope_count >= self.config.max_entries_per_scope() as i64 {
            query(
                "DELETE FROM yunxi_gag_entries
                 WHERE id IN (
                     SELECT id FROM yunxi_gag_entries
                     WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2 AND state = 'open'
                     ORDER BY created_at LIMIT 1
                 )",
            )
            .bind(scope_kind)
            .bind(scope_id)
            .execute(&mut *transaction)
            .await?;
        }
        let global_count: i64 =
            query_scalar("SELECT count(*) FROM yunxi_gag_entries WHERE state = 'open'")
                .fetch_one(&mut *transaction)
                .await?;
        if global_count >= self.config.max_global_entries() as i64 {
            query(
                "DELETE FROM yunxi_gag_entries
                 WHERE id IN (
                     SELECT id FROM yunxi_gag_entries
                     WHERE state = 'open' ORDER BY created_at LIMIT 1
                 )",
            )
            .execute(&mut *transaction)
            .await?;
        }
        query(
            "INSERT INTO yunxi_gag_entries
             (id, scope_kind, scope_id, kind, text, state, occurrence, importance, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, 'open', 1, $6, $7, $7)",
        )
        .bind(id)
        .bind(scope_kind)
        .bind(scope_id)
        .bind(kind.as_str())
        .bind(text)
        .bind(i32::from(importance.clamp(0, 100)))
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(id)
    }

    /// Open entries for a scope (plus global entries), oldest first.
    pub(crate) async fn list_open(
        &self,
        scope: GagScope,
        limit: usize,
    ) -> anyhow::Result<Vec<GagEntry>> {
        let (scope_kind, scope_id) = scope.kind_id();
        let rows = query(
            "SELECT id, kind, text, state, occurrence, importance, created_at, updated_at
             FROM yunxi_gag_entries
             WHERE state = 'open'
               AND (scope_kind = 'global'
                    OR (scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2))
             ORDER BY created_at ASC
             LIMIT $3",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        // `try_get` 而不是 `get`：列类型一旦和这里的 Rust 类型对不上，`get` 是直接
        // panic 的，而调用方（`#账本`、私聊上下文注入）根本没法处理。这里就踩过一次
        // ——`importance` 是 INTEGER，却按 i64 读，任何一条 open 条目都会让读账本
        // panic。让它变成一个可上报的错误。
        rows.into_iter()
            .map(|row| -> anyhow::Result<GagEntry> {
                Ok(GagEntry {
                    id: row.try_get(0)?,
                    kind: match row.try_get::<String, _>(1)?.as_str() {
                        "promise" => GagKind::Promise,
                        "grudge" => GagKind::Grudge,
                        _ => GagKind::Gag,
                    },
                    text: row.try_get(2)?,
                    state: row.try_get(3)?,
                    occurrence: row.try_get(4)?,
                    importance: row.try_get(5)?,
                    created_at: row.try_get(6)?,
                    updated_at: row.try_get(7)?,
                })
            })
            .collect()
    }

    /// Mark an entry fulfilled/voided by id. Returns false when not found.
    pub(crate) async fn fulfill(&self, id: Uuid) -> anyhow::Result<bool> {
        let result = query(
            "UPDATE yunxi_gag_entries SET state = 'fulfilled', updated_at = $2
             WHERE id = $1 AND state = 'open'",
        )
        .bind(id)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fulfill the single open entry whose id starts with `prefix` (the short
    /// id shown in the ledger list). Returns the matched id, or None when the
    /// prefix is missing/ambiguous/not open.
    pub(crate) async fn fulfill_by_prefix(&self, prefix: &str) -> anyhow::Result<Option<Uuid>> {
        let prefix = prefix.trim().to_lowercase();
        if prefix.is_empty() {
            return Ok(None);
        }
        // 用 UUID 前缀的范围比较而不是 `CAST(id AS TEXT) LIKE`：后者既走不了主键
        // 索引，又会让 `#还账 _` 里的 `_` 变成通配符——一次"前缀歧义"被报成
        // "没找到这条账"，而且每次调用都要对全表做 CAST。
        let Some((lower, upper)) = uuid_prefix_range(&prefix) else {
            return Ok(None);
        };
        let ids: Vec<Uuid> = query_scalar(
            "SELECT id FROM yunxi_gag_entries
             WHERE id >= $1 AND ($2::UUID IS NULL OR id < $2) AND state = 'open' LIMIT 2",
        )
        .bind(lower)
        .bind(upper)
        .fetch_all(&self.pool)
        .await?;
        if ids.len() != 1 {
            return Ok(None);
        }
        let id = ids[0];
        if self.fulfill(id).await? {
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }

    #[allow(dead_code)] // wired by the future full-gag management surface.
    pub(crate) async fn void(&self, id: Uuid) -> anyhow::Result<bool> {
        let result = query(
            "UPDATE yunxi_gag_entries SET state = 'void', updated_at = $2
             WHERE id = $1 AND state = 'open'",
        )
        .bind(id)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete everything for a scope (data-deletion path), returns removed rows.
    pub(crate) async fn delete_for_scope(&self, scope: GagScope) -> anyhow::Result<u64> {
        let (scope_kind, scope_id) = scope.kind_id();
        let result = query(
            "DELETE FROM yunxi_gag_entries
             WHERE scope_kind = $1 AND scope_id IS NOT DISTINCT FROM $2",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Prune old fulfilled/voided entries after the configured TTL.
    #[allow(dead_code)] // wired by the existing maintenance interval next.
    pub(crate) async fn prune_stale(&self) -> anyhow::Result<u64> {
        let cutoff = Utc::now() - chrono::Duration::days(self.config.entry_ttl_days() as i64);
        let result = query(
            "DELETE FROM yunxi_gag_entries
             WHERE state <> 'open' AND updated_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// 把 1..=32 位十六进制前缀翻译成 UUID 的半开区间 `[lower, upper)`。
///
/// 比较的是 `uuid` 类型本身而不是它的文本形式：`id >= lower AND id < upper` 能走
/// 主键索引，也不会像 `LIKE` 那样把用户输入里的 `_`/`%` 当成通配符。前缀全是 `f`
/// 时上界溢出（区间一直延伸到类型最大值），这时返回 `None` 作上界，SQL 用
/// `$2 IS NULL` 表达"没有上界"。
///
/// 非法输入（空、超过 32 位、含非十六进制字符）返回 `None`，调用方按"没找到"处理。
fn uuid_prefix_range(prefix: &str) -> Option<(Uuid, Option<Uuid>)> {
    const HEX_DIGITS: usize = 32;
    let normalized = prefix
        .chars()
        .filter(|character| *character != '-')
        .collect::<String>()
        .to_lowercase();
    if normalized.is_empty()
        || normalized.len() > HEX_DIGITS
        || !normalized
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u128::from_str_radix(&normalized, 16).ok()?;
    let shift = 4 * (HEX_DIGITS - normalized.len()) as u32;
    let lower = Uuid::from_u128(value.checked_shl(shift).unwrap_or(u128::MAX));
    let upper = value
        .checked_add(1)
        .and_then(|next| next.checked_shl(shift))
        .map(Uuid::from_u128);
    Some((lower, upper))
}

#[allow(dead_code)]
fn _assert_postgres_type(_: &Postgres) {}

#[cfg(test)]
mod tests {
    use super::uuid_prefix_range;
    use uuid::Uuid;

    #[test]
    fn uuid_prefix_range_brackets_exactly_the_matching_ids() {
        let id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").expect("uuid");
        let (lower, upper) = uuid_prefix_range("0123").expect("合法前缀");
        let upper = upper.expect("前缀不全为 f 时应有上界");
        assert!(id >= lower && id < upper, "命中前缀的 id 应落在区间内");

        // 换个前缀就不该命中。
        let other = Uuid::parse_str("11234567-89ab-cdef-0123-456789abcdef").expect("uuid");
        assert!(
            !(other >= lower && other < upper),
            "不同前缀不该落在同一区间"
        );

        // 前缀越长区间越窄，但始终包含目标。
        let (lower, upper) = uuid_prefix_range("0123456789ab").expect("合法前缀");
        let upper = upper.expect("应有上界");
        assert!(id >= lower && id < upper);
    }

    #[test]
    fn uuid_prefix_range_rejects_wildcards_and_junk() {
        // `_`/`%` 是老实现里最危险的两个字符（LIKE 通配符），现在直接判非法。
        assert!(uuid_prefix_range("_").is_none());
        assert!(uuid_prefix_range("%").is_none());
        assert!(uuid_prefix_range("").is_none());
        assert!(uuid_prefix_range("zz").is_none());
        assert!(uuid_prefix_range(&"a".repeat(33)).is_none());
        // 带连字符的完整 id 也认。
        assert!(uuid_prefix_range("01234567-89ab-cdef-0123-456789abcdef").is_some());
        // 全 f 的前缀没有上界，但仍然是一个合法区间。
        let (_, upper) = uuid_prefix_range(&"f".repeat(32)).expect("合法前缀");
        assert!(upper.is_none(), "全 f 前缀应返回无上界");
    }
}
