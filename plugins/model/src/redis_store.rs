//! Redis 运行态存储。
//!
//! Redis 只保存可重建、带 TTL 的热数据；长期记忆和用户资料仍由 PostgreSQL 负责。

use anyhow::{Context, Result, anyhow};
use kovi::tokio::runtime::{Handle, Id as RuntimeId};
use kovi::tokio::sync::Mutex;
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Script};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_KEY_PREFIX: &str = "kovi";
const REDIS_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REDIS_COMMAND_TIMEOUT: Duration = Duration::from_millis(800);
const REDIS_INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const REDIS_MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

static REDIS_STATE: LazyLock<Mutex<RedisConnectionState>> =
    LazyLock::new(|| Mutex::new(RedisConnectionState::default()));

struct RedisConnectionState {
    store: Option<Arc<RedisStore>>,
    configured_url: Option<String>,
    configured_prefix: String,
    retry_after: Option<Instant>,
    retry_delay: Duration,
}

impl Default for RedisConnectionState {
    fn default() -> Self {
        Self {
            store: None,
            configured_url: None,
            configured_prefix: DEFAULT_KEY_PREFIX.to_string(),
            retry_after: None,
            retry_delay: REDIS_INITIAL_RETRY_DELAY,
        }
    }
}

impl RedisConnectionState {
    fn schedule_retry(&mut self, now: Instant) {
        let retry_delay = self.retry_delay;
        self.retry_after = Some(now + retry_delay);
        self.retry_delay = retry_delay.saturating_mul(2).min(REDIS_MAX_RETRY_DELAY);
    }
}

#[derive(Clone)]
pub(crate) struct RedisStore {
    connection: ConnectionManager,
    key_prefix: String,
    runtime_id: RuntimeId,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RedisBotMessage {
    pub(crate) message_id: i32,
    pub(crate) content: String,
    pub(crate) sent_at_ms: i64,
}

#[derive(Debug, Serialize)]
struct RedisBotMessagePayload<'a> {
    message_id: i32,
    content: &'a str,
    sent_at_ms: i64,
}

/// 初始化 Redis。未设置 `REDIS_URL` 时保持纯本地运行，不阻断机器人启动。
pub(crate) async fn initialize() {
    if std::env::var("REDIS_URL")
        .ok()
        .is_none_or(|url| url.trim().is_empty())
    {
        println!("[INFO] 未设置 REDIS_URL，运行态继续使用本地内存");
        return;
    }

    if get().await.is_some() {
        println!("[INFO] Redis 运行态存储已启用");
    } else {
        eprintln!("[WARN] Redis 不可用，运行态回退本地内存");
    }
}

pub(crate) async fn get() -> Option<Arc<RedisStore>> {
    let url = std::env::var("REDIS_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())?;
    let url = url.trim().to_string();
    let key_prefix = std::env::var("REDIS_KEY_PREFIX")
        .ok()
        .filter(|prefix| !prefix.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_KEY_PREFIX.to_string());
    let runtime_id = Handle::current().id();
    let now = Instant::now();
    let mut state = REDIS_STATE.lock().await;

    // 配置改变时立即丢弃旧连接并重新建立，避免进程必须重启才能生效。
    if state.configured_url.as_deref() != Some(url.as_str())
        || state.configured_prefix != key_prefix
    {
        state.store = None;
        state.configured_url = Some(url.clone());
        state.configured_prefix = key_prefix.clone();
        state.retry_after = None;
        state.retry_delay = REDIS_INITIAL_RETRY_DELAY;
    }
    // redis::ConnectionManager caches reconnect futures and timer handles from the Tokio
    // runtime that created it. Reusing it from another runtime can poll a timer after its
    // original runtime has shut down (notably in parallel tests).
    if state
        .store
        .as_ref()
        .is_some_and(|store| store.runtime_id != runtime_id)
    {
        state.store = None;
        state.retry_after = None;
        state.retry_delay = REDIS_INITIAL_RETRY_DELAY;
    }
    if let Some(store) = &state.store {
        return Some(Arc::clone(store));
    }
    if state
        .retry_after
        .is_some_and(|retry_after| retry_after > now)
    {
        return None;
    }

    match connect(&url, key_prefix, runtime_id).await {
        Some(store) => {
            let store = Arc::new(store);
            state.store = Some(Arc::clone(&store));
            state.retry_after = None;
            state.retry_delay = REDIS_INITIAL_RETRY_DELAY;
            Some(store)
        }
        None => {
            state.schedule_retry(now);
            None
        }
    }
}

async fn connect(url: &str, key_prefix: String, runtime_id: RuntimeId) -> Option<RedisStore> {
    let client = match redis::Client::open(url) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[WARN] Redis URL 无效: {}", error);
            return None;
        }
    };
    let connection =
        match kovi::tokio::time::timeout(REDIS_CONNECT_TIMEOUT, client.get_connection_manager())
            .await
        {
            Ok(Ok(connection)) => connection,
            Ok(Err(error)) => {
                eprintln!("[WARN] Redis 连接失败: {}", error);
                return None;
            }
            Err(_) => {
                eprintln!("[WARN] Redis 连接超时");
                return None;
            }
        };
    Some(RedisStore {
        connection,
        key_prefix,
        runtime_id,
    })
}

async fn mark_connection_unavailable(store: &Arc<RedisStore>) {
    let mut state = REDIS_STATE.lock().await;
    if state
        .store
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, store))
    {
        state.store = None;
        state.retry_delay = REDIS_INITIAL_RETRY_DELAY;
        state.schedule_retry(Instant::now());
    }
}

/// 返回 Redis 当前配置与连通状态，供系统信息命令展示。
pub(crate) async fn health_status() -> String {
    let configured = std::env::var("REDIS_URL")
        .ok()
        .is_some_and(|url| !url.trim().is_empty());
    if !configured {
        return "未配置（本地内存兜底）".to_string();
    }

    let Some(store) = get().await else {
        return "已配置但不可用（本地内存兜底）".to_string();
    };
    match store.ping().await {
        Ok(()) => "已启用且连通".to_string(),
        Err(_) => {
            mark_connection_unavailable(&store).await;
            "已配置但不可用（本地内存兜底）".to_string()
        }
    }
}

/// 启动 readiness 使用的强校验：未配置 Redis 时明确表示该依赖是可选的；一旦配置，
/// 就不能用“本地兜底”冒充生产已就绪。
pub(crate) async fn check_readiness() -> Result<()> {
    let configured = std::env::var("REDIS_URL")
        .ok()
        .is_some_and(|url| !url.trim().is_empty());
    if !configured {
        return Ok(());
    }
    let store = get()
        .await
        .ok_or_else(|| anyhow!("REDIS_URL 已配置但 Redis 尚未连接"))?;
    store.ping().await
}

impl RedisStore {
    fn key(&self, suffix: &str) -> String {
        format!("{}:v1:{}", self.key_prefix, suffix)
    }

    fn scope_suffix(scope: &str, subject_id: i64) -> String {
        format!("{}:{}", scope, subject_id)
    }

    async fn ping(&self) -> Result<()> {
        let mut connection = self.connection.clone();
        let _: String = kovi::tokio::time::timeout(
            REDIS_COMMAND_TIMEOUT,
            redis::cmd("PING").query_async(&mut connection),
        )
        .await
        .map_err(|_| anyhow!("Redis 探活超时"))?
        .context("Redis 探活失败")?;
        Ok(())
    }

    pub(crate) async fn set_expiring_text(
        &self,
        suffix: &str,
        value: &str,
        ttl: Duration,
    ) -> Result<()> {
        let mut connection = self.connection.clone();
        kovi::tokio::time::timeout(
            REDIS_COMMAND_TIMEOUT,
            connection.set_ex(self.key(suffix), value, ttl.as_secs().max(1)),
        )
        .await
        .map_err(|_| anyhow!("Redis 写入临时状态超时"))?
        .context("Redis 写入临时状态失败")
    }

    pub(crate) async fn take_text(&self, suffix: &str) -> Result<Option<String>> {
        let mut connection = self.connection.clone();
        kovi::tokio::time::timeout(
            REDIS_COMMAND_TIMEOUT,
            Script::new(
                r#"
            local value = redis.call('GET', KEYS[1])
            redis.call('DEL', KEYS[1])
            return value
            "#,
            )
            .key(self.key(suffix))
            .invoke_async(&mut connection),
        )
        .await
        .map_err(|_| anyhow!("Redis 读取并删除临时状态超时"))?
        .context("Redis 读取并删除临时状态失败")
    }

    pub(crate) async fn delete(&self, suffix: &str) -> Result<()> {
        let mut connection = self.connection.clone();
        let _: i64 =
            kovi::tokio::time::timeout(REDIS_COMMAND_TIMEOUT, connection.del(self.key(suffix)))
                .await
                .map_err(|_| anyhow!("Redis 删除临时状态超时"))?
                .context("Redis 删除临时状态失败")?;
        Ok(())
    }

    /// 在时间窗口内原子递增计数；返回窗口内当前次数。
    pub(crate) async fn increment_expiring(&self, suffix: &str, window: Duration) -> Result<i64> {
        let mut connection = self.connection.clone();
        kovi::tokio::time::timeout(
            REDIS_COMMAND_TIMEOUT,
            Script::new(
                r#"
            local count = redis.call('INCR', KEYS[1])
            if count == 1 then
                redis.call('EXPIRE', KEYS[1], ARGV[1])
            end
            return count
            "#,
            )
            .key(self.key(suffix))
            .arg(window.as_secs().max(1))
            .invoke_async(&mut connection),
        )
        .await
        .map_err(|_| anyhow!("Redis 限流计数超时"))?
        .context("Redis 限流计数失败")
    }

    pub(crate) async fn record_bot_message(
        &self,
        scope: &str,
        subject_id: i64,
        message_id: i32,
        content: &str,
        ttl: Duration,
    ) -> Result<()> {
        let sent_at_ms = now_millis()?;
        let scope_suffix = Self::scope_suffix(scope, subject_id);
        let index_key = self.key(&format!("bot_messages:{scope_suffix}"));
        let entry_key = self.key(&format!("bot_message:{scope_suffix}:{message_id}"));
        let payload = serde_json::to_string(&RedisBotMessagePayload {
            message_id,
            content,
            sent_at_ms,
        })?;
        let cutoff = sent_at_ms.saturating_sub(ttl.as_millis().try_into().unwrap_or(i64::MAX));
        let mut connection = self.connection.clone();
        kovi::tokio::time::timeout(
            REDIS_COMMAND_TIMEOUT,
            redis::pipe()
                .atomic()
                .cmd("ZREMRANGEBYSCORE")
                .arg(&index_key)
                .arg(0)
                .arg(cutoff)
                .ignore()
                .cmd("SET")
                .arg(&entry_key)
                .arg(payload)
                .arg("EX")
                .arg(ttl.as_secs().max(1))
                .ignore()
                .cmd("ZADD")
                .arg(&index_key)
                .arg(sent_at_ms)
                .arg(message_id)
                .ignore()
                .cmd("EXPIRE")
                .arg(&index_key)
                .arg(ttl.as_secs().max(1))
                .ignore()
                .query_async::<()>(&mut connection),
        )
        .await
        .map_err(|_| anyhow!("Redis 记录芸汐消息超时"))?
        .context("Redis 记录芸汐消息失败")
    }

    pub(crate) async fn recent_bot_messages(
        &self,
        scope: &str,
        subject_id: i64,
        limit: usize,
        ttl: Duration,
    ) -> Result<Vec<RedisBotMessage>> {
        let now = now_millis()?;
        let cutoff = now.saturating_sub(ttl.as_millis().try_into().unwrap_or(i64::MAX));
        let index_key = self.key(&format!(
            "bot_messages:{}",
            Self::scope_suffix(scope, subject_id)
        ));
        let mut connection = self.connection.clone();
        let ids: Vec<String> = kovi::tokio::time::timeout(
            REDIS_COMMAND_TIMEOUT,
            redis::cmd("ZREVRANGEBYSCORE")
                .arg(&index_key)
                .arg(now)
                .arg(cutoff)
                .arg("LIMIT")
                .arg(0)
                .arg(limit)
                .query_async(&mut connection),
        )
        .await
        .map_err(|_| anyhow!("Redis 读取芸汐消息索引超时"))?
        .context("Redis 读取芸汐消息索引失败")?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let keys = ids
            .iter()
            .map(|message_id| {
                self.key(&format!(
                    "bot_message:{}:{}",
                    Self::scope_suffix(scope, subject_id),
                    message_id
                ))
            })
            .collect::<Vec<_>>();
        let payloads: Vec<Option<String>> = kovi::tokio::time::timeout(
            REDIS_COMMAND_TIMEOUT,
            redis::cmd("MGET").arg(keys).query_async(&mut connection),
        )
        .await
        .map_err(|_| anyhow!("Redis 读取芸汐消息超时"))?
        .context("Redis 读取芸汐消息失败")?;
        let messages = payloads
            .into_iter()
            .flatten()
            .filter_map(|payload| serde_json::from_str(&payload).ok())
            .collect::<Vec<_>>();
        Ok(messages)
    }

    pub(crate) async fn remove_bot_messages(
        &self,
        scope: &str,
        subject_id: i64,
        message_ids: &[i32],
    ) -> Result<()> {
        if message_ids.is_empty() {
            return Ok(());
        }
        let scope_suffix = Self::scope_suffix(scope, subject_id);
        let index_key = self.key(&format!("bot_messages:{scope_suffix}"));
        let entry_keys = message_ids
            .iter()
            .map(|message_id| self.key(&format!("bot_message:{scope_suffix}:{message_id}")))
            .collect::<Vec<_>>();
        let mut connection = self.connection.clone();
        kovi::tokio::time::timeout(
            REDIS_COMMAND_TIMEOUT,
            redis::pipe()
                .atomic()
                .cmd("DEL")
                .arg(entry_keys)
                .ignore()
                .cmd("ZREM")
                .arg(index_key)
                .arg(message_ids)
                .ignore()
                .query_async::<()>(&mut connection),
        )
        .await
        .map_err(|_| anyhow!("Redis 删除芸汐消息候选超时"))?
        .context("Redis 删除芸汐消息候选失败")
    }
}

fn now_millis() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .map_err(|error| anyhow!("系统时间异常: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{REDIS_INITIAL_RETRY_DELAY, REDIS_MAX_RETRY_DELAY, RedisConnectionState, get};
    use std::time::{Duration, Instant};

    #[test]
    fn connection_failures_schedule_bounded_retries_instead_of_caching_none_forever() {
        let mut state = RedisConnectionState::default();
        let now = Instant::now();
        state.schedule_retry(now);
        assert_eq!(state.retry_after, Some(now + REDIS_INITIAL_RETRY_DELAY));
        assert!(state.retry_delay > REDIS_INITIAL_RETRY_DELAY);

        for _ in 0..16 {
            state.schedule_retry(now);
        }
        assert_eq!(state.retry_delay, REDIS_MAX_RETRY_DELAY);
        assert!(state.retry_after.is_some());
    }

    #[test]
    #[ignore = "requires Redis via REDIS_URL"]
    fn redis_runtime_store_round_trips() {
        let suffix = format!(
            "integration:{}:{}",
            std::process::id(),
            super::now_millis().expect("系统时间应可用")
        );
        let first_runtime = kovi::tokio::runtime::Runtime::new().expect("应创建首个测试运行时");
        first_runtime.block_on(async {
            let store = get().await.expect("REDIS_URL 应指向可用 Redis");
            store
                .set_expiring_text(&suffix, "round-trip", Duration::from_secs(30))
                .await
                .expect("应写入临时文本");
        });
        drop(first_runtime);

        kovi::tokio::runtime::Runtime::new()
            .expect("应创建第二个测试运行时")
            .block_on(async {
                let store = get().await.expect("REDIS_URL 应指向可用 Redis");
                assert_eq!(
                    store.take_text(&suffix).await.expect("应取出临时文本"),
                    Some("round-trip".to_string())
                );
                assert_eq!(
                    store
                        .increment_expiring(&suffix, Duration::from_secs(30))
                        .await
                        .expect("首次计数应成功"),
                    1
                );
                assert_eq!(
                    store
                        .increment_expiring(&suffix, Duration::from_secs(30))
                        .await
                        .expect("第二次计数应成功"),
                    2
                );
                store.delete(&suffix).await.expect("应清理集成测试键");
            });
    }
}
