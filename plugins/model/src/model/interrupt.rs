//! 会话回复打断状态。

use kovi::tokio::sync::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReplyScope {
    Group(i64),
    Private(i64),
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

/// 新的相关消息到达时推进代数，使此前的模型结果和未发气泡全部失效。
pub(crate) async fn interrupt(scope: ReplyScope) -> ReplyTicket {
    let mut states = REPLY_STATES.lock().await;
    prune_states(&mut states);
    let state = states.entry(scope).or_default();
    state.generation = state.generation.wrapping_add(1);
    state.last_seen = Instant::now();
    ReplyTicket {
        scope,
        generation: state.generation,
    }
}

pub(crate) async fn is_current(ticket: ReplyTicket) -> bool {
    REPLY_STATES
        .lock()
        .await
        .get(&ticket.scope)
        .is_some_and(|state| state.generation == ticket.generation)
}

pub(crate) async fn mark_active(ticket: ReplyTicket) -> bool {
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

pub(crate) async fn finish(ticket: ReplyTicket) {
    let mut states = REPLY_STATES.lock().await;
    if let Some(state) = states.get_mut(&ticket.scope) {
        if state.active_generation == Some(ticket.generation) {
            state.active_generation = None;
        }
        state.last_seen = Instant::now();
    }
}

pub(crate) async fn is_active(scope: ReplyScope) -> bool {
    REPLY_STATES
        .lock()
        .await
        .get(&scope)
        .is_some_and(|state| state.active_generation.is_some())
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
    use super::{ReplyScope, finish, interrupt, is_active, is_current, mark_active};

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
}
