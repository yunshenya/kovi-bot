//! 会话回复打断状态。

use kovi::tokio::sync::Mutex;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReplyScope {
    Group(i64),
    Private(i64),
    /// 独立于聊天会话的持久化任务执行代数。
    Scheduled(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplyTicket {
    scope: ReplyScope,
    generation: u64,
}

#[derive(Debug)]
struct ReplyState {
    generation: u64,
    active_generation: Option<u64>,
    last_seen: Instant,
}

impl Default for ReplyState {
    fn default() -> Self {
        Self {
            generation: 0,
            active_generation: None,
            last_seen: Instant::now(),
        }
    }
}

static REPLY_STATES: LazyLock<Mutex<HashMap<ReplyScope, ReplyState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const SCOPE_LOCK_SHARDS: usize = 64;
static SCOPE_LOCKS: LazyLock<Vec<Arc<Mutex<()>>>> = LazyLock::new(|| {
    (0..SCOPE_LOCK_SHARDS)
        .map(|_| Arc::new(Mutex::new(())))
        .collect()
});

/// 同一会话的队列、回复代数和消息生命周期必须共用线性化点；分片只减少
/// 不同会话之间的锁竞争，不改变同一会话内的顺序。
pub(crate) fn scope_mutex(scope: ReplyScope) -> Arc<Mutex<()>> {
    let mut hasher = DefaultHasher::new();
    scope.hash(&mut hasher);
    Arc::clone(&SCOPE_LOCKS[(hasher.finish() as usize) % SCOPE_LOCK_SHARDS])
}

/// 新的相关消息到达时推进代数，使此前的模型结果和未发气泡全部失效。
pub(crate) async fn interrupt(scope: ReplyScope) -> ReplyTicket {
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    interrupt_locked(scope).await
}

pub(crate) async fn interrupt_locked(scope: ReplyScope) -> ReplyTicket {
    let mut states = REPLY_STATES.lock().await;
    prune_states(&mut states);
    let state = states.entry(scope).or_default();
    state.generation = state.generation.wrapping_add(1);
    state.active_generation = None;
    state.last_seen = Instant::now();
    ReplyTicket {
        scope,
        generation: state.generation,
    }
}

/// 仅当指定 ticket 仍是当前代数时推进代数。
/// 用于撤回等延迟事件，避免它们误中断随后已经开始的新回复。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn interrupt_if_current(ticket: ReplyTicket) -> bool {
    let lock = scope_mutex(ticket.scope);
    let _scope_guard = lock.lock().await;
    interrupt_if_current_locked(ticket).await
}

pub(crate) async fn interrupt_if_current_locked(ticket: ReplyTicket) -> bool {
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&ticket.scope) else {
        return false;
    };
    if state.generation != ticket.generation {
        return false;
    }
    state.generation = state.generation.wrapping_add(1);
    state.active_generation = None;
    state.last_seen = Instant::now();
    true
}

/// 在一轮回复正常完成后，原子领取同一会话的下一轮处理权。
/// 只有完成的 ticket 仍是当前代数且没有活跃回复时才能领取，因此旧 drainer
/// 不能覆盖已经在预处理的新消息，多个 drainer 也不能互相抢占。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn claim_follow_up(completed: ReplyTicket) -> Option<ReplyTicket> {
    let lock = scope_mutex(completed.scope);
    let _scope_guard = lock.lock().await;
    claim_follow_up_locked(completed).await
}

pub(crate) async fn claim_follow_up_locked(completed: ReplyTicket) -> Option<ReplyTicket> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&completed.scope)?;
    if state.generation != completed.generation || state.active_generation.is_some() {
        return None;
    }
    state.generation = state.generation.wrapping_add(1);
    state.active_generation = Some(state.generation);
    state.last_seen = Instant::now();
    Some(ReplyTicket {
        scope: completed.scope,
        generation: state.generation,
    })
}

pub(crate) async fn is_current(ticket: ReplyTicket) -> bool {
    let lock = scope_mutex(ticket.scope);
    let _scope_guard = lock.lock().await;
    is_current_locked(ticket).await
}

pub(crate) async fn is_current_locked(ticket: ReplyTicket) -> bool {
    REPLY_STATES
        .lock()
        .await
        .get(&ticket.scope)
        .is_some_and(|state| state.generation == ticket.generation)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn mark_active(ticket: ReplyTicket) -> bool {
    let lock = scope_mutex(ticket.scope);
    let _scope_guard = lock.lock().await;
    claim_active_locked(ticket).await
}

pub(crate) async fn claim_active_locked(ticket: ReplyTicket) -> bool {
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&ticket.scope) else {
        return false;
    };
    if state.generation != ticket.generation {
        return false;
    }
    state.active_generation = Some(ticket.generation);
    state.last_seen = Instant::now();
    true
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn finish(ticket: ReplyTicket) {
    let lock = scope_mutex(ticket.scope);
    let _scope_guard = lock.lock().await;
    finish_locked(ticket).await;
}

pub(crate) async fn finish_locked(ticket: ReplyTicket) {
    let mut states = REPLY_STATES.lock().await;
    if let Some(state) = states.get_mut(&ticket.scope) {
        if state.active_generation == Some(ticket.generation) {
            state.active_generation = None;
        }
        state.last_seen = Instant::now();
    }
}

pub(crate) async fn is_active(scope: ReplyScope) -> bool {
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    is_active_locked(scope).await
}

pub(crate) async fn is_active_locked(scope: ReplyScope) -> bool {
    REPLY_STATES
        .lock()
        .await
        .get(&scope)
        .is_some_and(|state| state.active_generation.is_some())
}

/// Remove all reply-generation state for a scope while its `scope_mutex` is
/// held by the caller. Data erasure uses this after the Core FIFO barrier,
/// because a drained adapter may have recreated state after the first cancel.
pub(crate) async fn clear_reply_state_locked(scope: ReplyScope) -> bool {
    REPLY_STATES.lock().await.remove(&scope).is_some()
}

fn prune_states(states: &mut HashMap<ReplyScope, ReplyState>) {
    if states.len() <= 2_048 {
        return;
    }
    states.retain(|_, state| {
        state.active_generation.is_some()
            || state.last_seen.elapsed() < Duration::from_secs(60 * 60)
    });
}

#[cfg(test)]
mod tests {
    use super::{
        ReplyScope, claim_follow_up, clear_reply_state_locked, finish, interrupt,
        interrupt_if_current, is_active, is_current, mark_active, scope_mutex,
    };

    #[test]
    fn a_new_generation_invalidates_the_old_reply() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_000_001);
                let old = interrupt(scope).await;
                assert!(is_current(old).await);
                let new = interrupt(scope).await;
                assert!(!is_current(old).await);
                assert!(is_current(new).await);
            });
    }

    #[test]
    fn stale_task_cannot_clear_a_new_active_reply() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_000_002);
                let old = interrupt(scope).await;
                assert!(mark_active(old).await);
                let new = interrupt(scope).await;
                assert!(mark_active(new).await);
                finish(old).await;
                assert!(is_active(scope).await);
                finish(new).await;
                assert!(!is_active(scope).await);
            });
    }

    #[test]
    fn delayed_interrupt_cannot_cancel_a_newer_generation() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_000_003);
                let old = interrupt(scope).await;
                let new = interrupt(scope).await;
                assert!(!interrupt_if_current(old).await);
                assert!(is_current(new).await);
                assert!(interrupt_if_current(new).await);
                assert!(!is_current(new).await);
            });
    }

    #[test]
    fn data_erasure_clear_removes_private_reply_state() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_000_008);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let lock = scope_mutex(scope);
                let _guard = lock.lock().await;
                assert!(clear_reply_state_locked(scope).await);
                drop(_guard);
                assert!(!is_current(ticket).await);
                assert!(!is_active(scope).await);
            });
    }

    #[test]
    fn scheduled_task_generation_is_independent_from_chat_generation() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scheduled = ReplyScope::Scheduled(9_000_007);
                let chat = ReplyScope::Private(9_000_007);
                let scheduled_ticket = interrupt(scheduled).await;
                let chat_ticket = interrupt(chat).await;
                assert!(is_current(scheduled_ticket).await);
                assert!(is_current(chat_ticket).await);
                let newer_chat_ticket = interrupt(chat).await;
                assert!(is_current(scheduled_ticket).await);
                assert!(!is_current(chat_ticket).await);
                assert!(is_current(newer_chat_ticket).await);
            });
    }

    #[test]
    fn stale_recall_cannot_interrupt_a_new_active_reply() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_000_005);
                let old = interrupt(scope).await;
                assert!(mark_active(old).await);
                let new = interrupt(scope).await;
                assert!(mark_active(new).await);

                assert!(!interrupt_if_current(old).await);
                assert!(is_current(new).await);
                assert!(is_active(scope).await);

                finish(new).await;
            });
    }

    #[test]
    fn only_current_completed_reply_can_claim_one_follow_up() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_000_004);
                let completed = interrupt(scope).await;
                assert!(mark_active(completed).await);
                finish(completed).await;

                let claimed = claim_follow_up(completed)
                    .await
                    .expect("当前完成轮应能领取下一轮");
                assert!(is_active(scope).await);
                assert!(claim_follow_up(completed).await.is_none());
                finish(claimed).await;

                let newer = interrupt(scope).await;
                assert!(claim_follow_up(claimed).await.is_none());
                assert!(is_current(newer).await);
            });
    }
}
