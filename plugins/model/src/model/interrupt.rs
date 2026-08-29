//! 会话回复打断状态。

use kovi::tokio::sync::{Mutex, Notify, watch};
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

const MAX_PENDING_OUTGOING_PER_SCOPE: usize = 8;
const MAX_COLLISIONS_PER_SCOPE: usize = 16;
const OUTGOING_TERMINAL_RETENTION: Duration = Duration::from_secs(60);
const OUTGOING_UNKNOWN_RETENTION: Duration = Duration::from_secs(60 * 60);
const MESSAGE_COLLISION_WINDOW: Duration = Duration::from_secs(3);
const COMMITTED_OUTGOING_LEASE: Duration = Duration::from_secs(120);
const INCOMING_RESERVATION_LEASE: Duration = Duration::from_secs(180);
const PRECOMMIT_VALIDATION_LEASE: Duration = Duration::from_secs(30);

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
    scope_epoch: u64,
    generation: u64,
    conversation_version: u64,
}

impl ReplyTicket {
    pub(crate) fn scope(self) -> ReplyScope {
        self.scope
    }

    pub(crate) fn scope_epoch(self) -> u64 {
        self.scope_epoch
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
    Unknown,
    Cancelled,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutgoingToken {
    ticket: ReplyTicket,
    fingerprint: u64,
    sequence: u64,
}

impl OutgoingToken {
    pub(crate) const fn ticket(self) -> ReplyTicket {
        self.ticket
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutgoingCommitRejection {
    Stale,
    DuplicateIdempotency,
}

/// Owns one committed send until the transport reports a terminal outcome.
/// Once the transport future has been polled, dropping it cannot prove that no
/// side effect occurred. Keep that outcome unknown for replay and collision
/// purposes; only an explicit transport failure is cancellable.
#[derive(Debug)]
pub(crate) struct CommittedOutgoing {
    token: Option<OutgoingToken>,
}

/// Pins a Prepared envelope while route and authorization are revalidated.
/// New ingress cannot freeze this token once the permit exists; it supersedes
/// the generation instead, so final commit never waits while security guards
/// are held.
#[derive(Debug)]
pub(crate) struct PreparedOutgoingCommit {
    token: Option<OutgoingToken>,
}

impl PreparedOutgoingCommit {
    async fn commit_state(
        &mut self,
        effective_fingerprint: u64,
        idempotency_key: Option<&str>,
    ) -> Result<OutgoingToken, OutgoingCommitRejection> {
        let token = self.token.ok_or(OutgoingCommitRejection::Stale)?;
        commit_prevalidated_outgoing(token, effective_fingerprint, idempotency_key).await?;
        self.token = None;
        Ok(token)
    }

    pub(crate) async fn commit(
        mut self,
        effective_fingerprint: u64,
        idempotency_key: Option<&str>,
    ) -> Result<CommittedOutgoing, OutgoingCommitRejection> {
        let token = self
            .commit_state(effective_fingerprint, idempotency_key)
            .await?;
        Ok(CommittedOutgoing { token: Some(token) })
    }
}

impl Drop for PreparedOutgoingCommit {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        if let Ok(runtime) = kovi::tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                mark_outgoing_failed(token).await;
            });
        }
    }
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
                finish_outgoing(token, OutgoingState::Unknown).await;
            });
        }
    }
}

#[derive(Debug, Clone)]
struct PendingOutgoing {
    token: OutgoingToken,
    effective_fingerprint: u64,
    semantic_preview: Option<String>,
    idempotency_key: Option<String>,
    source: OutgoingSource,
    state: OutgoingState,
    committed_at: Option<Instant>,
    terminal_at: Option<Instant>,
    collision_reported: bool,
}

#[derive(Debug)]
struct PendingIncoming {
    reservation_id: u64,
    ticket: ReplyTicket,
    frozen_token: Option<OutgoingToken>,
    fail_closed_terminal: OutgoingState,
    expires_at: Instant,
    resolved: watch::Sender<bool>,
}

/// An inbound turn observed while a reply is still generating. Unlike a
/// normal reservation, it keeps the current generation intact until the
/// semantic pass decides whether the in-flight reply still matters. It also
/// blocks proactive work and lets the old reply wait at its commit boundary
/// until this reservation is resolved.
#[derive(Debug)]
struct ActiveIncomingReservation {
    reservation_id: u64,
    ticket: ReplyTicket,
    expires_at: Instant,
    resolved: watch::Sender<bool>,
}

#[derive(Debug, Clone, Copy)]
struct PendingPrecommit {
    token: OutgoingToken,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MessageCollision {
    pub(crate) scope: ReplyScope,
    pub(crate) outgoing_generation: u64,
    pub(crate) conversation_version: u64,
    pub(crate) fingerprint: u64,
    pub(crate) source: OutgoingSource,
}

#[derive(Debug)]
struct ReplyState {
    scope_epoch: u64,
    generation: u64,
    conversation_version: u64,
    incoming_sequence: u64,
    active_generation: Option<u64>,
    pending_incoming: Option<PendingIncoming>,
    active_incoming: VecDeque<ActiveIncomingReservation>,
    pending_precommit: Option<PendingPrecommit>,
    outgoing_sequence: u64,
    pending_outgoing: VecDeque<PendingOutgoing>,
    collisions: VecDeque<MessageCollision>,
    last_seen: Instant,
}

impl Default for ReplyState {
    fn default() -> Self {
        Self {
            scope_epoch: NEXT_SCOPE_EPOCH.fetch_add(1, Ordering::Relaxed),
            generation: 0,
            conversation_version: 0,
            incoming_sequence: 0,
            active_generation: None,
            pending_incoming: None,
            active_incoming: VecDeque::new(),
            pending_precommit: None,
            outgoing_sequence: 0,
            pending_outgoing: VecDeque::new(),
            collisions: VecDeque::new(),
            last_seen: Instant::now(),
        }
    }
}

static REPLY_STATES: LazyLock<Mutex<HashMap<ReplyScope, ReplyState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_SCOPE_EPOCH: AtomicU64 = AtomicU64::new(1);
const MAX_PREPARED_SEMANTIC_PREVIEW_CHARS: usize = 4_096;
const SCOPE_LOCK_SHARDS: usize = 64;
static SCOPE_LOCKS: LazyLock<Vec<Arc<Mutex<()>>>> = LazyLock::new(|| {
    (0..SCOPE_LOCK_SHARDS)
        .map(|_| Arc::new(Mutex::new(())))
        .collect()
});
static SCOPE_NOTIFIERS: LazyLock<Vec<Arc<Notify>>> = LazyLock::new(|| {
    (0..SCOPE_LOCK_SHARDS)
        .map(|_| Arc::new(Notify::new()))
        .collect()
});

/// 同一会话的队列、回复代数和消息生命周期必须共用线性化点；分片只减少
/// 不同会话之间的锁竞争，不改变同一会话内的顺序。
pub(crate) fn scope_mutex(scope: ReplyScope) -> Arc<Mutex<()>> {
    Arc::clone(&SCOPE_LOCKS[scope_shard(scope)])
}

fn scope_shard(scope: ReplyScope) -> usize {
    let mut hasher = DefaultHasher::new();
    scope.hash(&mut hasher);
    (hasher.finish() as usize) % SCOPE_LOCK_SHARDS
}

fn scope_notifier(scope: ReplyScope) -> Arc<Notify> {
    Arc::clone(&SCOPE_NOTIFIERS[scope_shard(scope)])
}

/// 对尚未完成语义判定的入站先保留当前代数；确认内容失效，或调用通用打断时，
/// 才推进代数，使此前的模型结果和未发气泡失效。
pub(crate) async fn interrupt(scope: ReplyScope) -> ReplyTicket {
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    interrupt_locked(scope).await
}

pub(crate) async fn interrupt_locked(scope: ReplyScope) -> ReplyTicket {
    let mut states = REPLY_STATES.lock().await;
    prune_states(&mut states);
    let state = states.entry(scope).or_default();
    expire_coordination(scope, state, Instant::now());
    advance_generation(scope, state, OutgoingState::Superseded)
}

/// Freeze the current Prepared envelope while the already-admitted inbound
/// turn obtains its semantic classification. A second inbound reservation or
/// a sender that has begun final revalidation wins by forcing the caller onto
/// the normal fail-closed generation advance instead.
pub(crate) async fn try_freeze_prepared_for_incoming_locked(
    scope: ReplyScope,
) -> Option<(OutgoingToken, OutgoingSource, u64)> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&scope)?;
    let now = Instant::now();
    expire_coordination(scope, state, now);
    if state.pending_incoming.is_some() || state.pending_precommit.is_some() {
        return None;
    }
    prune_outgoing(state);
    let (token, source) = state
        .pending_outgoing
        .iter()
        .rev()
        .find(|pending| {
            pending.state == OutgoingState::Prepared && ticket_matches(state, pending.token.ticket)
        })
        .map(|pending| (pending.token, pending.source))?;
    let fail_closed_terminal = match source {
        OutgoingSource::Reply => OutgoingState::Superseded,
        OutgoingSource::Proactive => OutgoingState::Cancelled,
    };
    state.incoming_sequence = state.incoming_sequence.wrapping_add(1).max(1);
    let reservation_id = state.incoming_sequence;
    state.pending_incoming = Some(PendingIncoming {
        reservation_id,
        ticket: token.ticket,
        frozen_token: Some(token),
        fail_closed_terminal,
        expires_at: now + INCOMING_RESERVATION_LEASE,
        resolved: watch::channel(false).0,
    });
    state.last_seen = now;
    Some((token, source, reservation_id))
}

/// Reserve a newly advanced inbound generation until its handler becomes
/// active. This closes the queueing window where a proactive fallback used to
/// observe a false idle state and supersede an accepted user turn.
pub(crate) async fn reserve_incoming_locked(ticket: ReplyTicket) -> Option<u64> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&ticket.scope)?;
    let now = Instant::now();
    expire_coordination(ticket.scope, state, now);
    if !ticket_matches(state, ticket) || state.pending_incoming.is_some() {
        return None;
    }
    state.incoming_sequence = state.incoming_sequence.wrapping_add(1).max(1);
    let reservation_id = state.incoming_sequence;
    state.pending_incoming = Some(PendingIncoming {
        reservation_id,
        ticket,
        frozen_token: None,
        fail_closed_terminal: OutgoingState::Superseded,
        expires_at: now + INCOMING_RESERVATION_LEASE,
        resolved: watch::channel(false).0,
    });
    state.last_seen = now;
    Some(reservation_id)
}

/// Keep the current generation protected while an inbound turn is being
/// semantically classified against an in-flight reply. Multiple messages may
/// be waiting for the same active reply, so each admission receives its own
/// bounded reservation and can release it independently.
pub(crate) async fn reserve_active_incoming_locked(ticket: ReplyTicket) -> Option<u64> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&ticket.scope)?;
    let now = Instant::now();
    expire_coordination(ticket.scope, state, now);
    if !ticket_matches(state, ticket) {
        return None;
    }
    state.incoming_sequence = state.incoming_sequence.wrapping_add(1).max(1);
    let reservation_id = state.incoming_sequence;
    state.active_incoming.push_back(ActiveIncomingReservation {
        reservation_id,
        ticket,
        expires_at: now + INCOMING_RESERVATION_LEASE,
        resolved: watch::channel(false).0,
    });
    state.last_seen = now;
    Some(reservation_id)
}

/// Release an active reservation by its id. Reservation ids are unique within
/// a scope and remain authoritative when a semantic replacement rebinds the
/// reservation to a successor generation.
pub(crate) async fn release_active_incoming_by_id_locked(
    scope: ReplyScope,
    reservation_id: u64,
) -> Option<ReplyTicket> {
    if reservation_id == 0 {
        return None;
    }
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&scope)?;
    expire_coordination(scope, state, Instant::now());
    let position = state
        .active_incoming
        .iter()
        .position(|pending| pending.reservation_id == reservation_id)?;
    let reservation = state
        .active_incoming
        .remove(position)
        .expect("the active incoming reservation position must remain valid");
    let ticket = reservation.ticket;
    reservation.resolved.send_replace(true);
    state.last_seen = Instant::now();
    scope_notifier(scope).notify_waiters();
    Some(ticket)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn release_active_incoming(ticket: ReplyTicket, reservation_id: u64) -> bool {
    let lock = scope_mutex(ticket.scope);
    let _scope_guard = lock.lock().await;
    release_active_incoming_by_id_locked(ticket.scope, reservation_id)
        .await
        .is_some()
}

pub(crate) async fn release_incoming_locked(
    ticket: ReplyTicket,
    reservation_id: u64,
    frozen_token: Option<OutgoingToken>,
    fail_closed: bool,
) -> bool {
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&ticket.scope) else {
        return false;
    };
    expire_coordination(ticket.scope, state, Instant::now());
    let Some(reservation) = state.pending_incoming.take() else {
        return false;
    };
    if reservation.ticket != ticket
        || reservation.reservation_id != reservation_id
        || reservation.frozen_token != frozen_token
    {
        state.pending_incoming = Some(reservation);
        return false;
    }
    reservation.resolved.send_replace(true);
    scope_notifier(ticket.scope).notify_waiters();
    let frozen_token_is_current = reservation.frozen_token.is_some_and(|token| {
        ticket_matches(state, ticket)
            && state
                .pending_outgoing
                .iter()
                .any(|pending| pending.token == token && pending.state == OutgoingState::Prepared)
    });
    if fail_closed && frozen_token_is_current {
        advance_generation(ticket.scope, state, reservation.fail_closed_terminal);
    } else {
        state.last_seen = Instant::now();
    }
    true
}

pub(crate) async fn incoming_reservation_matches_locked(
    ticket: ReplyTicket,
    reservation_id: u64,
    frozen_token: Option<OutgoingToken>,
) -> bool {
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&ticket.scope) else {
        return false;
    };
    expire_coordination(ticket.scope, state, Instant::now());
    state.pending_incoming.as_ref().is_some_and(|pending| {
        pending.ticket == ticket
            && pending.reservation_id == reservation_id
            && pending.frozen_token == frozen_token
    })
}

pub(crate) async fn active_incoming_reservation_matches_locked(
    ticket: ReplyTicket,
    reservation_id: u64,
) -> bool {
    if reservation_id == 0 {
        return true;
    }
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&ticket.scope) else {
        return false;
    };
    expire_coordination(ticket.scope, state, Instant::now());
    state
        .active_incoming
        .iter()
        .any(|pending| pending.ticket == ticket && pending.reservation_id == reservation_id)
}

/// Wait until an active admission reaches the head of its scope's FIFO. The
/// returned ticket is the reservation's current ticket after any generation
/// hand-off. A missing reservation means it was resolved or invalidated.
pub(crate) async fn wait_for_active_incoming_turn(
    scope: ReplyScope,
    reservation_id: u64,
) -> Option<ReplyTicket> {
    if reservation_id == 0 {
        return current_ticket(scope).await;
    }
    let notifier = scope_notifier(scope);
    loop {
        let mut notified = Box::pin(notifier.notified());
        notified.as_mut().enable();
        let wait = {
            let lock = scope_mutex(scope);
            let _scope_guard = lock.lock().await;
            let mut states = REPLY_STATES.lock().await;
            let state = states.get_mut(&scope)?;
            let now = Instant::now();
            expire_coordination(scope, state, now);
            let position = state
                .active_incoming
                .iter()
                .position(|pending| pending.reservation_id == reservation_id)?;
            let reservation = &state.active_incoming[position];
            if position == 0 {
                return Some(reservation.ticket);
            }
            reservation.expires_at.saturating_duration_since(now)
        };
        let _ = kovi::tokio::time::timeout(wait, notified).await;
    }
}

/// Wait until all active semantic admissions have resolved. Direct control
/// responses use this after replacing an in-flight reply so the generic
/// tracked sender cannot observe a later marker and fail with `ConversationBusy`.
pub(crate) async fn wait_for_active_incoming_clear(scope: ReplyScope) -> bool {
    let notifier = scope_notifier(scope);
    loop {
        // Register before checking state so a resolution cannot race the
        // check and leave the waiter asleep indefinitely.
        let mut notified = Box::pin(notifier.notified());
        notified.as_mut().enable();
        let remaining = {
            let lock = scope_mutex(scope);
            let _scope_guard = lock.lock().await;
            let mut states = REPLY_STATES.lock().await;
            let Some(state) = states.get_mut(&scope) else {
                return true;
            };
            let now = Instant::now();
            expire_coordination(scope, state, now);
            state
                .active_incoming
                .iter()
                .map(|pending| pending.expires_at.saturating_duration_since(now))
                .min()
        };
        let Some(remaining) = remaining else {
            return true;
        };
        let _ = kovi::tokio::time::timeout(remaining, notified).await;
    }
}

/// Replace the active reply for the front admission while preserving all
/// later admissions. The remaining reservations are rebound to the returned
/// successor ticket, keeping their FIFO order across the generation change.
pub(crate) async fn supersede_active_incoming_locked(
    scope: ReplyScope,
    reservation_id: u64,
    prepared_terminal: OutgoingState,
) -> Option<ReplyTicket> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&scope)?;
    expire_coordination(scope, state, Instant::now());
    if reservation_id == 0 {
        return Some(advance_generation_preserving_active(
            scope,
            state,
            prepared_terminal,
        ));
    }
    let position = state
        .active_incoming
        .iter()
        .position(|pending| pending.reservation_id == reservation_id)?;
    if position != 0 {
        return None;
    }
    let reservation = state
        .active_incoming
        .pop_front()
        .expect("the active incoming reservation must remain present");
    reservation.resolved.send_replace(true);
    // A previous replacement may already own a normal reservation on this
    // generation while this later active marker reaches the front. Reusing
    // that successor keeps the earlier turn alive; advancing here would
    // otherwise discard its still-unclaimed admission.
    if state.active_generation.is_none()
        && let Some(pending) = state.pending_incoming.as_ref().filter(|pending| {
            pending.frozen_token.is_none() && ticket_matches(state, pending.ticket)
        })
    {
        let next = pending.ticket;
        scope_notifier(scope).notify_waiters();
        return Some(next);
    }
    let next = advance_generation_preserving_active(scope, state, prepared_terminal);
    scope_notifier(scope).notify_waiters();
    Some(next)
}

/// A stop request advances the conversation just like any other interrupt, but
/// records a cancellation rather than a semantic supersession.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn cancel_locked(scope: ReplyScope) -> ReplyTicket {
    let mut states = REPLY_STATES.lock().await;
    prune_states(&mut states);
    let state = states.entry(scope).or_default();
    expire_coordination(scope, state, Instant::now());
    advance_generation(scope, state, OutgoingState::Cancelled)
}

/// Cancel an ingress only if no newer event has crossed the conversation
/// linearization point. The previous generation may already have been marked
/// `Superseded` by the fail-closed ingress pass; when semantic understanding
/// confirms Stop, reclassify every retained unsent supersession in this scope
/// as `Cancelled`. A coalesced batch can advance more than one generation
/// before its final semantic result is available.
pub(crate) async fn cancel_if_current_locked(ticket: ReplyTicket) -> Option<ReplyTicket> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&ticket.scope)?;
    if !ticket_matches(state, ticket) {
        return None;
    }
    for pending in &mut state.pending_outgoing {
        if pending.state == OutgoingState::Superseded {
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
    if !ticket_matches(state, completed)
        || state.active_generation.is_some()
        || state.pending_incoming.is_some()
        || !state.active_incoming.is_empty()
    {
        return None;
    }
    state.generation = state.generation.wrapping_add(1);
    state.active_generation = Some(state.generation);
    state.last_seen = Instant::now();
    Some(ReplyTicket {
        scope: completed.scope,
        scope_epoch: state.scope_epoch,
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

/// Whether a ticket still belongs to the same scope instance. Normal inbound
/// turns preserve this epoch; data erasure removes the state, so late network
/// completions from the old instance cannot recreate persisted history.
pub(crate) async fn is_scope_epoch_current(ticket: ReplyTicket) -> bool {
    let lock = scope_mutex(ticket.scope);
    let _scope_guard = lock.lock().await;
    REPLY_STATES
        .lock()
        .await
        .get(&ticket.scope)
        .is_some_and(|state| state.scope_epoch == ticket.scope_epoch)
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
    expire_coordination(ticket.scope, state, Instant::now());
    if !ticket_matches(state, ticket) {
        return false;
    }
    // A later reply must not claim a generation while an earlier active
    // admission is still awaiting semantic refinement. The coordinator will
    // either release that admission or rebind it to a successor generation
    // before allowing the next turn to start.
    let owner_pending = state
        .pending_incoming
        .as_ref()
        .is_some_and(|pending| pending.ticket == ticket && pending.frozen_token.is_none());
    if !owner_pending
        && state
            .active_incoming
            .iter()
            .any(|pending| pending.ticket == ticket)
    {
        return false;
    }
    if owner_pending && let Some(pending) = state.pending_incoming.take() {
        pending.resolved.send_replace(true);
        scope_notifier(ticket.scope).notify_waiters();
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
        if ticket_matches(state, ticket) && state.active_generation == Some(ticket.generation) {
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

/// Return the latest coordination ticket for a scope. This is intentionally
/// separate from `active_ticket_locked`: a completed drainer may need to adopt
/// a successor generation after an active admission performed a semantic
/// replacement.
pub(crate) async fn current_ticket(scope: ReplyScope) -> Option<ReplyTicket> {
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    current_ticket_locked(scope).await
}

pub(crate) async fn current_ticket_locked(scope: ReplyScope) -> Option<ReplyTicket> {
    let states = REPLY_STATES.lock().await;
    let state = states.get(&scope)?;
    Some(ticket_for_state(scope, state))
}

fn ticket_for_state(scope: ReplyScope, state: &ReplyState) -> ReplyTicket {
    ReplyTicket {
        scope,
        scope_epoch: state.scope_epoch,
        generation: state.generation,
        conversation_version: state.conversation_version,
    }
}

/// Return the current coordination ticket while the caller holds
/// `scope_mutex`. This includes semantic admissions waiting behind a turn
/// whose active handler has just finished; later ingress must not supersede
/// that still-unresolved admission.
pub(crate) async fn active_ticket_locked(scope: ReplyScope) -> Option<ReplyTicket> {
    let states = REPLY_STATES.lock().await;
    let state = states.get(&scope)?;
    if let Some(generation) = state.active_generation
        && generation == state.generation
    {
        return Some(ticket_for_state(scope, state));
    }
    if let Some(pending) = state
        .pending_incoming
        .as_ref()
        .filter(|pending| pending.frozen_token.is_none() && ticket_matches(state, pending.ticket))
    {
        return Some(pending.ticket);
    }
    state
        .active_incoming
        .front()
        .filter(|pending| ticket_matches(state, pending.ticket))
        .map(|pending| pending.ticket)
}

/// Remove all reply-generation state for a scope while its `scope_mutex` is
/// held by the caller. Data erasure uses this after the Core FIFO barrier,
/// because a drained adapter may have recreated state after the first cancel.
pub(crate) async fn clear_reply_state_locked(scope: ReplyScope) -> bool {
    let removed = REPLY_STATES.lock().await.remove(&scope);
    if let Some(mut state) = removed {
        clear_pending_incoming(scope, &mut state);
        true
    } else {
        false
    }
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

#[cfg(test)]
pub(crate) async fn prepare_outgoing(
    ticket: ReplyTicket,
    fingerprint: u64,
    source: OutgoingSource,
) -> Option<OutgoingToken> {
    prepare_outgoing_with_semantic_preview(ticket, fingerprint, source, None).await
}

pub(crate) async fn prepare_outgoing_with_semantic_preview(
    ticket: ReplyTicket,
    fingerprint: u64,
    source: OutgoingSource,
    semantic_preview: Option<&str>,
) -> Option<OutgoingToken> {
    let lock = scope_mutex(ticket.scope);
    let _scope_guard = lock.lock().await;
    prepare_outgoing_locked(ticket, fingerprint, source, semantic_preview).await
}

/// Prepare a new proactive envelope only while the conversation is idle.
///
/// The idle check and generation advance share the conversation linearization
/// point with inbound admission. This keeps a scheduler from superseding an
/// active reactive turn; if ingress wins the lock after this function, the
/// normal executive policy can still defer the prepared proactive envelope.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn prepare_proactive_outgoing_if_idle(
    scope: ReplyScope,
    fingerprint: u64,
) -> Option<OutgoingToken> {
    prepare_proactive_outgoing_if_idle_with_semantic_preview(scope, fingerprint, None).await
}

pub(crate) async fn prepare_proactive_outgoing_if_idle_with_semantic_preview(
    scope: ReplyScope,
    fingerprint: u64,
    semantic_preview: Option<&str>,
) -> Option<OutgoingToken> {
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    if has_pending_incoming_locked(scope).await
        || is_active_locked(scope).await
        || prepared_outgoing_source_locked(scope).await.is_some()
    {
        return None;
    }

    let ticket = interrupt_locked(scope).await;
    if !claim_active_locked(ticket).await {
        return None;
    }
    let outgoing = prepare_outgoing_locked(
        ticket,
        fingerprint,
        OutgoingSource::Proactive,
        semantic_preview,
    )
    .await;
    finish_locked(ticket).await;
    outgoing
}

pub(crate) async fn has_pending_incoming_locked(scope: ReplyScope) -> bool {
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&scope) else {
        return false;
    };
    expire_coordination(scope, state, Instant::now());
    state.pending_incoming.is_some() || !state.active_incoming.is_empty()
}

/// Check whether an admission has another unresolved inbound turn ahead of it.
/// The caller must hold `scope_mutex`; the admission's own reservation is
/// deliberately excluded so a first turn can claim its generation normally.
pub(crate) async fn has_other_pending_incoming_locked(
    ticket: ReplyTicket,
    reservation_id: u64,
) -> bool {
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&ticket.scope) else {
        return false;
    };
    expire_coordination(ticket.scope, state, Instant::now());
    state
        .pending_incoming
        .as_ref()
        .is_some_and(|pending| pending.ticket != ticket || pending.reservation_id != reservation_id)
        || state
            .active_incoming
            .iter()
            .any(|pending| pending.ticket != ticket || pending.reservation_id != reservation_id)
}

/// Check whether an inbound admission still blocks the completed ticket from
/// claiming a queued follow-up. The caller must hold `scope_mutex`.
pub(crate) async fn pending_incoming_for_ticket_locked(ticket: ReplyTicket) -> bool {
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&ticket.scope) else {
        return false;
    };
    expire_coordination(ticket.scope, state, Instant::now());
    if !ticket_matches(state, ticket) {
        return false;
    }
    state
        .pending_incoming
        .as_ref()
        .is_some_and(|pending| pending.ticket == ticket)
        || state
            .active_incoming
            .iter()
            .any(|pending| pending.ticket == ticket)
}

/// Wait until all admissions attached to `ticket` resolve, or until the
/// ticket becomes stale. The notifier is process-local and deliberately
/// independent of `ReplyState`, so data-erasure removal also wakes waiters.
pub(crate) async fn wait_for_pending_incoming(ticket: ReplyTicket) -> bool {
    let notifier = scope_notifier(ticket.scope);
    loop {
        // Register before checking state so `notify_waiters` cannot slip
        // between the check and the await without waking this waiter.
        let mut notified = Box::pin(notifier.notified());
        notified.as_mut().enable();
        let remaining = {
            let lock = scope_mutex(ticket.scope);
            let _scope_guard = lock.lock().await;
            let mut states = REPLY_STATES.lock().await;
            let Some(state) = states.get_mut(&ticket.scope) else {
                return false;
            };
            let now = Instant::now();
            expire_coordination(ticket.scope, state, now);
            if !ticket_matches(state, ticket) {
                return false;
            }
            let next_expiry = state
                .pending_incoming
                .as_ref()
                .filter(|pending| pending.ticket == ticket)
                .map(|pending| pending.expires_at)
                .into_iter()
                .chain(
                    state
                        .active_incoming
                        .iter()
                        .filter(|pending| pending.ticket == ticket)
                        .map(|pending| pending.expires_at),
                )
                .min();
            let Some(next_expiry) = next_expiry else {
                return true;
            };
            next_expiry.saturating_duration_since(now)
        };
        let _ = kovi::tokio::time::timeout(remaining, notified).await;
    }
}

async fn prepare_outgoing_locked(
    ticket: ReplyTicket,
    fingerprint: u64,
    source: OutgoingSource,
    semantic_preview: Option<&str>,
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
        semantic_preview: semantic_preview.map(bounded_semantic_preview),
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

fn bounded_semantic_preview(content: &str) -> String {
    let mut chars = content.chars();
    let mut preview = chars
        .by_ref()
        .take(MAX_PREPARED_SEMANTIC_PREVIEW_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        preview.push_str("\n[truncated]");
    }
    preview
}

/// The only transition into the irreversible side-effect region. The caller
/// must resolve and authorize its destination before calling this function and
/// must perform the network request only after this function releases the
/// conversation lock.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn commit_outgoing(token: OutgoingToken) -> bool {
    commit_outgoing_with_context(token, token.fingerprint, None)
        .await
        .is_ok()
}

#[cfg_attr(not(test), allow(dead_code))]
async fn commit_outgoing_with_context(
    token: OutgoingToken,
    effective_fingerprint: u64,
    idempotency_key: Option<&str>,
) -> Result<(), OutgoingCommitRejection> {
    let mut prepared = begin_outgoing_commit(token).await?;
    prepared
        .commit_state(effective_fingerprint, idempotency_key)
        .await
        .map(|_| ())
}

/// Wait for any earlier semantic admission before pinning the token for final
/// route and authorization checks. Active-reply admissions are installed only
/// on the Host-serialized path, so this wait cannot deadlock Core's FIFO.
pub(crate) async fn begin_outgoing_commit(
    token: OutgoingToken,
) -> Result<PreparedOutgoingCommit, OutgoingCommitRejection> {
    loop {
        let wait = {
            let lock = scope_mutex(token.ticket.scope);
            let _scope_guard = lock.lock().await;
            let mut states = REPLY_STATES.lock().await;
            let Some(state) = states.get_mut(&token.ticket.scope) else {
                return Err(OutgoingCommitRejection::Stale);
            };
            let now = Instant::now();
            expire_coordination(token.ticket.scope, state, now);
            if !ticket_matches(state, token.ticket)
                || !state.pending_outgoing.iter().any(|pending| {
                    pending.token == token && pending.state == OutgoingState::Prepared
                })
            {
                supersede_prepared(state, token);
                return Err(OutgoingCommitRejection::Stale);
            }
            if let Some(incoming) = state
                .pending_incoming
                .as_ref()
                .filter(|incoming| incoming.frozen_token == Some(token))
            {
                Some((
                    incoming.resolved.subscribe(),
                    incoming.expires_at.saturating_duration_since(now),
                ))
            } else if let Some(incoming) = state
                .active_incoming
                .iter()
                .find(|incoming| incoming.ticket == token.ticket)
            {
                Some((
                    incoming.resolved.subscribe(),
                    incoming.expires_at.saturating_duration_since(now),
                ))
            } else {
                if state.pending_precommit.is_some() {
                    return Err(OutgoingCommitRejection::Stale);
                }
                state.pending_precommit = Some(PendingPrecommit {
                    token,
                    expires_at: now + PRECOMMIT_VALIDATION_LEASE,
                });
                state.last_seen = now;
                return Ok(PreparedOutgoingCommit { token: Some(token) });
            }
        };
        if let Some((mut resolved, remaining)) = wait
            && !*resolved.borrow()
        {
            let _ = kovi::tokio::time::timeout(remaining, resolved.changed()).await;
        }
    }
}

async fn commit_prevalidated_outgoing(
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
    expire_coordination(token.ticket.scope, state, Instant::now());
    if !ticket_matches(state, token.ticket)
        || state
            .pending_precommit
            .is_none_or(|pending| pending.token != token)
    {
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
        && state
            .pending_outgoing
            .iter()
            .enumerate()
            .any(|(index, pending)| {
                index != pending_index
                    && pending.idempotency_key.as_deref() == Some(idempotency_key)
                    && matches!(
                        pending.state,
                        OutgoingState::Committed | OutgoingState::Sent | OutgoingState::Unknown
                    )
            })
    {
        let pending = &mut state.pending_outgoing[pending_index];
        pending.state = OutgoingState::Cancelled;
        pending.terminal_at = Some(Instant::now());
        state.pending_precommit = None;
        return Err(OutgoingCommitRejection::DuplicateIdempotency);
    }
    let pending = &mut state.pending_outgoing[pending_index];
    pending.state = OutgoingState::Committed;
    pending.effective_fingerprint = effective_fingerprint;
    pending.idempotency_key = idempotency_key.map(ToOwned::to_owned);
    pending.committed_at = Some(Instant::now());
    state.pending_precommit = None;
    state.last_seen = Instant::now();
    schedule_committed_expiry(token);
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn commit_outgoing_guard(token: OutgoingToken) -> Option<CommittedOutgoing> {
    commit_outgoing(token)
        .await
        .then_some(CommittedOutgoing { token: Some(token) })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn commit_outgoing_guard_with_context(
    token: OutgoingToken,
    effective_fingerprint: u64,
    idempotency_key: Option<&str>,
) -> Result<CommittedOutgoing, OutgoingCommitRejection> {
    begin_outgoing_commit(token)
        .await?
        .commit(effective_fingerprint, idempotency_key)
        .await
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
            pending.state == OutgoingState::Prepared && ticket_matches(state, pending.token.ticket)
        })
        .map(|pending| pending.source)
}

/// Inspect the source only when the exact frozen envelope is still Prepared.
/// A replacement on the same ticket must never inherit an older admission's
/// semantic decision.
pub(crate) async fn prepared_outgoing_source_for_token_locked(
    token: OutgoingToken,
) -> Option<OutgoingSource> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&token.ticket.scope)?;
    prune_outgoing(state);
    if !ticket_matches(state, token.ticket) {
        return None;
    }
    state
        .pending_outgoing
        .iter()
        .find(|pending| pending.token == token && pending.state == OutgoingState::Prepared)
        .map(|pending| pending.source)
}

/// Return model context only for the exact envelope held by the current
/// inbound reservation. Callers must hold `scope_mutex` while using it.
pub(crate) async fn prepared_semantic_preview_for_token_locked(
    token: OutgoingToken,
) -> Option<String> {
    let mut states = REPLY_STATES.lock().await;
    let state = states.get_mut(&token.ticket.scope)?;
    prune_outgoing(state);
    if !ticket_matches(state, token.ticket) {
        return None;
    }
    state
        .pending_outgoing
        .iter()
        .find(|pending| pending.token == token && pending.state == OutgoingState::Prepared)
        .and_then(|pending| pending.semantic_preview.clone())
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

#[cfg_attr(not(test), allow(dead_code))]
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
    if state
        .pending_precommit
        .is_some_and(|pending| pending.token == token)
    {
        state.pending_precommit = None;
    }
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
        pending.state = OutgoingState::Unknown;
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

/// Restore collision observations that could not cross the Core queue. The
/// scope must still exist; data erasure wins over restoring stale telemetry.
pub(crate) async fn restore_message_collisions(
    scope: ReplyScope,
    collisions: Vec<MessageCollision>,
) -> usize {
    if collisions.is_empty() {
        return 0;
    }
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    let mut states = REPLY_STATES.lock().await;
    let Some(state) = states.get_mut(&scope) else {
        return 0;
    };
    let mut restored = 0;
    for collision in collisions.into_iter().rev() {
        if state.collisions.contains(&collision) {
            continue;
        }
        if state.collisions.len() == MAX_COLLISIONS_PER_SCOPE {
            state.collisions.pop_back();
        }
        state.collisions.push_front(collision);
        restored += 1;
    }
    state.last_seen = Instant::now();
    restored
}

fn advance_generation(
    scope: ReplyScope,
    state: &mut ReplyState,
    prepared_terminal: OutgoingState,
) -> ReplyTicket {
    advance_generation_with_active(scope, state, prepared_terminal, false)
}

/// Advance a generation while retaining unresolved active admissions. This is
/// used only after the FIFO-front admission has explicitly chosen Rewrite or
/// Merge; all later reservations are rebound to the successor ticket instead
/// of being discarded as stale.
fn advance_generation_preserving_active(
    scope: ReplyScope,
    state: &mut ReplyState,
    prepared_terminal: OutgoingState,
) -> ReplyTicket {
    advance_generation_with_active(scope, state, prepared_terminal, true)
}

fn advance_generation_with_active(
    scope: ReplyScope,
    state: &mut ReplyState,
    prepared_terminal: OutgoingState,
    preserve_active: bool,
) -> ReplyTicket {
    let now = Instant::now();
    if preserve_active {
        if let Some(incoming) = state.pending_incoming.take() {
            incoming.resolved.send_replace(true);
            scope_notifier(scope).notify_waiters();
        }
    } else {
        clear_pending_incoming(scope, state);
    }
    state.pending_precommit = None;
    for pending in &mut state.pending_outgoing {
        match pending.state {
            OutgoingState::Prepared => {
                pending.state = prepared_terminal;
                pending.terminal_at = Some(now);
            }
            OutgoingState::Committed | OutgoingState::Sent | OutgoingState::Unknown => {
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
    scope_notifier(scope).notify_waiters();
    prune_outgoing(state);
    let next = ticket_for_state(scope, state);
    if preserve_active {
        for incoming in &mut state.active_incoming {
            incoming.ticket = next;
        }
    }
    next
}

fn clear_pending_incoming(scope: ReplyScope, state: &mut ReplyState) {
    let mut cleared = false;
    if let Some(incoming) = state.pending_incoming.take() {
        incoming.resolved.send_replace(true);
        cleared = true;
    }
    for incoming in state.active_incoming.drain(..) {
        incoming.resolved.send_replace(true);
        cleared = true;
    }
    if cleared {
        scope_notifier(scope).notify_waiters();
    }
}

fn expire_coordination(scope: ReplyScope, state: &mut ReplyState, now: Instant) {
    let expired_incoming = state
        .pending_incoming
        .as_ref()
        .is_some_and(|pending| pending.expires_at <= now);
    if expired_incoming && let Some(incoming) = state.pending_incoming.take() {
        incoming.resolved.send_replace(true);
        scope_notifier(scope).notify_waiters();
        let frozen_token_is_current = incoming.frozen_token.is_some_and(|token| {
            ticket_matches(state, incoming.ticket)
                && state.pending_outgoing.iter().any(|pending| {
                    pending.token == token && pending.state == OutgoingState::Prepared
                })
        });
        if frozen_token_is_current {
            advance_generation(scope, state, incoming.fail_closed_terminal);
        }
    }

    // Remove only expired active admissions. Expiration is a lease cleanup,
    // not evidence that the in-flight reply became meaningless; therefore it
    // must never advance the generation or cancel a still-valid reply.
    let mut active_incoming_expired = false;
    let mut retained_active_incoming = VecDeque::with_capacity(state.active_incoming.len());
    while let Some(incoming) = state.active_incoming.pop_front() {
        if incoming.expires_at <= now {
            incoming.resolved.send_replace(true);
            active_incoming_expired = true;
        } else {
            retained_active_incoming.push_back(incoming);
        }
    }
    state.active_incoming = retained_active_incoming;
    if active_incoming_expired {
        state.last_seen = now;
        scope_notifier(scope).notify_waiters();
    }

    let expired_precommit = state
        .pending_precommit
        .is_some_and(|pending| pending.expires_at <= now);
    if expired_precommit && let Some(precommit) = state.pending_precommit.take() {
        if let Some(pending) = state.pending_outgoing.iter_mut().find(|pending| {
            pending.token == precommit.token && pending.state == OutgoingState::Prepared
        }) {
            pending.state = OutgoingState::Cancelled;
            pending.terminal_at = Some(now);
        }
        state.last_seen = now;
    }
}

fn ticket_matches(state: &ReplyState, ticket: ReplyTicket) -> bool {
    state.scope_epoch == ticket.scope_epoch
        && state.generation == ticket.generation
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
                OutgoingState::Prepared | OutgoingState::Committed | OutgoingState::Unknown
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
            pending.state = OutgoingState::Unknown;
            pending.terminal_at = Some(now);
        }
    }
    state
        .pending_outgoing
        .retain(|pending| match pending.state {
            OutgoingState::Prepared | OutgoingState::Committed => true,
            OutgoingState::Unknown => pending.terminal_at.is_some_and(|terminal_at| {
                now.duration_since(terminal_at) < OUTGOING_UNKNOWN_RETENTION
            }),
            OutgoingState::Sent | OutgoingState::Cancelled | OutgoingState::Superseded => {
                pending.terminal_at.is_some_and(|terminal_at| {
                    now.duration_since(terminal_at) < OUTGOING_TERMINAL_RETENTION
                })
            }
        });
}

fn prune_states(states: &mut HashMap<ReplyScope, ReplyState>) {
    if states.len() <= 2_048 {
        return;
    }
    states.retain(|_, state| {
        state.active_generation.is_some()
            || state.pending_incoming.is_some()
            || !state.active_incoming.is_empty()
            || state.pending_precommit.is_some()
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
        OutgoingToken, REPLY_STATES, ReplyScope, cancel_locked, cancel_prepared_proactive_locked,
        claim_follow_up, clear_reply_state_locked, commit_outgoing, commit_outgoing_guard,
        commit_outgoing_guard_with_context, contextual_outgoing_fingerprint,
        find_prepared_outgoing, finish, interrupt, interrupt_if_current, is_active, is_current,
        mark_active, mark_outgoing_failed, mark_outgoing_sent, outgoing_fingerprint,
        prepare_outgoing, prepare_proactive_outgoing_if_idle, prepared_outgoing_source_locked,
        release_active_incoming, reserve_active_incoming_locked, scope_mutex,
        take_message_collisions, wait_for_pending_incoming,
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
        let wait = kovi::tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if outgoing_state(token).await == Some(expected) {
                    break;
                }
                kovi::tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await;
        if wait.is_err() {
            assert_eq!(outgoing_state(token).await, Some(expected));
        }
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
    fn proactive_fallback_cannot_supersede_an_active_reactive_turn() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_000_009);
                let reactive = interrupt(scope).await;
                assert!(mark_active(reactive).await);

                assert!(
                    prepare_proactive_outgoing_if_idle(
                        scope,
                        outgoing_fingerprint("scheduled reach-out"),
                    )
                    .await
                    .is_none()
                );
                assert!(is_current(reactive).await);
                assert!(is_active(scope).await);

                let reply = prepare_outgoing(
                    reactive,
                    outgoing_fingerprint("user reply"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("active reactive turn must remain able to prepare its reply");
                mark_outgoing_failed(reply).await;
                finish(reactive).await;
            });
    }

    #[test]
    fn proactive_fallback_does_not_supersede_a_prepared_reactive_reply() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_000_010);
                let reactive = interrupt(scope).await;
                assert!(mark_active(reactive).await);
                let reply = prepare_outgoing(
                    reactive,
                    outgoing_fingerprint("prepared user reply"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reactive reply should prepare");
                finish(reactive).await;

                assert!(
                    prepare_proactive_outgoing_if_idle(
                        scope,
                        outgoing_fingerprint("scheduled reach-out"),
                    )
                    .await
                    .is_none()
                );
                assert!(is_current(reactive).await);
                assert!(commit_outgoing(reply).await);
                mark_outgoing_failed(reply).await;
            });
    }

    #[test]
    fn proactive_fallback_prepares_when_idle_and_remains_interruptible() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_000_011);
                let fingerprint = outgoing_fingerprint("scheduled reach-out");
                let proactive = prepare_proactive_outgoing_if_idle(scope, fingerprint)
                    .await
                    .expect("idle conversation should accept proactive preparation");
                assert!(!is_active(scope).await);
                assert_eq!(
                    find_prepared_outgoing(scope, fingerprint).await,
                    Some((proactive, OutgoingSource::Proactive))
                );

                let inbound = interrupt(scope).await;
                assert!(is_current(inbound).await);
                assert!(!commit_outgoing(proactive).await);
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
    fn follow_up_waits_for_all_active_admissions_to_resolve() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_000_012);
                let completed = interrupt(scope).await;
                assert!(mark_active(completed).await);
                let first = {
                    let lock = scope_mutex(scope);
                    let _guard = lock.lock().await;
                    reserve_active_incoming_locked(completed)
                        .await
                        .expect("应保留第一条活动入站")
                };
                let second = {
                    let lock = scope_mutex(scope);
                    let _guard = lock.lock().await;
                    reserve_active_incoming_locked(completed)
                        .await
                        .expect("应保留第二条活动入站")
                };
                finish(completed).await;

                assert!(claim_follow_up(completed).await.is_none());
                let waiter =
                    kovi::tokio::spawn(async move { wait_for_pending_incoming(completed).await });
                kovi::tokio::task::yield_now().await;
                assert!(!waiter.is_finished());

                assert!(release_active_incoming(completed, first).await);
                kovi::tokio::task::yield_now().await;
                assert!(!waiter.is_finished());
                assert!(claim_follow_up(completed).await.is_none());

                assert!(release_active_incoming(completed, second).await);
                assert!(waiter.await.expect("等待任务应完成"));
                let follow_up = claim_follow_up(completed)
                    .await
                    .expect("所有语义入站完成后应能领取排队回复");
                finish(follow_up).await;
            });
    }

    #[test]
    fn expired_admission_after_reply_finish_does_not_invalidate_the_queue_ticket() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_000_013);
                let completed = interrupt(scope).await;
                assert!(mark_active(completed).await);
                let reservation = {
                    let lock = scope_mutex(scope);
                    let _guard = lock.lock().await;
                    reserve_active_incoming_locked(completed)
                        .await
                        .expect("应保留活动入站")
                };
                finish(completed).await;
                {
                    let mut states = REPLY_STATES.lock().await;
                    let state = states.get_mut(&scope).expect("应保留会话状态");
                    let pending = state
                        .active_incoming
                        .iter_mut()
                        .find(|pending| pending.reservation_id == reservation)
                        .expect("应保留 admission");
                    pending.expires_at = std::time::Instant::now();
                }

                assert!(wait_for_pending_incoming(completed).await);
                assert!(is_current(completed).await);
                let follow_up = claim_follow_up(completed)
                    .await
                    .expect("过期 admission 释放后应能领取排队回复");
                finish(follow_up).await;
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
                let outgoing =
                    prepare_outgoing(ticket, prepared_fingerprint, OutgoingSource::Reply)
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
    fn aborting_a_committed_send_keeps_the_side_effect_fail_closed() {
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

                wait_for_outgoing_state(outgoing, OutgoingState::Unknown).await;
                let _newer = interrupt(scope).await;
                assert_eq!(take_message_collisions(scope).await.len(), 1);
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
