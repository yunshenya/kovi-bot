//! 群消息发送被 QQ 拒绝时的有界退避（禁言/风控降级）。
//!
//! 现象:群被禁言后 `sendMsg` 直接返回 `status=failed retcode=1200`，而
//! 发送方仍把它当作可重试（retryable）错误,导致"一边被禁言一边反复尝试
//! 发送"——白白烧模型成本、刷日志、还可能触发风控。
//!
//! 策略(doc §3 硬规则优先):
//! - 记录真实被拒(非 indeterminate)的群发送失败,指数退避 30s→1h;
//! - 退避期内该群的可见发送直接跳过(仅记账日志),自然放行探测:到期后
//!   下一条消息会真的尝试,成功即清除退避;
//! - 退避状态与 `#禁言` 的显式暂停合并 → `is_group_paused` 对非管理员
//!   回复与 Core 发送同时生效;管理员命令(`#结束禁言` 等)不受影响。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

/// 首次退避 30s,指数翻倍,上限 1h。
const SEND_DENIED_BASE_BACKOFF: Duration = Duration::from_secs(30);
const SEND_DENIED_MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// 记录的群上限(有界)。
const MAX_TRACKED_GROUPS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct GroupSendState {
    failures: u32,
    blocked_until: Option<Instant>,
}

impl GroupSendState {
    fn remaining(&self) -> Option<Duration> {
        self.blocked_until.and_then(|until| {
            until
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
        })
    }
}

fn backoff_for(failures: u32) -> Duration {
    let seconds = SEND_DENIED_BASE_BACKOFF
        .as_secs()
        .saturating_mul(1u64 << failures.min(7));
    Duration::from_secs(seconds.min(SEND_DENIED_MAX_BACKOFF.as_secs()))
}

#[derive(Debug, Default)]
pub(crate) struct GroupSendGuard {
    entries: HashMap<i64, GroupSendState>,
    order: VecDeque<i64>,
}

impl GroupSendGuard {
    pub(crate) fn record_rejection(&mut self, group_id: i64) {
        let now = Instant::now();
        let state = self.entry(group_id);
        state.failures = state.failures.saturating_add(1);
        state.blocked_until = Some(now + backoff_for(state.failures));
    }

    pub(crate) fn record_success(&mut self, group_id: i64) {
        if let Some(state) = self.entries.get_mut(&group_id) {
            state.failures = 0;
            state.blocked_until = None;
        }
    }

    /// 退避是否还在生效(内层;`is_group_paused` 会与显式暂停合并)。
    pub(crate) fn rejection_remaining(&self, group_id: i64) -> Option<Duration> {
        self.entries
            .get(&group_id)
            .and_then(GroupSendState::remaining)
    }

    fn entry(&mut self, group_id: i64) -> &mut GroupSendState {
        if !self.entries.contains_key(&group_id) {
            if self.entries.len() >= MAX_TRACKED_GROUPS
                && let Some(evicted) = self.order.pop_front()
            {
                self.entries.remove(&evicted);
            }
            self.order.push_back(group_id);
        }
        self.entries.entry(group_id).or_default()
    }
}

static GROUP_GUARD: OnceLock<StdMutex<GroupSendGuard>> = OnceLock::new();

fn guard() -> &'static StdMutex<GroupSendGuard> {
    GROUP_GUARD.get_or_init(|| StdMutex::new(GroupSendGuard::default()))
}

/// 记录一次真实的群发送被拒(status failed / retcode 1200 等),进入/延长
/// 退避。幂等,可多次调用。
pub(crate) async fn record_rejection(group_id: i64) {
    if let Ok(mut guard) = guard().lock() {
        guard.record_rejection(group_id);
    }
}

/// 发送成功后清除退避(自然放行探测)。
pub(crate) async fn record_success(group_id: i64) {
    if let Ok(mut guard) = guard().lock() {
        guard.record_success(group_id);
    }
}

/// 该群是否处于发送被拒退避中。
pub(crate) async fn is_send_rejected(group_id: i64) -> bool {
    guard()
        .lock()
        .map(|guard| guard.rejection_remaining(group_id).is_some())
        .unwrap_or(false)
}

/// 剩余退避时间(未退避/已到期返回 None)。仅测试/诊断使用;生产通过
/// `is_send_rejected` 参与 `is_group_paused` 融合。
#[cfg(test)]
pub(crate) async fn rejection_remaining(group_id: i64) -> Option<Duration> {
    guard()
        .lock()
        .ok()
        .and_then(|guard| guard.rejection_remaining(group_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_for(0), Duration::from_secs(30));
        assert_eq!(backoff_for(1), Duration::from_secs(60));
        assert_eq!(backoff_for(2), Duration::from_secs(120));
        assert_eq!(backoff_for(8), Duration::from_secs(3600));
    }

    #[test]
    fn success_clears_blocked_window() {
        let mut guard = GroupSendGuard::default();
        guard.record_rejection(42);
        guard.record_rejection(42);
        let remaining = guard.rejection_remaining(42).expect("blocked");
        assert!((Duration::from_secs(119)..=Duration::from_secs(120)).contains(&remaining));
        guard.record_success(42);
        assert_eq!(guard.rejection_remaining(42), None);
        assert!(guard.rejection_remaining(43).is_none());
    }

    #[test]
    fn is_send_rejected_after_rejection_and_cleared_by_success() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                assert!(!is_send_rejected(7).await);
                record_rejection(7).await;
                assert!(is_send_rejected(7).await);
                assert!(rejection_remaining(7).await.is_some());
                record_success(7).await;
                assert!(!is_send_rejected(7).await);
            })
    }

    #[test]
    fn entries_stay_bounded() {
        let mut guard = GroupSendGuard::default();
        for group_id in 0..(MAX_TRACKED_GROUPS as i64 + 16) {
            guard.record_rejection(group_id);
        }
        assert!(guard.entries.len() <= MAX_TRACKED_GROUPS);
        assert!(guard.rejection_remaining(0).is_none());
        assert!(
            guard
                .rejection_remaining(MAX_TRACKED_GROUPS as i64)
                .is_some()
        );
    }
}
