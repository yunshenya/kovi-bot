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
//! 这个模块只做三件事，都是只读观测：
//! 1. 记住每个会话的排队深度与**最后推进时间**（`drain_active` / `drain_drained`
//!    / `last_progress_at`），卡住时能一眼看出"排空在不在跑、多久没动了"；
//! 2. 与协调器票据、回复状态（[`super::interrupt::scope_reply_snapshot`]）合并成
//!    [`ScopeReport`]，让"僵尸回合"这种形态可判定，而不是靠人肉对日志；
//! 3. **回合级步骤台账与影子判定**（[`TurnWatch`] / [`scan_shadow`]）：在途回合
//!    走到哪一步、停在这一步多久，超过阈值只打 `[STALL] shadow=true … would_reclaim`
//!    日志，**不动任何东西**。这是给"自动回收"（阶段 B 的第三档）攒判据的：
//!    先量出正常回合最长多久、卡住的都停在哪一步，再决定阈值，而不是反过来。
//!
//! 它不改变任何回复行为：没有排队、没有阻塞、没有超时、没有回收，全部是记账。

use super::interrupt::{ReplyScope, ReplyStateSnapshot};
use std::sync::atomic::{AtomicU64, Ordering};
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

// ------------------------------------------------------------------ 回合步骤台账

/// 一个回复回合的步骤。顺序即正常推进顺序，卡住时"停在哪一步"就是最有用的那条信息。
///
/// 线上 2026-09-15 那次排查最大的困难不是"卡了多久"，而是"卡在哪"——排空只在进入
/// `process_group_reply_claimed` 之后才打点，卡在更早的 await 上时日志里一个字都没有。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnStep {
    /// 排空领走了这一条，还没建回合。
    Claimed,
    /// 建回合窗口（会话状态、接话协调）。
    BeginTurn,
    /// 情绪、对话记录、上下文记忆、历史。
    Memory,
    /// 需要时压缩滚动摘要（会调模型）。
    Compress,
    /// 主模型回合（工具链在这个调用内部跑）。
    Model,
    /// 解析/修复成可发送的回复计划。
    Compose,
    /// 生成回复里的出站提交（prepare/commit）。
    Commit,
    /// 交给 QQ 发送。
    Send,
    /// 收尾（记焦点、状态回收）。
    Finish,
}

impl TurnStep {
    fn label(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::BeginTurn => "begin_turn",
            Self::Memory => "memory",
            Self::Compress => "compress",
            Self::Model => "model",
            Self::Compose => "compose",
            Self::Commit => "commit",
            Self::Send => "send",
            Self::Finish => "finish",
        }
    }
}

/// 在途回合的观测记录。
#[derive(Debug, Clone)]
struct TurnObservation {
    scope: ReplyScope,
    step: TurnStep,
    step_at: Instant,
    started_at: Instant,
    /// 已经为这个回合打过一次 `[STALL]`（超时才再打一次，避免每 30 秒刷屏）。
    notified: bool,
}

/// 台账里的一条：状态是**共享**的，因为写的人（链上任何一层）和读的人（后台扫描、
/// 影子判定）在不同的任务里，同一个 `TurnWatch` 的克隆要看到同一份。
#[derive(Debug, Clone)]
struct ObservedTurn {
    id: u64,
    state: std::sync::Arc<StdMutex<TurnObservation>>,
}

static TURN_OBSERVATIONS: OnceLock<StdMutex<Vec<ObservedTurn>>> = OnceLock::new();
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);

fn observations() -> &'static StdMutex<Vec<ObservedTurn>> {
    TURN_OBSERVATIONS.get_or_init(|| StdMutex::new(Vec::new()))
}

/// 观测记录的保质期：超过它直接删。这是"观测表不许无界增长"的硬兜底——
/// `TurnWatch` 的 Drop 正常会清掉，但任务被 abort 时 Drop 未必跑得到。
const OBSERVATION_TTL: Duration = Duration::from_secs(6 * 3600);

/// 交给异步任务的那份句柄。
///
/// 记录是**强引用**——任务活多久，那条记录就活多久，中间谁析构都不影响它。
#[derive(Debug, Clone)]
struct ActiveTurn {
    state: std::sync::Arc<StdMutex<TurnObservation>>,
}

kovi::tokio::task_local! {
    /// 当前异步任务正在处理的回合。
    ///
    /// 用 task-local 而不是给每个函数加参数：群聊回复这条链有十来层调用，逐个透传
    /// 既容易漏、又会把签名全改一遍（`llm_trace` 的用途标签是同一个理由）。它跨
    /// `spawn` 会丢，所以每一处 `spawn` 必须显式带（见 [`TurnWatch::enter`]）。
    static TURN_OBSERVATION: ActiveTurn;
}

/// 一个在途回合的观测凭据：谁在处理这个回合，谁就用它把"做到哪一步"写进台账。
///
/// 生命周期上只认**强引用**：`observe` 造的立案句柄与 [`TurnWatch::enter`] 交给
/// 异步任务的句柄各持一份强引用，最后一个析构时（正常收尾、取消、panic 都会走到）
/// 记录才从台账里摘掉。克隆出来随手传的短命句柄拿的是弱引用——它们析构不该把台账
/// 里那条删掉，否则"谁最后走谁负责收尾"会变成"谁先走谁把记录清了"。
#[derive(Debug, Clone)]
pub(crate) struct TurnWatch {
    id: u64,
    /// 从台账里找到这一条用的句柄。弱引用：析构时不代表回合结束。
    record: std::sync::Weak<StdMutex<TurnObservation>>,
    /// 只有立案句柄与 `enter` 交给任务的那份才是强引用。
    owner: Option<std::sync::Arc<StdMutex<TurnObservation>>>,
}

impl TurnWatch {
    /// 为一个正在处理的回合立案。
    ///
    /// `processing` 是"发送者: 正文"的摘要，顺手记进会话账本供后台显示。
    /// 排队等了多久不在这里记——那是队列账本的 `oldest_enqueued_at`（折队之后
    /// "这一轮什么时候开始干"与"用户等了多久"是两件事，后台分别显示）。
    pub(crate) fn observe(scope: ReplyScope, processing: Option<&str>) -> Self {
        let id = NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let state = std::sync::Arc::new(StdMutex::new(TurnObservation {
            scope,
            step: TurnStep::Claimed,
            step_at: now,
            started_at: now,
            notified: false,
        }));
        if let Ok(mut observations) = observations().lock() {
            prune_observations(&mut observations, now);
            observations.push(ObservedTurn {
                id,
                state: std::sync::Arc::clone(&state),
            });
        }
        // 顺手记下"正在处理哪条"，后台不必再单独问一次。
        if let Some(processing) = processing {
            note_processing(scope, processing);
        }
        Self {
            id,
            record: std::sync::Arc::downgrade(&state),
            owner: Some(state),
        }
    }

    /// 台账里的那一条（把自己的持强引用丢给异步任务时用）。
    fn record(&self) -> Option<std::sync::Arc<StdMutex<TurnObservation>>> {
        self.record.upgrade()
    }

    /// 在后续的 await 上继续带着这个回合的观测。
    ///
    /// 值跨 `spawn` 会丢，所以**任何把回合工作丢进新任务的地方都必须显式包一层**
    /// （群里原本用 spawn 跑的那几处就是这么处理的）；漏了不会出错，只是这一段
    /// 不打点——观测是尽力而为，绝不能因为它失败而影响回复。
    pub(crate) async fn enter<F: std::future::Future>(&self, future: F) -> F::Output {
        // 把强引用挂进 task-local：异步任务活多久，这条记录就活多久。
        match self.record() {
            Some(state) => TURN_OBSERVATION.scope(ActiveTurn { state }, future).await,
            // 记录已经没了（不该发生）：照常跑，只是这一段不打点。
            None => future.await,
        }
    }

    /// 记录"现在做到哪一步了"。
    pub(crate) fn step(step: TurnStep) {
        let Ok(active) = TURN_OBSERVATION.try_with(|active| active.state.clone()) else {
            // 没有回合在处理（后台任务、命令回执、拨测）：不打点也不算错。
            return;
        };
        step_of(&active, step);
    }

    /// 这个回合当前停在哪一步、停了多久——供测试与诊断直接问，不必走后台接口。
    #[cfg(test)]
    pub(crate) fn current(&self) -> (TurnStep, Duration) {
        let Some(record) = self.record() else {
            return (TurnStep::Claimed, Duration::ZERO);
        };
        let Ok(state) = record.lock() else {
            return (TurnStep::Claimed, Duration::ZERO);
        };
        (
            state.step,
            Instant::now().saturating_duration_since(state.step_at),
        )
    }

    /// 测试专用：伪造"某一步已经停了这么久"，用来验证影子判定与阈值——
    /// `Instant` 不能凭空构造，只有记账本身能往前挪。
    #[cfg(test)]
    pub(crate) fn backdate_for_test(&self, stalled: Duration) {
        let Some(record) = self.record() else {
            return;
        };
        if let Ok(mut state) = record.lock() {
            state.step_at = Instant::now() - stalled;
            state.started_at = Instant::now() - stalled;
        }
    }
}

impl Drop for TurnWatch {
    fn drop(&mut self) {
        // 只有最后一个持强引用的句柄负责收尾：异步任务还拿着记录时（强引用不止一份），
        // 谁先析构都不该把台账里那条摘掉。
        // 注意：task-local 里那份强引用要等任务结束才释放，所以**不能**用
        // `strong_count == 1` 判断"回合结束"——它会让台账永远清不掉。
        let Some(owner) = self.owner.take() else {
            return;
        };
        if std::sync::Arc::strong_count(&owner) > 2 {
            return;
        }
        let Ok(mut observations) = observations().lock() else {
            return;
        };
        observations.retain(|entry| entry.id != self.id);
    }
}

fn step_of(state: &StdMutex<TurnObservation>, step: TurnStep) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    state.step = step;
    state.step_at = Instant::now();
}

fn prune_observations(observations: &mut Vec<ObservedTurn>, now: Instant) {
    observations.retain(|entry| {
        entry
            .state
            .lock()
            .map(|state| now.saturating_duration_since(state.started_at) < OBSERVATION_TTL)
            // 锁坏了（不该发生）就留着，宁可多一条也不要误删正在跑的回合。
            .unwrap_or(true)
    });
}

fn observation_of(scope: ReplyScope) -> Option<(u64, TurnObservation)> {
    let observations = observations().lock().ok()?;
    observations
        .iter()
        .filter_map(|entry| {
            let state = entry.state.lock().ok()?;
            (state.scope == scope).then(|| (entry.id, state.clone()))
        })
        .max_by_key(|(_, state)| state.started_at)
}

// ------------------------------------------------------------------ 影子判定

/// 影子判定要看的三个数字。抽出来是为了能**直接测判定规则本身**——
/// `Instant` 改不动，但"什么样的数字组合算卡住、算该回收"是纯逻辑，必须被测试钉住。
#[derive(Debug, Clone, Copy)]
struct StallEvidence {
    /// 排空任务超过多久没推进（`None` = 没有在跑的排空）。
    drain_stalled_secs: Option<u64>,
    /// 当前步骤停了多少秒（`None` = 没有在途回合）。
    step_stalled_secs: Option<u64>,
    /// 这个回合总共等了多久（含排队）。
    turn_waited_secs: Option<u64>,
}

/// 是不是"卡住了"：排空 或 回合步骤，任一超过阈值。
fn is_stalled(evidence: &StallEvidence, stall_secs: u64) -> bool {
    evidence
        .drain_stalled_secs
        .into_iter()
        .chain(evidence.step_stalled_secs)
        .any(|secs| secs >= stall_secs)
}

/// 是不是"该回收了"。比"卡住"更严：既要卡得足够久，也要这个回合**真的等了足够久**
/// ——刚接手几秒的新回合不该因为上一轮留下的陈旧时间戳被判死。
fn should_reclaim(evidence: &StallEvidence, stall_secs: u64, reclaim_secs: u64) -> bool {
    is_stalled(evidence, stall_secs)
        && evidence
            .turn_waited_secs
            .is_some_and(|secs| secs >= reclaim_secs)
}

/// 影子档：把"卡住的回合"写成一行日志，**不做任何回收动作**。
///
/// 这是阶段 B 的第一档：它的产物不是行为，而是判据——正常回合最长多久、卡住的都
/// 停在哪一步、`would_reclaim` 会不会误报。有了这些数据才谈得上把 [`should_reclaim`]
/// 接上真正的动作。开关是 `traffic.turn_stall_secs`，写成 0 即关闭。
pub(crate) async fn scan_shadow(stall_after: Duration, reclaim_after: Duration) {
    for report in report(stall_after).await {
        if let Some(line) = shadow_line(&report, stall_after, reclaim_after) {
            println!("{line}");
        }
    }
}

/// 一个会话这一轮该不该报、报什么。返回 `None` 表示"正常，不用报"。
///
/// 与实际打日志共用同一条路径（[`scan_shadow`] 只负责 `println!`），所以测试钉住的
/// 就是真的会写进 journal 的那一行。
fn shadow_line(
    report: &ScopeReport,
    stall_after: Duration,
    reclaim_after: Duration,
) -> Option<String> {
    let evidence = StallEvidence {
        drain_stalled_secs: report
            .drain_active
            .then_some(report.drain_last_progress_secs)
            .flatten(),
        step_stalled_secs: report.turn_step_secs,
        turn_waited_secs: report.turn_waiting_secs,
    };
    // 队列非空但没人排空是另一种形态：那个回合已经没了，新消息只会一直排队。
    let drain_missing = report.queued > 0 && !report.drain_active;
    let reclaim =
        drain_missing || should_reclaim(&evidence, stall_after.as_secs(), reclaim_after.as_secs());
    if !report.stuck && !reclaim {
        return None;
    }
    // 判据真正用的那个静默时长：排空没推进、或回合停在同一步，取更大的那个。
    let stalled_secs = evidence
        .drain_stalled_secs
        .into_iter()
        .chain(evidence.step_stalled_secs)
        .max()
        .unwrap_or_default();
    // 同一个回合每超时一轮才报一次：卡住时每 30 秒扫一遍，不能每遍都刷同一行。
    if let Some(id) = report.turn_observation {
        let freshly_stalled = report
            .drain_last_progress_secs
            .into_iter()
            .chain(report.turn_step_secs)
            .all(|secs| secs < stall_after.as_secs().saturating_mul(2));
        if mark_notified(id) && !freshly_stalled {
            return None;
        }
    }
    Some(format!(
        // 字段名跟设计稿对齐：`group=…` / `step=…` / `stalled=…s` 是当初说好的那三个
        // （`stalled` 取排空与步骤两个静默时长里更大的那个，也就是判据真正用的那个数）。
        // 其余字段是为了出事时不必再登机器：谁在等、队列多长、排空还活着没有。
        "[STALL] shadow=true group={} step={} stalled={}s waited={}s queue={} oldest_queued={}s \
         drain_active={} drain_progress={}s would_reclaim={} reason={}",
        report.subject_id,
        report.turn_step.unwrap_or("none"),
        stalled_secs,
        report.turn_waiting_secs.unwrap_or_default(),
        report.queued,
        report.oldest_queued_secs.unwrap_or_default(),
        report.drain_active,
        report.drain_last_progress_secs.unwrap_or_default(),
        reclaim,
        report.stuck_reason.as_deref().unwrap_or(""),
    ))
}

/// 标记"这个回合已经报过一次"，返回它此前是否已报过。
fn mark_notified(id: u64) -> bool {
    let Ok(observations) = observations().lock() else {
        return true;
    };
    let Some(entry) = observations.iter().find(|entry| entry.id == id) else {
        return true;
    };
    let Ok(mut state) = entry.state.lock() else {
        return true;
    };
    let already = state.notified;
    state.notified = true;
    already
}

/// 记下"正在处理哪一条"，供后台显示。传的是拼好的"发送者: 正文"，这里只做截断。
pub(crate) fn note_processing(scope: ReplyScope, processing: &str) {
    let processing = preview(processing);
    update(scope, |state| {
        state.processing = processing;
    });
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
    /// 在途回合卡在哪一步（见 [`TurnStep`]），以及停在这一步多久了。
    pub(crate) turn_step: Option<&'static str>,
    pub(crate) turn_step_secs: Option<u64>,
    /// 这一轮从拿到回合到现在（含排队等待）。
    pub(crate) turn_waiting_secs: Option<u64>,
    /// 在途回合的观测 id（影子判定用它做"报过没报过"的去重）。
    pub(crate) turn_observation: Option<u64>,
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

    // 回合级步骤台账：卡住时"停在哪一步"是最有用的那条信息，比"卡了多久"更值钱。
    let observed = observation_of(scope);
    let turn_step = observed.as_ref().map(|(_, entry)| entry.step.label());
    let turn_step_secs = observed
        .as_ref()
        .map(|(_, entry)| now.saturating_duration_since(entry.step_at).as_secs());
    let turn_waiting_secs = observed
        .as_ref()
        .map(|(_, entry)| now.saturating_duration_since(entry.started_at).as_secs());
    let turn_observation = observed.as_ref().map(|(id, _)| *id);

    let mut stuck_reason = None;
    if record.queued > 0 {
        if !record.drain.active {
            stuck_reason = Some("队列非空但没有排空任务在跑".to_string());
        } else if drain_last_progress_secs.is_some_and(|secs| secs >= stuck_after.as_secs()) {
            stuck_reason = Some(format!(
                "排空任务还在，但已 {} 秒没有推进（卡在 {}）",
                drain_last_progress_secs.unwrap_or_default(),
                turn_step.unwrap_or("unknown")
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
        turn_step,
        turn_step_secs,
        turn_waiting_secs,
        turn_observation,
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

    // ---------------------------------------------------------- 影子档与步骤台账

    /// 判据本身必须被钉住：什么样的数字组合算卡住、什么才算该回收。
    /// 时间戳在测试里改不动，但阈值逻辑是纯函数，正是最该测的那部分。
    #[test]
    fn stall_and_reclaim_thresholds_are_ordered_and_gated() {
        let quiet = StallEvidence {
            drain_stalled_secs: Some(10),
            step_stalled_secs: Some(10),
            turn_waited_secs: Some(10),
        };
        assert!(!is_stalled(&quiet, 300));
        assert!(!should_reclaim(&quiet, 300, 600));

        // 排空停住：够久才算卡住，够久**且**这一轮确实等了够久才算该回收。
        let stalled = StallEvidence {
            drain_stalled_secs: Some(301),
            step_stalled_secs: Some(301),
            turn_waited_secs: Some(301),
        };
        assert!(is_stalled(&stalled, 300));
        assert!(
            !should_reclaim(&stalled, 300, 600),
            "刚接手几分钟的新回合不该被判死"
        );

        let reclaimable = StallEvidence {
            turn_waited_secs: Some(601),
            ..stalled
        };
        assert!(should_reclaim(&reclaimable, 300, 600));

        // 没有任何在途证据（队列空、也没回合）：既不卡住也不该回收。
        let empty = StallEvidence {
            drain_stalled_secs: None,
            step_stalled_secs: None,
            turn_waited_secs: None,
        };
        assert!(!is_stalled(&empty, 1));
        assert!(!should_reclaim(&empty, 1, 1));
    }

    /// 影子档那行日志：必须带步骤、时长与 `would_reclaim`，且正常回合一个字都不打。
    #[test]
    fn shadow_line_names_the_step_and_would_reclaim() {
        let scope = scope(9_100_012);
        // 没有任何台账、也没有队列：正常空闲，不该打日志。
        assert!(
            shadow_line(
                &report_now(scope, 300),
                Duration::from_secs(300),
                Duration::from_secs(600)
            )
            .is_none(),
            "空闲会话不该产生 [STALL]"
        );

        let watch = TurnWatch::observe(scope, Some("小明: 在吗"));
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(watch.enter(async {
                TurnWatch::step(TurnStep::Send);
            }));
        watch.backdate_for_test(Duration::from_secs(900));
        let line = shadow_line(
            &report_now(scope, 300),
            Duration::from_secs(300),
            Duration::from_secs(600),
        )
        .expect("停在这一步 900 秒必须报出来");
        assert!(line.contains("shadow=true"), "{line}");
        assert!(
            line.contains(&format!("group={}", scope_id(scope))),
            "要指名道姓说是哪个会话: {line}"
        );
        assert!(line.contains("step=send"), "要指名道姓说卡在哪一步: {line}");
        assert!(line.contains("would_reclaim=true"), "{line}");
        assert!(
            line.contains("stalled=900s") || line.contains("stalled=901s"),
            "要带上停了多久: {line}"
        );

        // 同一个回合下一轮扫描不该重复刷屏（阈值还没翻倍）。
        assert!(
            shadow_line(
                &report_now(scope, 300),
                Duration::from_secs(300),
                Duration::from_secs(600)
            )
            .is_none(),
            "报过一次之后不该每 30 秒刷同一行"
        );
    }

    /// 步骤台账：`TurnWatch` 记的是"最后一步做完到哪了"，Drop 之后必须查无此回合。
    #[test]
    fn turn_watch_records_steps_and_cleans_up() {
        let scope = scope(9_100_010);
        let watch = TurnWatch::observe(scope, Some("小明: 在吗"));
        let (step, _) = watch.current();
        assert_eq!(step, TurnStep::Claimed, "刚立案时是 Claimed");

        {
            // 克隆交给异步任务：它在 `enter` 期间持强引用，所以这一轮结束前
            // 台账里的记录不该因为别的句柄析构而消失。
            let entered = watch.clone();
            kovi::tokio::runtime::Runtime::new()
                .expect("应创建测试运行时")
                .block_on(entered.enter(async {
                    TurnWatch::step(TurnStep::Model);
                }));
        }
        let (step, _) = watch.current();
        assert_eq!(
            step,
            TurnStep::Model,
            "同一个回合的克隆要看到同一份状态（跨任务传播靠共享状态，不靠 task-local 复制）"
        );
        assert!(observation_of(scope).is_some(), "在途回合应当还在台账里");

        drop(watch);
        assert!(
            observation_of(scope).is_none(),
            "回合结束（Drop）之后台账里不该再留着它"
        );
    }

    /// 台账 → 报账行：停在哪一步、停了多久都要能读出来，这就是后台与影子日志的字段。
    #[test]
    fn a_stalled_turn_reports_its_step_and_age() {
        let scope = scope(9_100_011);
        let watch = TurnWatch::observe(scope, Some("小明: 在吗"));
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(watch.enter(async {
                TurnWatch::step(TurnStep::Commit);
            }));
        watch.backdate_for_test(Duration::from_secs(900));

        let report = report_now(scope, 300);
        assert_eq!(report.turn_step, Some("commit"), "要能说出卡在哪一步");
        assert!(
            report.turn_step_secs.is_some_and(|secs| secs >= 900),
            "要能说出停在这一步多久: {:?}",
            report.turn_step_secs
        );
        assert!(
            report.turn_waiting_secs.is_some_and(|secs| secs >= 900),
            "这一轮总共等了多久也要报出来: {:?}",
            report.turn_waiting_secs
        );
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
