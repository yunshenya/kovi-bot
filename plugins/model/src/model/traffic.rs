//! 统一的入站流量闸门和文本资源边界。

use crate::config;
use kovi::tokio::sync::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InboundScope {
    Group { group_id: i64, user_id: i64 },
    Private(i64),
}

#[derive(Default)]
struct ScopeTraffic {
    recent: VecDeque<Instant>,
    blocked_until: Option<Instant>,
    last_seen: Option<Instant>,
}

#[derive(Default)]
struct TrafficState {
    scopes: HashMap<InboundScope, ScopeTraffic>,
    global_recent: VecDeque<Instant>,
}

static TRAFFIC_STATE: LazyLock<Mutex<TrafficState>> =
    LazyLock::new(|| Mutex::new(TrafficState::default()));

/// 在任何模型、数据库或外部下载之前执行。返回 `true` 表示本次输入应被抑制。
///
/// 受信任的管理员可以跳过单用户冷却，但仍计入并受全局资源上限约束，避免配置错误
/// 或消息回环拖垮整个进程。
pub(crate) async fn should_suppress(scope: InboundScope, trusted: bool) -> bool {
    let limits = config::get().traffic().clone();
    if !limits.enabled() {
        return false;
    }

    let now = Instant::now();
    let window = Duration::from_secs(limits.window_secs());
    let cooldown = Duration::from_secs(limits.cooldown_secs());
    let mut state = TRAFFIC_STATE.lock().await;

    state
        .global_recent
        .retain(|seen_at| now.duration_since(*seen_at) < window);
    if state.global_recent.len() >= limits.global_limit() {
        return true;
    }

    // 顺手淘汰长期未出现的 scope，避免独立用户数无限增长。
    let stale_after = window.saturating_add(cooldown).saturating_mul(2);
    state.scopes.retain(|_, traffic| {
        traffic
            .last_seen
            .is_some_and(|seen_at| now.duration_since(seen_at) < stale_after)
    });

    if trusted {
        state.global_recent.push_back(now);
        return false;
    }

    let traffic = state.scopes.entry(scope).or_default();
    traffic.last_seen = Some(now);
    traffic
        .recent
        .retain(|seen_at| now.duration_since(*seen_at) < window);
    if traffic.blocked_until.is_some_and(|deadline| deadline > now) {
        return true;
    }
    traffic.blocked_until = None;
    if traffic.recent.len() >= limits.per_user_limit() {
        traffic.blocked_until = Some(now + cooldown);
        traffic.recent.clear();
        return true;
    }

    traffic.recent.push_back(now);
    state.global_recent.push_back(now);
    false
}

pub(crate) async fn clear_private_traffic(user_id: i64) -> bool {
    TRAFFIC_STATE
        .lock()
        .await
        .scopes
        .remove(&InboundScope::Private(user_id))
        .is_some()
}

pub(crate) fn bounded_input(value: &str) -> String {
    truncate_chars(value, config::get().traffic().max_input_chars())
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::{InboundScope, ScopeTraffic, TRAFFIC_STATE, clear_private_traffic, truncate_chars};

    #[test]
    fn input_truncation_is_unicode_safe_and_bounded() {
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("你好世界", 3), "你好…");
        assert_eq!(truncate_chars("ok", 4), "ok");
    }

    /// `trusted` 的作用域不受"按人限流 + 120 秒整段封锁"影响——点名她与管理员
    /// 走的就是这条（线上 2026-09-14 20:44：一分钟 48 条刷屏触发了封锁，随后
    /// 4 次 `[at] 说句话` 全被吞掉，群友以为被拉黑）。
    #[test]
    fn trusted_scopes_skip_the_per_user_block() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let limits = crate::config::get().traffic().clone();
                let scope = InboundScope::Group {
                    group_id: 9_150_001,
                    user_id: 9_150_002,
                };
                // 全局额度是进程级的，别的测试可能已经把它打满（打满时**所有人**
                // 都会被抑制，这是设计如此）。这里只验按人限流：清掉全局计数，并且
                // **只**删自己这一格——`scopes.clear()` 会顺手删掉并行测试的状态，
                // 让它们的断言凭空失败（踩过一次）。
                {
                    let mut state = TRAFFIC_STATE.lock().await;
                    state.global_recent.clear();
                    state.scopes.remove(&scope);
                }

                // 先用普通身份把额度用满：前 per_user_limit 条放行，再多一条触发封锁。
                for _ in 0..limits.per_user_limit() {
                    assert!(
                        !super::should_suppress(scope, false).await,
                        "额度内不该被抑制"
                    );
                }
                assert!(
                    super::should_suppress(scope, false).await,
                    "超出按人上限后应进入封锁期"
                );

                // 封锁期内：普通身份继续被拒，受信任身份（点名/管理员）放行。
                assert!(super::should_suppress(scope, false).await);
                for _ in 0..8 {
                    assert!(
                        !super::should_suppress(scope, true).await,
                        "受信任的消息不该被按人封锁吞掉"
                    );
                }

                TRAFFIC_STATE.lock().await.scopes.remove(&scope);
            });
    }

    #[test]
    fn data_erasure_clear_removes_only_the_private_traffic_scope() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let private = InboundScope::Private(9_100_001);
                let other = InboundScope::Private(9_100_002);
                {
                    let mut state = TRAFFIC_STATE.lock().await;
                    // `last_seen` 必须是真的时刻：`should_suppress` 每次都会淘汰
                    // `last_seen` 过旧（以及为 None）的 scope，而测试是并行跑的——
                    // 插入 `default()`（last_seen=None）时，别处一次并发调用就能把它
                    // 顺手删掉，这个断言随即凭空失败（踩过一次）。
                    let now = Some(std::time::Instant::now());
                    state.scopes.insert(
                        private,
                        ScopeTraffic {
                            last_seen: now,
                            ..ScopeTraffic::default()
                        },
                    );
                    state.scopes.insert(
                        other,
                        ScopeTraffic {
                            last_seen: now,
                            ..ScopeTraffic::default()
                        },
                    );
                }

                assert!(clear_private_traffic(9_100_001).await);
                let state = TRAFFIC_STATE.lock().await;
                assert!(!state.scopes.contains_key(&private));
                assert!(state.scopes.contains_key(&other));
            });
    }
}
