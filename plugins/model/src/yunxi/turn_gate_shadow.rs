//! TurnGate response head 影子账本 (Phase 3, doc §8.3/§9 Phase 3)。
//!
//! 只做观测,不改变发送路由:批次形成时记录 response head 的"会怎么选",
//! 该批次的真实走向(发了可见消息/保持沉默)到达后在 FIFO 排队配对,
//! 汇总误接话(会答但没发,FP)与漏接话(会沉默但发了,FN)。所有计数有界、
//! 线程安全;bundle 未加载时整个影子不运行,零行为影响。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, OnceLock};
use yunxi_core::{ResponseOutput, TurnResponseDecision, TurnScope};

/// 每 scope 待配对批次上限(超出的旧批次丢弃,只保真实走向统计)。
const MAX_PENDING_PER_SCOPE: usize = 64;
/// 每 N 次真实走向打一次汇总日志。
const SUMMARY_EVERY: u64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateVerdict {
    WouldReply,
    WouldSilent,
    Abstain,
}

impl GateVerdict {
    fn of(output: &ResponseOutput) -> Self {
        match output.decision {
            TurnResponseDecision::Answer
            | TurnResponseDecision::Continue
            | TurnResponseDecision::Ack => Self::WouldReply,
            TurnResponseDecision::Ignore | TurnResponseDecision::Wait => Self::WouldSilent,
            TurnResponseDecision::Abstain => Self::Abstain,
        }
    }
}

#[derive(Debug, Default)]
struct ScopeCounters {
    gate_reply: AtomicU64,
    gate_silent: AtomicU64,
    gate_abstain: AtomicU64,
    actual_replied: AtomicU64,
    actual_silent: AtomicU64,
    /// 影子说 answer/continue/ack,实际保持沉默(误接话候选)。
    would_reply_but_silent: AtomicU64,
    /// 影子说 ignore/wait,实际回复了(漏接话候选)。
    would_silent_but_replied: AtomicU64,
    agreed: AtomicU64,
}

fn counters() -> &'static StdMutex<HashMap<TurnScope, ScopeCounters>> {
    static COUNTERS: OnceLock<StdMutex<HashMap<TurnScope, ScopeCounters>>> = OnceLock::new();
    COUNTERS.get_or_init(|| {
        StdMutex::new(
            [TurnScope::Private, TurnScope::Group]
                .map(|scope| (scope, ScopeCounters::default()))
                .into_iter()
                .collect(),
        )
    })
}

fn pending() -> &'static StdMutex<HashMap<TurnScope, VecDeque<GateVerdict>>> {
    static PENDING: OnceLock<StdMutex<HashMap<TurnScope, VecDeque<GateVerdict>>>> = OnceLock::new();
    PENDING.get_or_init(|| {
        StdMutex::new(
            [TurnScope::Private, TurnScope::Group]
                .map(|scope| (scope, VecDeque::new()))
                .into_iter()
                .collect(),
        )
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ShadowSummary {
    pub(crate) gate_reply: u64,
    pub(crate) gate_silent: u64,
    pub(crate) gate_abstain: u64,
    pub(crate) actual_replied: u64,
    pub(crate) actual_silent: u64,
    pub(crate) would_reply_but_silent: u64,
    pub(crate) would_silent_but_replied: u64,
    pub(crate) agreed: u64,
}

pub(crate) fn summary(scope: TurnScope) -> ShadowSummary {
    let Ok(counters) = counters().lock() else {
        return ShadowSummary::default();
    };
    let Some(counters) = counters.get(&scope) else {
        return ShadowSummary::default();
    };
    ShadowSummary {
        gate_reply: counters.gate_reply.load(Ordering::Relaxed),
        gate_silent: counters.gate_silent.load(Ordering::Relaxed),
        gate_abstain: counters.gate_abstain.load(Ordering::Relaxed),
        actual_replied: counters.actual_replied.load(Ordering::Relaxed),
        actual_silent: counters.actual_silent.load(Ordering::Relaxed),
        would_reply_but_silent: counters.would_reply_but_silent.load(Ordering::Relaxed),
        would_silent_but_replied: counters.would_silent_but_replied.load(Ordering::Relaxed),
        agreed: counters.agreed.load(Ordering::Relaxed),
    }
}

/// 批次形成:记录 response head 的"会怎么选",进入配对队列。
pub(crate) fn record_batch(scope: TurnScope, output: &ResponseOutput) {
    let verdict = GateVerdict::of(output);
    if let Ok(counters) = counters().lock()
        && let Some(counters) = counters.get(&scope)
    {
        match verdict {
            GateVerdict::WouldReply => counters.gate_reply.fetch_add(1, Ordering::Relaxed),
            GateVerdict::WouldSilent => counters.gate_silent.fetch_add(1, Ordering::Relaxed),
            GateVerdict::Abstain => counters.gate_abstain.fetch_add(1, Ordering::Relaxed),
        };
    }
    if let Ok(mut pending) = pending().lock()
        && let Some(queue) = pending.get_mut(&scope)
    {
        if queue.len() >= MAX_PENDING_PER_SCOPE {
            queue.pop_front();
        }
        queue.push_back(verdict);
    }
    kovi::log::debug!(
        "Yunxi TurnGate shadow: scope={:?} response={:?} confidence={:.3}",
        scope,
        output.decision,
        output.confidence,
    );
}

/// 真实走向到达:与最旧未配对批次的网关判断做 FP/FN 配对。
pub(crate) fn record_outcome(scope: TurnScope, replied: bool) {
    let verdict = pending()
        .lock()
        .ok()
        .and_then(|mut pending| pending.get_mut(&scope).and_then(VecDeque::pop_front));
    if let Ok(counters) = counters().lock()
        && let Some(counters) = counters.get(&scope)
    {
        if replied {
            counters.actual_replied.fetch_add(1, Ordering::Relaxed);
        } else {
            counters.actual_silent.fetch_add(1, Ordering::Relaxed);
        }
        // abstain 不参与误接/漏接判定(无决策立场),只算一致观测。
        let (fp, fn_, agree) = match (verdict, replied) {
            (Some(GateVerdict::WouldReply), false) => (1, 0, 0),
            (Some(GateVerdict::WouldSilent), true) => (0, 1, 0),
            (Some(_), _) => (0, 0, 1),
            (None, _) => (0, 0, 0),
        };
        counters
            .would_reply_but_silent
            .fetch_add(fp, Ordering::Relaxed);
        counters
            .would_silent_but_replied
            .fetch_add(fn_, Ordering::Relaxed);
        counters.agreed.fetch_add(agree, Ordering::Relaxed);
    }
    maybe_log_summary(scope);
}

fn maybe_log_summary(scope: TurnScope) {
    let summary = summary(scope);
    let total = summary.actual_replied.saturating_add(summary.actual_silent);
    if total > 0 && total.is_multiple_of(SUMMARY_EVERY) {
        kovi::log::info!(
            "[TURNGATE-SHADOW] scope={:?} gate_reply={} gate_silent={} gate_abstain={} actual_replied={} actual_silent={} fp_would_reply_but_silent={} fn_would_silent_but_replied={} agreed={}",
            scope,
            summary.gate_reply,
            summary.gate_silent,
            summary.gate_abstain,
            summary.actual_replied,
            summary.actual_silent,
            summary.would_reply_but_silent,
            summary.would_silent_but_replied,
            summary.agreed,
        );
    }
}

/// Drop 时自动记录真实走向的守卫:批次形成后创建,回复分支标记 replied,
/// 其余退出路径(沉默)由 Drop 默认记录。
pub(crate) struct OutcomeGuard {
    scope: TurnScope,
    replied: bool,
}

impl OutcomeGuard {
    pub(crate) fn new(scope: TurnScope) -> Self {
        Self {
            scope,
            replied: false,
        }
    }

    pub(crate) fn mark_replied(&mut self, replied: bool) {
        self.replied = replied;
    }
}

impl Drop for OutcomeGuard {
    fn drop(&mut self) {
        record_outcome(self.scope, self.replied);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(decision: TurnResponseDecision) -> ResponseOutput {
        ResponseOutput {
            decision,
            confidence: 0.9,
        }
    }

    #[test]
    fn fp_fp_pairing_counts_correctly() {
        let scope = TurnScope::Group;
        let before = summary(scope);
        record_batch(scope, &output(TurnResponseDecision::Answer));
        record_outcome(scope, false); // 会答但沉默 → FP
        record_batch(scope, &output(TurnResponseDecision::Ignore));
        record_outcome(scope, true); // 会沉默但发了 → FN
        record_batch(scope, &output(TurnResponseDecision::Ack));
        record_outcome(scope, true); // 一致
        record_outcome(scope, false); // 无配对:只计实际走向
        let after = summary(scope);
        assert_eq!(
            after.would_reply_but_silent - before.would_reply_but_silent,
            1
        );
        assert_eq!(
            after.would_silent_but_replied - before.would_silent_but_replied,
            1
        );
        assert_eq!(after.agreed - before.agreed, 1);
        assert_eq!(after.actual_silent - before.actual_silent, 2);
        assert_eq!(after.actual_replied - before.actual_replied, 2);
    }

    #[test]
    fn guard_records_silent_by_default() {
        let scope = TurnScope::Private;
        let before = summary(scope);
        {
            let _guard = OutcomeGuard::new(scope);
        }
        assert_eq!(summary(scope).actual_silent - before.actual_silent, 1);
    }
}
