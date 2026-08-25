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

    #[test]
    fn data_erasure_clear_removes_only_the_private_traffic_scope() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let private = InboundScope::Private(9_100_001);
                let other = InboundScope::Private(9_100_002);
                {
                    let mut state = TRAFFIC_STATE.lock().await;
                    state.scopes.insert(private, ScopeTraffic::default());
                    state.scopes.insert(other, ScopeTraffic::default());
                }

                assert!(clear_private_traffic(9_100_001).await);
                let state = TRAFFIC_STATE.lock().await;
                assert!(!state.scopes.contains_key(&private));
                assert!(state.scopes.contains_key(&other));
            });
    }
}
