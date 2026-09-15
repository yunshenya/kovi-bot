//! waiting room（排队窗口）的运行时状态与"卡住"判定。
//!
//! 为什么需要这个模块：排空只在**回合收尾**时触发（外加 `QueueThenDrain` 残局与
//! 看门狗两层兜底），而 2026-09-15 线上出现过第三种形态——排空任务**拿到了回合却
//! 永久停住**：它持有的票让协调器一直认为"本群有回复在途"，新消息只能继续排队；
//! 看门狗要抢的正是同一张票，抢不到就每 30 秒白扫一次（journal 里只有
//! `waiting room 看门狗超时`）。那个群从 09-14 23:38 到 09-15 10:25 一句话都没回，
//! 而**日志里没有任何一行**能指出卡在哪：排空只在进入 `process_group_reply_claimed`
//! 之后才打点，卡在更早的 await 上时就完全静默。
//!
//! 这个模块只做两件事，都是只读观测：
//! 1. 记住每个会话的排队深度与**最后推进时间**（`drain_active` / `drain_drained`
//!    / `last_progress_at`），卡住时能一眼看出"排空在不在跑、多久没动了"；
//! 2. 与协调器票据、回复状态（[`super::interrupt::scope_reply_snapshot`]）合并成
//!    [`ScopeReport`]，让"僵尸回合"这种形态可判定，而不是靠人肉对日志。
//!
//! 它不改变任何回复行为：没有排队、没有阻塞、没有超时，全部是记账。

use super::interrupt::{ReplyScope, ReplyStateSnapshot};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

/// 一次排空（drain）在某个会话上的活性记账。
#[derive(Debug, Default)]
struct DrainState {
    /// 有没有一个排空任务正持有这个会话的回合。`false` + 队列非空 = 没人管。
    active: bool,
    /// 本轮已经排掉的条数；`> 0` 说明它至少推进过一次。
    drained: usize,
    /// 这一轮是**哪一刻**拿到回合的。只在 `begin_drain` 写一次，不随后续推进刷新，
    /// 所以它回答的是"这个回合已经活了多久"——僵尸回合正是这个数字无限长大。
    turn_started_at: Option<Instant>,
    /// 最后一次推进的时刻（每排掉一条、以及每次开始/收尾都会刷新）。
    last_progress_at: Option<Instant>,
}

/// 会话级的等待房间账本。
#[derive(Debug, Default)]
struct ScopeState {
    queued: usize,
    oldest_enqueued_at: Option<Instant>,
    /// 最老一条的发送者与正文摘要（只在能拿到队列内容时填）。
    oldest: Option<QueueDetail>,
    /// 正在处理的那条（排空领走时填）。
    processing: Option<String>,
    drain: DrainState,
    /// 这个会话上排空任务的世代，每开始一次 +1。
    ///
    /// 必须是**按会话**的，不能是全进程一个计数器：全群共用一个计数器时，
    /// A 群开始排空会把 B 群正在跑的那次判成"过期的旧任务"，B 群的推进与收尾
    /// 全部丢失（活性记账反而变成假的"排空在跑"）。这一点有测试盯着。
    drain_generation: u64,
}

/// 队列里某条消息的摘要：后台是用来看"谁在等"的，不搬运全文。
#[derive(Debug, Clone)]
struct QueueDetail {
    sender: String,
    preview: String,
}

static SCOPE_STATES: OnceLock<StdMutex<Vec<(ReplyScope, ScopeState)>>> = OnceLock::new();

fn states() -> &'static StdMutex<Vec<(ReplyScope, ScopeState)>> {
    SCOPE_STATES.get_or_init(|| StdMutex::new(Vec::new()))
}

/// 队列侧的一瞥：某条队列当前有多长、最老的那条是什么。
///
/// 由 `group.rs` / `private.rs` 在**已经持有队列锁**时组装——它们各自持有那张
/// 表的锁，所以这里不再去碰队列本身，避免两把锁互相等待。正在处理的那条走
/// [`note_processing`]，因为只有排空循环知道它领走了谁。
#[derive(Debug, Clone)]
pub(crate) struct QueueView {
    pub(crate) queued: usize,
    pub(crate) oldest_enqueued_at: Option<Instant>,
}

/// 排空的活性凭据：`begin` 之后由 `note_progress` 推进，随 Drop 无条件收尾。
///
/// 用 Drop 而不是在每个 `return` 前手写收尾：排空循环里有多条提前返回与取消路径
/// （会话被打断、进程收尾、任务被 abort），漏掉任何一条都会在后台留下"排空还在跑"
/// 的假象——那正是这个观测要消灭的东西。
#[derive(Debug)]
pub(crate) struct DrainGuard {
    scope: ReplyScope,
    generation: u64,
}

impl DrainGuard {
    pub(crate) fn begin(scope: ReplyScope) -> Self {
        Self {
            scope,
            generation: begin_drain(scope),
        }
    }

    /// 报告"又排掉了一条"。
    pub(crate) fn note_progress(&self) {
        note_progress(self.scope, self.generation);
    }
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        finish_drain(self.scope, self.generation);
    }
}

/// 记下"正在处理哪一条"，供后台显示。
pub(crate) fn note_processing(scope: ReplyScope, sender: &str, message: &str) {
    let processing = preview(&format!("{sender}: {message}"));
    update(scope, |state| {
        state.processing = processing;
    });
}

/// 排空开始：拿到回合、准备处理队列。返回这一轮的世代号。
pub(crate) fn begin_drain(scope: ReplyScope) -> u64 {
    let now = Instant::now();
    let mut generation = 1;
    update(scope, |state| {
        state.drain_generation = state.drain_generation.wrapping_add(1);
        generation = state.drain_generation;
        state.drain = DrainState {
            active: true,
            drained: 0,
            turn_started_at: Some(now),
            last_progress_at: Some(now),
        };
    });
    generation
}

/// 排空推进了一条。被取代的旧排空返回 `false`，不刷新新排空的活性。
pub(crate) fn note_progress(scope: ReplyScope, generation: u64) -> bool {
    let mut recorded = false;
    update(scope, |state| {
        if state.drain_generation != generation {
            return;
        }
        let drain = &mut state.drain;
        drain.active = true;
        drain.drained = drain.drained.saturating_add(1);
        drain.last_progress_at = Some(Instant::now());
        recorded = true;
    });
    recorded
}

/// 排空收尾（无论排空还是中途返回）。
///
/// **不动 `last_progress_at`**：这个字段是活性证据，收尾时刻会把它抹平成
/// "刚刚还在动"，正好掩盖掉"卡了很久之后才被取消"这件事。被取代的旧排空
/// （世代对不上）什么都不做——它无权清掉新排空的活性。
pub(crate) fn finish_drain(scope: ReplyScope, generation: u64) {
    update(scope, |state| {
        if state.drain_generation == generation {
            state.drain.active = false;
        }
    });
}

/// 队列深度与最老一条的摘要。
///
/// `oldest_enqueued_at` 只由这里维护：队列被丢弃（`stop_group_reply`、数据擦除、
/// 控制命令顶掉在途回复）时也要调用一次，否则"最老一条等了多久"会一直停在上一次。
/// `oldest` 只有能读到队列内容的调用方（快照函数）才给得出，其余调用点传 `None`——
/// 那时只更新条数，不抹掉已经记下的摘要。
pub(crate) fn record_queue(scope: ReplyScope, view: &QueueView, oldest: Option<(&str, &str)>) {
    update(scope, |state| {
        state.queued = view.queued;
        state.oldest_enqueued_at = view.oldest_enqueued_at;
        if let Some((sender, message)) = oldest {
            state.oldest = Some(QueueDetail {
                sender: sender.to_string(),
                preview: preview(message).unwrap_or_default(),
            });
        }
        if view.queued == 0 {
            state.oldest = None;
            state.processing = None;
            state.drain.drained = 0;
        }
    });
}

fn update(scope: ReplyScope, edit: impl FnOnce(&mut ScopeState)) {
    let Ok(mut states) = states().lock() else {
        // 观测路径绝不把聊天拖下水：锁坏了就丢这一次记账。
        return;
    };
    if let Some((_, state)) = states.iter_mut().find(|(known, _)| *known == scope) {
        edit(state);
        return;
    }
    let mut state = ScopeState::default();
    edit(&mut state);
    states.push((scope, state));
}

/// 会话的报账行：队列 + 排空活性 + 协调器票据 + 卡住判定。
#[derive(Debug, Clone)]
pub(crate) struct ScopeReport {
    pub(crate) kind: &'static str,
    pub(crate) subject_id: i64,
    pub(crate) queued: usize,
    pub(crate) oldest_queued_secs: Option<u64>,
    pub(crate) oldest_sender: Option<String>,
    pub(crate) oldest_preview: Option<String>,
    pub(crate) processing: Option<String>,
    pub(crate) drain_active: bool,
    pub(crate) drain_drained: usize,
    pub(crate) drain_last_progress_secs: Option<u64>,
    /// 在途回合的票据。它的代数来自协调器（真正的回合身份），不是排空世代号。
    pub(crate) ticket: Option<TicketView>,
    pub(crate) reply: ReplyStateSnapshot,
    pub(crate) stuck: bool,
    pub(crate) stuck_reason: Option<String>,
    /// 供页面直接显示的一句话（中文，和后台其它文案保持一致）。
    pub(crate) summary: String,
}

/// 当前回合票的年龄：它是"有回复在途"的唯一凭据。
#[derive(Debug, Clone, Copy)]
pub(crate) struct TicketView {
    pub(crate) generation: u64,
    pub(crate) age_secs: u64,
}

/// 队列里那条消息的摘要：后台是用来看"谁在等"的，不搬运全文。
const PREVIEW_CHARS: usize = 48;

fn preview(message: &str) -> Option<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut chars = trimmed.chars();
    let head: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        Some(format!("{head}…"))
    } else {
        Some(head)
    }
}

fn scope_kind(scope: ReplyScope) -> &'static str {
    match scope {
        ReplyScope::Group(_) => "group",
        ReplyScope::Private(_) => "private",
        ReplyScope::Scheduled(_) => "scheduled",
        ReplyScope::Call(_) => "call",
    }
}

fn scope_id(scope: ReplyScope) -> i64 {
    match scope {
        ReplyScope::Group(id)
        | ReplyScope::Private(id)
        | ReplyScope::Scheduled(id)
        | ReplyScope::Call(id) => id,
    }
}

/// 全部会话的报账行，卡住的排在最前面。
///
/// 范围是"等待房间里有队列的会话" ∪ "协调器里有回复状态的会话"——只扫前者会漏掉
/// 最要命的那一类：队列空、但有一个僵尸回合挂着（2026-09-15 那次是排空任务停在
/// 半路，队列里还躺着几条）。只报**有内容**的行：排队非空、或有在途回合、或有已经
/// 生成好却发不出去的回复；彻底空闲的会话不占版面。
pub(crate) async fn report(stuck_after: Duration) -> Vec<ScopeReport> {
    let mut scopes = {
        let Ok(states) = states().lock() else {
            return Vec::new();
        };
        states.iter().map(|(scope, _)| *scope).collect::<Vec<_>>()
    };
    for scope in super::interrupt::known_scopes().await {
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    let mut reports = Vec::with_capacity(scopes.len());
    for scope in scopes {
        let report = scope_report(scope, stuck_after).await;
        let interesting = report.queued > 0
            || report.reply.is_active()
            || report.reply.prepared_outgoing > 0
            || report.reply.pending_incoming > 0
            || report.reply.active_incoming > 0;
        if interesting {
            reports.push(report);
        }
    }
    reports.sort_by(|left, right| {
        right
            .stuck
            .cmp(&left.stuck)
            .then(right.queued.cmp(&left.queued))
            .then(
                right
                    .oldest_queued_secs
                    .unwrap_or_default()
                    .cmp(&left.oldest_queued_secs.unwrap_or_default()),
            )
    });
    reports
}

async fn scope_report(scope: ReplyScope, stuck_after: Duration) -> ScopeReport {
    let record = scope_record(scope);
    let reply = super::interrupt::scope_reply_snapshot(scope).await;
    let now = Instant::now();
    let oldest_queued_secs = record
        .oldest_enqueued_at
        .map(|at| now.saturating_duration_since(at).as_secs());
    let drain_last_progress_secs = record
        .drain
        .last_progress_at
        .map(|at| now.saturating_duration_since(at).as_secs());
    let ticket = record.drain.active.then(|| TicketView {
        generation: reply.generation,
        // 这一轮从拿到回合到现在。卡住时它只会越来越大，`drain_last_progress_secs`
        // 同时停住——两个数字一起看就是"回合活着但人不动了"。
        age_secs: record
            .drain
            .turn_started_at
            .map(|at| now.saturating_duration_since(at).as_secs())
            .unwrap_or_default(),
    });

    let mut stuck_reason = None;
    if record.queued > 0 {
        if !record.drain.active {
            stuck_reason = Some("队列非空但没有排空任务在跑".to_string());
        } else if drain_last_progress_secs.is_some_and(|secs| secs >= stuck_after.as_secs()) {
            stuck_reason = Some(format!(
                "排空任务还在，但已 {} 秒没有推进",
                drain_last_progress_secs.unwrap_or_default()
            ));
        }
    }
    // 第二条独立证据：回复已经生成好（`Prepared`）、却迟迟没提交出去。队列可能
    // 已经空了，这时只有它能指出"回合卡在 commit 之前"——2026-09-15 那次正卡在
    // 这一段，而当时队列里的数字反而让人以为只是"还没轮到"。
    if stuck_reason.is_none() && reply.has_stuck_prepared(stuck_after) {
        stuck_reason = Some(format!(
            "已生成但 {} 秒发不出去的回复有 {} 条",
            reply.oldest_prepared_secs.unwrap_or_default(),
            reply.prepared_outgoing
        ));
    }
    let stuck = stuck_reason.is_some();

    let summary = if stuck {
        format!(
            "⚠ 疑似卡住：排队 {} 条，最老一条已等 {} 秒；{}",
            record.queued,
            oldest_queued_secs.unwrap_or_default(),
            stuck_reason.as_deref().unwrap_or_default()
        )
    } else if record.queued > 0 {
        format!(
            "排队 {} 条（正在排空，{} 秒前推进过）",
            record.queued,
            drain_last_progress_secs.unwrap_or_default()
        )
    } else if reply.is_active() {
        format!(
            "有回复在生成（回合已 {} 秒）",
            ticket.as_ref().map_or(0, |ticket| ticket.age_secs)
        )
    } else {
        "空闲".to_string()
    };

    ScopeReport {
        kind: scope_kind(scope),
        subject_id: scope_id(scope),
        queued: record.queued,
        oldest_queued_secs,
        oldest_sender: record.oldest.as_ref().map(|detail| detail.sender.clone()),
        oldest_preview: record.oldest.as_ref().map(|detail| detail.preview.clone()),
        processing: record.processing,
        drain_active: record.drain.active,
        drain_drained: record.drain.drained,
        drain_last_progress_secs,
        ticket,
        reply,
        stuck,
        stuck_reason,
        summary,
    }
}

/// 一次快照要读的全部记账（含队列内容摘要）。
#[derive(Debug, Default)]
struct ScopeRecord {
    queued: usize,
    oldest_enqueued_at: Option<Instant>,
    oldest: Option<QueueDetail>,
    processing: Option<String>,
    drain: DrainState,
}

fn scope_record(scope: ReplyScope) -> ScopeRecord {
    let Ok(states) = states().lock() else {
        return ScopeRecord::default();
    };
    states
        .iter()
        .find(|(known, _)| *known == scope)
        .map(|(_, state)| ScopeRecord {
            queued: state.queued,
            oldest_enqueued_at: state.oldest_enqueued_at,
            oldest: state.oldest.clone(),
            processing: state.processing.clone(),
            drain: DrainState {
                active: state.drain.active,
                drained: state.drain.drained,
                turn_started_at: state.drain.turn_started_at,
                last_progress_at: state.drain.last_progress_at,
            },
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(id: i64) -> ReplyScope {
        ReplyScope::Group(id)
    }

    fn report_now(scope: ReplyScope, stuck_after_secs: u64) -> ScopeReport {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(scope_report(scope, Duration::from_secs(stuck_after_secs)))
    }

    #[test]
    fn an_empty_waiting_room_is_idle_not_stuck() {
        let report = report_now(scope(9_100_001), 180);
        assert_eq!(report.queued, 0);
        assert!(!report.stuck);
        assert_eq!(report.summary, "空闲");
    }

    #[test]
    fn a_queue_with_no_active_drain_is_stuck_immediately() {
        let scope = scope(9_100_002);
        record_queue(
            scope,
            &QueueView {
                queued: 3,
                oldest_enqueued_at: Some(Instant::now() - Duration::from_secs(600)),
            },
            Some(("小明", "在吗")),
        );
        let report = report_now(scope, 180);
        assert_eq!(report.queued, 3);
        assert!(report.stuck, "队列非空却没人排空必须判成卡住");
        assert_eq!(
            report.stuck_reason.as_deref(),
            Some("队列非空但没有排空任务在跑")
        );
        assert_eq!(report.oldest_sender.as_deref(), Some("小明"));
        assert_eq!(report.oldest_preview.as_deref(), Some("在吗"));
        assert!(
            report.summary.contains("疑似卡住"),
            "摘要要让人一眼看出问题: {}",
            report.summary
        );
    }

    #[test]
    fn a_long_waiting_message_is_previewed_not_dumped() {
        let scope = scope(9_100_006);
        let long = "等".repeat(PREVIEW_CHARS * 3);
        record_queue(
            scope,
            &QueueView {
                queued: 1,
                oldest_enqueued_at: Some(Instant::now()),
            },
            Some(("小明", &long)),
        );
        let preview = report_now(scope, 180).oldest_preview.expect("应当有摘要");
        assert_eq!(
            preview.chars().count(),
            PREVIEW_CHARS + 1,
            "超长正文只留摘要"
        );
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn a_drain_that_keeps_progressing_is_not_stuck() {
        let scope = scope(9_100_003);
        let generation = begin_drain(scope);
        record_queue(
            scope,
            &QueueView {
                queued: 1,
                oldest_enqueued_at: Some(Instant::now() - Duration::from_secs(5)),
            },
            None,
        );
        assert!(note_progress(scope, generation));
        let report = report_now(scope, 180);
        assert!(report.drain_active);
        assert_eq!(report.drain_drained, 1);
        assert!(!report.stuck, "刚推进过的排空不算卡住: {}", report.summary);
        finish_drain(scope, generation);
        assert!(!report_now(scope, 180).drain_active);
    }

    #[test]
    fn a_stale_generation_cannot_touch_the_current_drain() {
        let scope = scope(9_100_004);
        let cancelled = begin_drain(scope);
        let current = begin_drain(scope);
        assert!(
            !note_progress(scope, cancelled),
            "被取代的旧排空不能刷新新排空的活性"
        );
        finish_drain(scope, cancelled);
        let report = report_now(scope, 180);
        assert!(report.drain_active, "旧任务收尾不得清掉新任务的活性");
        assert_eq!(report.drain_drained, 0);
        finish_drain(scope, current);
        assert!(!report_now(scope, 180).drain_active);
    }

    #[test]
    fn dropping_the_queue_resets_the_backlog_but_keeps_the_drain_ledger() {
        let scope = scope(9_100_005);
        let generation = begin_drain(scope);
        record_queue(
            scope,
            &QueueView {
                queued: 2,
                oldest_enqueued_at: Some(Instant::now()),
            },
            Some(("小明", "在吗")),
        );
        assert!(note_progress(scope, generation));
        // 队列被丢弃（数据擦除 / 控制命令顶掉在途回复）：排队数要归零，
        // 否则"最老一条等了多久"会一直停在上一次，看上去像永久卡住。
        record_queue(
            scope,
            &QueueView {
                queued: 0,
                oldest_enqueued_at: None,
            },
            None,
        );
        let report = report_now(scope, 180);
        assert_eq!(report.queued, 0);
        assert_eq!(report.oldest_queued_secs, None);
        assert_eq!(report.oldest_preview, None);
        assert!(!report.stuck);
    }
}
