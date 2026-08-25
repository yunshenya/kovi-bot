//! 会话回复打断状态。

use kovi::tokio::sync::Mutex;
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

const MAX_PENDING_OUTGOING_PER_SCOPE: usize = 8;
const MAX_COLLISIONS_PER_SCOPE: usize = 16;
const OUTGOING_TERMINAL_RETENTION: Duration = Duration::from_secs(60);
const MESSAGE_COLLISION_WINDOW: Duration = Duration::from_secs(3);
const COMMITTED_OUTGOING_LEASE: Duration = Duration::from_secs(120);

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
    conversation_version: u64,
}

impl ReplyTicket {
    pub(crate) fn scope(self) -> ReplyScope {
        self.scope
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutgoingSource {
    Reply,
    Proactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutgoingState {
    Prepared,
    Committed,
    Sent,
    Cancelled,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutgoingToken {
    ticket: ReplyTicket,
    fingerprint: u64,
    sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutgoingCommitRejection {
    Stale,
    DuplicateIdempotency,
}

/// Owns one committed send until the transport reports a terminal outcome.
/// Dropping the guard schedules best-effort cancellation; the committed lease
/// remains the final backstop if the task or runtime is already unwinding.
#[derive(Debug)]
pub(crate) struct CommittedOutgoing {
    token: Option<OutgoingToken>,
}

impl CommittedOutgoing {
    pub(crate) async fn mark_sent(mut self) {
        self.finish(OutgoingState::Sent).await;
    }

    pub(crate) async fn mark_failed(mut self) {
        self.finish(OutgoingState::Cancelled).await;
    }

    async fn finish(&mut self, terminal: OutgoingState) {
        let Some(token) = self.token else {
            return;
        };
        finish_outgoing(token, terminal).await;
        self.token = None;
    }
}

impl Drop for CommittedOutgoing {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        if let Ok(runtime) = kovi::tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                finish_outgoing(token, OutgoingState::Cancelled).await;
            });
        }
    }
}

#[derive(Debug, Clone)]
struct PendingOutgoing {
    token: OutgoingToken,
    effective_fingerprint: u64,
    idempotency_key: Option<String>,
    source: OutgoingSource,
    state: OutgoingState,
    committed_at: Option<Instant>,
    terminal_at: Option<Instant>,
    collision_reported: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MessageCollision {
    pub(crate) scope: ReplyScope,
    pub(crate) outgoing_generation: u64,
    pub(crate) conversation_version: u64,
    pub(crate) fingerprint: u64,
    pub(crate) source: OutgoingSource,
}

#[derive(Debug)]
struct ReplyState {
    generation: u64,
    conversation_version: u64,
    active_generation: Option<u64>,
    outgoing_sequence: u64,
    pending_outgoing: VecDeque<PendingOutgoing>,
    collisions: VecDeque<MessageCollision>,
    last_seen: Instant,
}

impl Default for ReplyState {
    fn default() -> Self {
        Self {
            generation: 0,
            conversation_version: 0,
            active_generation: None,
            outgoing_sequence: 0,
            pending_outgoing: VecDeque::new(),
            collisions: VecDeque::new(),
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
    advance_generation(scope, state, OutgoingState::Superseded)
}

/// A stop request advances the conversation just like any other interrupt, but
/// records a cancellation rather than a semantic supersession.
pub(crate) async fn cancel_locked(scope: ReplyScope) -> ReplyTicket {
    let mut states = REPLY_STATES.lock().await;
    prune_states(&mut states);
    let state = states.entry(scope).or_default();
    advance_generation(scope, state, OutgoingState::Cancelled)
}

/// Cancel an ingress only if no newer event has crossed the conversation
/// linearization point. The previous generation may already have been marked
/// `Superseded` by the fail-closed ingress pass; when semantic understanding
/// confirms Stop, reclassify exactly that generation as `Cancelled`.
pub(crate) async fn cancel_if_current_locked(ticket: ReplyTicket) -> Option<ReplyTicket> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&ticket.scope)?;
    if !ticket_matches(state, ticket) {
        return None;
    }
    for pending in &mut state.pending_outgoing {
        if pending.state == OutgoingState::Superseded
            && pending
                .token
                .ticket
                .generation
                .wrapping_add(1)
                == ticket.generation
            && pending
                .token
                .ticket
                .conversation_version
                .wrapping_add(1)
                == ticket.conversation_version
        {
            pending.state = OutgoingState::Cancelled;
        }
    }
    Some(advance_generation(
        ticket.scope,
        state,
        OutgoingState::Cancelled,
    ))
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
    if !ticket_matches(state, ticket) {
        return false;
    }
    advance_generation(ticket.scope, state, OutgoingState::Superseded);
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
    if !ticket_matches(state, completed) || state.active_generation.is_some() {
        return None;
    }
    state.generation = state.generation.wrapping_add(1);
    state.active_generation = Some(state.generation);
    state.last_seen = Instant::now();
    Some(ReplyTicket {
        scope: completed.scope,
        generation: state.generation,
        conversation_version: state.conversation_version,
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
        .is_some_and(|state| ticket_matches(state, ticket))
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
    if !ticket_matches(state, ticket) {
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

/// Return the current active ticket while the caller holds `scope_mutex`.
/// Reactive host-side sends can borrow this ticket without advancing or
/// finishing the conversation generation owned by the active handler.
pub(crate) async fn active_ticket_locked(scope: ReplyScope) -> Option<ReplyTicket> {
    let states = REPLY_STATES.lock().await;
    let state = states.get(&scope)?;
    let generation = state.active_generation?;
    (generation == state.generation).then_some(ReplyTicket {
        scope,
        generation,
        conversation_version: state.conversation_version,
    })
}

/// Remove all reply-generation state for a scope while its `scope_mutex` is
/// held by the caller. Data erasure uses this after the Core FIFO barrier,
/// because a drained adapter may have recreated state after the first cancel.
pub(crate) async fn clear_reply_state_locked(scope: ReplyScope) -> bool {
    REPLY_STATES.lock().await.remove(&scope).is_some()
}

/// Stable, process-local fingerprint used to bind a prepared payload to the
/// exact content that reaches the platform adapter.
pub(crate) fn outgoing_fingerprint(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Fingerprint the complete platform envelope that will cross the irreversible
/// send boundary. Length and option markers are provided by `Hash`, so fields
/// such as `(content="ab", key="c")` cannot alias `(content="a", key="bc")`
/// through concatenation.
pub(crate) fn contextual_outgoing_fingerprint(
    scope: ReplyScope,
    content: &str,
    reply_to: Option<i64>,
    mention_user_ids: &[i64],
    idempotency_key: Option<&str>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    "yunxi-outgoing-envelope-v1".hash(&mut hasher);
    scope.hash(&mut hasher);
    content.hash(&mut hasher);
    reply_to.hash(&mut hasher);
    mention_user_ids.hash(&mut hasher);
    idempotency_key.hash(&mut hasher);
    hasher.finish()
}

pub(crate) async fn prepare_outgoing(
    ticket: ReplyTicket,
    fingerprint: u64,
    source: OutgoingSource,
) -> Option<OutgoingToken> {
    let lock = scope_mutex(ticket.scope);
    let _scope_guard = lock.lock().await;
    prepare_outgoing_locked(ticket, fingerprint, source).await
}

async fn prepare_outgoing_locked(
    ticket: ReplyTicket,
    fingerprint: u64,
    source: OutgoingSource,
) -> Option<OutgoingToken> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&ticket.scope)?;
    prune_outgoing(state);
    if !ticket_matches(state, ticket) || state.active_generation != Some(ticket.generation) {
        return None;
    }
    if state.pending_outgoing.iter().any(|pending| {
        pending.token.ticket == ticket
            && matches!(
                pending.state,
                OutgoingState::Prepared | OutgoingState::Committed
            )
    }) {
        return None;
    }
    make_outgoing_room(state)?;
    state.outgoing_sequence = state.outgoing_sequence.wrapping_add(1);
    let token = OutgoingToken {
        ticket,
        fingerprint,
        sequence: state.outgoing_sequence,
    };
    state.pending_outgoing.push_back(PendingOutgoing {
        token,
        effective_fingerprint: fingerprint,
        idempotency_key: None,
        source,
        state: OutgoingState::Prepared,
        committed_at: None,
        terminal_at: None,
        collision_reported: false,
    });
    state.last_seen = Instant::now();
    Some(token)
}

/// The only transition into the irreversible side-effect region. The caller
/// must resolve and authorize its destination before calling this function and
/// must perform the network request only after this function releases the
/// conversation lock.
pub(crate) async fn commit_outgoing(token: OutgoingToken) -> bool {
    commit_outgoing_with_context(token, token.fingerprint, None)
        .await
        .is_ok()
}

async fn commit_outgoing_with_context(
    token: OutgoingToken,
    effective_fingerprint: u64,
    idempotency_key: Option<&str>,
) -> Result<(), OutgoingCommitRejection> {
    let lock = scope_mutex(token.ticket.scope);
    let _scope_guard = lock.lock().await;
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&token.ticket.scope) else {
        return Err(OutgoingCommitRejection::Stale);
    };
    if !ticket_matches(state, token.ticket) {
        supersede_prepared(state, token);
        return Err(OutgoingCommitRejection::Stale);
    }
    let Some(pending_index) = state
        .pending_outgoing
        .iter()
        .position(|pending| pending.token == token)
    else {
        return Err(OutgoingCommitRejection::Stale);
    };
    if state.pending_outgoing[pending_index].state != OutgoingState::Prepared {
        return Err(OutgoingCommitRejection::Stale);
    }
    if let Some(idempotency_key) = idempotency_key
        && state.pending_outgoing.iter().enumerate().any(|(index, pending)| {
            index != pending_index
                && pending.idempotency_key.as_deref() == Some(idempotency_key)
                && matches!(pending.state, OutgoingState::Committed | OutgoingState::Sent)
        })
    {
        let pending = &mut state.pending_outgoing[pending_index];
        pending.state = OutgoingState::Cancelled;
        pending.terminal_at = Some(Instant::now());
        return Err(OutgoingCommitRejection::DuplicateIdempotency);
    }
    let pending = &mut state.pending_outgoing[pending_index];
    pending.state = OutgoingState::Committed;
    pending.effective_fingerprint = effective_fingerprint;
    pending.idempotency_key = idempotency_key.map(ToOwned::to_owned);
    pending.committed_at = Some(Instant::now());
    state.last_seen = Instant::now();
    schedule_committed_expiry(token);
    Ok(())
}

pub(crate) async fn commit_outgoing_guard(token: OutgoingToken) -> Option<CommittedOutgoing> {
    commit_outgoing(token)
        .await
        .then_some(CommittedOutgoing { token: Some(token) })
}

pub(crate) async fn commit_outgoing_guard_with_context(
    token: OutgoingToken,
    effective_fingerprint: u64,
    idempotency_key: Option<&str>,
) -> Result<CommittedOutgoing, OutgoingCommitRejection> {
    commit_outgoing_with_context(token, effective_fingerprint, idempotency_key).await?;
    Ok(CommittedOutgoing { token: Some(token) })
}

pub(crate) async fn find_prepared_outgoing(
    scope: ReplyScope,
    fingerprint: u64,
) -> Option<(OutgoingToken, OutgoingSource)> {
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&scope)?;
    prune_outgoing(state);
    state
        .pending_outgoing
        .iter()
        .rev()
        .find(|pending| {
            pending.state == OutgoingState::Prepared
                && pending.token.fingerprint == fingerprint
                && ticket_matches(state, pending.token.ticket)
        })
        .map(|pending| (pending.token, pending.source))
}

/// Inspect the source of the current prepared envelope while the caller holds
/// `scope_mutex`. Semantic admission must use this authoritative value instead
/// of trusting a source label supplied by an ingress caller.
pub(crate) async fn prepared_outgoing_source_locked(scope: ReplyScope) -> Option<OutgoingSource> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&scope)?;
    prune_outgoing(state);
    state
        .pending_outgoing
        .iter()
        .rev()
        .find(|pending| {
            pending.state == OutgoingState::Prepared
                && ticket_matches(state, pending.token.ticket)
        })
        .map(|pending| pending.source)
}

/// Cancel only a current prepared proactive envelope without advancing the
/// conversation generation. A direct reply can then be queued or claimed by
/// the normal coalescing path, while an incorrectly classified ingress cannot
/// cancel a prepared reactive reply.
pub(crate) async fn cancel_prepared_proactive_locked(scope: ReplyScope) -> bool {
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&scope) else {
        return false;
    };
    prune_outgoing(state);
    let generation = state.generation;
    let conversation_version = state.conversation_version;
    let Some(pending) = state.pending_outgoing.iter_mut().rev().find(|pending| {
        pending.state == OutgoingState::Prepared
            && pending.source == OutgoingSource::Proactive
            && pending.token.ticket.generation == generation
            && pending.token.ticket.conversation_version == conversation_version
    }) else {
        return false;
    };
    let now = Instant::now();
    pending.state = OutgoingState::Cancelled;
    pending.terminal_at = Some(now);
    state.last_seen = now;
    true
}

pub(crate) async fn mark_outgoing_sent(token: OutgoingToken) {
    finish_outgoing(token, OutgoingState::Sent).await;
}

pub(crate) async fn mark_outgoing_failed(token: OutgoingToken) {
    finish_outgoing(token, OutgoingState::Cancelled).await;
}

async fn finish_outgoing(token: OutgoingToken, terminal: OutgoingState) {
    let lock = scope_mutex(token.ticket.scope);
    let _scope_guard = lock.lock().await;
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&token.ticket.scope) else {
        return;
    };
    if let Some(pending) = state
        .pending_outgoing
        .iter_mut()
        .find(|pending| pending.token == token)
        && matches!(
            pending.state,
            OutgoingState::Prepared | OutgoingState::Committed
        )
    {
        pending.state = terminal;
        pending.terminal_at = Some(Instant::now());
    }
    state.last_seen = Instant::now();
    prune_outgoing(state);
}

fn schedule_committed_expiry(token: OutgoingToken) {
    let Ok(runtime) = kovi::tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        kovi::tokio::time::sleep(COMMITTED_OUTGOING_LEASE).await;
        expire_committed_outgoing(token).await;
    });
}

async fn expire_committed_outgoing(token: OutgoingToken) {
    let lock = scope_mutex(token.ticket.scope);
    let _scope_guard = lock.lock().await;
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&token.ticket.scope) else {
        return;
    };
    if let Some(pending) = state
        .pending_outgoing
        .iter_mut()
        .find(|pending| pending.token == token && pending.state == OutgoingState::Committed)
    {
        pending.state = OutgoingState::Cancelled;
        pending.terminal_at = Some(Instant::now());
    }
    state.last_seen = Instant::now();
    prune_outgoing(state);
}

pub(crate) async fn take_message_collisions(scope: ReplyScope) -> Vec<MessageCollision> {
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&scope) else {
        return Vec::new();
    };
    state.collisions.drain(..).collect()
}

fn advance_generation(
    scope: ReplyScope,
    state: &mut ReplyState,
    prepared_terminal: OutgoingState,
) -> ReplyTicket {
    let now = Instant::now();
    for pending in &mut state.pending_outgoing {
        match pending.state {
            OutgoingState::Prepared => {
                pending.state = prepared_terminal;
                pending.terminal_at = Some(now);
            }
            OutgoingState::Committed | OutgoingState::Sent => {
                if !pending.collision_reported
                    && pending.committed_at.is_some_and(|committed_at| {
                        now.duration_since(committed_at) <= MESSAGE_COLLISION_WINDOW
                    })
                {
                    if state.collisions.len() == MAX_COLLISIONS_PER_SCOPE {
                        state.collisions.pop_front();
                    }
                    state.collisions.push_back(MessageCollision {
                        scope,
                        outgoing_generation: pending.token.ticket.generation,
                        conversation_version: pending.token.ticket.conversation_version,
                        fingerprint: pending.effective_fingerprint,
                        source: pending.source,
                    });
                    pending.collision_reported = true;
                }
            }
            OutgoingState::Cancelled | OutgoingState::Superseded => {}
        }
    }
    state.generation = state.generation.wrapping_add(1);
    state.conversation_version = state.conversation_version.wrapping_add(1);
    state.active_generation = None;
    state.last_seen = now;
    prune_outgoing(state);
    ReplyTicket {
        scope,
        generation: state.generation,
        conversation_version: state.conversation_version,
    }
}

fn ticket_matches(state: &ReplyState, ticket: ReplyTicket) -> bool {
    state.generation == ticket.generation
        && state.conversation_version == ticket.conversation_version
}

fn supersede_prepared(state: &mut ReplyState, token: OutgoingToken) {
    if let Some(pending) = state
        .pending_outgoing
        .iter_mut()
        .find(|pending| pending.token == token && pending.state == OutgoingState::Prepared)
    {
        pending.state = OutgoingState::Superseded;
        pending.terminal_at = Some(Instant::now());
    }
}

fn make_outgoing_room(state: &mut ReplyState) -> Option<()> {
    while state.pending_outgoing.len() >= MAX_PENDING_OUTGOING_PER_SCOPE {
        let removable = state.pending_outgoing.iter().position(|pending| {
            !matches!(
                pending.state,
                OutgoingState::Prepared | OutgoingState::Committed
            )
        })?;
        state.pending_outgoing.remove(removable);
    }
    Some(())
}

fn prune_outgoing(state: &mut ReplyState) {
    let now = Instant::now();
    for pending in &mut state.pending_outgoing {
        if pending.state == OutgoingState::Committed
            && pending.committed_at.is_some_and(|committed_at| {
                now.duration_since(committed_at) >= COMMITTED_OUTGOING_LEASE
            })
        {
            pending.state = OutgoingState::Cancelled;
            pending.terminal_at = Some(now);
        }
    }
    state.pending_outgoing.retain(|pending| {
        matches!(
            pending.state,
            OutgoingState::Prepared | OutgoingState::Committed
        ) || pending.terminal_at.is_some_and(|terminal_at| {
            now.duration_since(terminal_at) < OUTGOING_TERMINAL_RETENTION
        })
    });
}

fn prune_states(states: &mut HashMap<ReplyScope, ReplyState>) {
    if states.len() <= 2_048 {
        return;
    }
    states.retain(|_, state| {
        state.active_generation.is_some()
            || state.pending_outgoing.iter().any(|pending| {
                matches!(
                    pending.state,
                    OutgoingState::Prepared | OutgoingState::Committed
                )
            })
            || state.last_seen.elapsed() < Duration::from_secs(60 * 60)
    });
}

#[cfg(test)]
pub(crate) async fn test_outgoing_state(token: OutgoingToken) -> Option<OutgoingState> {
    REPLY_STATES
        .lock()
        .await
        .get(&token.ticket.scope)
        .and_then(|state| {
            state
                .pending_outgoing
                .iter()
                .find(|pending| pending.token == token)
                .map(|pending| pending.state)
        })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PENDING_OUTGOING_PER_SCOPE, OutgoingCommitRejection, OutgoingSource, OutgoingState,
        OutgoingToken, REPLY_STATES, ReplyScope, cancel_locked,
        cancel_prepared_proactive_locked, claim_follow_up, clear_reply_state_locked,
        commit_outgoing, commit_outgoing_guard,
        commit_outgoing_guard_with_context, contextual_outgoing_fingerprint,
        find_prepared_outgoing, finish, interrupt, interrupt_if_current, is_active, is_current,
        mark_active, mark_outgoing_failed, mark_outgoing_sent, outgoing_fingerprint,
        prepare_outgoing,
        prepared_outgoing_source_locked, scope_mutex, take_message_collisions,
    };

    async fn outgoing_state(token: OutgoingToken) -> Option<OutgoingState> {
        REPLY_STATES
            .lock()
            .await
            .get(&token.ticket.scope)
            .and_then(|state| {
                state
                    .pending_outgoing
                    .iter()
                    .find(|pending| pending.token == token)
                    .map(|pending| pending.state)
            })
    }

    async fn wait_for_outgoing_state(token: OutgoingToken, expected: OutgoingState) {
        for _ in 0..64 {
            if outgoing_state(token).await == Some(expected) {
                return;
            }
            kovi::tokio::task::yield_now().await;
        }
        assert_eq!(outgoing_state(token).await, Some(expected));
    }

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

    #[test]
    fn prepared_payload_is_superseded_before_commit() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_100_001);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let fingerprint = outgoing_fingerprint("old answer");
                let outgoing = prepare_outgoing(ticket, fingerprint, OutgoingSource::Reply)
                    .await
                    .expect("current reply should prepare");

                let newer = interrupt(scope).await;

                assert!(!commit_outgoing(outgoing).await);
                assert!(find_prepared_outgoing(scope, fingerprint).await.is_none());
                assert!(is_current(newer).await);
                assert!(take_message_collisions(scope).await.is_empty());
            });
    }

    #[test]
    fn inbound_after_commit_records_a_collision_without_revoking_the_send() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_100_002);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let fingerprint = outgoing_fingerprint("already leaving");
                let outgoing = prepare_outgoing(ticket, fingerprint, OutgoingSource::Proactive)
                    .await
                    .expect("current proactive output should prepare");
                assert!(commit_outgoing(outgoing).await);

                let newer = interrupt(scope).await;
                mark_outgoing_sent(outgoing).await;
                let collisions = take_message_collisions(scope).await;

                assert!(is_current(newer).await);
                assert_eq!(collisions.len(), 1);
                assert_eq!(collisions[0].scope, scope);
                assert_eq!(collisions[0].fingerprint, fingerprint);
                assert_eq!(collisions[0].outgoing_generation, 1);
                assert_eq!(collisions[0].conversation_version, 1);
                assert_eq!(collisions[0].source, OutgoingSource::Proactive);
            });
    }

    #[test]
    fn pending_outgoing_storage_stays_bounded_across_duplicate_bubbles() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_100_003);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let fingerprint = outgoing_fingerprint("same bubble");

                for _ in 0..MAX_PENDING_OUTGOING_PER_SCOPE * 3 {
                    let outgoing = prepare_outgoing(ticket, fingerprint, OutgoingSource::Reply)
                        .await
                        .expect("terminal entries should make room for the next bubble");
                    assert!(commit_outgoing(outgoing).await);
                    mark_outgoing_sent(outgoing).await;
                }

                assert!(find_prepared_outgoing(scope, fingerprint).await.is_none());
            });
    }

    #[test]
    fn complete_fingerprint_binds_destination_reply_mentions_and_idempotency() {
        let baseline = contextual_outgoing_fingerprint(
            ReplyScope::Group(9_100_030),
            "same content",
            Some(41),
            &[51, 52],
            Some("action:one"),
        );
        for changed in [
            contextual_outgoing_fingerprint(
                ReplyScope::Group(9_100_031),
                "same content",
                Some(41),
                &[51, 52],
                Some("action:one"),
            ),
            contextual_outgoing_fingerprint(
                ReplyScope::Group(9_100_030),
                "changed content",
                Some(41),
                &[51, 52],
                Some("action:one"),
            ),
            contextual_outgoing_fingerprint(
                ReplyScope::Group(9_100_030),
                "same content",
                Some(42),
                &[51, 52],
                Some("action:one"),
            ),
            contextual_outgoing_fingerprint(
                ReplyScope::Group(9_100_030),
                "same content",
                Some(41),
                &[51, 53],
                Some("action:one"),
            ),
            contextual_outgoing_fingerprint(
                ReplyScope::Group(9_100_030),
                "same content",
                Some(41),
                &[51, 52],
                Some("action:two"),
            ),
        ] {
            assert_ne!(baseline, changed);
        }
    }

    #[test]
    fn sent_idempotency_key_cannot_commit_again_in_a_new_generation() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_100_031);
                let first_ticket = interrupt(scope).await;
                assert!(mark_active(first_ticket).await);
                let first = prepare_outgoing(
                    first_ticket,
                    outgoing_fingerprint("first"),
                    OutgoingSource::Proactive,
                )
                .await
                .expect("first outgoing should prepare");
                let first_guard = commit_outgoing_guard_with_context(
                    first,
                    contextual_outgoing_fingerprint(
                        scope,
                        "first",
                        None,
                        &[],
                        Some("stable-action-key"),
                    ),
                    Some("stable-action-key"),
                )
                .await
                .expect("first idempotency reservation should commit");
                first_guard.mark_sent().await;

                let second_ticket = interrupt(scope).await;
                assert!(mark_active(second_ticket).await);
                let second = prepare_outgoing(
                    second_ticket,
                    outgoing_fingerprint("second"),
                    OutgoingSource::Proactive,
                )
                .await
                .expect("second outgoing should prepare");
                let rejected = commit_outgoing_guard_with_context(
                    second,
                    contextual_outgoing_fingerprint(
                        scope,
                        "second",
                        None,
                        &[],
                        Some("stable-action-key"),
                    ),
                    Some("stable-action-key"),
                )
                .await;

                assert!(matches!(
                    rejected,
                    Err(OutgoingCommitRejection::DuplicateIdempotency)
                ));
                assert_eq!(outgoing_state(second).await, Some(OutgoingState::Cancelled));
            });
    }

    #[test]
    fn collision_reports_the_fingerprint_bound_at_commit() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_100_032);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let prepared_fingerprint = outgoing_fingerprint("body");
                let committed_fingerprint = contextual_outgoing_fingerprint(
                    scope,
                    "body",
                    Some(71),
                    &[81],
                    Some("collision-action"),
                );
                let outgoing = prepare_outgoing(
                    ticket,
                    prepared_fingerprint,
                    OutgoingSource::Reply,
                )
                .await
                .expect("outgoing should prepare");
                let guard = commit_outgoing_guard_with_context(
                    outgoing,
                    committed_fingerprint,
                    Some("collision-action"),
                )
                .await
                .expect("outgoing should commit");

                let _inbound = interrupt(scope).await;
                guard.mark_sent().await;
                let collisions = take_message_collisions(scope).await;

                assert_eq!(collisions.len(), 1);
                assert_eq!(collisions[0].fingerprint, committed_fingerprint);
                assert_ne!(collisions[0].fingerprint, prepared_fingerprint);
            });
    }

    #[test]
    fn stop_cancels_prepared_output() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_100_004);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("do not send"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply should prepare");
                let lock = scope_mutex(scope);
                let _guard = lock.lock().await;
                let stopped = cancel_locked(scope).await;
                drop(_guard);

                assert!(is_current(stopped).await);
                assert!(!commit_outgoing(outgoing).await);
            });
    }

    #[test]
    fn aborting_a_committed_send_drops_the_guard_and_cleans_the_lease() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_100_005);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("cancel after commit"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply should prepare");
                let committed = std::sync::Arc::new(kovi::tokio::sync::Notify::new());
                let release = std::sync::Arc::new(kovi::tokio::sync::Notify::new());
                let task = {
                    let committed = std::sync::Arc::clone(&committed);
                    let release = std::sync::Arc::clone(&release);
                    kovi::tokio::spawn(async move {
                        let _guard = commit_outgoing_guard(outgoing)
                            .await
                            .expect("prepared output should commit");
                        committed.notify_one();
                        release.notified().await;
                    })
                };
                committed.notified().await;
                task.abort();
                let _ = task.await;

                wait_for_outgoing_state(outgoing, OutgoingState::Cancelled).await;
                let _newer = interrupt(scope).await;
                assert!(take_message_collisions(scope).await.is_empty());
            });
    }

    #[test]
    fn send_failure_finishes_a_committed_guard_without_a_collision() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_100_006);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("transport failure"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply should prepare");
                let guard = commit_outgoing_guard(outgoing)
                    .await
                    .expect("prepared output should commit");
                guard.mark_failed().await;

                assert_eq!(
                    outgoing_state(outgoing).await,
                    Some(OutgoingState::Cancelled)
                );
                let _newer = interrupt(scope).await;
                assert!(take_message_collisions(scope).await.is_empty());
            });
    }

    #[test]
    fn cancelled_commits_make_room_beyond_the_per_scope_capacity() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_100_007);
                for index in 0..MAX_PENDING_OUTGOING_PER_SCOPE * 3 {
                    let ticket = interrupt(scope).await;
                    assert!(mark_active(ticket).await);
                    let outgoing = prepare_outgoing(
                        ticket,
                        outgoing_fingerprint(&format!("failed send {index}")),
                        OutgoingSource::Reply,
                    )
                    .await
                    .expect("terminal commits must keep releasing capacity");
                    let guard = commit_outgoing_guard(outgoing)
                        .await
                        .expect("prepared output should commit");
                    guard.mark_failed().await;
                }

                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);
                assert!(
                    prepare_outgoing(
                        ticket,
                        outgoing_fingerprint("capacity remains available"),
                        OutgoingSource::Reply,
                    )
                    .await
                    .is_some()
                );
            });
    }

    #[test]
    fn semantic_admission_reads_real_source_and_only_cancels_proactive() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let proactive_scope = ReplyScope::Private(9_100_040);
                let proactive_ticket = interrupt(proactive_scope).await;
                assert!(mark_active(proactive_ticket).await);
                let proactive = prepare_outgoing(
                    proactive_ticket,
                    outgoing_fingerprint("proactive"),
                    OutgoingSource::Proactive,
                )
                .await
                .expect("proactive output should prepare");
                let lock = scope_mutex(proactive_scope);
                let guard = lock.lock().await;
                assert_eq!(
                    prepared_outgoing_source_locked(proactive_scope).await,
                    Some(OutgoingSource::Proactive)
                );
                assert!(cancel_prepared_proactive_locked(proactive_scope).await);
                drop(guard);
                assert!(is_current(proactive_ticket).await);
                assert!(!commit_outgoing(proactive).await);

                let reply_scope = ReplyScope::Private(9_100_041);
                let reply_ticket = interrupt(reply_scope).await;
                assert!(mark_active(reply_ticket).await);
                let reply = prepare_outgoing(
                    reply_ticket,
                    outgoing_fingerprint("reply"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply output should prepare");
                let lock = scope_mutex(reply_scope);
                let guard = lock.lock().await;
                assert_eq!(
                    prepared_outgoing_source_locked(reply_scope).await,
                    Some(OutgoingSource::Reply)
                );
                assert!(!cancel_prepared_proactive_locked(reply_scope).await);
                drop(guard);
                assert!(commit_outgoing(reply).await);
                mark_outgoing_failed(reply).await;
            });
    }
}
