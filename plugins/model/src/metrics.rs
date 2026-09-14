//! 用量记账：芸汐每天干了多少活。
//!
//! 管理后台的「记忆」页要显示"保存/召回/反思/心智模型/模型调用"这类用量卡，
//! 并且带**对比上周**。周环比必须有历史，而这条链路原本只有进程内的环形轨迹
//! （[`crate::model::llm_trace`]）——重启即清零、也不落盘。所以这里补一层最小
//! 的按日累计：
//!
//! - 事件发生时只加内存里的计数器（`record`），不碰数据库；
//! - 每 30 秒或有人要读数时（`flush`）把累计量 upsert 进 `yunxi_metrics_daily`，
//!   所以一次聊天里写十条记忆只产生一次入库；
//! - 读数时按「最近 7 天 / 前 7 天」聚合，得到真实的周环比。
//!
//! 计数是加法、键是 `(day, metric)`，因此它天然可重放、不会和领域表打架。

use crate::memory::MEMORY_MANAGER;
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate};
use serde_json::{Value, json};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// 指标表名。
const TABLE: &str = "yunxi_metrics_daily";
/// 保留多少天的历史（周环比只需要 14 天，留一倍余量便于排查）。
const RETENTION_DAYS: i64 = 90;
/// 内存缓冲最多攒多少条（指标种类很少，这是防呆上限）。
const MAX_BUFFERED_METRICS: usize = 32;

/// 记账的指标种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Metric {
    /// 写进记忆的内容量（长期记忆 + Mind 记录），单位 token（估算）。
    MemorySavedTokens,
    /// 召回进上下文的内容量，单位 token（估算）。
    MemoryRecalledTokens,
    /// Mind 反思运行次数。
    ReflectionCalls,
    /// Mind 快照注入提示词的估算 token（对应 Hindsight 的「获取模型」）。
    MindInjectedTokens,
    /// 模型调用次数（成功与失败都算）。
    LlmCalls,
    /// 模型收发的估算 token（提示词 + 回复，只统计成功的调用）。
    LlmTokens,
}

impl Metric {
    /// 落库用的稳定键名。改这里的名字等于把历史拆成两段，别改。
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::MemorySavedTokens => "memory_saved_tokens",
            Self::MemoryRecalledTokens => "memory_recalled_tokens",
            Self::ReflectionCalls => "reflection_calls",
            Self::MindInjectedTokens => "mind_injected_tokens",
            Self::LlmCalls => "llm_calls",
            Self::LlmTokens => "llm_tokens",
        }
    }

    /// 卡片上的单位。
    pub(crate) const fn unit(self) -> &'static str {
        match self {
            Self::ReflectionCalls | Self::LlmCalls => "calls",
            _ => "tokens",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::MemorySavedTokens => "保存",
            Self::MemoryRecalledTokens => "召回",
            Self::ReflectionCalls => "反思",
            Self::MindInjectedTokens => "心智模型",
            Self::LlmCalls => "模型调用",
            Self::LlmTokens => "模型 token",
        }
    }

    const fn hint(self) -> &'static str {
        match self {
            Self::MemorySavedTokens => "写进记忆的内容量",
            Self::MemoryRecalledTokens => "召回进上下文的内容量",
            Self::ReflectionCalls => "Mind 反思运行次数",
            Self::MindInjectedTokens => "Mind 快照注入提示词",
            Self::LlmCalls => "模型调用次数",
            Self::LlmTokens => "模型收发的估算 token",
        }
    }

    const ALL: [Metric; 6] = [
        Metric::MemorySavedTokens,
        Metric::MemoryRecalledTokens,
        Metric::ReflectionCalls,
        Metric::MindInjectedTokens,
        Metric::LlmTokens,
        Metric::LlmCalls,
    ];
}

/// 待落库的累计量：`(天, 指标) -> 数量`。
static BUFFER: LazyLock<Mutex<HashMap<(NaiveDate, &'static str), i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 记一笔。只动内存，失败也不影响调用方——记账不该拖累聊天。
pub(crate) fn record(metric: Metric, amount: u64) {
    if amount == 0 {
        return;
    }
    let today = Local::now().date_naive();
    let Ok(mut buffer) = BUFFER.lock() else {
        return;
    };
    if buffer.len() >= MAX_BUFFERED_METRICS && !buffer.contains_key(&(today, metric.key())) {
        // 正常永远不会走到这里（指标种类是个位数）；真走到了说明有人在
        // 用变化的键名记账，宁可丢这一笔也不要让内存无界增长。
        return;
    }
    *buffer.entry((today, metric.key())).or_insert(0) += amount as i64;
}

/// 估算一段文本的 token 数。
///
/// 中文一个字≈一个 token，ASCII 大约四字符一个。这是估算不是精确值，
/// 界面上因此写「≈」——但它对"这周比上周多记了多少"这类比较足够稳。
pub(crate) fn approx_tokens(text: &str) -> u64 {
    let mut wide = 0_u64;
    let mut narrow = 0_u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            narrow += 1;
        } else {
            wide += 1;
        }
    }
    wide + narrow.div_ceil(4)
}

/// 建表（幂等）。
pub(crate) async fn ensure_schema() -> anyhow::Result<()> {
    let pool = pool()?;
    query(&format!(
        "CREATE TABLE IF NOT EXISTS {TABLE} (
             day DATE NOT NULL,
             metric TEXT NOT NULL CHECK (octet_length(metric) BETWEEN 1 AND 64),
             amount BIGINT NOT NULL DEFAULT 0 CHECK (amount >= 0),
             updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
             PRIMARY KEY (day, metric)
         )"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

/// 把内存里的累计量写进数据库。由后台任务周期调用，也可在读数前调用。
pub(crate) async fn flush() {
    // 取快照，不是 drain：drain 之后、写库之前是一段 await，future 一旦被取消
    // （管理后台的请求中断、进程关机）这一窗计数就永久丢了，而它可能攒了好几天。
    // 快照语义下"写入成功才扣减"，失败与取消都原样留着，也就不需要再把数据放回去。
    let pending: Vec<(NaiveDate, &'static str, i64)> = {
        let Ok(buffer) = BUFFER.lock() else {
            return;
        };
        if buffer.is_empty() {
            return;
        }
        buffer
            .iter()
            .map(|((day, metric), amount)| (*day, *metric, *amount))
            .collect()
    };

    let Ok(pool) = pool() else {
        // 池还没就绪：缓冲原样留着，下一次再试。
        return;
    };

    for (day, metric, amount) in pending {
        let result = query(&format!(
            "INSERT INTO {TABLE} (day, metric, amount) VALUES ($1, $2, $3)
             ON CONFLICT (day, metric) DO UPDATE
             SET amount = {TABLE}.amount + EXCLUDED.amount, updated_at = NOW()"
        ))
        .bind(day)
        .bind(metric)
        .bind(amount)
        .execute(pool)
        .await;
        match result {
            Ok(_) => subtract(day, metric, amount),
            Err(error) => {
                // 写失败就不用扣减：缓冲里那份留着，下一轮继续尝试。
                eprintln!("[WARN] 用量指标写入失败 ({metric}): {error}");
            }
        }
    }

    // 顺手清理过期历史；一天最多删一次，代价可以忽略。
    let cutoff = Local::now().date_naive() - ChronoDuration::days(RETENTION_DAYS);
    let _ = query(&format!("DELETE FROM {TABLE} WHERE day < $1"))
        .bind(cutoff)
        .execute(pool)
        .await;
}

/// 写库成功后，把快照里的这一笔从缓冲里扣掉。
///
/// 扣减而不是删除，是因为写库期间可能又记了几笔：那些是快照之后新增的，必须留下。
fn subtract(day: NaiveDate, metric: &'static str, amount: i64) {
    let Ok(mut buffer) = BUFFER.lock() else {
        return;
    };
    if let Some(current) = buffer.get_mut(&(day, metric)) {
        *current -= amount;
        if *current <= 0 {
            buffer.remove(&(day, metric));
        }
    }
}

fn pool() -> anyhow::Result<&'static PgPool> {
    MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow::anyhow!("PostgreSQL 记忆连接池尚未初始化"))
}

/// 一周的起止（含今天在内的最近 7 天）。
fn week_start(today: NaiveDate) -> NaiveDate {
    today - ChronoDuration::days(6)
}

/// 读一周的用量卡：本周、上周、以及环比。
///
/// 读之前先 flush，因此界面上看到的数字包含刚刚发生、还没落库的那些。
pub(crate) async fn weekly_cards() -> Value {
    flush().await;

    let today = Local::now().date_naive();
    let this_start = week_start(today);
    let prev_start = this_start - ChronoDuration::days(7);

    let mut this_week: HashMap<&'static str, i64> = HashMap::new();
    let mut last_week: HashMap<&'static str, i64> = HashMap::new();

    if let Ok(pool) = pool()
        && let Ok(rows) = query(&format!(
            "SELECT metric, day, amount FROM {TABLE} WHERE day >= $1"
        ))
        .bind(prev_start)
        .fetch_all(pool)
        .await
    {
        for row in rows {
            let Ok(metric) = row.try_get::<String, _>("metric") else {
                continue;
            };
            let Ok(day) = row.try_get::<NaiveDate, _>("day") else {
                continue;
            };
            let amount: i64 = row.try_get("amount").unwrap_or_default();
            if day >= this_start {
                *this_week.entry(leak_metric_key(&metric)).or_insert(0) += amount;
            } else {
                *last_week.entry(leak_metric_key(&metric)).or_insert(0) += amount;
            }
        }
    }

    // 还没落库、但已经记在内存里的今天，也要算进本周。
    if let Ok(buffer) = BUFFER.lock() {
        for ((day, metric), amount) in buffer.iter() {
            if *day >= this_start {
                *this_week.entry(*metric).or_insert(0) += *amount;
            }
        }
    }

    let cards: Vec<Value> = Metric::ALL
        .iter()
        .map(|metric| {
            let current = *this_week.get(metric.key()).unwrap_or(&0);
            let previous = *last_week.get(metric.key()).unwrap_or(&0);
            json!({
                "key": metric.key(),
                "label": metric.label(),
                "unit": metric.unit(),
                "hint": metric.hint(),
                "current": current,
                "previous": previous,
                "delta_percent": percent_change(current, previous),
            })
        })
        .collect();

    json!({
        "period_start": this_start.to_string(),
        "period_end": today.to_string(),
        "weekday": today.weekday().to_string(),
        "cards": cards,
    })
}

/// 把库里的键映射回已知指标，未知键忽略（老版本写下的键不该让新版本崩）。
fn leak_metric_key(key: &str) -> &'static str {
    Metric::ALL
        .iter()
        .map(|metric| metric.key())
        .find(|known| *known == key)
        // 未知键归到一个不影响显示的名字上：仍会被统计，但不占卡片位。
        .unwrap_or("unknown")
}

/// 环比百分比；上周为 0 时返回 null（界面显示"本周 +N"而不是除零）。
fn percent_change(current: i64, previous: i64) -> Option<i64> {
    if previous == 0 {
        return None;
    }
    Some(((current - previous) as f64 / previous as f64 * 100.0).round() as i64)
}

/// 后台周期落库任务。
pub(crate) async fn run_flush_loop() {
    loop {
        kovi::tokio::time::sleep(kovi::tokio::time::Duration::from_secs(30)).await;
        flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_cjk_aware() {
        // 中文按字，ASCII 按四字符。
        assert_eq!(approx_tokens("你好世界"), 4);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("你好abcd"), 3);
        assert_eq!(approx_tokens(""), 0);
    }

    #[test]
    fn week_start_covers_seven_days_including_today() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 13).expect("日期应合法");
        let start = week_start(today);
        assert_eq!(
            start,
            NaiveDate::from_ymd_opt(2026, 9, 7).expect("日期应合法")
        );
        assert_eq!((today - start).num_days(), 6);
    }

    #[test]
    fn percent_change_handles_a_missing_baseline() {
        assert_eq!(percent_change(10, 0), None);
        assert_eq!(percent_change(10, 10), Some(0));
        assert_eq!(percent_change(15, 10), Some(50));
        assert_eq!(percent_change(5, 10), Some(-50));
    }

    #[test]
    fn metric_keys_are_unique_and_stable() {
        let mut keys: Vec<&str> = Metric::ALL.iter().map(|metric| metric.key()).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(count, keys.len());
        // 键名一旦落库就是历史数据的一部分，这里把它钉住。
        assert!(keys.contains(&"memory_saved_tokens"));
        assert!(keys.contains(&"llm_calls"));
        assert!(keys.contains(&"llm_tokens"));
    }

    #[test]
    fn buffered_records_accumulate_per_day_and_metric() {
        // 直接操作缓冲，避免测试之间互相干扰：先记下当前值再比对增量。
        let today = Local::now().date_naive();
        let before = BUFFER
            .lock()
            .map(|buffer| {
                buffer
                    .get(&(today, Metric::ReflectionCalls.key()))
                    .copied()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        record(Metric::ReflectionCalls, 3);
        let after = BUFFER
            .lock()
            .map(|buffer| {
                buffer
                    .get(&(today, Metric::ReflectionCalls.key()))
                    .copied()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        assert_eq!(after - before, 3);
        // 0 不该留下痕迹。
        record(Metric::ReflectionCalls, 0);
        let unchanged = BUFFER
            .lock()
            .map(|buffer| {
                buffer
                    .get(&(today, Metric::ReflectionCalls.key()))
                    .copied()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        assert_eq!(unchanged, after);
    }

    #[test]
    fn unknown_metric_keys_do_not_break_reads() {
        assert_eq!(
            leak_metric_key("memory_saved_tokens"),
            "memory_saved_tokens"
        );
        assert_eq!(leak_metric_key("some_future_metric"), "unknown");
    }
}
