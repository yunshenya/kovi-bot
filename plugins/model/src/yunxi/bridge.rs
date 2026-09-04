//! Bounded QQ -> Yunxi Core bridge.
//!
//! The bridge deliberately sits beside the existing Kovi handlers. It copies
//! only the small set of fields needed by Core, then resolves platform
//! identities on a single background worker. Core owns admitted supported
//! messages; Host handlers remain responsible for specialized capabilities.

use super::qq;
use crate::model::{
    IncomingAdmission, ReplyScope, is_recent_bot_message, restore_message_collisions,
    take_message_collisions,
};
use anyhow::Context;
use chrono::{DateTime, TimeZone, Utc};
use kovi::RuntimeBot;
use kovi::bot::message::Message;
use kovi::event::{GroupMsgEvent, PrivateMsgEvent};
use kovi::tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, mpsc, oneshot};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use yunxi_core::{
    ActionArbiter, ActionArbiterConfig, ActionPort, ActionResult, Admission, Attachment,
    AttachmentKind, ChannelAdapter, CognitiveIntent, CognitiveRuntime, ConversationId,
    ConversationKind, ConversationMemberStore, ConversationTurnDirective, CoreServices,
    EventPriority, EventScope, EventType, ExternalConversation, IdentityStore,
    MessageCollisionDetectedEvent, MessageContent, MessageId, MessageReceivedEvent, ModelBackend,
    OpenLoopStore, PersonId, PlannedProcessingOutcome, ProcessingOutcome, ProposedAction,
    RuntimeConfig, RuntimeHandle, WorldEvent, WorldEventKind,
};

pub(crate) const CORE_INGRESS_CAPACITY: usize = 256;
pub(crate) const MESSAGE_REFERENCE_CAPACITY: usize = 4_096;
/// Upper bound for a visible host callback waiting for Core ingress space.
///
/// The queue is intentionally bounded, but an unbounded `send().await` here
/// would let a stalled identity/runtime worker pin every Kovi message handler.
const CORE_RELIABLE_INGRESS_TIMEOUT: Duration = Duration::from_secs(5);
/// Total budget for one message on the serialized ingress worker. This covers
/// identity resolution, durable quote checks, and runtime admission; a slow
/// database must not hold every later reply behind it indefinitely.
const CORE_INGRESS_PROCESSING_TIMEOUT: Duration = Duration::from_secs(10);
/// Commands that require an ingress acknowledgement must not wait forever for
/// either queue capacity or a stalled worker. An acknowledgement timeout is
/// intentionally reported as indeterminate because the command was accepted.
const CORE_INGRESS_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// An admitted action must not hold the serialized ingress worker while a
/// host adapter waits forever on a database, authorization, or platform API.
/// The arbiter marks an admitted reservation indeterminate if this future is
/// cancelled, so the timeout cannot make a later retry duplicate a send.
/// Keep this above the transport's bounded enqueue + response waits so the
/// adapter can persist its own definite or indeterminate terminal outcome.
const CORE_ACTION_DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// First action acknowledgement deadline after the command has entered ingress.
/// A command that is still pending at this boundary is atomically cancelled;
/// one already claimed by the worker receives a separate completion grace.
const CORE_ACTION_INITIAL_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(50);
/// Once the ingress worker has claimed an action, leave enough time for its
/// complete bounded dispatch even if it started just before the initial
/// acknowledgement deadline, including bounded runtime feedback admission.
const CORE_ACTION_COMPLETION_GRACE: Duration =
    CORE_ACTION_DISPATCH_TIMEOUT.saturating_add(Duration::from_secs(10));
/// High-priority runtime events use backpressure in `RuntimeHandle::submit`.
/// Keep that wait finite so one unhealthy runtime cannot stop the ingress loop.
const CORE_RUNTIME_SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);
/// A durable reply mapping is authoritative, but it must not turn a transient
/// database outage into a permanently blocked Core ingress worker.
const CORE_REPLY_MAPPING_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_TRACKED_USERS: usize = 256;
const MAX_TRACKED_DIRECT_CONVERSATIONS_PER_USER: usize = 256;
const MAX_BLOCKED_USERS: usize = 256;
const MAX_BLOCKED_GROUPS: usize = 256;
const MAX_PRIVATE_HANDLER_GATES: usize = 1_024;
const MAX_MESSAGE_CHARS: usize = 8_192;
const MAX_MESSAGE_BYTES: usize = 32 * 1_024;
const MAX_AMBIENT_ATTENTION_GATES: usize = 256;

#[derive(Debug, Default)]
struct AmbientAttentionGate {
    eligible_messages_since_sample: u32,
    last_candidate: Option<Instant>,
    decision_attempts: VecDeque<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct AmbientAttentionPolicy {
    enabled: bool,
    min_eligible_messages: u32,
    candidate_cooldown_secs: u64,
    response_probability_percent: u8,
    min_message_chars: usize,
    decision_rate_window_secs: u64,
    decision_rate_limit: usize,
}

#[derive(Debug)]
struct AmbientAttentionRegistry {
    entries: HashMap<i64, AmbientAttentionGate>,
    order: VecDeque<i64>,
}

impl AmbientAttentionRegistry {
    fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(MAX_AMBIENT_ATTENTION_GATES),
            order: VecDeque::with_capacity(MAX_AMBIENT_ATTENTION_GATES),
        }
    }

    fn gate_mut(&mut self, group_id: i64) -> &mut AmbientAttentionGate {
        if !self.entries.contains_key(&group_id) {
            if self.entries.len() >= MAX_AMBIENT_ATTENTION_GATES
                && let Some(evicted) = self.order.pop_front()
            {
                self.entries.remove(&evicted);
            }
            self.order.push_back(group_id);
            self.entries
                .insert(group_id, AmbientAttentionGate::default());
        }
        self.entries
            .get_mut(&group_id)
            .expect("ambient attention gate was inserted above")
    }

    fn should_request(
        &mut self,
        group_id: i64,
        message_id: i32,
        text_chars: usize,
        has_image: bool,
        policy: AmbientAttentionPolicy,
    ) -> bool {
        if !policy.enabled || (!has_image && text_chars < policy.min_message_chars) {
            return false;
        }
        let now = Instant::now();
        let gate = self.gate_mut(group_id);
        gate.eligible_messages_since_sample = gate.eligible_messages_since_sample.saturating_add(1);
        if gate.eligible_messages_since_sample < policy.min_eligible_messages {
            return false;
        }
        gate.eligible_messages_since_sample = 0;

        if gate.last_candidate.is_some_and(|last| {
            now.duration_since(last) < Duration::from_secs(policy.candidate_cooldown_secs)
        }) {
            return false;
        }
        let rate_window = Duration::from_secs(policy.decision_rate_window_secs);
        while gate
            .decision_attempts
            .front()
            .is_some_and(|attempt| now.duration_since(*attempt) >= rate_window)
        {
            gate.decision_attempts.pop_front();
        }
        if gate.decision_attempts.len() >= policy.decision_rate_limit {
            return false;
        }

        let mut hasher = DefaultHasher::new();
        group_id.hash(&mut hasher);
        message_id.hash(&mut hasher);
        let sample = hasher.finish() % 100;
        if sample >= u64::from(policy.response_probability_percent) {
            return false;
        }

        gate.last_candidate = Some(now);
        gate.decision_attempts.push_back(now);
        true
    }

    fn clear(&mut self, group_id: i64) {
        self.entries.remove(&group_id);
        self.order.retain(|candidate| *candidate != group_id);
    }
}

impl Default for AmbientAttentionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of the synchronous, non-blocking ingress operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    Accepted,
    DroppedAtCapacity,
    Blocked,
    SkippedInvalid,
}

/// The Core-owned handling mode selected before conversation admission.
/// Observation-only group chatter must never interrupt an in-flight reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupCoreHandling {
    Unsupported,
    Observe,
    Decide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupHandlingDecision {
    pub(crate) handling: GroupCoreHandling,
    /// True only for a locally sampled ambient turn. Quote candidates cross
    /// ingress first and receive visible permission only after their durable
    /// target mapping is resolved by the worker.
    pub(crate) planner_attention_requested: bool,
    /// True for an explicit message-count command that should remain Core-owned
    /// even while another reply is active. This includes @self/reply candidates;
    /// the worker still resolves whether a reply target is actually Yunxi.
    pub(crate) explicit_batch_request: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ActionCommandState {
    Pending = 0,
    Running = 1,
    Finished = 2,
    Cancelled = 3,
}

#[derive(Debug)]
struct ActionCommandControl {
    state: AtomicU8,
}

impl ActionCommandControl {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(ActionCommandState::Pending as u8),
        }
    }

    fn state(&self) -> ActionCommandState {
        match self.state.load(Ordering::Acquire) {
            value if value == ActionCommandState::Pending as u8 => ActionCommandState::Pending,
            value if value == ActionCommandState::Running as u8 => ActionCommandState::Running,
            value if value == ActionCommandState::Finished as u8 => ActionCommandState::Finished,
            value if value == ActionCommandState::Cancelled as u8 => ActionCommandState::Cancelled,
            _ => unreachable!("action command state is always written by this type"),
        }
    }

    fn claim(&self) -> bool {
        self.state
            .compare_exchange(
                ActionCommandState::Pending as u8,
                ActionCommandState::Running as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn cancel_pending(&self) -> bool {
        self.state
            .compare_exchange(
                ActionCommandState::Pending as u8,
                ActionCommandState::Cancelled as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish(&self) {
        let transitioned = self.state.compare_exchange(
            ActionCommandState::Running as u8,
            ActionCommandState::Finished as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        debug_assert_eq!(transitioned, Ok(ActionCommandState::Running as u8));
    }
}

struct PendingActionCommandCancellation<'a> {
    control: &'a ActionCommandControl,
}

impl Drop for PendingActionCommandCancellation<'_> {
    fn drop(&mut self) {
        self.control.cancel_pending();
    }
}

enum IngressCommand {
    Message(InboundMessage),
    ProjectDestination {
        destination: crate::model::MessageDestination,
        priority: EventPriority,
        kind: WorldEventKind,
        acknowledge: oneshot::Sender<Result<Admission, String>>,
    },
    FlushMessageCollisions {
        sender_user_id: i64,
        address: ConversationAddress,
        acknowledge: oneshot::Sender<anyhow::Result<usize>>,
    },
    DispatchAction {
        user_id: i64,
        action: ProposedAction,
        control: Arc<ActionCommandControl>,
        acknowledge: oneshot::Sender<Result<Option<ActionResult>, IngressCommandError>>,
    },
    BeginDataErasure {
        user_id: i64,
        acknowledge: oneshot::Sender<anyhow::Result<DataErasureAck>>,
    },
    EndDataErasure {
        user_id: i64,
        blocked_user_ids: Vec<i64>,
        runtime_barrier_person_id: yunxi_core::PersonId,
        acknowledge: oneshot::Sender<anyhow::Result<()>>,
    },
    BeginGroupDataErasure {
        group_id: i64,
        acknowledge: oneshot::Sender<anyhow::Result<GroupDataErasureAck>>,
    },
    EndGroupDataErasure {
        group_id: i64,
        conversation_ids: Vec<ConversationId>,
        acknowledge: oneshot::Sender<anyhow::Result<()>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IngressCommandError {
    message: String,
    indeterminate: bool,
}

impl IngressCommandError {
    fn definite(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            indeterminate: false,
        }
    }

    fn indeterminate(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            indeterminate: true,
        }
    }

    pub(crate) const fn is_indeterminate(&self) -> bool {
        self.indeterminate
    }
}

impl fmt::Display for IngressCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IngressCommandError {}

async fn send_ingress_command_with_ack<T>(
    ingress: &mpsc::Sender<IngressCommand>,
    command: IngressCommand,
    acknowledged: oneshot::Receiver<T>,
    wait: Duration,
    operation: &'static str,
) -> Result<T, IngressCommandError> {
    send_ingress_command_with_ack_timeouts(ingress, command, acknowledged, wait, wait, operation)
        .await
}

async fn send_ingress_command_with_ack_timeouts<T>(
    ingress: &mpsc::Sender<IngressCommand>,
    command: IngressCommand,
    acknowledged: oneshot::Receiver<T>,
    enqueue_wait: Duration,
    acknowledgement_wait: Duration,
    operation: &'static str,
) -> Result<T, IngressCommandError> {
    // Tokio's bounded `send` is cancellation-safe: an enqueue timeout means
    // the command was not sent. It also detects a receiver that closes at the
    // capacity boundary, which `Permit::send` cannot report.
    match kovi::tokio::time::timeout(enqueue_wait, ingress.send(command)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(IngressCommandError::definite(format!(
                "Yunxi {operation} ingress is closed"
            )));
        }
        Err(_) => {
            return Err(IngressCommandError::definite(format!(
                "Yunxi {operation} ingress enqueue timed out after {}ms",
                enqueue_wait.as_millis()
            )));
        }
    }
    match kovi::tokio::time::timeout(acknowledgement_wait, acknowledged).await {
        Ok(Ok(result)) => Ok(result),
        // The command was already accepted by the bounded ingress queue. A
        // worker cancellation or crash can happen after a host side effect,
        // so a lost acknowledgement is never proof that the operation did
        // not run.
        Ok(Err(_)) => Err(IngressCommandError::indeterminate(format!(
            "Yunxi {operation} acknowledgement was dropped after enqueue; outcome may be indeterminate"
        ))),
        Err(_) => Err(IngressCommandError::indeterminate(format!(
            "Yunxi {operation} acknowledgement timed out after {}ms; outcome may be indeterminate",
            acknowledgement_wait.as_millis()
        ))),
    }
}

async fn send_action_ingress_command_with_ack<T>(
    ingress: &mpsc::Sender<IngressCommand>,
    command: IngressCommand,
    mut acknowledged: oneshot::Receiver<T>,
    control: &ActionCommandControl,
    enqueue_wait: Duration,
    initial_acknowledgement_wait: Duration,
    completion_grace: Duration,
) -> Result<T, IngressCommandError> {
    match kovi::tokio::time::timeout(enqueue_wait, ingress.send(command)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(IngressCommandError::definite(
                "Yunxi action dispatch ingress is closed",
            ));
        }
        Err(_) => {
            return Err(IngressCommandError::definite(format!(
                "Yunxi action dispatch ingress enqueue timed out after {}ms",
                enqueue_wait.as_millis()
            )));
        }
    }
    let _cancellation = PendingActionCommandCancellation { control };

    match kovi::tokio::time::timeout(initial_acknowledgement_wait, &mut acknowledged).await {
        Ok(Ok(result)) => return Ok(result),
        Ok(Err(_)) => {
            if control.cancel_pending() || control.state() == ActionCommandState::Cancelled {
                return Err(IngressCommandError::definite(
                    "Yunxi action dispatch acknowledgement was dropped before execution",
                ));
            }
            return Err(IngressCommandError::indeterminate(
                "Yunxi action dispatch acknowledgement was dropped after execution started; outcome may be indeterminate",
            ));
        }
        Err(_) => {}
    }

    if control.cancel_pending() || control.state() == ActionCommandState::Cancelled {
        return Err(IngressCommandError::definite(format!(
            "Yunxi action dispatch was cancelled before execution after waiting {}ms in ingress",
            initial_acknowledgement_wait.as_millis()
        )));
    }

    match kovi::tokio::time::timeout(completion_grace, &mut acknowledged).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Err(IngressCommandError::indeterminate(
            "Yunxi action dispatch acknowledgement was dropped after execution started; outcome may be indeterminate",
        )),
        Err(_) => match acknowledged.try_recv() {
            Ok(result) => Ok(result),
            Err(_) => Err(IngressCommandError::indeterminate(format!(
                "Yunxi action dispatch did not finish within the {}ms completion grace after execution started; outcome may be indeterminate",
                completion_grace.as_millis()
            ))),
        },
    }
}

#[derive(Debug, Clone)]
struct DataErasureAck {
    canonical_person_id: Option<yunxi_core::PersonId>,
    runtime_barrier_person_id: yunxi_core::PersonId,
    blocked_user_ids: Vec<i64>,
    direct_conversation_ids: Vec<ConversationId>,
    purged_conversations: usize,
    cleared_references: usize,
    cleared_person_routes: usize,
    cleared_conversation_routes: usize,
    cleared_tracked_routes: usize,
}

#[derive(Debug, Clone)]
struct GroupDataErasureAck {
    conversation_ids: Vec<ConversationId>,
    purged_runtime_states: usize,
    cleared_references: usize,
    cleared_person_routes: usize,
    cleared_conversation_routes: usize,
}

pub(crate) struct GroupDataErasure {
    bridge: Arc<CoreBridge>,
    group_id: i64,
    conversation_ids: Vec<ConversationId>,
    #[cfg_attr(not(test), allow(dead_code))]
    ack: GroupDataErasureAck,
    finished: bool,
}

impl GroupDataErasure {
    pub(crate) fn conversation_ids(&self) -> &[ConversationId] {
        &self.conversation_ids
    }

    pub(crate) async fn finish(mut self) -> anyhow::Result<()> {
        self.bridge
            .end_group_data_erasure(self.group_id, self.conversation_ids.clone())
            .await?;
        self.finished = true;
        Ok(())
    }

    #[cfg(test)]
    fn ack(&self) -> GroupDataErasureAck {
        self.ack.clone()
    }
}

impl Drop for GroupDataErasure {
    fn drop(&mut self) {
        if !self.finished {
            kovi::log::warn!(
                "Yunxi group data-erasure barrier dropped without resume (QQ group: {})",
                self.group_id
            );
        }
    }
}

pub(crate) struct UserDataErasure {
    bridge: Arc<CoreBridge>,
    user_id: i64,
    blocked_user_ids: Vec<i64>,
    runtime_barrier_person_id: yunxi_core::PersonId,
    #[cfg_attr(not(test), allow(dead_code))]
    ack: DataErasureAck,
    finished: bool,
}

impl UserDataErasure {
    pub(crate) fn qq_user_ids(&self) -> &[i64] {
        &self.blocked_user_ids
    }

    pub(crate) const fn canonical_person_id(&self) -> Option<yunxi_core::PersonId> {
        self.ack.canonical_person_id
    }

    pub(crate) fn direct_conversation_ids(&self) -> &[ConversationId] {
        &self.ack.direct_conversation_ids
    }

    pub(crate) async fn finish(mut self) -> anyhow::Result<()> {
        self.bridge
            .end_user_data_erasure(
                self.user_id,
                self.blocked_user_ids.clone(),
                self.runtime_barrier_person_id,
            )
            .await?;
        self.finished = true;
        Ok(())
    }

    #[cfg(test)]
    fn ack(&self) -> DataErasureAck {
        self.ack.clone()
    }
}

impl Drop for UserDataErasure {
    fn drop(&mut self) {
        if !self.finished {
            kovi::log::warn!(
                "Yunxi data-erasure barrier dropped without resume (QQ user: {})",
                self.user_id
            );
        }
    }
}

#[derive(Debug)]
struct PrivateHandlerGate {
    lock: Arc<RwLock<()>>,
    epoch: AtomicU64,
    deletion_pending: AtomicBool,
}

impl PrivateHandlerGate {
    fn new() -> Self {
        Self {
            lock: Arc::new(RwLock::new(())),
            epoch: AtomicU64::new(0),
            deletion_pending: AtomicBool::new(false),
        }
    }
}

#[derive(Debug, Default)]
struct PrivateHandlerGateEntries {
    entries: HashMap<i64, Arc<PrivateHandlerGate>>,
    order: VecDeque<i64>,
}

#[derive(Debug)]
struct PrivateHandlerGateRegistry {
    entries: StdMutex<PrivateHandlerGateEntries>,
    capacity: usize,
}

impl PrivateHandlerGateRegistry {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "private handler gates must be bounded");
        Self {
            entries: StdMutex::new(PrivateHandlerGateEntries::default()),
            capacity,
        }
    }

    fn gate(&self, user_id: i64) -> Option<Arc<PrivateHandlerGate>> {
        let mut state = self.entries.lock().ok()?;
        if let Some(gate) = state.entries.get(&user_id).cloned() {
            state.order.retain(|candidate| *candidate != user_id);
            state.order.push_back(user_id);
            return Some(gate);
        }
        while state.entries.len() >= self.capacity {
            let position = state.order.iter().position(|candidate| {
                state.entries.get(candidate).is_some_and(|gate| {
                    Arc::strong_count(gate) == 1 && !gate.deletion_pending.load(Ordering::Acquire)
                })
            })?;
            let evicted = state.order.remove(position)?;
            state.entries.remove(&evicted);
        }
        let gate = Arc::new(PrivateHandlerGate::new());
        state.entries.insert(user_id, Arc::clone(&gate));
        state.order.push_back(user_id);
        Some(gate)
    }

    fn deletion_pending(&self, user_id: i64) -> bool {
        self.entries.lock().map_or(true, |state| {
            state
                .entries
                .get(&user_id)
                .is_some_and(|gate| gate.deletion_pending.load(Ordering::Acquire))
        })
    }

    fn capture_data_erasure(&self, user_id: i64) -> Option<PrivateDataErasureToken> {
        let gate = self.gate(user_id)?;
        gate.deletion_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        let epoch = gate.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        Some(PrivateDataErasureToken {
            gate,
            epoch,
            armed: true,
        })
    }
}

pub(crate) struct PrivateHandlerToken {
    gate: Arc<PrivateHandlerGate>,
    epoch: u64,
}

impl PrivateHandlerToken {
    pub(crate) async fn enter(self) -> Option<PrivateHandlerPermit> {
        let gate = self.gate;
        let guard = Arc::clone(&gate.lock).read_owned().await;
        if gate.epoch.load(Ordering::Acquire) != self.epoch
            || gate.deletion_pending.load(Ordering::Acquire)
        {
            return None;
        }
        Some(PrivateHandlerPermit {
            _guard: guard,
            _gate: gate,
        })
    }
}

pub(crate) struct PrivateHandlerPermit {
    _guard: OwnedRwLockReadGuard<()>,
    _gate: Arc<PrivateHandlerGate>,
}

pub(crate) struct PrivateDataErasureToken {
    gate: Arc<PrivateHandlerGate>,
    epoch: u64,
    armed: bool,
}

impl PrivateDataErasureToken {
    pub(crate) async fn enter(mut self) -> Option<PrivateDataErasurePermit> {
        let guard = Arc::clone(&self.gate.lock).write_owned().await;
        if self.gate.epoch.load(Ordering::Acquire) != self.epoch
            || !self.gate.deletion_pending.load(Ordering::Acquire)
        {
            return None;
        }
        self.armed = false;
        Some(PrivateDataErasurePermit {
            _guard: guard,
            gate: Arc::clone(&self.gate),
        })
    }
}

impl Drop for PrivateDataErasureToken {
    fn drop(&mut self) {
        if self.armed {
            self.gate.deletion_pending.store(false, Ordering::Release);
            self.gate.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }
}

pub(crate) struct PrivateDataErasurePermit {
    _guard: OwnedRwLockWriteGuard<()>,
    gate: Arc<PrivateHandlerGate>,
}

impl Drop for PrivateDataErasurePermit {
    fn drop(&mut self) {
        self.gate.deletion_pending.store(false, Ordering::Release);
        self.gate.epoch.fetch_add(1, Ordering::AcqRel);
    }
}

/// A handle held by the host's event closures. It owns no Kovi event and never
/// performs an await on the hot path.
#[derive(Clone)]
pub(crate) struct CoreBridge {
    ingress: mpsc::Sender<IngressCommand>,
    runtime: RuntimeHandle,
    action_arbiter: Option<Arc<ActionArbiter>>,
    action_port: Option<Arc<dyn ActionPort>>,
    blocked_users: Arc<StdMutex<HashSet<i64>>>,
    blocked_groups: Arc<StdMutex<HashSet<i64>>>,
    private_handler_gates: Arc<PrivateHandlerGateRegistry>,
    group_handler_gates: Arc<PrivateHandlerGateRegistry>,
    ambient_attention: Arc<StdMutex<AmbientAttentionRegistry>>,
}

impl fmt::Debug for CoreBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoreBridge")
            .field("action_arbiter", &self.action_arbiter.is_some())
            .field("action_port", &self.action_port.is_some())
            .field(
                "blocked_users",
                &self
                    .blocked_users
                    .lock()
                    .map_or(MAX_BLOCKED_USERS, |users| users.len()),
            )
            .field(
                "blocked_groups",
                &self
                    .blocked_groups
                    .lock()
                    .map_or(MAX_BLOCKED_GROUPS, |groups| groups.len()),
            )
            .field("private_handler_gates", &"bounded per-user epochs")
            .field("group_handler_gates", &"bounded per-group epochs")
            .field("ambient_attention", &"bounded per-group sampling")
            .finish_non_exhaustive()
    }
}

impl CoreBridge {
    #[allow(dead_code)]
    pub(crate) fn start(store: Arc<dyn IdentityStore>) -> Arc<Self> {
        Self::start_inner(store, None, None, None, None, None)
    }

    #[allow(dead_code)]
    pub(crate) fn start_with_open_loops(
        store: Arc<dyn IdentityStore>,
        open_loop_store: Arc<dyn OpenLoopStore>,
    ) -> Arc<Self> {
        Self::start_inner(store, Some(open_loop_store), None, None, None, None)
    }

    /// Start the bridge with a real Kovi action adapter. The old constructor
    /// remains observe-only for tests and hosts that have not opted into action
    /// execution yet.
    pub(crate) fn start_with_open_loops_and_actions(
        store: Arc<super::identity_store::PostgresIdentityStore>,
        open_loop_store: Arc<dyn OpenLoopStore>,
        bot: Arc<RuntimeBot>,
    ) -> Arc<Self> {
        let intrinsic = super::intrinsic_runtime::install();
        let model = super::core_model::KoviModelBackend::new_with_intrinsic(
            Arc::clone(&bot),
            Arc::clone(&store),
            Arc::clone(&intrinsic),
        );
        let open_loop_store_for_adapter = Arc::clone(&open_loop_store);
        let goal_store_for_adapter: Arc<dyn yunxi_core::GoalStore> = super::goal_store()
            .expect("Yunxi goal store must be initialized before the action adapter");
        let adapter = super::delivery::QqActionAdapter::new(
            bot,
            Arc::clone(&store),
            open_loop_store_for_adapter,
            goal_store_for_adapter,
            model.tool_turn_registry(),
        );
        let mut services = CoreServices::new(Arc::clone(&model) as Arc<dyn ModelBackend>)
            .with_identity(Arc::clone(&store) as Arc<dyn IdentityStore>)
            .with_conversation_members(
                Arc::clone(&store) as Arc<dyn yunxi_core::ConversationMemberStore>
            )
            .with_open_loops(Arc::clone(&open_loop_store) as Arc<dyn OpenLoopStore>);
        if let Some(memory) = super::memory_store() {
            services = services.with_memory(memory);
        }
        if let Some(relations) = super::relation_store() {
            services = services.with_relations(relations);
        }
        if let Some(affect) = super::affect_store() {
            services = services.with_affect(affect);
        }
        if let Some(goals) = super::goal_store() {
            services = services.with_goals(goals);
        }
        if let Some(provider) = super::mind_runtime() {
            services = services.with_mind_snapshot_provider(provider);
        }
        Self::start_inner(
            store.clone(),
            Some(open_loop_store),
            Some(adapter),
            Some(services),
            Some(model),
            Some(Arc::clone(&store)),
        )
    }

    fn start_inner(
        store: Arc<dyn IdentityStore>,
        open_loop_store: Option<Arc<dyn OpenLoopStore>>,
        action_adapter: Option<Arc<super::delivery::QqActionAdapter>>,
        services: Option<CoreServices>,
        model_backend: Option<Arc<super::core_model::KoviModelBackend>>,
        message_store: Option<Arc<super::identity_store::PostgresIdentityStore>>,
    ) -> Arc<Self> {
        let (ingress, receiver) = mpsc::channel(CORE_INGRESS_CAPACITY);
        let blocked_users = Arc::new(StdMutex::new(HashSet::with_capacity(
            MAX_BLOCKED_USERS.min(32),
        )));
        let blocked_groups = Arc::new(StdMutex::new(HashSet::with_capacity(
            MAX_BLOCKED_GROUPS.min(32),
        )));
        let private_handler_gates =
            Arc::new(PrivateHandlerGateRegistry::new(MAX_PRIVATE_HANDLER_GATES));
        let group_handler_gates =
            Arc::new(PrivateHandlerGateRegistry::new(MAX_PRIVATE_HANDLER_GATES));
        let ambient_attention = Arc::new(StdMutex::new(AmbientAttentionRegistry::new()));
        let (runtime_handle, mut runtime) = services.map_or_else(
            || {
                CognitiveRuntime::new(RuntimeConfig::default())
                    .expect("default Yunxi runtime configuration must be valid")
            },
            |services| {
                CognitiveRuntime::new_with_services(RuntimeConfig::default(), services)
                    .expect("default Yunxi runtime configuration must be valid")
            },
        );
        let intrinsic = model_backend
            .as_ref()
            .map(|backend| backend.intrinsic_runtime())
            .unwrap_or_else(super::intrinsic_runtime::install);
        let executive =
            yunxi_core::ExecutiveController::new(crate::config::get().executive().policy())
                .expect("validated Yunxi Executive policy must be constructible");
        let current_capability = intrinsic.capability_snapshot();
        executive
            .set_capability(current_capability.clone())
            .expect("validated Intrinsic capability snapshot must be accepted");
        if let Some(mut persisted) = super::executive_bootstrap_snapshot() {
            // Health, external availability, and the loaded manifest are
            // startup facts. Never trust a previous process's capability bit.
            persisted.cognitive_capability = current_capability;
            if let Err(error) = executive.restore_snapshot(persisted) {
                kovi::log::warn!(
                    "Yunxi Executive bootstrap restore was rejected; starting with clean state: {error}"
                );
            }
        }
        let _ = super::install_executive_controller(executive.clone());
        runtime.set_executive(executive);
        kovi::tokio::spawn(async {
            if let Err(error) = super::persist_executive_snapshot().await {
                kovi::log::warn!("Yunxi Executive initial persistence failed: {error}");
            }
        });
        let mind_config = crate::config::get().mind().clone();
        if mind_config.mind_planner_enabled()
            && let Some(provider) = super::mind_runtime()
        {
            runtime.install_mind_snapshot_provider(provider);
            runtime.set_mind_influence_mode(mind_config.influence_mode());
            runtime
                .set_mind_snapshot_limits(mind_config.snapshot_limits())
                .expect("validated Mind snapshot limits");
            runtime.set_mind_snapshot_timeout(std::time::Duration::from_millis(
                mind_config.snapshot_timeout_ms(),
            ));
        }

        let (action_arbiter, action_port): (
            Option<Arc<ActionArbiter>>,
            Option<Arc<dyn ActionPort>>,
        ) = action_adapter.map_or((None, None), |adapter| {
            let resolver: Arc<dyn yunxi_core::DeliveryResolver> = adapter.clone();
            let arbiter = ActionArbiter::new(
                ActionArbiterConfig::default().with_capabilities(adapter.capabilities()),
            )
            .with_delivery_resolver(resolver);
            let port: Arc<dyn ActionPort> = adapter;
            (Some(Arc::new(arbiter)), Some(port))
        });

        let scheduler_runtime = runtime_handle.clone();
        let bridge_runtime = runtime_handle.clone();
        let incoming_releaser = model_backend
            .as_ref()
            .map(|backend| Arc::clone(backend) as Arc<dyn IncomingAdmissionReleaser>);
        let executive_store = super::executive_store();
        kovi::tokio::spawn(run_ingress(
            receiver,
            store,
            runtime_handle,
            model_backend,
            message_store,
            executive_store,
            Arc::clone(&blocked_users),
            Arc::clone(&blocked_groups),
            action_arbiter.clone(),
            action_port.clone(),
            Arc::clone(&private_handler_gates),
        ));
        kovi::tokio::spawn(run_runtime(
            runtime,
            action_arbiter.clone(),
            action_port.clone(),
            incoming_releaser,
        ));
        if let Some(open_loop_store) = open_loop_store {
            super::open_loop_scheduler::start(open_loop_store, scheduler_runtime);
        }
        Arc::new(Self {
            ingress,
            runtime: bridge_runtime,
            action_arbiter,
            action_port,
            blocked_users,
            blocked_groups,
            private_handler_gates,
            group_handler_gates,
            ambient_attention,
        })
    }

    /// Dispatch an admitted Core action through the configured host adapter,
    /// attempt to feed the result back into the same runtime event stream, and
    /// then acknowledge the terminal dispatch result. Hosts using the
    /// compatibility constructor receive `None` and keep their existing
    /// observe-only behavior.
    #[allow(dead_code)]
    pub(crate) async fn dispatch_action(
        &self,
        user_id: i64,
        action: ProposedAction,
    ) -> Result<Option<ActionResult>, IngressCommandError> {
        if !valid_qq_id(user_id)
            || self.is_user_blocked(user_id)
            || self.action_arbiter.is_none()
            || self.action_port.is_none()
        {
            return Ok(None);
        }
        let (acknowledge, acknowledged) = oneshot::channel();
        let control = Arc::new(ActionCommandControl::new());
        match send_action_ingress_command_with_ack(
            &self.ingress,
            IngressCommand::DispatchAction {
                user_id,
                action,
                control: Arc::clone(&control),
                acknowledge,
            },
            acknowledged,
            &control,
            CORE_INGRESS_COMMAND_TIMEOUT,
            CORE_ACTION_INITIAL_ACKNOWLEDGEMENT_TIMEOUT,
            CORE_ACTION_COMPLETION_GRACE,
        )
        .await
        {
            Ok(result) => match result {
                Ok(result) => Ok(result),
                Err(error) => {
                    kovi::log::warn!("{error}");
                    Err(error)
                }
            },
            Err(error) => {
                kovi::log::warn!("{error}");
                Err(error)
            }
        }
    }

    /// Submit an already-canonical host event directly to the Core runtime.
    /// Callers remain responsible for choosing priority and bounding any wait
    /// for reliable-event backpressure.
    pub(crate) async fn submit_event(
        &self,
        event: WorldEvent,
    ) -> Result<Admission, yunxi_core::SubmitError> {
        self.runtime.submit(event).await
    }

    /// Submit one autonomous conversation turn. The caller has already
    /// claimed the conversation in the host-side autonomous registry; Core
    /// still applies its normal event queue, attention, planner, arbiter, and
    /// delivery boundaries.
    pub(crate) async fn submit_autonomous_conversation_tick(
        &self,
        conversation_id: ConversationId,
        conversation_kind: ConversationKind,
        person_id: Option<PersonId>,
        claim_token: u64,
    ) -> Result<Admission, yunxi_core::SubmitError> {
        self.runtime
            .submit(WorldEvent::new(
                Utc::now(),
                EventScope::Conversation { conversation_id },
                EventPriority::Low,
                WorldEventKind::AutonomousConversationTick(
                    yunxi_core::AutonomousConversationTickEvent {
                        conversation_kind: Some(conversation_kind),
                        person_id,
                        claim_token: Some(claim_token),
                    },
                ),
            ))
            .await
    }

    pub(crate) async fn project_destination(
        &self,
        destination: crate::model::MessageDestination,
        priority: EventPriority,
        kind: WorldEventKind,
    ) -> Result<Admission, String> {
        if self.destination_is_blocked(destination) {
            return Err("Yunxi destination is blocked by a data-erasure barrier".to_string());
        }
        let (acknowledge, acknowledged) = oneshot::channel();
        send_ingress_command_with_ack(
            &self.ingress,
            IngressCommand::ProjectDestination {
                destination,
                priority,
                kind,
                acknowledge,
            },
            acknowledged,
            CORE_INGRESS_COMMAND_TIMEOUT,
            "destination projection",
        )
        .await
        .map_err(|error| error.to_string())?
    }

    #[allow(dead_code)]
    pub(crate) fn enqueue_group(
        &self,
        event: &GroupMsgEvent,
        incoming_admission: IncomingAdmission,
        replies_to_agent: bool,
    ) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_group(event, true) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.replies_to_agent_hint = replies_to_agent;
        message.incoming_admission = Some(incoming_admission);
        self.try_enqueue(message)
    }

    /// Enqueue a visible group event with bounded backpressure. Explicitly
    /// addressed messages and reply candidates get a short opportunity to
    /// wait for ingress capacity without pinning the host callback forever.
    pub(crate) async fn enqueue_group_reliably(
        &self,
        event: &GroupMsgEvent,
        incoming_admission: IncomingAdmission,
        planner_attention_requested: bool,
    ) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_group(event, planner_attention_requested)
        else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.incoming_admission = Some(incoming_admission);
        self.send_reliably(message).await
    }

    pub(crate) fn enqueue_group_observation(&self, event: &GroupMsgEvent) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_group(event, false) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.visible_reply_allowed = false;
        self.try_enqueue(message)
    }

    #[allow(dead_code)]
    pub(crate) fn enqueue_private(
        &self,
        event: &PrivateMsgEvent,
        incoming_admission: IncomingAdmission,
    ) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_private(event) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.incoming_admission = Some(incoming_admission);
        self.try_enqueue(message)
    }

    /// Enqueue a visible private event with bounded backpressure. The event
    /// remains owned by Core until the ingress worker receives it, the channel
    /// closes, or the bounded wait expires.
    pub(crate) async fn enqueue_private_reliably(
        &self,
        event: &PrivateMsgEvent,
        incoming_admission: IncomingAdmission,
    ) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_private(event) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.incoming_admission = Some(incoming_admission);
        self.send_reliably(message).await
    }

    pub(crate) fn enqueue_private_observation(&self, event: &PrivateMsgEvent) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_private(event) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.visible_reply_allowed = false;
        self.try_enqueue(message)
    }

    async fn send_reliably(&self, message: InboundMessage) -> EnqueueOutcome {
        self.send_reliably_with_timeout(message, CORE_RELIABLE_INGRESS_TIMEOUT)
            .await
    }

    async fn send_reliably_with_timeout(
        &self,
        message: InboundMessage,
        wait: Duration,
    ) -> EnqueueOutcome {
        if self.is_user_blocked(message.sender_user_id) || self.address_is_blocked(message.address)
        {
            return EnqueueOutcome::Blocked;
        }
        let metadata = (
            message.address,
            message.sender_user_id,
            message.external_message_id,
            message.visible_reply_allowed,
        );
        match kovi::tokio::time::timeout(wait, self.ingress.send(IngressCommand::Message(message)))
            .await
        {
            Ok(Ok(())) => EnqueueOutcome::Accepted,
            Ok(Err(_)) => {
                kovi::log::error!(
                    "Yunxi Core reliable ingress closed: address={:?} sender_user_id={} external_message_id={:?} visible_reply_allowed={} action=drop",
                    metadata.0,
                    metadata.1,
                    metadata.2,
                    metadata.3,
                );
                EnqueueOutcome::SkippedInvalid
            }
            Err(_) => {
                kovi::log::error!(
                    "Yunxi Core reliable ingress timed out: address={:?} sender_user_id={} external_message_id={:?} visible_reply_allowed={} wait_ms={} action=drop",
                    metadata.0,
                    metadata.1,
                    metadata.2,
                    metadata.3,
                    wait.as_millis(),
                );
                EnqueueOutcome::DroppedAtCapacity
            }
        }
    }

    /// Reliably flush collision records for a Host-owned group event. Unlike
    /// normal message ingress, this waits for queue capacity and worker
    /// acknowledgement so a full Core queue cannot hide a committed send.
    pub(crate) async fn flush_group_collisions(
        &self,
        event: &GroupMsgEvent,
    ) -> anyhow::Result<usize> {
        let Some(message) = InboundMessage::from_group(event, false) else {
            return Ok(0);
        };
        self.flush_message_collisions(message.sender_user_id, message.address)
            .await
    }

    /// Reliably flush collision records for a Host-owned direct event.
    pub(crate) async fn flush_private_collisions(
        &self,
        event: &PrivateMsgEvent,
    ) -> anyhow::Result<usize> {
        let Some(message) = InboundMessage::from_private(event) else {
            return Ok(0);
        };
        self.flush_message_collisions(message.sender_user_id, message.address)
            .await
    }

    async fn flush_message_collisions(
        &self,
        sender_user_id: i64,
        address: ConversationAddress,
    ) -> anyhow::Result<usize> {
        if self.is_user_blocked(sender_user_id) || self.address_is_blocked(address) {
            return Ok(0);
        }
        let (acknowledge, acknowledged) = oneshot::channel();
        send_ingress_command_with_ack(
            &self.ingress,
            IngressCommand::FlushMessageCollisions {
                sender_user_id,
                address,
                acknowledge,
            },
            acknowledged,
            CORE_INGRESS_COMMAND_TIMEOUT,
            "collision flush",
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
    }

    /// Whether this private event is owned by the Core direct-conversation
    /// path. Ordinary images are supported; control commands and other media
    /// stay on the Host handler so their specialized behavior remains.
    pub(crate) fn handles_private(&self, event: &PrivateMsgEvent) -> bool {
        core_cutover_enabled("YUNXI_CORE_PRIVATE_CUTOVER", true)
            && self.action_arbiter.is_some()
            && self.action_port.is_some()
            && core_private_payload_is_supported(&event.message, event.borrow_text())
            && InboundMessage::from_private(event).is_some()
    }

    /// Core owns ordinary group text/images as bounded observations. Only an
    /// explicit address or a locally sampled ambient candidate receives a
    /// reply admission; background chatter cannot supersede an active reply.
    pub(crate) fn supports_group(&self, event: &GroupMsgEvent) -> bool {
        core_cutover_enabled("YUNXI_CORE_GROUP_CUTOVER", true)
            && self.action_arbiter.is_some()
            && self.action_port.is_some()
            && core_group_payload_is_supported(&event.message, event.borrow_text())
            && InboundMessage::from_group(event, false).is_some()
    }

    pub(crate) fn classify_group(&self, event: &GroupMsgEvent) -> GroupHandlingDecision {
        if !self.supports_group(event) {
            return GroupHandlingDecision {
                handling: GroupCoreHandling::Unsupported,
                planner_attention_requested: false,
                explicit_batch_request: false,
            };
        }
        let addressed = message_at_self(&event.message, event.self_id)
            || event.borrow_text().is_some_and(text_mentions_agent);
        let explicit_batch_request = event.borrow_text().is_some_and(|text| {
            super::core_model::requested_message_count(&bounded_text(text)).is_some()
                && !event.message.iter().any(|segment| {
                    segment.type_ == "at"
                        && segment
                            .data
                            .get("qq")
                            .and_then(value_as_i64)
                            .is_some_and(|value| value != event.self_id)
                })
        });
        // A syntactically valid reply segment must cross the admission boundary
        // before any Redis/PostgreSQL lookup. The single ingress worker resolves
        // whether it actually targets Yunxi once the conversation is canonical.
        let reply_target_candidate = reply_message_id(&event.message).is_some();
        let planner_attention_requested = !addressed
            && !reply_target_candidate
            && !explicit_batch_request
            && self.should_request_ambient_attention(event);
        let handling = if addressed
            || reply_target_candidate
            || explicit_batch_request
            || planner_attention_requested
        {
            GroupCoreHandling::Decide
        } else {
            GroupCoreHandling::Observe
        };
        GroupHandlingDecision {
            handling,
            planner_attention_requested,
            explicit_batch_request,
        }
    }

    fn should_request_ambient_attention(&self, event: &GroupMsgEvent) -> bool {
        // Messages explicitly routed to another member, or quote replies that
        // require Host-side context resolution, are observations rather than
        // opportunities for an unsolicited interjection.
        if !ambient_group_payload_can_be_sampled(&event.message) {
            return false;
        }
        let text = event.borrow_text().unwrap_or_default().trim();
        let has_image = event.message.iter().any(|segment| segment.type_ == "image");
        let model_config = crate::config::get();
        let config = model_config.group_interjection();
        let policy = AmbientAttentionPolicy {
            enabled: config.enabled(),
            min_eligible_messages: config.min_eligible_messages(),
            candidate_cooldown_secs: config.cooldown_secs().max(config.decision_cooldown_secs()),
            response_probability_percent: config.response_probability_percent(),
            min_message_chars: config.min_message_chars(),
            decision_rate_window_secs: config.decision_rate_window_secs(),
            decision_rate_limit: config.decision_rate_limit(),
        };
        self.ambient_attention
            .lock()
            .ok()
            .is_some_and(|mut registry| {
                registry.should_request(
                    event.group_id,
                    event.message_id,
                    text.chars().count(),
                    has_image,
                    policy,
                )
            })
    }

    fn try_enqueue(&self, message: InboundMessage) -> EnqueueOutcome {
        if self.is_user_blocked(message.sender_user_id) || self.address_is_blocked(message.address)
        {
            kovi::log::debug!(
                "Yunxi Core ingress blocked: address={:?} sender_user_id={} external_message_id={:?} visible_reply_allowed={}",
                message.address,
                message.sender_user_id,
                message.external_message_id,
                message.visible_reply_allowed,
            );
            return EnqueueOutcome::Blocked;
        }
        let metadata = (
            message.address,
            message.sender_user_id,
            message.external_message_id,
            message.visible_reply_allowed,
        );
        match self.ingress.try_send(IngressCommand::Message(message)) {
            Ok(()) => EnqueueOutcome::Accepted,
            Err(mpsc::error::TrySendError::Full(_)) => {
                kovi::log::warn!(
                    "Yunxi Core ingress queue full: address={:?} sender_user_id={} external_message_id={:?} visible_reply_allowed={} action=drop",
                    metadata.0,
                    metadata.1,
                    metadata.2,
                    metadata.3,
                );
                EnqueueOutcome::DroppedAtCapacity
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                kovi::log::error!(
                    "Yunxi Core ingress closed: address={:?} sender_user_id={} external_message_id={:?} visible_reply_allowed={} action=drop",
                    metadata.0,
                    metadata.1,
                    metadata.2,
                    metadata.3,
                );
                EnqueueOutcome::SkippedInvalid
            }
        }
    }

    pub(crate) fn is_user_blocked(&self, user_id: i64) -> bool {
        is_blocked(self.blocked_users.as_ref(), user_id)
            || self.private_handler_gates.deletion_pending(user_id)
    }

    pub(crate) fn is_group_blocked(&self, group_id: i64) -> bool {
        is_blocked(self.blocked_groups.as_ref(), group_id)
    }

    fn address_is_blocked(&self, address: ConversationAddress) -> bool {
        matches!(address, ConversationAddress::Group { group_id } if self.is_group_blocked(group_id))
    }

    fn destination_is_blocked(&self, destination: crate::model::MessageDestination) -> bool {
        match destination {
            crate::model::MessageDestination::Group(group_id) => self.is_group_blocked(group_id),
            crate::model::MessageDestination::Private(user_id) => self.is_user_blocked(user_id),
        }
    }

    pub(crate) fn capture_private_handler(&self, user_id: i64) -> Option<PrivateHandlerToken> {
        if !valid_qq_id(user_id) || is_blocked(self.blocked_users.as_ref(), user_id) {
            return None;
        }
        let gate = self.private_handler_gates.gate(user_id)?;
        if gate.deletion_pending.load(Ordering::Acquire) {
            return None;
        }
        Some(PrivateHandlerToken {
            epoch: gate.epoch.load(Ordering::Acquire),
            gate,
        })
    }

    pub(crate) fn capture_private_data_erasure(
        &self,
        user_id: i64,
    ) -> Option<PrivateDataErasureToken> {
        if !valid_qq_id(user_id) || is_blocked(self.blocked_users.as_ref(), user_id) {
            return None;
        }
        self.private_handler_gates.capture_data_erasure(user_id)
    }

    pub(crate) fn capture_group_handler(&self, group_id: i64) -> Option<PrivateHandlerToken> {
        if !valid_qq_id(group_id) || self.is_group_blocked(group_id) {
            return None;
        }
        let gate = self.group_handler_gates.gate(group_id)?;
        if gate.deletion_pending.load(Ordering::Acquire) {
            return None;
        }
        Some(PrivateHandlerToken {
            epoch: gate.epoch.load(Ordering::Acquire),
            gate,
        })
    }

    pub(crate) fn capture_group_data_erasure(
        &self,
        group_id: i64,
    ) -> Option<PrivateDataErasureToken> {
        if !valid_qq_id(group_id) || self.is_group_blocked(group_id) {
            return None;
        }
        self.group_handler_gates.capture_data_erasure(group_id)
    }

    pub(crate) async fn begin_user_data_erasure(
        self: &Arc<Self>,
        user_id: i64,
    ) -> anyhow::Result<UserDataErasure> {
        anyhow::ensure!(valid_qq_id(user_id), "invalid QQ user id for data erasure");
        {
            let mut blocked = self
                .blocked_users
                .lock()
                .map_err(|_| anyhow::anyhow!("Yunxi blocked-user state is poisoned"))?;
            anyhow::ensure!(
                !blocked.contains(&user_id),
                "a data erasure is already active for this QQ user"
            );
            anyhow::ensure!(
                blocked.len() < MAX_BLOCKED_USERS,
                "too many concurrent Yunxi data erasures"
            );
            blocked.insert(user_id);
        }

        // The caller has already interrupted and released the host reply scope.
        // Make the FIFO begin command the first await after synchronously
        // closing the shared ingress gate, so direct action commands cannot
        // overtake deletion through the proactive-send side channel.
        let (acknowledge, acknowledged) = oneshot::channel();
        if self
            .ingress
            .send(IngressCommand::BeginDataErasure {
                user_id,
                acknowledge,
            })
            .await
            .is_err()
        {
            self.unblock_user(user_id);
            return Err(anyhow::anyhow!("Yunxi ingress is closed"));
        }
        let ack = match acknowledged.await {
            Ok(Ok(ack)) => ack,
            Ok(Err(error)) => {
                self.unblock_user(user_id);
                return Err(error);
            }
            Err(_) => {
                // The command entered the FIFO, so a missing acknowledgement
                // leaves Core's barrier state unknown. Keep the synchronous
                // host gate closed instead of risking identity recreation.
                return Err(anyhow::anyhow!(
                    "Yunxi erasure acknowledgement was dropped; ingress remains blocked"
                ));
            }
        };
        Ok(UserDataErasure {
            bridge: Arc::clone(self),
            user_id,
            blocked_user_ids: ack.blocked_user_ids.clone(),
            runtime_barrier_person_id: ack.runtime_barrier_person_id,
            ack,
            finished: false,
        })
    }

    async fn end_user_data_erasure(
        &self,
        user_id: i64,
        blocked_user_ids: Vec<i64>,
        runtime_barrier_person_id: yunxi_core::PersonId,
    ) -> anyhow::Result<()> {
        let (acknowledge, acknowledged) = oneshot::channel();
        self.ingress
            .send(IngressCommand::EndDataErasure {
                user_id,
                blocked_user_ids,
                runtime_barrier_person_id,
                acknowledge,
            })
            .await
            .map_err(|_| anyhow::anyhow!("Yunxi ingress is closed"))?;
        acknowledged
            .await
            .map_err(|_| anyhow::anyhow!("Yunxi resume acknowledgement was dropped"))??;
        self.unblock_user(user_id);
        Ok(())
    }

    pub(crate) async fn begin_group_data_erasure(
        self: &Arc<Self>,
        group_id: i64,
    ) -> anyhow::Result<GroupDataErasure> {
        anyhow::ensure!(
            valid_qq_id(group_id),
            "invalid QQ group id for data erasure"
        );
        {
            let mut blocked = self
                .blocked_groups
                .lock()
                .map_err(|_| anyhow::anyhow!("Yunxi blocked-group state is poisoned"))?;
            anyhow::ensure!(
                !blocked.contains(&group_id),
                "a data erasure is already active for this QQ group"
            );
            anyhow::ensure!(
                blocked.len() < MAX_BLOCKED_GROUPS,
                "too many concurrent Yunxi group data erasures"
            );
            blocked.insert(group_id);
        }
        if let Ok(mut attention) = self.ambient_attention.lock() {
            attention.clear(group_id);
        }

        let (acknowledge, acknowledged) = oneshot::channel();
        if self
            .ingress
            .send(IngressCommand::BeginGroupDataErasure {
                group_id,
                acknowledge,
            })
            .await
            .is_err()
        {
            self.unblock_group(group_id);
            return Err(anyhow::anyhow!("Yunxi ingress is closed"));
        }
        let ack = match acknowledged.await {
            Ok(Ok(ack)) => ack,
            Ok(Err(error)) => {
                self.unblock_group(group_id);
                return Err(error);
            }
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "Yunxi group erasure acknowledgement was dropped; ingress remains blocked"
                ));
            }
        };
        Ok(GroupDataErasure {
            bridge: Arc::clone(self),
            group_id,
            conversation_ids: ack.conversation_ids.clone(),
            ack,
            finished: false,
        })
    }

    async fn end_group_data_erasure(
        &self,
        group_id: i64,
        conversation_ids: Vec<ConversationId>,
    ) -> anyhow::Result<()> {
        let (acknowledge, acknowledged) = oneshot::channel();
        self.ingress
            .send(IngressCommand::EndGroupDataErasure {
                group_id,
                conversation_ids,
                acknowledge,
            })
            .await
            .map_err(|_| anyhow::anyhow!("Yunxi ingress is closed"))?;
        acknowledged
            .await
            .map_err(|_| anyhow::anyhow!("Yunxi group resume acknowledgement was dropped"))??;
        self.unblock_group(group_id);
        Ok(())
    }

    fn unblock_user(&self, user_id: i64) {
        if let Ok(mut blocked) = self.blocked_users.lock() {
            blocked.remove(&user_id);
        }
    }

    fn unblock_group(&self, group_id: i64) {
        unblock(self.blocked_groups.as_ref(), group_id);
    }

    /// Submit a best-effort global idle observation without waiting on the
    /// ingress or runtime queue. Low-priority runtime admission is bounded and
    /// may be dropped when the host is busy.
    pub(crate) fn observe_idle_tick(&self) {
        let runtime = self.runtime.clone();
        kovi::tokio::spawn(async move {
            if let Some(mind) = super::mind_runtime() {
                mind.trigger_reflection(yunxi_core::ReflectionTrigger::Idle)
                    .await;
            }
            let event = idle_tick_event(Utc::now());
            let _ = runtime.submit(event).await;
        });
    }

    pub(crate) fn observe_maintenance_tick(&self) {
        let runtime = self.runtime.clone();
        kovi::tokio::spawn(async move {
            if let Some(mind) = super::mind_runtime() {
                mind.trigger_reflection(yunxi_core::ReflectionTrigger::Maintenance)
                    .await;
            }
            let event = WorldEvent::new(
                Utc::now(),
                EventScope::Global,
                EventPriority::Low,
                WorldEventKind::MaintenanceTick,
            );
            let _ = runtime.submit(event).await;
        });
    }
}

#[allow(dead_code)]
fn action_result_event(
    action: &ProposedAction,
    result: &ActionResult,
    occurred_at: DateTime<Utc>,
) -> Option<WorldEvent> {
    let idempotency_key = action.idempotency_key()?.to_owned();
    let scope = match action.scope() {
        yunxi_core::ActionScope::Conversation(conversation_id) => {
            EventScope::Conversation { conversation_id }
        }
        yunxi_core::ActionScope::Person(person_id) => EventScope::Person { person_id },
        yunxi_core::ActionScope::Global => EventScope::Global,
    };
    let kind = match result {
        ActionResult::Executed {
            outcome: yunxi_core::ActionPortOutcome::ToolCompleted { operation, output },
            ..
        } => WorldEventKind::ToolCompleted(yunxi_core::ToolCompletedEvent {
            operation: operation.clone(),
            output: output.clone(),
            requires_follow_up: true,
        }),
        ActionResult::Executed {
            outcome:
                yunxi_core::ActionPortOutcome::ToolFailed {
                    operation,
                    error_category,
                    detail,
                },
            ..
        } => WorldEventKind::ToolFailed(yunxi_core::ToolFailedEvent {
            operation: operation.clone(),
            error_category: error_category.clone(),
            detail: detail.clone(),
            requires_follow_up: true,
        }),
        ActionResult::Executed {
            outcome: yunxi_core::ActionPortOutcome::Delivered { .. },
            ..
        } => WorldEventKind::ActionSucceeded(yunxi_core::ActionSucceededEvent { idempotency_key }),
        ActionResult::Executed {
            outcome: yunxi_core::ActionPortOutcome::DeliveryIndeterminate { reason, .. },
            ..
        } => WorldEventKind::ActionFailed(yunxi_core::ActionFailedEvent {
            idempotency_key,
            error_category: format!("delivery_indeterminate:{reason}"),
        }),
        ActionResult::Executed {
            outcome: yunxi_core::ActionPortOutcome::Deferred { reason },
            ..
        } => WorldEventKind::ActionFailed(yunxi_core::ActionFailedEvent {
            idempotency_key,
            error_category: format!("deferred:{reason}"),
        }),
        ActionResult::Failed { error, .. } => {
            WorldEventKind::ActionFailed(yunxi_core::ActionFailedEvent {
                idempotency_key,
                error_category: error.category.clone(),
            })
        }
        ActionResult::Rejected(rejection) => {
            WorldEventKind::ActionRejected(yunxi_core::ActionRejectedEvent {
                idempotency_key,
                reason: rejection.to_string(),
            })
        }
        ActionResult::Noop => return None,
    };
    let event = WorldEvent::new(occurred_at, scope, EventPriority::High, kind);
    Some(match action.actor() {
        Some(actor) => event.with_actor(actor),
        None => event,
    })
}

fn idle_tick_event(occurred_at: DateTime<Utc>) -> WorldEvent {
    WorldEvent::new(
        occurred_at,
        EventScope::Global,
        EventPriority::Low,
        WorldEventKind::IdleTick,
    )
}

#[derive(Debug, Clone, Copy)]
enum ConversationAddress {
    Group { group_id: i64 },
    Direct { self_id: i64, peer_user_id: i64 },
}

impl ConversationAddress {
    fn external(&self) -> Result<ExternalConversation, qq::QqReferenceError> {
        match *self {
            Self::Group { group_id } => qq::group(group_id),
            Self::Direct {
                self_id,
                peer_user_id,
            } => qq::direct(self_id, peer_user_id),
        }
    }

    fn reply_scope(self) -> ReplyScope {
        match self {
            Self::Group { group_id } => ReplyScope::Group(group_id),
            Self::Direct { peer_user_id, .. } => ReplyScope::Private(peer_user_id),
        }
    }

    fn kind(self) -> ConversationKind {
        match self {
            Self::Group { .. } => ConversationKind::Group,
            Self::Direct { .. } => ConversationKind::Direct,
        }
    }
}

/// This is the only data allowed to cross from a Kovi event into the bridge.
/// In particular, it contains no `Arc<Event>`, message segments, JSON, or bot
/// handle.
#[derive(Debug, Clone)]
struct InboundMessage {
    address: ConversationAddress,
    sender_user_id: i64,
    external_message_id: Option<i64>,
    reply_to_external_message_id: Option<i64>,
    /// Resolved during synchronous ingress classification for reply-only
    /// group messages. This avoids a second asynchronous lookup after queueing.
    replies_to_agent_hint: bool,
    text: String,
    attachments: Vec<Attachment>,
    /// Host-only locators used to materialize the current turn's images. They
    /// are bound to the Core MessageId in a bounded one-shot cache and never
    /// enter the platform-neutral event or persistent state.
    vision_attachments: Vec<crate::vision::ImageAttachment>,
    timestamp: DateTime<Utc>,
    addressed_to_agent: bool,
    visible_reply_allowed: bool,
    explicit_request: bool,
    stop_requested: bool,
    /// Ambient group turns set this only when the local sampler has decided
    /// that Core may spend a model turn evaluating a possible interjection.
    planner_attention_requested: bool,
    /// Host conversation admission captured before this event entered the
    /// asynchronous Core queues. It is consumed exactly once by the model
    /// backend and never crosses into the platform-neutral Core event schema.
    incoming_admission: Option<IncomingAdmission>,
}

impl InboundMessage {
    fn from_group(event: &GroupMsgEvent, planner_attention_requested: bool) -> Option<Self> {
        valid_qq_id(event.self_id)
            .then_some(())
            .and_then(|()| valid_qq_id(event.group_id).then_some(()))
            .and_then(|()| valid_qq_id(event.user_id).then_some(()))?;
        if event.user_id == event.self_id {
            return None;
        }
        let text = bounded_text(event.borrow_text().unwrap_or_default());
        let explicit_request = group_message_requests_explicit_batch(&event.message, &text);
        let attachments = normalize_attachments(&event.message);
        let vision_attachments = crate::vision::extract_image_attachments(&event.message);
        Some(Self {
            address: ConversationAddress::Group {
                group_id: event.group_id,
            },
            sender_user_id: event.user_id,
            external_message_id: positive_message_id(event.message_id),
            reply_to_external_message_id: reply_message_id(&event.message),
            replies_to_agent_hint: false,
            addressed_to_agent: message_at_self(&event.message, event.self_id)
                || text_mentions_agent(&text),
            visible_reply_allowed: true,
            explicit_request,
            stop_requested: false,
            planner_attention_requested,
            incoming_admission: None,
            text,
            attachments,
            vision_attachments,
            timestamp: event_timestamp(event.time),
        })
    }

    fn from_private(event: &PrivateMsgEvent) -> Option<Self> {
        valid_qq_id(event.self_id).then_some(())?;
        valid_qq_id(event.user_id).then_some(())?;
        if event.user_id == event.self_id {
            return None;
        }
        let text = bounded_text(event.borrow_text().unwrap_or_default());
        let attachments = normalize_attachments(&event.message);
        let vision_attachments = crate::vision::extract_image_attachments(&event.message);
        Some(Self {
            address: ConversationAddress::Direct {
                self_id: event.self_id,
                peer_user_id: event.user_id,
            },
            sender_user_id: event.user_id,
            external_message_id: positive_message_id(event.message_id),
            reply_to_external_message_id: reply_message_id(&event.message),
            replies_to_agent_hint: false,
            addressed_to_agent: true,
            visible_reply_allowed: true,
            explicit_request: true,
            stop_requested: false,
            planner_attention_requested: true,
            incoming_admission: None,
            text,
            attachments,
            vision_attachments,
            timestamp: event_timestamp(event.time),
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct MessageReference {
    message_id: MessageId,
    from_agent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MessageReferenceKey {
    conversation_id: ConversationId,
    external_message_id: i64,
}

/// A small LRU used only for references that have already crossed the Core
/// boundary. It is intentionally owned by the single ingress worker, so no
/// lock can be held across identity resolution.
#[derive(Debug)]
struct MessageReferenceCache {
    entries: HashMap<MessageReferenceKey, MessageReference>,
    order: VecDeque<MessageReferenceKey>,
    capacity: usize,
}

impl MessageReferenceCache {
    fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "message reference cache must be bounded and non-empty"
        );
        Self {
            entries: HashMap::with_capacity(capacity.min(128)),
            order: VecDeque::with_capacity(capacity.min(128)),
            capacity,
        }
    }

    fn get(&mut self, key: MessageReferenceKey) -> Option<MessageReference> {
        let value = self.entries.get(&key).copied()?;
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: MessageReferenceKey, value: MessageReference) {
        self.entries.insert(key, value);
        self.touch(key);
        while self.entries.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if oldest != key || self.entries.len() > self.capacity {
                self.entries.remove(&oldest);
            }
        }
    }

    fn touch(&mut self, key: MessageReferenceKey) {
        self.order.retain(|candidate| *candidate != key);
        self.order.push_back(key);
    }

    fn remove_conversations(&mut self, conversation_ids: &[ConversationId]) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|key, _| !conversation_ids.contains(&key.conversation_id));
        self.order
            .retain(|key| !conversation_ids.contains(&key.conversation_id));
        before.saturating_sub(self.entries.len())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug, Clone, Default)]
struct TrackedUserRoutes {
    person_id: Option<yunxi_core::PersonId>,
    direct_conversation_ids: VecDeque<ConversationId>,
}

#[derive(Debug)]
struct IngressRouteTracker {
    entries: HashMap<i64, TrackedUserRoutes>,
    order: VecDeque<i64>,
    capacity: usize,
    conversations_per_user: usize,
}

impl IngressRouteTracker {
    fn new(capacity: usize, conversations_per_user: usize) -> Self {
        assert!(capacity > 0, "route tracker must be bounded and non-empty");
        assert!(
            conversations_per_user > 0,
            "per-user route tracker must be bounded and non-empty"
        );
        Self {
            entries: HashMap::with_capacity(capacity.min(32)),
            order: VecDeque::with_capacity(capacity.min(32)),
            capacity,
            conversations_per_user,
        }
    }

    fn record(
        &mut self,
        user_id: i64,
        person_id: yunxi_core::PersonId,
        direct_conversation_id: Option<ConversationId>,
    ) {
        if !self.entries.contains_key(&user_id) {
            while self.entries.len() >= self.capacity {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                self.entries.remove(&oldest);
            }
        }
        let routes = self.entries.entry(user_id).or_default();
        if routes.person_id.is_some_and(|current| current != person_id) {
            routes.direct_conversation_ids.clear();
        }
        routes.person_id = Some(person_id);
        if let Some(conversation_id) = direct_conversation_id {
            routes
                .direct_conversation_ids
                .retain(|candidate| *candidate != conversation_id);
            routes.direct_conversation_ids.push_back(conversation_id);
            while routes.direct_conversation_ids.len() > self.conversations_per_user {
                routes.direct_conversation_ids.pop_front();
            }
        }
        self.order.retain(|candidate| *candidate != user_id);
        self.order.push_back(user_id);
    }

    fn get(&self, user_id: i64) -> Option<TrackedUserRoutes> {
        self.entries.get(&user_id).cloned()
    }

    fn remove(&mut self, user_id: i64) -> bool {
        let removed = self.entries.remove(&user_id).is_some();
        if removed {
            self.order.retain(|candidate| *candidate != user_id);
        }
        removed
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

// The ingress loop owns several independent bounded lifecycles; keeping them
// explicit makes the FIFO and erasure barriers auditable at the call site.
#[allow(clippy::too_many_arguments)]
async fn run_ingress(
    mut receiver: mpsc::Receiver<IngressCommand>,
    store: Arc<dyn IdentityStore>,
    runtime: RuntimeHandle,
    model_backend: Option<Arc<super::core_model::KoviModelBackend>>,
    message_store: Option<Arc<super::identity_store::PostgresIdentityStore>>,
    executive_store: Option<Arc<super::executive_store::PostgresExecutiveStore>>,
    blocked_users: Arc<StdMutex<HashSet<i64>>>,
    blocked_groups: Arc<StdMutex<HashSet<i64>>>,
    action_arbiter: Option<Arc<ActionArbiter>>,
    action_port: Option<Arc<dyn ActionPort>>,
    private_handler_gates: Arc<PrivateHandlerGateRegistry>,
) {
    let mut references = MessageReferenceCache::new(MESSAGE_REFERENCE_CAPACITY);
    let mut routes =
        IngressRouteTracker::new(MAX_TRACKED_USERS, MAX_TRACKED_DIRECT_CONVERSATIONS_PER_USER);
    let mut blocked_at_ingress = HashSet::with_capacity(MAX_BLOCKED_USERS.min(32));
    let mut blocked_groups_at_ingress = HashSet::with_capacity(MAX_BLOCKED_GROUPS.min(32));
    let mut group_erasure_conversations: HashMap<i64, Vec<ConversationId>> = HashMap::new();
    let mut alias_handler_barriers: HashMap<i64, Vec<PrivateDataErasurePermit>> = HashMap::new();
    while let Some(command) = receiver.recv().await {
        match command {
            IngressCommand::Message(message) => {
                if blocked_at_ingress.contains(&message.sender_user_id)
                    || matches!(
                        message.address,
                        ConversationAddress::Group { group_id }
                            if blocked_groups_at_ingress.contains(&group_id)
                    )
                {
                    if let Some(admission) = message.incoming_admission {
                        crate::model::ConversationCoordinator::abandon_incoming(admission).await;
                    }
                    continue;
                }
                let result = kovi::tokio::time::timeout(
                    CORE_INGRESS_PROCESSING_TIMEOUT,
                    resolve_and_submit_inner(
                        &message,
                        store.as_ref(),
                        &runtime,
                        &mut references,
                        model_backend.clone(),
                        message_store.as_deref(),
                        Some(&mut routes),
                    ),
                )
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        // `resolve_and_submit_inner` owns the admission guard
                        // for both ordinary errors and cancellation. Do not
                        // abandon it here: an accepted Core event may already
                        // own the ticket when a later bookkeeping step fails.
                        eprintln!(
                            "[WARN] Yunxi Core message dropped during ingress: sender_user_id={} external_message_id={:?} address={:?} error={error}",
                            message.sender_user_id, message.external_message_id, message.address,
                        );
                    }
                    Err(_) => {
                        // Dropping the timed-out future runs the same guard;
                        // it also removes a context inserted just before the
                        // cancellation boundary.
                        eprintln!(
                            "[WARN] Yunxi Core message ingress processing timed out after {}ms: sender_user_id={} external_message_id={:?} address={:?}",
                            CORE_INGRESS_PROCESSING_TIMEOUT.as_millis(),
                            message.sender_user_id,
                            message.external_message_id,
                            message.address,
                        );
                    }
                }
            }
            IngressCommand::ProjectDestination {
                destination,
                priority,
                kind,
                acknowledge,
            } => {
                let blocked = match destination {
                    crate::model::MessageDestination::Private(user_id) => {
                        blocked_at_ingress.contains(&user_id)
                    }
                    crate::model::MessageDestination::Group(group_id) => {
                        blocked_groups_at_ingress.contains(&group_id)
                    }
                };
                let result = if blocked {
                    Err("Yunxi destination is blocked by a data-erasure barrier".to_string())
                } else {
                    match kovi::tokio::time::timeout(
                        CORE_INGRESS_PROCESSING_TIMEOUT,
                        resolve_projected_destination(
                            destination,
                            priority,
                            kind,
                            store.as_ref(),
                            &runtime,
                        ),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(format!(
                            "Yunxi destination projection timed out after {}ms",
                            CORE_INGRESS_PROCESSING_TIMEOUT.as_millis()
                        )),
                    }
                };
                let _ = acknowledge.send(result);
            }
            IngressCommand::FlushMessageCollisions {
                sender_user_id,
                address,
                acknowledge,
            } => {
                let result = if blocked_at_ingress.contains(&sender_user_id)
                    || matches!(
                        address,
                        ConversationAddress::Group { group_id }
                            if blocked_groups_at_ingress.contains(&group_id)
                    ) {
                    Err(anyhow::anyhow!(
                        "Yunxi collision flush blocked by data-erasure barrier"
                    ))
                } else {
                    resolve_and_submit_collisions(
                        address,
                        sender_user_id,
                        store.as_ref(),
                        message_store.as_deref(),
                        &runtime,
                    )
                    .await
                };
                if let Err(error) = &result {
                    kovi::log::warn!(
                        "Yunxi message collision flush failed for QQ user {sender_user_id}: {error}"
                    );
                }
                let _ = acknowledge.send(result);
            }
            IngressCommand::DispatchAction {
                user_id,
                action,
                control,
                acknowledge,
            } => {
                if !control.claim() {
                    debug_assert_eq!(control.state(), ActionCommandState::Cancelled);
                    continue;
                }
                let blocked_conversation = match action.scope() {
                    yunxi_core::ActionScope::Conversation(conversation_id) => {
                        group_erasure_conversations
                            .values()
                            .any(|blocked| blocked.contains(&conversation_id))
                    }
                    yunxi_core::ActionScope::Person(_) | yunxi_core::ActionScope::Global => false,
                };
                if blocked_at_ingress.contains(&user_id) || blocked_conversation {
                    control.finish();
                    let _ = acknowledge.send(Ok(None));
                    continue;
                }
                let result = if let (Some(arbiter), Some(port)) =
                    (action_arbiter.as_deref(), action_port.as_deref())
                {
                    let result = dispatch_action_with_timeout(
                        arbiter,
                        port,
                        action.clone(),
                        CORE_ACTION_DISPATCH_TIMEOUT,
                    )
                    .await;
                    if matches!(
                        result,
                        ActionResult::Executed {
                            outcome: yunxi_core::ActionPortOutcome::DeliveryIndeterminate { .. },
                            ..
                        }
                    ) {
                        kovi::log::warn!(
                            "Yunxi action dispatch completed with an indeterminate outcome"
                        );
                    }
                    let feedback = action_result_event(&action, &result, Utc::now());
                    if let Some(event) = feedback
                        && let Err(error) = submit_runtime_with_timeout(
                            &runtime,
                            event,
                            CORE_RUNTIME_SUBMIT_TIMEOUT,
                        )
                        .await
                    {
                        kovi::log::warn!("Yunxi action result could not enter runtime: {error}");
                    }
                    control.finish();
                    let _ = acknowledge.send(Ok(Some(result)));
                    continue;
                } else {
                    Ok(None)
                };
                control.finish();
                let _ = acknowledge.send(result);
            }
            IngressCommand::BeginDataErasure {
                user_id,
                acknowledge,
            } => {
                blocked_at_ingress.insert(user_id);
                let result = begin_data_erasure_at_ingress_barrier(
                    user_id,
                    &runtime,
                    &mut references,
                    &mut routes,
                    model_backend.as_deref(),
                    message_store.as_deref(),
                    executive_store.as_deref(),
                    &mut blocked_at_ingress,
                    &blocked_users,
                    &private_handler_gates,
                    &mut alias_handler_barriers,
                )
                .await;
                if let Ok(ack) = &result {
                    kovi::log::debug!(
                        "Yunxi data-erasure barrier ready: user={} aliases={} canonical_person={:?} runtime={} references={} person_routes={} conversation_routes={} tracked={}",
                        user_id,
                        ack.blocked_user_ids.len(),
                        ack.canonical_person_id,
                        ack.purged_conversations,
                        ack.cleared_references,
                        ack.cleared_person_routes,
                        ack.cleared_conversation_routes,
                        ack.cleared_tracked_routes,
                    );
                }
                if result.is_err() && !alias_handler_barriers.contains_key(&user_id) {
                    // A failure before Core's FIFO barrier was established is
                    // recoverable in this process. Once the alias permit map
                    // exists, the barrier is intentionally retained closed.
                    blocked_at_ingress.remove(&user_id);
                    unblock(&blocked_users, user_id);
                }
                if let Err(result) = acknowledge.send(result)
                    && let Ok(ack) = result
                {
                    match end_data_erasure_at_ingress_barrier(
                        ack.runtime_barrier_person_id,
                        &runtime,
                    )
                    .await
                    {
                        Ok(()) => {
                            alias_handler_barriers.remove(&user_id);
                            unblock_users(
                                &mut blocked_at_ingress,
                                &blocked_users,
                                &ack.blocked_user_ids,
                            );
                        }
                        Err(error) => {
                            // Losing the caller after Core accepted begin must
                            // not reopen host ingress unless Core also confirms
                            // resume. Retaining both gates is the fail-closed
                            // state and prevents identity recreation.
                            kovi::log::warn!(
                                "Yunxi erasure caller disappeared and resume failed; retaining ingress block for QQ user {user_id}: {error}"
                            );
                        }
                    }
                }
            }
            IngressCommand::EndDataErasure {
                user_id,
                blocked_user_ids,
                runtime_barrier_person_id,
                acknowledge,
            } => {
                let result =
                    end_data_erasure_at_ingress_barrier(runtime_barrier_person_id, &runtime).await;
                if result.is_ok() {
                    alias_handler_barriers.remove(&user_id);
                    routes.remove(user_id);
                    for blocked_user_id in &blocked_user_ids {
                        routes.remove(*blocked_user_id);
                    }
                    unblock_users(&mut blocked_at_ingress, &blocked_users, &blocked_user_ids);
                }
                let _ = acknowledge.send(result);
            }
            IngressCommand::BeginGroupDataErasure {
                group_id,
                acknowledge,
            } => {
                blocked_groups_at_ingress.insert(group_id);
                let result = begin_group_data_erasure_at_ingress_barrier(
                    group_id,
                    &runtime,
                    &mut references,
                    model_backend.as_deref(),
                    message_store.as_deref(),
                    executive_store.as_deref(),
                    &mut group_erasure_conversations,
                )
                .await;
                if let Ok(ack) = &result {
                    kovi::log::debug!(
                        "Yunxi group data-erasure barrier ready: group={} conversations={:?} runtime_states={} references={} person_routes={} conversation_routes={}",
                        group_id,
                        ack.conversation_ids,
                        ack.purged_runtime_states,
                        ack.cleared_references,
                        ack.cleared_person_routes,
                        ack.cleared_conversation_routes,
                    );
                }
                if result.is_err() && !group_erasure_conversations.contains_key(&group_id) {
                    blocked_groups_at_ingress.remove(&group_id);
                    unblock(&blocked_groups, group_id);
                }
                if let Err(result) = acknowledge.send(result)
                    && let Ok(ack) = result
                {
                    match end_group_data_erasure_at_ingress_barrier(
                        group_id,
                        ack.conversation_ids,
                        &runtime,
                        &mut group_erasure_conversations,
                    )
                    .await
                    {
                        Ok(()) => {
                            blocked_groups_at_ingress.remove(&group_id);
                            unblock(&blocked_groups, group_id);
                        }
                        Err(error) => kovi::log::warn!(
                            "Yunxi group erasure caller disappeared and resume failed; retaining ingress block for QQ group {group_id}: {error}"
                        ),
                    }
                }
            }
            IngressCommand::EndGroupDataErasure {
                group_id,
                conversation_ids,
                acknowledge,
            } => {
                let result = end_group_data_erasure_at_ingress_barrier(
                    group_id,
                    conversation_ids,
                    &runtime,
                    &mut group_erasure_conversations,
                )
                .await;
                if result.is_ok() {
                    blocked_groups_at_ingress.remove(&group_id);
                    unblock(&blocked_groups, group_id);
                }
                let _ = acknowledge.send(result);
            }
        }
    }
}

/// Bound host-side action execution independently from the command
/// acknowledgement. The arbiter owns cancellation safety for an admitted
/// idempotency reservation, so dropping this future cannot make a later retry
/// issue a duplicate platform action.
async fn dispatch_action_with_timeout(
    arbiter: &ActionArbiter,
    port: &dyn ActionPort,
    action: ProposedAction,
    wait: Duration,
) -> ActionResult {
    arbiter.dispatch_with_timeout(action, port, wait).await
}

#[allow(clippy::too_many_arguments)]
async fn begin_data_erasure_at_ingress_barrier(
    user_id: i64,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
    routes: &mut IngressRouteTracker,
    model_backend: Option<&super::core_model::KoviModelBackend>,
    message_store: Option<&super::identity_store::PostgresIdentityStore>,
    executive_store: Option<&super::executive_store::PostgresExecutiveStore>,
    blocked_at_ingress: &mut HashSet<i64>,
    blocked_users: &StdMutex<HashSet<i64>>,
    private_handler_gates: &PrivateHandlerGateRegistry,
    alias_handler_barriers: &mut HashMap<i64, Vec<PrivateDataErasurePermit>>,
) -> anyhow::Result<DataErasureAck> {
    anyhow::ensure!(
        !alias_handler_barriers.contains_key(&user_id),
        "duplicate Yunxi alias handler barrier"
    );
    let persistent = if let Some(message_store) = message_store {
        message_store
            .qq_person_domain_targets(user_id)
            .await
            .map_err(anyhow::Error::from)?
    } else {
        super::identity_store::QqPersonDomainTargets::default()
    };
    let mut lookup_user_ids = persistent.qq_user_ids.clone();
    lookup_user_ids.push(user_id);
    lookup_user_ids.sort_unstable();
    lookup_user_ids.dedup();
    let tracked = collect_tracked_routes(routes, &lookup_user_ids)?;
    let targets = merge_data_erasure_targets(user_id, persistent, tracked)?;
    block_user_aliases(
        user_id,
        &targets.blocked_user_ids,
        blocked_at_ingress,
        blocked_users,
    )?;
    let alias_permits = match acquire_alias_handler_barriers(
        user_id,
        &targets.blocked_user_ids,
        private_handler_gates,
    )
    .await
    {
        Ok(permits) => permits,
        Err(error) => {
            unblock_users(blocked_at_ingress, blocked_users, &targets.blocked_user_ids);
            return Err(error);
        }
    };
    let purged_conversations = match runtime
        .begin_data_erasure(
            targets.runtime_barrier_person_id,
            targets.direct_conversation_ids.iter().copied(),
        )
        .await
    {
        Ok(purged) => purged,
        Err(error) => {
            unblock_users(blocked_at_ingress, blocked_users, &targets.blocked_user_ids);
            return Err(anyhow::Error::from(error));
        }
    };
    super::autonomous::forget(&targets.direct_conversation_ids);
    // From this point onward Core's FIFO barrier is live. Retain the host
    // permits even if a later purge fails, so ingress cannot reopen around
    // data that has not yet been fully erased.
    alias_handler_barriers.insert(user_id, alias_permits);
    let cleared_references = references.remove_conversations(&targets.direct_conversation_ids);
    let (cleared_person_routes, cleared_conversation_routes) =
        if let Some(model_backend) = model_backend {
            model_backend
                .purge_private_message_contexts(&targets.blocked_user_ids)
                .await?;
            model_backend
                .purge_routes(
                    targets.canonical_person_id,
                    &targets.direct_conversation_ids,
                )
                .await
        } else {
            (0, 0)
        };
    let mut executive_scopes = Vec::with_capacity(
        usize::from(targets.canonical_person_id.is_some())
            .saturating_add(targets.direct_conversation_ids.len()),
    );
    if let Some(person_id) = targets.canonical_person_id {
        executive_scopes.push(yunxi_core::ExecutiveScope::Person { person_id });
    }
    executive_scopes.extend(
        targets
            .direct_conversation_ids
            .iter()
            .copied()
            .map(|conversation_id| yunxi_core::ExecutiveScope::Conversation { conversation_id }),
    );
    super::erase_executive_scopes_with_store(
        &executive_scopes,
        message_store.is_some(),
        executive_store,
    )
    .await?;
    let cleared_tracked_routes = targets
        .blocked_user_ids
        .iter()
        .filter(|user_id| routes.remove(**user_id))
        .count();
    Ok(DataErasureAck {
        canonical_person_id: targets.canonical_person_id,
        runtime_barrier_person_id: targets.runtime_barrier_person_id,
        blocked_user_ids: targets.blocked_user_ids,
        direct_conversation_ids: targets.direct_conversation_ids,
        purged_conversations,
        cleared_references,
        cleared_person_routes,
        cleared_conversation_routes,
        cleared_tracked_routes,
    })
}

async fn acquire_alias_handler_barriers(
    initiating_user_id: i64,
    user_ids: &[i64],
    private_handler_gates: &PrivateHandlerGateRegistry,
) -> anyhow::Result<Vec<PrivateDataErasurePermit>> {
    let mut tokens = Vec::with_capacity(user_ids.len().saturating_sub(1));
    for user_id in user_ids {
        if *user_id == initiating_user_id {
            continue;
        }
        let token = private_handler_gates
            .capture_data_erasure(*user_id)
            .ok_or_else(|| anyhow::anyhow!("a QQ alias already has a pending data erasure"))?;
        tokens.push(token);
    }
    let mut permits = Vec::with_capacity(tokens.len());
    for token in tokens {
        let permit = token
            .enter()
            .await
            .ok_or_else(|| anyhow::anyhow!("a QQ alias data-erasure epoch changed"))?;
        permits.push(permit);
    }
    Ok(permits)
}

#[derive(Debug)]
struct DataErasureTargets {
    canonical_person_id: Option<yunxi_core::PersonId>,
    runtime_barrier_person_id: yunxi_core::PersonId,
    blocked_user_ids: Vec<i64>,
    direct_conversation_ids: Vec<ConversationId>,
}

fn merge_data_erasure_targets(
    user_id: i64,
    persistent: super::identity_store::QqPersonDomainTargets,
    tracked: TrackedUserRoutes,
) -> anyhow::Result<DataErasureTargets> {
    if let (Some(persistent), Some(tracked)) = (persistent.person_id, tracked.person_id) {
        anyhow::ensure!(
            persistent == tracked,
            "Yunxi persistent and ingress person routes disagree"
        );
    }
    let canonical_person_id = persistent.person_id.or(tracked.person_id);
    let mut blocked_user_ids = persistent.qq_user_ids;
    blocked_user_ids.push(user_id);
    blocked_user_ids.sort_unstable();
    blocked_user_ids.dedup();
    anyhow::ensure!(
        blocked_user_ids.len() <= MAX_BLOCKED_USERS
            && blocked_user_ids.iter().all(|user_id| valid_qq_id(*user_id)),
        "invalid or excessive QQ aliases for data erasure"
    );
    let mut direct_conversation_ids = persistent.direct_conversation_ids;
    direct_conversation_ids.extend(tracked.direct_conversation_ids);
    direct_conversation_ids.sort_unstable();
    direct_conversation_ids.dedup();
    anyhow::ensure!(
        direct_conversation_ids.len() <= MAX_TRACKED_DIRECT_CONVERSATIONS_PER_USER,
        "too many direct conversations to erase safely"
    );

    // Older deletion code may already have removed the QQ Person mapping but
    // left direct conversations behind. Core requires a PersonId to key its
    // barrier, so use an ephemeral internal scope only for begin/end while
    // preserving the missing canonical id for model-route cleanup.
    let runtime_barrier_person_id = canonical_person_id.unwrap_or_default();
    Ok(DataErasureTargets {
        canonical_person_id,
        runtime_barrier_person_id,
        blocked_user_ids,
        direct_conversation_ids,
    })
}

fn collect_tracked_routes(
    routes: &IngressRouteTracker,
    user_ids: &[i64],
) -> anyhow::Result<TrackedUserRoutes> {
    let mut combined = TrackedUserRoutes::default();
    for user_id in user_ids {
        let Some(tracked) = routes.get(*user_id) else {
            continue;
        };
        if let (Some(current), Some(candidate)) = (combined.person_id, tracked.person_id) {
            anyhow::ensure!(
                current == candidate,
                "Yunxi QQ aliases have conflicting ingress person routes"
            );
        }
        combined.person_id = combined.person_id.or(tracked.person_id);
        combined
            .direct_conversation_ids
            .extend(tracked.direct_conversation_ids);
    }
    Ok(combined)
}

async fn end_data_erasure_at_ingress_barrier(
    runtime_barrier_person_id: yunxi_core::PersonId,
    runtime: &RuntimeHandle,
) -> anyhow::Result<()> {
    let resumed = runtime
        .end_data_erasure(runtime_barrier_person_id)
        .await
        .map_err(anyhow::Error::from)?;
    anyhow::ensure!(
        resumed,
        "Yunxi runtime had no matching data-erasure barrier"
    );
    Ok(())
}

async fn begin_group_data_erasure_at_ingress_barrier(
    group_id: i64,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
    model_backend: Option<&super::core_model::KoviModelBackend>,
    message_store: Option<&super::identity_store::PostgresIdentityStore>,
    executive_store: Option<&super::executive_store::PostgresExecutiveStore>,
    active: &mut HashMap<i64, Vec<ConversationId>>,
) -> anyhow::Result<GroupDataErasureAck> {
    anyhow::ensure!(
        !active.contains_key(&group_id),
        "duplicate Yunxi group data-erasure barrier"
    );
    let message_store = message_store
        .context("Yunxi PostgreSQL identity store is unavailable for group data erasure")?;
    let persistent_conversation_id = message_store
        .qq_group_conversation_id(group_id)
        .await
        .map_err(anyhow::Error::from)?;
    let (mut conversation_ids, cleared_person_routes, cleared_conversation_routes) =
        if let Some(model_backend) = model_backend {
            model_backend.purge_group_routes(group_id).await
        } else {
            (Vec::new(), 0, 0)
        };
    if let Some(conversation_id) = persistent_conversation_id
        && !conversation_ids.contains(&conversation_id)
    {
        conversation_ids.push(conversation_id);
    }
    let purged_runtime_states = if conversation_ids.is_empty() {
        0
    } else {
        runtime
            .begin_conversation_data_erasures(conversation_ids.iter().copied())
            .await
            .map_err(anyhow::Error::from)?
    };
    super::autonomous::forget(&conversation_ids);
    // Mark the barrier before later cleanup can fail. An error after this
    // point deliberately leaves both Core and host ingress closed.
    active.insert(group_id, conversation_ids.clone());
    let cleared_references = references.remove_conversations(&conversation_ids);
    if let Some(model_backend) = model_backend {
        model_backend.purge_group_message_contexts(group_id).await?;
    }
    let executive_scopes = conversation_ids
        .iter()
        .copied()
        .map(|conversation_id| yunxi_core::ExecutiveScope::Conversation { conversation_id })
        .collect::<Vec<_>>();
    super::erase_executive_scopes_with_store(&executive_scopes, true, executive_store).await?;
    Ok(GroupDataErasureAck {
        conversation_ids,
        purged_runtime_states,
        cleared_references,
        cleared_person_routes,
        cleared_conversation_routes,
    })
}

async fn end_group_data_erasure_at_ingress_barrier(
    group_id: i64,
    conversation_ids: Vec<ConversationId>,
    runtime: &RuntimeHandle,
    active: &mut HashMap<i64, Vec<ConversationId>>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        active.get(&group_id) == Some(&conversation_ids),
        "Yunxi ingress had no matching group data-erasure barrier"
    );
    if !conversation_ids.is_empty() {
        let resumed = runtime
            .end_conversation_data_erasures(conversation_ids.iter().copied())
            .await
            .map_err(anyhow::Error::from)?;
        anyhow::ensure!(
            resumed == conversation_ids.len(),
            "Yunxi runtime resumed {resumed} of {} conversation data-erasure barriers",
            conversation_ids.len()
        );
    }
    active.remove(&group_id);
    Ok(())
}

fn block_user_aliases(
    initiating_user_id: i64,
    user_ids: &[i64],
    blocked_at_ingress: &mut HashSet<i64>,
    blocked_users: &StdMutex<HashSet<i64>>,
) -> anyhow::Result<()> {
    let mut shared = blocked_users
        .lock()
        .map_err(|_| anyhow::anyhow!("Yunxi blocked-user state is poisoned"))?;
    anyhow::ensure!(
        user_ids.iter().all(|user_id| {
            *user_id == initiating_user_id
                || (!shared.contains(user_id) && !blocked_at_ingress.contains(user_id))
        }),
        "a QQ alias is already covered by another data erasure"
    );
    let shared_additions = user_ids
        .iter()
        .filter(|user_id| !shared.contains(user_id))
        .count();
    let local_additions = user_ids
        .iter()
        .filter(|user_id| !blocked_at_ingress.contains(user_id))
        .count();
    anyhow::ensure!(
        shared.len().saturating_add(shared_additions) <= MAX_BLOCKED_USERS
            && blocked_at_ingress.len().saturating_add(local_additions) <= MAX_BLOCKED_USERS,
        "too many QQ users are blocked by concurrent data erasures"
    );
    shared.extend(user_ids.iter().copied());
    blocked_at_ingress.extend(user_ids.iter().copied());
    Ok(())
}

fn unblock_users(
    blocked_at_ingress: &mut HashSet<i64>,
    blocked_users: &StdMutex<HashSet<i64>>,
    user_ids: &[i64],
) {
    for user_id in user_ids {
        blocked_at_ingress.remove(user_id);
    }
    if let Ok(mut shared) = blocked_users.lock() {
        for user_id in user_ids {
            shared.remove(user_id);
        }
    }
}

fn is_blocked(blocked_users: &StdMutex<HashSet<i64>>, user_id: i64) -> bool {
    blocked_users
        .lock()
        .map_or(true, |blocked| blocked.contains(&user_id))
}

fn unblock(blocked_users: &StdMutex<HashSet<i64>>, user_id: i64) {
    if let Ok(mut blocked) = blocked_users.lock() {
        blocked.remove(&user_id);
    }
}

type IncomingAdmissionReleaseFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

trait IncomingAdmissionReleaser: Send + Sync {
    fn discard<'a>(&'a self, message_id: MessageId) -> IncomingAdmissionReleaseFuture<'a>;
}

impl IncomingAdmissionReleaser for super::core_model::KoviModelBackend {
    fn discard(&self, message_id: MessageId) -> IncomingAdmissionReleaseFuture<'_> {
        Box::pin(async move {
            self.discard_incoming(message_id).await;
        })
    }
}

async fn release_rejected_incoming(
    event: &WorldEvent,
    releaser: Option<&dyn IncomingAdmissionReleaser>,
) {
    let (Some(releaser), WorldEventKind::MessageReceived(message)) = (releaser, event.kind())
    else {
        return;
    };
    releaser.discard(message.message_id).await;
}

/// Cancellation-safe ownership for an admission while the serialized Host
/// ingress worker resolves identities and constructs the Core event. If the
/// worker is cancelled by its processing deadline after registering a host
/// context, remove that context before abandoning the admission.
struct IngressAdmissionGuard {
    admission: Option<IncomingAdmission>,
    model_backend: Option<Arc<super::core_model::KoviModelBackend>>,
    message_id: MessageId,
    context_registered: bool,
}

impl IngressAdmissionGuard {
    fn new(
        admission: IncomingAdmission,
        model_backend: Option<Arc<super::core_model::KoviModelBackend>>,
        message_id: MessageId,
    ) -> Self {
        Self {
            admission: Some(admission),
            model_backend,
            message_id,
            context_registered: false,
        }
    }

    fn disarm(&mut self) {
        self.admission = None;
    }

    fn mark_context_registered(&mut self) {
        self.context_registered = true;
    }
}

impl Drop for IngressAdmissionGuard {
    fn drop(&mut self) {
        let Some(admission) = self.admission.take() else {
            return;
        };
        let model_backend = self
            .context_registered
            .then(|| self.model_backend.clone())
            .flatten();
        let message_id = self.message_id;
        if let Ok(runtime) = kovi::tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(model_backend) = model_backend {
                    model_backend.discard_incoming(message_id).await;
                }
                crate::model::ConversationCoordinator::abandon_incoming(admission).await;
            });
        }
    }
}

fn autonomous_tick_conversation_id(event: &WorldEvent) -> Option<ConversationId> {
    matches!(event.kind(), WorldEventKind::AutonomousConversationTick(_))
        .then(|| event.scope().conversation_id())
        .flatten()
}

fn autonomous_tick_claim_token(event: &WorldEvent) -> Option<u64> {
    match event.kind() {
        WorldEventKind::AutonomousConversationTick(tick) => tick.claim_token,
        _ => None,
    }
}

fn release_autonomous_claim_for_event(event: &WorldEvent) {
    let Some(conversation_id) = autonomous_tick_conversation_id(event) else {
        return;
    };
    if let Some(token) = autonomous_tick_claim_token(event) {
        super::autonomous::release_claim_token(conversation_id, token);
    } else {
        // Compatibility for ticks emitted by an older host before claim
        // tokens were added to the event payload.
        super::autonomous::release_claim(conversation_id);
    }
}

fn retry_autonomous_claim_for_event(event: &WorldEvent) {
    let Some(conversation_id) = autonomous_tick_conversation_id(event) else {
        return;
    };
    if let Some(token) = autonomous_tick_claim_token(event) {
        super::autonomous::retry_claim_token(conversation_id, token);
    } else {
        super::autonomous::retry_claim(conversation_id);
    }
}

fn autonomous_claim_is_current_for_event(event: &WorldEvent) -> bool {
    let Some(conversation_id) = autonomous_tick_conversation_id(event) else {
        return true;
    };
    let Some(token) = autonomous_tick_claim_token(event) else {
        // Legacy ticks have no fencing token and are handled by the
        // compatibility release/finish paths below.
        return true;
    };
    super::autonomous::claim_is_current(conversation_id, token)
}

/// A failed autonomous action should be retried when it never crossed an
/// irreversible delivery boundary. Indeterminate delivery is intentionally
/// terminal: replaying it could duplicate a message whose platform outcome
/// is unknown.
fn autonomous_action_needs_retry(actions: &[ActionResult]) -> bool {
    actions.iter().any(|action| match action {
        ActionResult::Executed {
            outcome: yunxi_core::ActionPortOutcome::Deferred { .. },
            ..
        } => true,
        ActionResult::Failed { error, .. } => error.retryable,
        ActionResult::Rejected(rejection) => matches!(
            rejection,
            yunxi_core::ActionRejection::CapabilityUnavailable { .. }
                | yunxi_core::ActionRejection::CooldownActive { .. }
                | yunxi_core::ActionRejection::RateLimitExceeded { .. }
                | yunxi_core::ActionRejection::IdempotencyStateFull { .. }
                | yunxi_core::ActionRejection::Stale { .. }
                | yunxi_core::ActionRejection::TargetUnavailable { .. }
                | yunxi_core::ActionRejection::DeliveryResolutionFailed { .. }
        ),
        ActionResult::Noop
        | ActionResult::Executed {
            outcome:
                yunxi_core::ActionPortOutcome::Delivered { .. }
                | yunxi_core::ActionPortOutcome::DeliveryIndeterminate { .. }
                | yunxi_core::ActionPortOutcome::ToolCompleted { .. }
                | yunxi_core::ActionPortOutcome::ToolFailed { .. },
            ..
        } => false,
    })
}

fn autonomous_tick_should_retry(
    actions: &[ActionResult],
    delivered: bool,
    directive: Option<ConversationTurnDirective>,
) -> bool {
    if delivered {
        return false;
    }
    // Action results are stronger evidence than the model's directive. A
    // deferred/retryable result means the proposed message never crossed the
    // side-effect boundary and should get another bounded attempt.
    if !actions.is_empty() {
        return autonomous_action_needs_retry(actions);
    }
    // A Continue directive without a visible action means the planner/model
    // decided there was another thought but failed to materialize it. Keep a
    // bounded retry for that explicit signal. A missing/Wait/End directive is
    // a legitimate silent turn and must not become a hot loop.
    directive == Some(ConversationTurnDirective::Continue)
}

async fn run_runtime(
    mut runtime: CognitiveRuntime,
    action_arbiter: Option<Arc<ActionArbiter>>,
    action_port: Option<Arc<dyn ActionPort>>,
    incoming_releaser: Option<Arc<dyn IncomingAdmissionReleaser>>,
) {
    let planned = runtime.planner().is_some();
    if planned
        && let (Some(arbiter), Some(port)) = (action_arbiter.as_deref(), action_port.as_deref())
    {
        while let Some((event, outcome)) = {
            super::refresh_executive_capability();
            runtime
                .process_next_with_planner_and_actions_with_event_and_guard(
                    arbiter,
                    port,
                    &autonomous_claim_is_current_for_event,
                )
                .await
        } {
            match outcome {
                Ok(PlannedProcessingOutcome::Planned {
                    observation,
                    plan,
                    actions,
                    ..
                }) => {
                    let autonomous_tick =
                        observation.event_type == EventType::AutonomousConversationTick;
                    let conversation_id = observation.scope.conversation_id();
                    let requested_directive = conversation_id.and_then(|conversation_id| {
                        plan.state_updates.iter().find_map(|update| match update {
                            yunxi_core::StateUpdateProposal::ConversationDirective {
                                conversation_id: update_conversation_id,
                                directive,
                            } if *update_conversation_id == conversation_id => Some(*directive),
                            _ => None,
                        })
                    });
                    let mut autonomous_delivered = false;
                    for (intent, action) in plan.intents.iter().zip(actions.iter()) {
                        if let CognitiveIntent::SendMessage {
                            conversation_id, ..
                        } = intent
                            && matches!(
                                action,
                                ActionResult::Executed {
                                    outcome: yunxi_core::ActionPortOutcome::Delivered { .. },
                                    ..
                                }
                            )
                        {
                            if !autonomous_tick {
                                let proactive_config = crate::config::get().proactive().clone();
                                let effective_directive =
                                    super::autonomous::record_outbound_with_directive(
                                        *conversation_id,
                                        Utc::now(),
                                        requested_directive,
                                        Some(&proactive_config),
                                    );
                                if let Some(directive) = effective_directive {
                                    kovi::log::info!(
                                        "Yunxi conversation continuation registered: conversation_id={conversation_id} model_directive={requested_directive:?} effective_directive={directive:?}"
                                    );
                                }
                            }
                            if autonomous_tick {
                                autonomous_delivered = true;
                            }
                        }
                    }
                    if autonomous_tick
                        && let Some(conversation_id) = observation.scope.conversation_id()
                    {
                        if !autonomous_claim_is_current_for_event(&event) {
                            kovi::log::info!(
                                "Yunxi autonomous tick superseded before completion: event_id={} conversation_id={}",
                                observation.event_id,
                                conversation_id,
                            );
                        } else if autonomous_tick_should_retry(
                            &actions,
                            autonomous_delivered,
                            requested_directive,
                        ) {
                            if let Some(token) = autonomous_tick_claim_token(&event) {
                                super::autonomous::retry_claim_token(conversation_id, token);
                            } else {
                                super::autonomous::retry_claim(conversation_id);
                            }
                            kovi::log::warn!(
                                "Yunxi autonomous turn scheduled for retry: event_id={} conversation_id={} directive={requested_directive:?} actions={:?}",
                                observation.event_id,
                                conversation_id,
                                actions,
                            );
                        } else {
                            let directive = match (autonomous_delivered, requested_directive) {
                                (true, Some(directive)) => directive,
                                (false, Some(ConversationTurnDirective::End)) => {
                                    ConversationTurnDirective::End
                                }
                                _ => ConversationTurnDirective::Wait,
                            };
                            if let Some(token) = autonomous_tick_claim_token(&event) {
                                super::autonomous::finish_claim_token(
                                    conversation_id,
                                    token,
                                    Utc::now(),
                                    autonomous_delivered,
                                    directive,
                                    crate::config::get().proactive(),
                                );
                            } else {
                                super::autonomous::finish_claim(
                                    conversation_id,
                                    Utc::now(),
                                    autonomous_delivered,
                                    directive,
                                    crate::config::get().proactive(),
                                );
                            }
                        }
                    }
                    let has_action_failure = actions.iter().any(|action| !action.is_success());
                    if has_action_failure {
                        kovi::log::warn!(
                            "Yunxi Core turn outcome: event_id={} type={:?} scope={:?} attention={:?} disposition={:?} intents={} actions={:?}",
                            observation.event_id,
                            observation.event_type,
                            observation.scope,
                            observation.attention,
                            plan.disposition,
                            plan.intents.len(),
                            actions,
                        );
                    } else {
                        // Most turns are routine ObserveOnly/Silent observations with no
                        // action. Keep them out of the warned journal so real failures
                        // stay visible; under RUST_LOG=info debug! is suppressed.
                        kovi::log::debug!(
                            "Yunxi Core turn outcome: event_id={} type={:?} scope={:?} attention={:?} disposition={:?} intents={} actions={}",
                            observation.event_id,
                            observation.event_type,
                            observation.scope,
                            observation.attention,
                            plan.disposition,
                            plan.intents.len(),
                            actions.len(),
                        );
                    }
                }
                Ok(PlannedProcessingOutcome::RejectedEvent { event, .. })
                | Ok(PlannedProcessingOutcome::RejectedState { event, .. }) => {
                    // A planner/state rejection can be caused by a transient
                    // queue or persistence race. Use the same bounded retry
                    // path as a planner error; an invalid autonomous event
                    // will still suspend after the retry budget is exhausted.
                    retry_autonomous_claim_for_event(&event);
                    release_rejected_incoming(&event, incoming_releaser.as_deref()).await;
                    kovi::log::warn!("Yunxi Core planner rejected an event");
                }
                Err(error) => {
                    if let Some(conversation_id) = autonomous_tick_conversation_id(&event) {
                        retry_autonomous_claim_for_event(&event);
                        kovi::log::warn!(
                            "Yunxi autonomous planner failure will be retried: conversation_id={} error={error}",
                            conversation_id,
                        );
                    }
                    // The runtime has consumed the event even when planning
                    // fails. Release the exact host admission carried by a
                    // visible message; otherwise the conversation remains
                    // permanently marked as active and later replies can be
                    // suppressed behind a phantom turn.
                    release_rejected_incoming(&event, incoming_releaser.as_deref()).await;
                    kovi::log::error!("Yunxi Core planner failed before action outcome: {error}")
                }
            }
            persist_executive_after_turn().await;
        }
        return;
    }
    while let Some((event, outcome)) = {
        super::refresh_executive_capability();
        runtime.process_next_with_event().await
    } {
        match outcome {
            ProcessingOutcome::Observed(observation) => {
                if autonomous_tick_conversation_id(&event).is_some() {
                    // A compatibility bridge without planner/action support
                    // cannot deliver a continuation. Do not leave its host
                    // claim leased until the timeout.
                    release_autonomous_claim_for_event(&event);
                }
                kovi::log::debug!(
                    "Yunxi Core event observed: id={} type={:?} scope={:?} priority={:?} attention={:?} state={:?}",
                    observation.event_id,
                    observation.event_type,
                    observation.scope,
                    observation.priority,
                    observation.attention,
                    observation.state,
                );
            }
            ProcessingOutcome::RejectedEvent { event, .. }
            | ProcessingOutcome::RejectedState { event, .. } => {
                release_autonomous_claim_for_event(&event);
                release_rejected_incoming(&event, incoming_releaser.as_deref()).await;
                kovi::log::warn!("Yunxi Core runtime rejected an event");
            }
        }
        persist_executive_after_turn().await;
    }
}

async fn persist_executive_after_turn() {
    if let Err(error) = super::persist_executive_snapshot().await {
        // Persistence is a recovery aid, not a reason to stop the bounded
        // deterministic runtime. The next turn will retry the latest state.
        kovi::log::warn!("Yunxi Executive turn persistence failed: {error}");
    }
}

#[allow(dead_code)]
async fn resolve_and_submit(
    message: &InboundMessage,
    store: &dyn IdentityStore,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
) -> anyhow::Result<()> {
    resolve_and_submit_inner(message, store, runtime, references, None, None, None).await
}

/// Submit an event while bounding the wait imposed by High/Critical runtime
/// backpressure. The event is moved into the cancellable future, so a timeout
/// cannot later enqueue it unexpectedly; callers can therefore release any
/// host-side admission and report a deterministic drop.
async fn submit_runtime_with_timeout(
    runtime: &RuntimeHandle,
    event: WorldEvent,
    wait: Duration,
) -> anyhow::Result<Admission> {
    let event_id = event.id();
    let event_scope = event.scope();
    let event_priority = event.priority();
    match kovi::tokio::time::timeout(wait, runtime.submit(event)).await {
        Ok(Ok(admission)) => Ok(admission),
        Ok(Err(error)) => Err(anyhow::anyhow!(error)),
        Err(_) => {
            kovi::log::error!(
                "Yunxi Core runtime submit timed out: event_id={} scope={:?} priority={:?} wait_ms={} action=drop",
                event_id,
                event_scope,
                event_priority,
                wait.as_millis(),
            );
            Err(anyhow::anyhow!(
                "Yunxi runtime submit timed out: event_id={} scope={:?} priority={:?} wait_ms={}",
                event_id,
                event_scope,
                event_priority,
                wait.as_millis(),
            ))
        }
    }
}

async fn resolve_projected_destination(
    destination: crate::model::MessageDestination,
    priority: EventPriority,
    kind: WorldEventKind,
    store: &dyn IdentityStore,
    runtime: &RuntimeHandle,
) -> Result<Admission, String> {
    let scope = match destination {
        crate::model::MessageDestination::Private(user_id) => {
            let external = qq::person(user_id).map_err(|error| error.to_string())?;
            let person_id = store
                .resolve_external_identity(&external)
                .await
                .map_err(|error| error.to_string())?;
            EventScope::Person { person_id }
        }
        crate::model::MessageDestination::Group(group_id) => {
            let external = qq::group(group_id).map_err(|error| error.to_string())?;
            let conversation_id = store
                .resolve_external_conversation(&external)
                .await
                .map_err(|error| error.to_string())?;
            EventScope::Conversation { conversation_id }
        }
    };
    submit_runtime_with_timeout(
        runtime,
        WorldEvent::new(Utc::now(), scope, priority, kind),
        CORE_RUNTIME_SUBMIT_TIMEOUT,
    )
    .await
    .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn resolve_and_submit_inner(
    message: &InboundMessage,
    store: &dyn IdentityStore,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
    model_backend: Option<Arc<super::core_model::KoviModelBackend>>,
    message_store: Option<&super::identity_store::PostgresIdentityStore>,
    route_tracker: Option<&mut IngressRouteTracker>,
) -> anyhow::Result<()> {
    let message_id = MessageId::new();
    let mut admission_guard = message
        .incoming_admission
        .map(|admission| IngressAdmissionGuard::new(admission, model_backend.clone(), message_id));
    let external_identity = qq::person(message.sender_user_id)?;
    let external_conversation = message.address.external()?;
    let person_id = store
        .resolve_external_identity(&external_identity)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    let direct_resolution =
        matches!(message.address, ConversationAddress::Direct { .. }) && message_store.is_some();
    let conversation_id = if direct_resolution {
        message_store
            .expect("direct resolution requires the configured PostgreSQL message store")
            .resolve_direct_for_person(person_id, &external_conversation)
            .await
            .map_err(|error| anyhow::anyhow!(error))?
    } else {
        store
            .resolve_external_conversation(&external_conversation)
            .await
            .map_err(|error| anyhow::anyhow!(error))?
    };
    let reply_scope = message.address.reply_scope();

    if let Some(route_tracker) = route_tracker {
        route_tracker.record(
            message.sender_user_id,
            person_id,
            matches!(message.address, ConversationAddress::Direct { .. })
                .then_some(conversation_id),
        );
    }

    if !direct_resolution && let Some(member_store) = message_store.as_ref() {
        let member = yunxi_core::ConversationMember::new(conversation_id, person_id);
        if let Err(error) = member_store.upsert(&member).await {
            kovi::log::warn!("Yunxi conversation-member upsert failed: {error}");
        }
    }

    if let Some(model_backend) = model_backend.as_deref() {
        let conversation = match message.address {
            ConversationAddress::Group { group_id } => {
                super::core_model::QqConversation::Group { group_id }
            }
            ConversationAddress::Direct { peer_user_id, .. } => {
                super::core_model::QqConversation::Private {
                    user_id: peer_user_id,
                }
            }
        };
        model_backend
            .register(
                conversation_id,
                person_id,
                conversation,
                message.sender_user_id,
            )
            .await;
    }

    let reference_key =
        message
            .external_message_id
            .map(|external_message_id| MessageReferenceKey {
                conversation_id,
                external_message_id,
            });
    if let Some(key) = reference_key
        && references.get(key).is_some()
    {
        if let Some(admission) = message.incoming_admission {
            crate::model::ConversationCoordinator::abandon_incoming(admission).await;
        }
        if let Some(guard) = admission_guard.as_mut() {
            guard.disarm();
        }
        if let Err(error) = submit_message_collisions(reply_scope, conversation_id, runtime).await {
            kovi::log::warn!("Yunxi message collision event was not admitted: {error}");
        }
        return Ok(());
    }
    let reply_reference = message
        .reply_to_external_message_id
        .and_then(|external_message_id| {
            references.get(MessageReferenceKey {
                conversation_id,
                external_message_id,
            })
        });
    let recent_agent_reply =
        resolve_recent_agent_reply(message, conversation_id, reply_reference, message_store).await;
    let visible_reply_allowed = effective_visible_reply_allowed(message, recent_agent_reply);

    let priority = if message.address.kind() == ConversationKind::Direct
        || message.addressed_to_agent
        || recent_agent_reply
        || message.stop_requested
        || message.explicit_request
        || message.planner_attention_requested
    {
        EventPriority::High
    } else {
        EventPriority::Normal
    };
    let requested_message_count = (message.address.kind() == ConversationKind::Direct
        || message.addressed_to_agent
        || recent_agent_reply
        || message.explicit_request)
        .then(|| super::core_model::requested_message_count(&message.text))
        .flatten();
    let event = WorldEvent::message_received(
        priority,
        MessageReceivedEvent {
            message_id,
            conversation_id,
            sender: person_id,
            content: MessageContent::text(message.text.clone())
                .with_attachments(message.attachments.clone())
                .map_err(|error| anyhow::anyhow!(error))?,
            reply_to: reply_reference.map(|reference| reference.message_id),
            timestamp: message.timestamp,
            conversation_kind: message.address.kind(),
            addressed_to_agent: message.addressed_to_agent,
            replies_to_agent: recent_agent_reply,
            stop_requested: message.stop_requested,
            explicit_request: message.explicit_request,
            visible_reply_allowed,
        },
    )
    .with_requested_message_count(requested_message_count);
    // Persist the external reference before admitting the event. The runtime
    // may process a high-priority message immediately and a reply action must
    // be able to resolve its Core MessageId without racing this write.
    if let Some(external_message_id) = message.external_message_id
        && let Some(message_store) = message_store
        && let Err(error) = message_store
            .record_qq_message_mapping(message_id, conversation_id, external_message_id, "inbound")
            .await
    {
        kovi::log::warn!("Yunxi inbound message mapping could not be persisted: {error}");
    }
    let registered_incoming = if let (Some(model_backend), Some(incoming_admission)) =
        (model_backend.as_deref(), message.incoming_admission)
    {
        if priority != EventPriority::High {
            crate::model::ConversationCoordinator::abandon_incoming(incoming_admission).await;
            if let Some(guard) = admission_guard.as_mut() {
                guard.disarm();
            }
            false
        } else if incoming_admission.ticket.scope() == reply_scope {
            if let Some(guard) = admission_guard.as_mut() {
                // `register_incoming` can be cancelled after inserting the
                // context while it releases a displaced admission.
                guard.mark_context_registered();
            }
            model_backend
                .register_incoming(
                    message_id,
                    incoming_admission,
                    message.vision_attachments.clone(),
                )
                .await;
            true
        } else {
            kovi::log::warn!(
                "Yunxi incoming admission scope does not match the resolved message route"
            );
            crate::model::ConversationCoordinator::abandon_incoming(incoming_admission).await;
            if let Some(guard) = admission_guard.as_mut() {
                guard.disarm();
            }
            false
        }
    } else {
        if let Some(incoming_admission) = message.incoming_admission {
            crate::model::ConversationCoordinator::abandon_incoming(incoming_admission).await;
            if let Some(guard) = admission_guard.as_mut() {
                guard.disarm();
            }
        }
        false
    };
    if let Some(mind) = super::mind_runtime() {
        let timeout = std::time::Duration::from_millis(mind.config().event_update_timeout_ms());
        match kovi::tokio::time::timeout(timeout, mind.observe_event(&event)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => kovi::log::warn!("Yunxi Mind event update failed soft: {error}"),
            Err(_) => kovi::log::warn!("Yunxi Mind event update timed out and failed soft"),
        }
    }
    let event_id = event.id();
    let event_scope = event.scope();
    let event_priority = event.priority();
    let admission = match submit_runtime_with_timeout(runtime, event, CORE_RUNTIME_SUBMIT_TIMEOUT)
        .await
    {
        Ok(admission) => admission,
        Err(error) => {
            if registered_incoming {
                if let Some(model_backend) = model_backend.as_ref() {
                    model_backend.discard_incoming(message_id).await;
                }
                if let Some(guard) = admission_guard.as_mut() {
                    guard.disarm();
                }
            }
            kovi::log::error!(
                "Yunxi Core runtime rejected inbound event: event_id={} message_id={} scope={:?} priority={:?} error={error}",
                event_id,
                message_id,
                event_scope,
                event_priority,
            );
            return Err(anyhow::anyhow!(error));
        }
    };
    if matches!(admission, Admission::DroppedAtCapacity) {
        kovi::log::warn!(
            "Yunxi Core runtime queue full: event_id={} message_id={} scope={:?} priority={:?} action=drop",
            event_id,
            message_id,
            event_scope,
            event_priority,
        );
    }
    if registered_incoming
        && !matches!(admission, Admission::Accepted)
        && let Some(model_backend) = model_backend.as_ref()
    {
        model_backend.discard_incoming(message_id).await;
        if let Some(guard) = admission_guard.as_mut() {
            guard.disarm();
        }
    } else if matches!(admission, Admission::Accepted) {
        // Core now owns the registered host context and will consume it in the
        // planner, so cancellation of this ingress future must not abandon it.
        if let Some(guard) = admission_guard.as_mut() {
            guard.disarm();
        }
    }
    if let Err(error) = submit_message_collisions(reply_scope, conversation_id, runtime).await {
        kovi::log::warn!("Yunxi message collision event was not admitted: {error}");
    }
    if matches!(admission, Admission::Accepted)
        && let Some(key) = reference_key
    {
        references.insert(
            key,
            MessageReference {
                message_id,
                from_agent: false,
            },
        );
    }
    if matches!(admission, Admission::Accepted) {
        if message.address.kind() == ConversationKind::Group {
            super::autonomous::observe_group_activity(conversation_id, message.timestamp);
        }
        super::autonomous::observe_inbound_from_person(
            conversation_id,
            message.address.kind(),
            message.timestamp,
            message.address.kind() == ConversationKind::Direct
                || message.addressed_to_agent
                || message.explicit_request
                || message.planner_attention_requested
                || recent_agent_reply,
            person_id,
        );
    }
    Ok(())
}

async fn resolve_and_submit_collisions(
    address: ConversationAddress,
    sender_user_id: i64,
    store: &dyn IdentityStore,
    message_store: Option<&super::identity_store::PostgresIdentityStore>,
    runtime: &RuntimeHandle,
) -> anyhow::Result<usize> {
    let external_conversation = address.external()?;
    let conversation_id = kovi::tokio::time::timeout(CORE_INGRESS_PROCESSING_TIMEOUT, async {
        if matches!(address, ConversationAddress::Direct { .. })
            && let Some(message_store) = message_store
        {
            let external_identity = qq::person(sender_user_id)?;
            let person_id = store
                .resolve_external_identity(&external_identity)
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
            message_store
                .resolve_direct_for_person(person_id, &external_conversation)
                .await
                .map_err(|error| anyhow::anyhow!(error))
        } else {
            store
                .resolve_external_conversation(&external_conversation)
                .await
                .map_err(|error| anyhow::anyhow!(error))
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Yunxi collision conversation lookup timed out after {}ms",
            CORE_INGRESS_PROCESSING_TIMEOUT.as_millis()
        )
    })??;
    submit_message_collisions(address.reply_scope(), conversation_id, runtime).await
}

async fn submit_message_collisions(
    reply_scope: ReplyScope,
    conversation_id: ConversationId,
    runtime: &RuntimeHandle,
) -> anyhow::Result<usize> {
    let collisions = take_message_collisions(reply_scope).await;
    let collision_count = collisions.len();
    let mut pending = collisions.into_iter();
    while let Some(collision) = pending.next() {
        kovi::log::debug!(
            "Yunxi message collision detected: scope={:?} source={:?}",
            collision.scope,
            collision.source,
        );
        let collision_event = WorldEvent::new(
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::MessageCollisionDetected(MessageCollisionDetectedEvent {
                conversation_id,
                outgoing_generation: collision.outgoing_generation,
                conversation_version: collision.conversation_version,
                fingerprint: collision.fingerprint,
            }),
        );
        // Shadow-mode World Model: record "both sides spoke almost at once"
        // as a ConversationEvent observation + scene version touch. It is a
        // normal world state change, never an apology/retract signal, and it
        // cannot influence delivery here (v4 appendix §3, §7).
        super::world_model::record_collision(conversation_id);
        if let Some(mind) = super::mind_runtime() {
            let timeout = std::time::Duration::from_millis(mind.config().event_update_timeout_ms());
            match kovi::tokio::time::timeout(timeout, mind.observe_event(&collision_event)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    kovi::log::warn!("Yunxi Mind collision observation failed soft: {error}");
                }
                Err(_) => {
                    kovi::log::warn!("Yunxi Mind collision observation timed out and failed soft");
                }
            }
        }
        let admission =
            submit_runtime_with_timeout(runtime, collision_event, CORE_RUNTIME_SUBMIT_TIMEOUT)
                .await;
        if !matches!(admission, Ok(Admission::Accepted)) {
            let remaining = std::iter::once(collision)
                .chain(pending)
                .collect::<Vec<_>>();
            let remaining_count = remaining.len();
            let restored = restore_message_collisions(reply_scope, remaining).await;
            if restored != remaining_count {
                kovi::log::warn!(
                    "Yunxi collision retry state disappeared during data erasure: restored={restored} expected={remaining_count}"
                );
            }
            return match admission {
                Err(error) => Err(error),
                Ok(Admission::DroppedAtCapacity) => Err(anyhow::anyhow!(
                    "Yunxi runtime dropped a collision at capacity"
                )),
                Ok(Admission::Accepted) => unreachable!("accepted admission matched above"),
            };
        }
    }
    Ok(collision_count)
}

async fn recent_bot_message(
    address: ConversationAddress,
    reply_to_external_message_id: Option<i64>,
) -> bool {
    let Some(message_id) = reply_to_external_message_id.and_then(|value| i32::try_from(value).ok())
    else {
        return false;
    };
    is_recent_bot_message(address.reply_scope(), message_id).await
}

/// Resolve a quote after it has crossed ingress admission. Durable outbound
/// mappings survive Redis restarts and are checked first; the short-lived
/// recall cache remains useful for legacy sends that predate the mapping.
///
/// A storage timeout/error is deliberately fail-open for a syntactically valid
/// quote. The message is already a bounded, visible Core candidate, and losing
/// it solely because a side-channel is unhealthy is the failure mode this
/// bridge is intended to avoid.
async fn resolve_recent_agent_reply(
    message: &InboundMessage,
    conversation_id: ConversationId,
    reply_reference: Option<MessageReference>,
    message_store: Option<&super::identity_store::PostgresIdentityStore>,
) -> bool {
    if message.replies_to_agent_hint
        || reply_reference.is_some_and(|reference| reference.from_agent)
    {
        return true;
    }
    let Some(external_message_id) = message.reply_to_external_message_id else {
        return false;
    };

    if let Some(message_store) = message_store {
        match kovi::tokio::time::timeout(
            CORE_REPLY_MAPPING_TIMEOUT,
            message_store.qq_outbound_message_exists(conversation_id, external_message_id),
        )
        .await
        {
            Ok(Ok(true)) => return true,
            Ok(Ok(false)) => {}
            Ok(Err(error)) => {
                kovi::log::warn!(
                    "Yunxi durable reply mapping lookup failed; treating quote as a Core reply candidate: {error}"
                );
                return true;
            }
            Err(_) => {
                kovi::log::warn!(
                    "Yunxi durable reply mapping lookup timed out; treating quote as a Core reply candidate"
                );
                return true;
            }
        }
    }

    match kovi::tokio::time::timeout(
        CORE_REPLY_MAPPING_TIMEOUT,
        recent_bot_message(message.address, Some(external_message_id)),
    )
    .await
    {
        Ok(found) => found,
        Err(_) => {
            kovi::log::warn!(
                "Yunxi Redis reply mapping lookup timed out; retaining the admitted Core candidate"
            );
            false
        }
    }
}

/// A group quote is only a visible turn after the worker proves that it
/// targets Yunxi (or deliberately fails open on a mapping outage). This keeps
/// replies to other members from bypassing the ambient-attention sampler just
/// because they carried a syntactically valid OneBot `reply` segment.
fn effective_visible_reply_allowed(message: &InboundMessage, recent_agent_reply: bool) -> bool {
    message.visible_reply_allowed
        && (message.address.kind() == ConversationKind::Direct
            || message.addressed_to_agent
            || recent_agent_reply
            || message.stop_requested
            || message.explicit_request
            || message.planner_attention_requested)
}

fn valid_qq_id(value: i64) -> bool {
    value > 0
}

fn positive_message_id(value: i32) -> Option<i64> {
    (value > 0).then_some(i64::from(value))
}

fn event_timestamp(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .unwrap_or_else(Utc::now)
}

fn bounded_text(value: &str) -> String {
    let mut bounded = String::with_capacity(value.len().min(MAX_MESSAGE_BYTES));
    for character in value.chars().take(MAX_MESSAGE_CHARS) {
        if bounded.len() + character.len_utf8() > MAX_MESSAGE_BYTES {
            break;
        }
        bounded.push(character);
    }
    bounded
}

/// Convert supported OneBot media segments into bounded opaque Core
/// references. The adapter never stores the segment JSON itself in Core.
fn normalize_attachments(message: &Message) -> Vec<Attachment> {
    message
        .iter()
        .filter_map(|segment| {
            let kind = match segment.type_.as_str() {
                "image" => AttachmentKind::Image,
                "record" | "audio" => AttachmentKind::Audio,
                "video" => AttachmentKind::Video,
                "file" => AttachmentKind::File,
                _ => return None,
            };
            let reference = ["file_unique", "md5", "file_id", "file", "url"]
                .iter()
                .find_map(|field| segment.data.get(*field).and_then(|value| value.as_str()))
                .map(bounded_text)
                .filter(|value| !value.trim().is_empty())?;
            Attachment::new(kind, reference).ok()
        })
        .take(16)
        .collect()
}

fn core_private_payload_is_supported(message: &Message, text: Option<&str>) -> bool {
    core_chat_payload_is_supported(message, text, false)
}

fn core_group_payload_is_supported(message: &Message, text: Option<&str>) -> bool {
    core_chat_payload_is_supported(message, text, true)
}

fn core_chat_payload_is_supported(message: &Message, text: Option<&str>, group: bool) -> bool {
    let text = text.unwrap_or_default().trim();
    let image_segments = message
        .iter()
        .filter(|segment| segment.type_ == "image")
        .count();
    let reply_segments = message
        .iter()
        .filter(|segment| segment.type_ == "reply")
        .count();
    let segments_supported = message.iter().all(|segment| {
        matches!(segment.type_.as_str(), "text" | "image" | "reply")
            || (group && segment.type_ == "at")
    });
    segments_supported
        && (!text.is_empty() || image_segments > 0 || reply_segments > 0)
        && !text.starts_with('#')
        && crate::vision::image_segments_are_resolvable(message)
}

fn message_at_self(message: &Message, self_id: i64) -> bool {
    message.iter().any(|segment| {
        // @所有人 同样包含芸汐，按点名处理。
        segment.type_ == "at"
            && segment
                .data
                .get("qq")
                .is_some_and(|qq| qq.as_str() == Some("all") || value_as_i64(qq) == Some(self_id))
    })
}

fn value_as_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse::<i64>().ok()))
}

fn reply_message_id(message: &Message) -> Option<i64> {
    message.iter().find_map(|segment| {
        (segment.type_ == "reply")
            .then(|| segment.data.get("id").and_then(value_as_i64))
            .flatten()
            .filter(|value| *value > 0)
    })
}

fn text_mentions_agent(message: &str) -> bool {
    ["芸汐", "云汐"].iter().any(|name| message.contains(name))
}

fn group_message_requests_explicit_batch(message: &Message, text: &str) -> bool {
    ambient_group_payload_can_be_sampled(message)
        && super::core_model::requested_message_count(text).is_some()
}

/// Read an explicit rollout switch while keeping the repository default
/// enabled. Unknown values fall back to the supplied default instead of
/// silently changing ownership at the ingress boundary.
fn core_cutover_enabled(name: &str, default: bool) -> bool {
    core_cutover_enabled_from_value(
        std::env::var_os(name)
            .as_deref()
            .and_then(|value| value.to_str()),
        default,
    )
}

fn core_cutover_enabled_from_value(value: Option<&str>, default: bool) -> bool {
    let Some(value) = value else {
        return default;
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}

fn ambient_group_payload_can_be_sampled(message: &Message) -> bool {
    !message
        .iter()
        .any(|segment| matches!(segment.type_.as_str(), "at" | "reply"))
}

#[cfg(test)]
mod tests {
    use super::{
        ActionCommandControl, ActionCommandState, ConversationAddress, CoreBridge, EnqueueOutcome,
        InboundMessage, IncomingAdmissionReleaseFuture, IncomingAdmissionReleaser,
        IngressRouteTracker, MessageReference, MessageReferenceCache, MessageReferenceKey,
        acquire_alias_handler_barriers, action_result_event, ambient_group_payload_can_be_sampled,
        block_user_aliases, bounded_text, core_cutover_enabled_from_value,
        core_group_payload_is_supported, core_private_payload_is_supported,
        dispatch_action_with_timeout, effective_visible_reply_allowed,
        group_message_requests_explicit_batch, idle_tick_event, merge_data_erasure_targets,
        message_at_self, normalize_attachments, reply_message_id, resolve_and_submit, run_ingress,
        run_runtime, send_action_ingress_command_with_ack, send_ingress_command_with_ack,
        send_ingress_command_with_ack_timeouts, submit_message_collisions,
        submit_runtime_with_timeout, text_mentions_agent, unblock_users,
    };
    use crate::model::{
        OutgoingSource, ReplyScope, commit_outgoing, interrupt, mark_active, mark_outgoing_sent,
        outgoing_fingerprint, prepare_outgoing, take_message_collisions,
    };
    use chrono::Utc;
    use kovi::bot::message::{Message, Segment};
    use kovi::tokio::sync::{Notify, mpsc};
    use serde_json::json;
    use sqlx_postgres::PgPoolOptions;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration as StdDuration;
    use yunxi_core::{
        ActionArbiter, ActionArbiterConfig, ActionPort, ActionPortFuture, ActionPortOutcome,
        ActionRejection, ActionResult, Admission, AttachmentKind, AttentionDisposition,
        ConversationId, ConversationKind, EnvironmentCapabilities, EventPriority, EventType,
        IdentityStore, IdentityStoreError, IdentityStoreFuture, MessageContent, MessageId,
        MessageReceivedEvent, PersonId, ProcessingOutcome, ProposedAction, RuntimeConfig,
        WorldEvent,
    };

    struct FakeIdentityStore {
        person_id: PersonId,
        conversation_id: ConversationId,
        stored_kind: ConversationKind,
    }

    struct FailingIdentityStore;

    #[derive(Default)]
    struct TestIncomingAdmissionReleaser {
        admissions: kovi::tokio::sync::Mutex<HashMap<MessageId, crate::model::IncomingAdmission>>,
        discarded: StdMutex<Vec<MessageId>>,
    }

    impl IncomingAdmissionReleaser for TestIncomingAdmissionReleaser {
        fn discard<'a>(&'a self, message_id: MessageId) -> IncomingAdmissionReleaseFuture<'a> {
            Box::pin(async move {
                if let Some(admission) = self.admissions.lock().await.remove(&message_id) {
                    crate::model::ConversationCoordinator::abandon_incoming(admission).await;
                }
                self.discarded
                    .lock()
                    .expect("discard recorder lock")
                    .push(message_id);
            })
        }
    }

    struct BlockingActionPort {
        conversation_id: ConversationId,
        calls: AtomicUsize,
        entered: Notify,
        release: Notify,
    }

    impl ActionPort for BlockingActionPort {
        fn execute<'a>(&'a self, _action: &'a ProposedAction) -> ActionPortFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
                Ok(ActionPortOutcome::Delivered {
                    external_reference: None,
                    message_id: None,
                    conversation_id: Some(self.conversation_id),
                })
            })
        }
    }

    #[test]
    fn pending_action_ack_timeout_cancels_before_worker_side_effect() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let user_id = 456;
            let conversation_id = ConversationId::new();
            let person_id = PersonId::new();
            let (ingress, receiver) = mpsc::channel(2);
            let (runtime_handle, _core_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let arbiter = Arc::new(ActionArbiter::new(
                ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
            ));
            let port = Arc::new(BlockingActionPort {
                conversation_id,
                calls: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Notify::new(),
            });
            let action_port: Arc<dyn ActionPort> = port.clone();
            let control = Arc::new(ActionCommandControl::new());
            let action = ProposedAction::send_message(
                conversation_id,
                MessageContent::text("must stay cancelled"),
            )
            .expect("valid action");
            let (acknowledge, acknowledged) = kovi::tokio::sync::oneshot::channel();

            let error = send_action_ingress_command_with_ack(
                &ingress,
                super::IngressCommand::DispatchAction {
                    user_id,
                    action,
                    control: Arc::clone(&control),
                    acknowledge,
                },
                acknowledged,
                &control,
                StdDuration::from_secs(1),
                StdDuration::from_millis(10),
                StdDuration::from_millis(100),
            )
            .await
            .expect_err("a pending command must be cancelled at its first deadline");
            assert!(!error.is_indeterminate());
            assert_eq!(control.state(), ActionCommandState::Cancelled);

            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id,
                conversation_id,
                stored_kind: ConversationKind::Direct,
            });
            let ingress_task = kovi::tokio::spawn(run_ingress(
                receiver,
                store,
                runtime_handle,
                None,
                None,
                None,
                Arc::new(StdMutex::new(HashSet::new())),
                Arc::new(StdMutex::new(HashSet::new())),
                Some(arbiter),
                Some(action_port),
                Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            ));
            drop(ingress);
            kovi::tokio::time::timeout(StdDuration::from_secs(1), ingress_task)
                .await
                .expect("cancelled command must not stall the worker")
                .expect("ingress worker");
            assert_eq!(port.calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn running_action_gets_completion_grace_after_first_ack_deadline() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let user_id = 456;
            let conversation_id = ConversationId::new();
            let person_id = PersonId::new();
            let (ingress, receiver) = mpsc::channel(2);
            let (runtime_handle, _core_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let arbiter = Arc::new(ActionArbiter::new(
                ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
            ));
            let port = Arc::new(BlockingActionPort {
                conversation_id,
                calls: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Notify::new(),
            });
            let action_port: Arc<dyn ActionPort> = port.clone();
            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id,
                conversation_id,
                stored_kind: ConversationKind::Direct,
            });
            let ingress_task = kovi::tokio::spawn(run_ingress(
                receiver,
                store,
                runtime_handle,
                None,
                None,
                None,
                Arc::new(StdMutex::new(HashSet::new())),
                Arc::new(StdMutex::new(HashSet::new())),
                Some(arbiter),
                Some(action_port),
                Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            ));
            let control = Arc::new(ActionCommandControl::new());
            let action = ProposedAction::send_message(
                conversation_id,
                MessageContent::text("finish during grace"),
            )
            .expect("valid action");
            let (acknowledge, acknowledged) = kovi::tokio::sync::oneshot::channel();
            let dispatch = send_action_ingress_command_with_ack(
                &ingress,
                super::IngressCommand::DispatchAction {
                    user_id,
                    action,
                    control: Arc::clone(&control),
                    acknowledge,
                },
                acknowledged,
                &control,
                StdDuration::from_secs(1),
                StdDuration::from_millis(10),
                StdDuration::from_millis(250),
            );
            let release = async {
                kovi::tokio::time::timeout(StdDuration::from_secs(1), port.entered.notified())
                    .await
                    .expect("worker must claim the action");
                kovi::tokio::time::sleep(StdDuration::from_millis(30)).await;
                assert_eq!(control.state(), ActionCommandState::Running);
                port.release.notify_one();
            };
            let (result, ()) = kovi::tokio::join!(dispatch, release);
            assert!(matches!(
                result,
                Ok(Ok(Some(ActionResult::Executed {
                    outcome: ActionPortOutcome::Delivered { .. },
                    ..
                })))
            ));
            assert_eq!(control.state(), ActionCommandState::Finished);
            assert_eq!(port.calls.load(Ordering::SeqCst), 1);

            drop(ingress);
            kovi::tokio::time::timeout(StdDuration::from_secs(1), ingress_task)
                .await
                .expect("ingress worker must stop")
                .expect("ingress worker");
        });
    }

    #[test]
    fn action_dispatch_timeout_releases_the_worker_without_permitting_replay() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let conversation_id = ConversationId::new();
            let arbiter = ActionArbiter::new(
                ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
            );
            let port = BlockingActionPort {
                conversation_id,
                calls: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Notify::new(),
            };
            let action =
                ProposedAction::send_message(conversation_id, MessageContent::text("timeout"))
                    .expect("valid action");
            let entered = &port.entered;
            let action_id = action.action_id().expect("action id");
            let dispatch = dispatch_action_with_timeout(
                &arbiter,
                &port,
                action.clone(),
                StdDuration::from_millis(10),
            );
            let (result, _) = kovi::tokio::join!(dispatch, entered.notified());
            assert!(matches!(
                result,
                ActionResult::Executed {
                    outcome: ActionPortOutcome::DeliveryIndeterminate { .. },
                    ..
                }
            ));
            let replay_port = BlockingActionPort {
                conversation_id,
                calls: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Notify::new(),
            };
            assert!(matches!(
                arbiter.dispatch(action, &replay_port).await,
                ActionResult::Rejected(ActionRejection::Duplicate {
                    original_action_id,
                    ..
                }) if original_action_id == action_id
            ));
            assert_eq!(port.calls.load(Ordering::SeqCst), 1);
            assert_eq!(replay_port.calls.load(Ordering::SeqCst), 0);
        });
    }

    impl IdentityStore for FailingIdentityStore {
        fn resolve_external_identity<'a>(
            &'a self,
            _external: &'a yunxi_core::ExternalIdentity,
        ) -> IdentityStoreFuture<'a, PersonId> {
            Box::pin(async {
                Err(IdentityStoreError::storage(std::io::Error::other(
                    "identity lookup unavailable",
                )))
            })
        }

        fn resolve_external_conversation<'a>(
            &'a self,
            _external: &'a yunxi_core::ExternalConversation,
        ) -> IdentityStoreFuture<'a, ConversationId> {
            Box::pin(async {
                Err(IdentityStoreError::storage(std::io::Error::other(
                    "conversation lookup unavailable",
                )))
            })
        }
    }

    impl IdentityStore for FakeIdentityStore {
        fn resolve_external_identity<'a>(
            &'a self,
            _external: &'a yunxi_core::ExternalIdentity,
        ) -> IdentityStoreFuture<'a, PersonId> {
            Box::pin(async move { Ok(self.person_id) })
        }

        fn resolve_external_conversation<'a>(
            &'a self,
            external: &'a yunxi_core::ExternalConversation,
        ) -> IdentityStoreFuture<'a, ConversationId> {
            Box::pin(async move {
                if external.kind() != self.stored_kind {
                    return Err(IdentityStoreError::ConversationKindMismatch {
                        requested: external.kind(),
                        stored: self.stored_kind,
                    });
                }
                Ok(self.conversation_id)
            })
        }
    }

    fn inbound(address: ConversationAddress, addressed_to_agent: bool) -> InboundMessage {
        InboundMessage {
            address,
            sender_user_id: 456,
            external_message_id: Some(789),
            reply_to_external_message_id: None,
            replies_to_agent_hint: false,
            text: "hello".to_string(),
            attachments: Vec::new(),
            vision_attachments: Vec::new(),
            timestamp: Utc::now(),
            addressed_to_agent,
            visible_reply_allowed: true,
            explicit_request: false,
            stop_requested: false,
            planner_attention_requested: addressed_to_agent,
            incoming_admission: None,
        }
    }

    #[test]
    fn quote_candidate_requires_an_agent_mapping_before_visible_reply() {
        let mut message = inbound(ConversationAddress::Group { group_id: 123 }, false);
        message.reply_to_external_message_id = Some(456);

        assert!(!effective_visible_reply_allowed(&message, false));
        assert!(effective_visible_reply_allowed(&message, true));

        message.planner_attention_requested = true;
        assert!(effective_visible_reply_allowed(&message, false));

        message.visible_reply_allowed = false;
        assert!(!effective_visible_reply_allowed(&message, true));
    }

    #[test]
    fn text_is_bounded_by_unicode_chars_and_bytes() {
        let bounded = bounded_text(&"界".repeat(20_000));
        assert_eq!(bounded.chars().count(), 8_192);
        assert!(bounded.len() <= 32 * 1_024);
    }

    #[test]
    fn reply_ids_accept_numbers_and_decimal_strings() {
        let message = Message::from(vec![
            Segment::new("reply", json!({"id": "12345"})),
            Segment::new("reply", json!({"id": 67890})),
        ]);
        assert_eq!(reply_message_id(&message), Some(12345));
    }

    #[test]
    fn structured_at_and_name_detection_are_conservative() {
        let at = Message::from(vec![Segment::new("at", json!({"qq": "123"}))]);
        assert!(message_at_self(&at, 123));
        assert!(!message_at_self(&at, 456));
        let at_all = Message::from(vec![Segment::new("at", json!({"qq": "all"}))]);
        assert!(message_at_self(&at_all, 123));
        assert!(!ambient_group_payload_can_be_sampled(&at));
        let reply = Message::from(vec![Segment::new("reply", json!({"id": "456"}))]);
        assert!(!ambient_group_payload_can_be_sampled(&reply));
        assert!(ambient_group_payload_can_be_sampled(&Message::from(
            "大家觉得这个怎么样"
        )));
        assert!(text_mentions_agent("芸汐，看看这个"));
    }

    #[test]
    fn explicit_group_message_batches_receive_reply_admission_without_a_mention() {
        let plain = Message::from("给我发两条消息");
        assert!(group_message_requests_explicit_batch(
            &plain,
            "给我发两条消息"
        ));
        assert!(group_message_requests_explicit_batch(
            &Message::from("请发送 3 条自然回复"),
            "请发送 3 条自然回复"
        ));
        assert!(!group_message_requests_explicit_batch(
            &Message::from("帮我检查这两条消息为什么没发出去"),
            "帮我检查这两条消息为什么没发出去"
        ));
        assert!(!group_message_requests_explicit_batch(
            &Message::from("他说发两条消息"),
            "他说发两条消息"
        ));

        let at_other = Message::from(vec![
            Segment::new("at", json!({"qq": "456"})),
            Segment::new("text", json!({"text": "给我发两条消息"})),
        ]);
        assert!(!group_message_requests_explicit_batch(
            &at_other,
            "给我发两条消息"
        ));
        let reply_other = Message::from(vec![
            Segment::new("reply", json!({"id": "789"})),
            Segment::new("text", json!({"text": "给我发两条消息"})),
        ]);
        assert!(!group_message_requests_explicit_batch(
            &reply_other,
            "给我发两条消息"
        ));
    }

    #[test]
    fn cutover_flags_have_explicit_boolean_parsing_and_safe_defaults() {
        for value in ["1", "true", "YES", "on"] {
            assert!(core_cutover_enabled_from_value(Some(value), false));
        }
        for value in ["0", "false", "NO", "off"] {
            assert!(!core_cutover_enabled_from_value(Some(value), true));
        }
        assert!(core_cutover_enabled_from_value(None, true));
        assert!(!core_cutover_enabled_from_value(Some("unexpected"), false));
    }

    #[test]
    fn core_private_media_support_is_limited_to_resolvable_images() {
        let text = Message::from("你好呀");
        assert!(core_private_payload_is_supported(&text, Some("你好呀")));

        let image = Message::from(vec![Segment::new(
            "image",
            json!({"file_unique": "image-hash", "file": "image.png"}),
        )]);
        assert!(core_private_payload_is_supported(&image, None));

        let text_and_image = Message::from(vec![
            Segment::new("text", json!({"text": "看看这个"})),
            Segment::new(
                "image",
                json!({"file_unique": "image-hash", "url": "https://example.test/image.png"}),
            ),
        ]);
        assert!(core_private_payload_is_supported(
            &text_and_image,
            Some("看看这个")
        ));

        let unusable_image = Message::from(vec![Segment::new("image", json!({}))]);
        assert!(!core_private_payload_is_supported(&unusable_image, None));
        let audio = Message::from(vec![Segment::new("record", json!({"file": "voice.amr"}))]);
        assert!(!core_private_payload_is_supported(&audio, None));
        assert!(!core_private_payload_is_supported(
            &Message::from("#看图"),
            Some("#看图")
        ));
    }

    #[test]
    fn core_group_media_accepts_ambient_content() {
        let addressed = Message::from(vec![
            Segment::new("text", json!({"text": "芸汐看看这个"})),
            Segment::new(
                "image",
                json!({"file_unique": "image-hash", "file": "image.png"}),
            ),
        ]);
        assert!(core_group_payload_is_supported(
            &addressed,
            Some("芸汐看看这个")
        ));

        let structured_at = Message::from(vec![
            Segment::new("at", json!({"qq": "123"})),
            Segment::new(
                "image",
                json!({"file_unique": "image-hash", "file": "image.png"}),
            ),
        ]);
        assert!(core_group_payload_is_supported(&structured_at, None));

        let ambient = Message::from(vec![Segment::new(
            "image",
            json!({"file_unique": "image-hash", "file": "image.png"}),
        )]);
        assert!(core_group_payload_is_supported(&ambient, None));

        let reply_with_text = Message::from(vec![
            Segment::new("reply", json!({"id": "12345"})),
            Segment::new("text", json!({"text": "接着说"})),
        ]);
        assert!(core_group_payload_is_supported(
            &reply_with_text,
            Some("接着说")
        ));

        let reply_only = Message::from(vec![Segment::new("reply", json!({"id": "12345"}))]);
        assert!(core_group_payload_is_supported(&reply_only, None));
        assert!(core_private_payload_is_supported(&reply_only, None));
    }

    #[test]
    fn ambient_attention_is_sampled_after_a_bounded_message_floor() {
        let mut registry = super::AmbientAttentionRegistry::new();
        let policy = super::AmbientAttentionPolicy {
            enabled: true,
            min_eligible_messages: 2,
            candidate_cooldown_secs: 180,
            response_probability_percent: 100,
            min_message_chars: 4,
            decision_rate_window_secs: 600,
            decision_rate_limit: 3,
        };
        let sample = |registry: &mut super::AmbientAttentionRegistry, message_id| {
            registry.should_request(123, message_id, 8, false, policy)
        };

        assert!(!sample(&mut registry, 1));
        assert!(sample(&mut registry, 2));
        assert!(!sample(&mut registry, 3));
    }

    #[test]
    fn reference_cache_is_bounded_and_isolated_by_conversation() {
        let first_conversation = ConversationId::new();
        let second_conversation = ConversationId::new();
        let mut cache = MessageReferenceCache::new(2);
        let first_key = MessageReferenceKey {
            conversation_id: first_conversation,
            external_message_id: 1,
        };
        cache.insert(
            first_key,
            MessageReference {
                message_id: MessageId::new(),
                from_agent: false,
            },
        );
        cache.insert(
            MessageReferenceKey {
                conversation_id: second_conversation,
                external_message_id: 1,
            },
            MessageReference {
                message_id: MessageId::new(),
                from_agent: false,
            },
        );
        cache.insert(
            MessageReferenceKey {
                conversation_id: first_conversation,
                external_message_id: 2,
            },
            MessageReference {
                message_id: MessageId::new(),
                from_agent: false,
            },
        );
        assert_eq!(cache.len(), 2);
        assert!(cache.get(first_key).is_none());
        assert!(
            cache
                .get(MessageReferenceKey {
                    conversation_id: first_conversation,
                    external_message_id: 2,
                })
                .is_some()
        );
        assert!(
            cache
                .get(MessageReferenceKey {
                    conversation_id: second_conversation,
                    external_message_id: 1,
                })
                .is_some()
        );
    }

    #[test]
    fn ingress_drops_at_capacity_without_waiting() {
        let (ingress, _receiver) = mpsc::channel(1);
        let (runtime, _consumer) =
            yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        let bridge = CoreBridge {
            ingress,
            runtime,
            action_arbiter: None,
            action_port: None,
            blocked_users: Arc::new(StdMutex::new(HashSet::new())),
            blocked_groups: Arc::new(StdMutex::new(HashSet::new())),
            private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
        };
        let message = inbound(ConversationAddress::Group { group_id: 123 }, false);

        assert_eq!(
            bridge.try_enqueue(message.clone()),
            EnqueueOutcome::Accepted
        );
        assert_eq!(
            bridge.try_enqueue(message),
            EnqueueOutcome::DroppedAtCapacity
        );
    }

    #[test]
    fn reliable_ingress_waits_for_capacity_and_then_accepts() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (ingress, mut receiver) = mpsc::channel(1);
            let (runtime_handle, _consumer) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let bridge = Arc::new(CoreBridge {
                ingress,
                runtime: runtime_handle,
                action_arbiter: None,
                action_port: None,
                blocked_users: Arc::new(StdMutex::new(HashSet::new())),
                blocked_groups: Arc::new(StdMutex::new(HashSet::new())),
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
            });
            assert_eq!(
                bridge.try_enqueue(inbound(ConversationAddress::Group { group_id: 123 }, true)),
                EnqueueOutcome::Accepted
            );

            let waiting_bridge = Arc::clone(&bridge);
            let mut waiting = kovi::tokio::spawn(async move {
                waiting_bridge
                    .send_reliably(inbound(ConversationAddress::Group { group_id: 123 }, true))
                    .await
            });
            assert!(
                kovi::tokio::time::timeout(StdDuration::from_millis(20), &mut waiting)
                    .await
                    .is_err(),
                "reliable ingress must wait while the queue is full"
            );
            receiver
                .recv()
                .await
                .expect("first message should be queued");
            assert_eq!(
                waiting.await.expect("waiting task should join"),
                EnqueueOutcome::Accepted
            );
            assert!(
                receiver.recv().await.is_some(),
                "waiting message should arrive"
            );
        });
    }

    #[test]
    fn reliable_ingress_times_out_when_capacity_never_returns() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (ingress, _receiver) = mpsc::channel(1);
            let (runtime_handle, _consumer) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let bridge = CoreBridge {
                ingress,
                runtime: runtime_handle,
                action_arbiter: None,
                action_port: None,
                blocked_users: Arc::new(StdMutex::new(HashSet::new())),
                blocked_groups: Arc::new(StdMutex::new(HashSet::new())),
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
            };
            assert_eq!(
                bridge.try_enqueue(inbound(ConversationAddress::Group { group_id: 123 }, true)),
                EnqueueOutcome::Accepted
            );
            let started = std::time::Instant::now();
            let outcome = bridge
                .send_reliably_with_timeout(
                    inbound(ConversationAddress::Group { group_id: 123 }, true),
                    StdDuration::from_millis(10),
                )
                .await;
            assert_eq!(outcome, EnqueueOutcome::DroppedAtCapacity);
            assert!(started.elapsed() < StdDuration::from_secs(1));
        });
    }

    #[test]
    fn acknowledged_ingress_commands_bound_enqueue_and_worker_waits() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (full_ingress, mut full_receiver) = mpsc::channel(1);
            full_ingress
                .send(super::IngressCommand::Message(inbound(
                    ConversationAddress::Group { group_id: 123 },
                    false,
                )))
                .await
                .expect("fill ingress queue");
            let (acknowledge, acknowledged) = kovi::tokio::sync::oneshot::channel();
            let enqueue_error = send_ingress_command_with_ack(
                &full_ingress,
                super::IngressCommand::ProjectDestination {
                    destination: crate::model::MessageDestination::Group(123),
                    priority: EventPriority::Normal,
                    kind: yunxi_core::WorldEventKind::HostStarted,
                    acknowledge,
                },
                acknowledged,
                StdDuration::from_millis(10),
                "test projection",
            )
            .await
            .expect_err("full ingress queue must time out");
            assert!(enqueue_error.to_string().contains("enqueue timed out"));
            assert!(matches!(
                full_receiver.recv().await,
                Some(super::IngressCommand::Message(_))
            ));
            assert!(full_receiver.try_recv().is_err());

            let (idle_ingress, _idle_receiver) = mpsc::channel(1);
            let (acknowledge, acknowledged) = kovi::tokio::sync::oneshot::channel();
            let acknowledgement_error = send_ingress_command_with_ack_timeouts(
                &idle_ingress,
                super::IngressCommand::ProjectDestination {
                    destination: crate::model::MessageDestination::Group(123),
                    priority: EventPriority::Normal,
                    kind: yunxi_core::WorldEventKind::HostStarted,
                    acknowledge,
                },
                acknowledged,
                StdDuration::from_secs(1),
                StdDuration::from_millis(10),
                "test projection",
            )
            .await
            .expect_err("stalled ingress worker must time out");
            assert!(
                acknowledgement_error
                    .to_string()
                    .contains("outcome may be indeterminate")
            );
        });
    }

    #[test]
    fn high_priority_runtime_submit_times_out_when_runtime_queue_is_full() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (handle, _core_runtime) = yunxi_core::CognitiveRuntime::new(RuntimeConfig {
                event_queue_capacity: 1,
                ..RuntimeConfig::default()
            })
            .expect("valid runtime");
            let first = WorldEvent::new(
                Utc::now(),
                yunxi_core::EventScope::Global,
                EventPriority::High,
                yunxi_core::WorldEventKind::HostStarted,
            );
            assert_eq!(handle.submit(first).await, Ok(Admission::Accepted));
            let second = WorldEvent::new(
                Utc::now(),
                yunxi_core::EventScope::Global,
                EventPriority::High,
                yunxi_core::WorldEventKind::HostStarted,
            );
            let error = submit_runtime_with_timeout(&handle, second, StdDuration::from_millis(10))
                .await
                .expect_err("full high-priority queue should time out");
            assert!(error.to_string().contains("timed out"));
        });
    }

    #[test]
    fn group_data_erasure_gate_synchronously_blocks_core_ingress_and_resumes() {
        let (ingress, _receiver) = mpsc::channel(1);
        let (runtime, _consumer) =
            yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        let bridge = CoreBridge {
            ingress,
            runtime,
            action_arbiter: None,
            action_port: None,
            blocked_users: Arc::new(StdMutex::new(HashSet::new())),
            blocked_groups: Arc::new(StdMutex::new(HashSet::from([123]))),
            private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
        };

        assert!(bridge.is_group_blocked(123));
        assert_eq!(
            bridge.try_enqueue(inbound(ConversationAddress::Group { group_id: 123 }, true)),
            EnqueueOutcome::Blocked
        );
        bridge.unblock_group(123);
        assert!(!bridge.is_group_blocked(123));
        assert_eq!(
            bridge.try_enqueue(inbound(ConversationAddress::Group { group_id: 123 }, true)),
            EnqueueOutcome::Accepted
        );
    }

    #[test]
    fn collision_flush_waits_for_full_ingress_and_reaches_core() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let user_id = 9_200_456;
            let queued_user_id = 9_200_457;
            let self_id = 111;
            let conversation_id = ConversationId::new();
            let scope = ReplyScope::Private(user_id);
            let ticket = interrupt(scope).await;
            assert!(mark_active(ticket).await);
            let outgoing = prepare_outgoing(
                ticket,
                outgoing_fingerprint("already committed"),
                OutgoingSource::Reply,
            )
            .await
            .expect("outgoing should prepare");
            assert!(commit_outgoing(outgoing).await);
            let _ = interrupt(scope).await;
            mark_outgoing_sent(outgoing).await;

            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id,
                stored_kind: ConversationKind::Direct,
            });
            let (ingress, receiver) = mpsc::channel(1);
            let (runtime_handle, mut core_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let blocked_users = Arc::new(StdMutex::new(HashSet::new()));
            let blocked_groups = Arc::new(StdMutex::new(HashSet::new()));
            let bridge = Arc::new(CoreBridge {
                ingress,
                runtime: runtime_handle.clone(),
                action_arbiter: None,
                action_port: None,
                blocked_users: Arc::clone(&blocked_users),
                blocked_groups: Arc::clone(&blocked_groups),
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
            });
            let mut queued = inbound(
                ConversationAddress::Direct {
                    self_id,
                    peer_user_id: queued_user_id,
                },
                true,
            );
            queued.sender_user_id = queued_user_id;
            queued.external_message_id = Some(790);
            assert_eq!(bridge.try_enqueue(queued), EnqueueOutcome::Accepted);
            assert_eq!(
                bridge.try_enqueue(inbound(
                    ConversationAddress::Direct {
                        self_id,
                        peer_user_id: user_id,
                    },
                    true,
                )),
                EnqueueOutcome::DroppedAtCapacity
            );

            let flush_bridge = Arc::clone(&bridge);
            let mut flush = kovi::tokio::spawn(async move {
                flush_bridge
                    .flush_message_collisions(
                        user_id,
                        ConversationAddress::Direct {
                            self_id,
                            peer_user_id: user_id,
                        },
                    )
                    .await
            });
            assert!(
                kovi::tokio::time::timeout(StdDuration::from_millis(20), &mut flush)
                    .await
                    .is_err(),
                "reliable collision flush must wait while ingress is full"
            );

            kovi::tokio::spawn(run_ingress(
                receiver,
                store,
                runtime_handle,
                None,
                None,
                None,
                blocked_users,
                blocked_groups,
                None,
                None,
                Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            ));
            assert_eq!(
                flush
                    .await
                    .expect("flush task should complete")
                    .expect("collision should resolve"),
                1
            );

            let ProcessingOutcome::Observed(message) = core_runtime
                .process_next()
                .await
                .expect("queued message should reach Core")
            else {
                panic!("queued message should be observed");
            };
            assert_eq!(message.event_type, EventType::MessageReceived);
            let ProcessingOutcome::Observed(collision) = core_runtime
                .process_next()
                .await
                .expect("collision should reach Core")
            else {
                panic!("collision should be observed");
            };
            assert_eq!(collision.event_type, EventType::MessageCollisionDetected);
            assert_eq!(
                collision.scope,
                yunxi_core::EventScope::Conversation { conversation_id }
            );
            assert_eq!(collision.priority, EventPriority::High);
        });
    }

    #[test]
    fn collision_flush_restores_unsubmitted_records_when_runtime_is_closed() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let scope = ReplyScope::Private(9_200_458);
            let ticket = interrupt(scope).await;
            assert!(mark_active(ticket).await);
            let outgoing = prepare_outgoing(
                ticket,
                outgoing_fingerprint("committed before runtime close"),
                OutgoingSource::Reply,
            )
            .await
            .expect("outgoing should prepare");
            assert!(commit_outgoing(outgoing).await);
            let _ = interrupt(scope).await;
            mark_outgoing_sent(outgoing).await;

            let (runtime_handle, core_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            drop(core_runtime);
            let error = submit_message_collisions(scope, ConversationId::new(), &runtime_handle)
                .await
                .expect_err("closed runtime must reject the collision");
            assert!(error.to_string().contains("closed"));

            let restored = take_message_collisions(scope).await;
            assert_eq!(restored.len(), 1);
            assert_eq!(restored[0].scope, scope);
        });
    }

    #[test]
    fn lost_begin_acknowledgement_keeps_host_ingress_fail_closed() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (ingress, mut receiver) = mpsc::channel(1);
            let (runtime_handle, _consumer) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let bridge = Arc::new(CoreBridge {
                ingress,
                runtime: runtime_handle,
                action_arbiter: None,
                action_port: None,
                blocked_users: Arc::new(StdMutex::new(HashSet::new())),
                blocked_groups: Arc::new(StdMutex::new(HashSet::new())),
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
            });
            kovi::tokio::spawn(async move {
                let Some(super::IngressCommand::BeginDataErasure { acknowledge, .. }) =
                    receiver.recv().await
                else {
                    panic!("expected begin command");
                };
                drop(acknowledge);
            });

            let Err(error) = bridge.begin_user_data_erasure(456).await else {
                panic!("lost acknowledgement must fail");
            };
            assert!(error.to_string().contains("remains blocked"));
            assert!(bridge.is_user_blocked(456));
        });
    }

    #[test]
    fn lost_group_begin_acknowledgement_keeps_host_ingress_fail_closed() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (ingress, mut receiver) = mpsc::channel(1);
            let (runtime_handle, _consumer) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let bridge = Arc::new(CoreBridge {
                ingress,
                runtime: runtime_handle,
                action_arbiter: None,
                action_port: None,
                blocked_users: Arc::new(StdMutex::new(HashSet::new())),
                blocked_groups: Arc::new(StdMutex::new(HashSet::new())),
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
            });
            kovi::tokio::spawn(async move {
                let Some(super::IngressCommand::BeginGroupDataErasure { acknowledge, .. }) =
                    receiver.recv().await
                else {
                    panic!("expected group begin command");
                };
                drop(acknowledge);
            });

            let Err(error) = bridge.begin_group_data_erasure(456).await else {
                panic!("lost acknowledgement must fail");
            };
            assert!(error.to_string().contains("remains blocked"));
            assert!(bridge.is_group_blocked(456));
        });
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_group_erasure_barrier_purges_runtime_references_and_mapping() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
            let pool = PgPoolOptions::new()
                .max_connections(2)
                .connect(&database_url)
                .await
                .expect("connect PostgreSQL");
            let executive_store = Arc::new(
                crate::yunxi::executive_store::PostgresExecutiveStore::new(pool.clone()),
            );
            executive_store
                .initialize_schema()
                .await
                .expect("initialize Executive schema");
            let store = Arc::new(crate::yunxi::identity_store::PostgresIdentityStore::new(
                pool,
            ));
            store
                .initialize_schema()
                .await
                .expect("initialize identity schema");
            let group_id = i64::try_from(
                (uuid::Uuid::new_v4().as_u128() % 8_000_000_000_u128) + 1_000_000_000_u128,
            )
            .expect("bounded test group id");
            let user_id = i64::try_from(
                (uuid::Uuid::new_v4().as_u128() % 8_000_000_000_u128) + 1_000_000_000_u128,
            )
            .expect("bounded test user id");
            let (runtime_handle, mut core_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let (ingress, receiver) = mpsc::channel(8);
            let blocked_users = Arc::new(StdMutex::new(HashSet::new()));
            let blocked_groups = Arc::new(StdMutex::new(HashSet::new()));
            let identity_store: Arc<dyn IdentityStore> = store.clone();
            let ingress_task = kovi::tokio::spawn(run_ingress(
                receiver,
                identity_store,
                runtime_handle.clone(),
                None,
                Some(Arc::clone(&store)),
                Some(executive_store),
                Arc::clone(&blocked_users),
                Arc::clone(&blocked_groups),
                None,
                None,
                Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            ));
            let driver = kovi::tokio::spawn(async move {
                while core_runtime.process_next().await.is_some() {}
                core_runtime
            });
            let bridge = Arc::new(CoreBridge {
                ingress,
                runtime: runtime_handle.clone(),
                action_arbiter: None,
                action_port: None,
                blocked_users,
                blocked_groups,
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
            });
            let mut message = inbound(ConversationAddress::Group { group_id }, true);
            message.sender_user_id = user_id;
            message.external_message_id = Some(42);
            assert_eq!(bridge.try_enqueue(message), EnqueueOutcome::Accepted);

            let erasure = bridge
                .begin_group_data_erasure(group_id)
                .await
                .expect("begin group barrier");
            let ack = erasure.ack();
            assert_eq!(ack.conversation_ids.len(), 1);
            let conversation_id = ack.conversation_ids[0];
            assert_eq!(ack.purged_runtime_states, 1);
            assert_eq!(ack.cleared_references, 1);

            assert!(
                store
                    .delete_qq_group_domain_data(group_id)
                    .await
                    .expect("delete canonical group")
                    > 0
            );
            assert!(
                bridge
                    .project_destination(
                        crate::model::MessageDestination::Group(group_id),
                        EventPriority::High,
                        yunxi_core::WorldEventKind::ActionSucceeded(
                            yunxi_core::ActionSucceededEvent {
                                idempotency_key: "blocked-projection".to_string(),
                            },
                        ),
                    )
                    .await
                    .is_err(),
                "projection must be rejected before it can recreate the deleted mapping"
            );
            let (projection_acknowledge, projected) = kovi::tokio::sync::oneshot::channel();
            bridge
                .ingress
                .send(super::IngressCommand::ProjectDestination {
                    destination: crate::model::MessageDestination::Group(group_id),
                    priority: EventPriority::High,
                    kind: yunxi_core::WorldEventKind::ActionSucceeded(
                        yunxi_core::ActionSucceededEvent {
                            idempotency_key: "fifo-blocked-projection".to_string(),
                        },
                    ),
                    acknowledge: projection_acknowledge,
                })
                .await
                .expect("enqueue projection behind begin barrier");
            assert!(
                projected
                    .await
                    .expect("projection acknowledgement")
                    .is_err(),
                "ingress FIFO must reject a raced projection before identity resolution"
            );
            assert_eq!(
                store
                    .qq_group_conversation_id(group_id)
                    .await
                    .expect("lookup group during barrier"),
                None
            );
            runtime_handle
                .submit(WorldEvent::new(
                    Utc::now(),
                    yunxi_core::EventScope::Conversation { conversation_id },
                    EventPriority::High,
                    yunxi_core::WorldEventKind::ActionSucceeded(yunxi_core::ActionSucceededEvent {
                        idempotency_key: "late-group-receipt".to_string(),
                    }),
                ))
                .await
                .expect("enqueue blocked late receipt");
            erasure.finish().await.expect("end group barrier");
            assert_eq!(
                store
                    .qq_group_conversation_id(group_id)
                    .await
                    .expect("lookup deleted group"),
                None
            );
            store
                .delete_person_domain_data(
                    &crate::yunxi::qq::person(user_id).expect("valid QQ user"),
                    &crate::yunxi::qq::direct(9_999_999_999, user_id)
                        .expect("valid cleanup direct route"),
                )
                .await
                .expect("clean up test person");

            drop(bridge);
            drop(runtime_handle);
            ingress_task.await.expect("ingress worker");
            let core_runtime = driver.await.expect("runtime driver");
            assert!(core_runtime.state().conversation(conversation_id).is_none());
        });
    }

    #[test]
    fn private_handler_epoch_drains_old_work_and_rejects_waiters_across_erasure() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Direct,
            });
            let bridge = CoreBridge::start(store);
            let active = bridge
                .capture_private_handler(456)
                .expect("handler token")
                .enter()
                .await
                .expect("handler permit");
            let stale = bridge
                .capture_private_handler(456)
                .expect("waiting handler token");
            let erasure = bridge
                .capture_private_data_erasure(456)
                .expect("erasure token");
            assert!(bridge.is_user_blocked(456));
            assert!(bridge.capture_private_handler(456).is_none());

            let mut entering = kovi::tokio::spawn(async move { erasure.enter().await });
            assert!(
                kovi::tokio::time::timeout(StdDuration::from_millis(20), &mut entering)
                    .await
                    .is_err(),
                "write permit must drain the active legacy handler"
            );
            drop(active);
            let erasure_permit = entering
                .await
                .expect("erasure permit task")
                .expect("erasure permit");
            drop(erasure_permit);

            assert!(stale.enter().await.is_none());
            assert!(!bridge.is_user_blocked(456));
            assert!(
                bridge
                    .capture_private_handler(456)
                    .expect("fresh handler token")
                    .enter()
                    .await
                    .is_some()
            );
        });
    }

    #[test]
    fn group_handler_epoch_drains_old_work_and_rejects_waiters_across_erasure() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Group,
            });
            let bridge = CoreBridge::start(store);
            let active = bridge
                .capture_group_handler(456)
                .expect("group handler token")
                .enter()
                .await
                .expect("group handler permit");
            let stale = bridge
                .capture_group_handler(456)
                .expect("waiting group handler token");
            let erasure = bridge
                .capture_group_data_erasure(456)
                .expect("group erasure token");
            assert!(bridge.capture_group_handler(456).is_none());

            let mut entering = kovi::tokio::spawn(async move { erasure.enter().await });
            assert!(
                kovi::tokio::time::timeout(StdDuration::from_millis(20), &mut entering)
                    .await
                    .is_err(),
                "group write permit must drain the active legacy handler"
            );
            drop(active);
            let erasure_permit = entering
                .await
                .expect("group erasure permit task")
                .expect("group erasure permit");
            drop(erasure_permit);

            assert!(stale.enter().await.is_none());
            assert!(
                bridge
                    .capture_group_handler(456)
                    .expect("fresh group handler token")
                    .enter()
                    .await
                    .is_some()
            );
        });
    }

    #[test]
    fn rejected_state_releases_the_exact_registered_incoming_admission() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let conversation_id = ConversationId::new();
            let sender = PersonId::new();
            let scope = ReplyScope::Private(9_200_459);
            let admission = crate::model::ConversationCoordinator::begin_incoming(scope).await;
            let rejected_message_id = MessageId::new();
            let releaser = Arc::new(TestIncomingAdmissionReleaser::default());
            releaser
                .admissions
                .lock()
                .await
                .insert(rejected_message_id, admission);
            let message = |message_id, conversation_kind| {
                WorldEvent::message_received(
                    EventPriority::Critical,
                    MessageReceivedEvent {
                        message_id,
                        conversation_id,
                        sender,
                        content: MessageContent::text("hello"),
                        reply_to: None,
                        timestamp: Utc::now(),
                        conversation_kind,
                        addressed_to_agent: true,
                        replies_to_agent: false,
                        stop_requested: false,
                        explicit_request: true,
                        visible_reply_allowed: true,
                    },
                )
            };
            let (runtime_handle, core_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            assert_eq!(
                runtime_handle
                    .submit(message(MessageId::new(), ConversationKind::Group))
                    .await,
                Ok(Admission::Accepted)
            );
            assert_eq!(
                runtime_handle
                    .submit(message(rejected_message_id, ConversationKind::Direct))
                    .await,
                Ok(Admission::Accepted)
            );
            drop(runtime_handle);
            let runtime_releaser: Arc<dyn IncomingAdmissionReleaser> = releaser.clone();

            run_runtime(core_runtime, None, None, Some(runtime_releaser)).await;

            assert!(releaser.admissions.lock().await.is_empty());
            assert_eq!(
                releaser
                    .discarded
                    .lock()
                    .expect("discard recorder lock")
                    .as_slice(),
                &[rejected_message_id]
            );
            assert!(
                !crate::model::ConversationCoordinator::abandon_incoming(admission).await,
                "runtime rejection must have already released the exact reservation"
            );
        });
    }

    #[test]
    fn erasure_barrier_is_fifo_clears_ingress_state_and_resumes() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let person_id = PersonId::new();
            let conversation_id = ConversationId::new();
            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id,
                conversation_id,
                stored_kind: ConversationKind::Direct,
            });
            let bridge = CoreBridge::start(store);
            let message = inbound(
                ConversationAddress::Direct {
                    self_id: 111,
                    peer_user_id: 456,
                },
                true,
            );

            assert_eq!(
                bridge.try_enqueue(message.clone()),
                EnqueueOutcome::Accepted
            );
            let erasure = bridge
                .begin_user_data_erasure(456)
                .await
                .expect("FIFO barrier should acknowledge");
            let ack = erasure.ack();
            assert_eq!(ack.canonical_person_id, Some(person_id));
            assert_eq!(ack.runtime_barrier_person_id, person_id);
            assert_eq!(ack.purged_conversations, 1);
            assert_eq!(ack.cleared_references, 1);
            assert_eq!(ack.cleared_tracked_routes, 1);
            assert!(bridge.is_user_blocked(456));
            assert_eq!(bridge.try_enqueue(message.clone()), EnqueueOutcome::Blocked);

            erasure.finish().await.expect("barrier should resume");
            assert!(!bridge.is_user_blocked(456));
            assert_eq!(bridge.try_enqueue(message), EnqueueOutcome::Accepted);
        });
    }

    #[test]
    fn erasure_barrier_drains_prior_direct_dispatch_and_blocks_later_dispatch() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let user_id = 456;
            let person_id = PersonId::new();
            let conversation_id = ConversationId::new();
            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id,
                conversation_id,
                stored_kind: ConversationKind::Direct,
            });
            let (ingress, receiver) = mpsc::channel(super::CORE_INGRESS_CAPACITY);
            let (runtime_handle, core_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let blocked_users = Arc::new(StdMutex::new(HashSet::new()));
            let blocked_groups = Arc::new(StdMutex::new(HashSet::new()));
            let arbiter = Arc::new(ActionArbiter::new(
                ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
            ));
            let port = Arc::new(BlockingActionPort {
                conversation_id,
                calls: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Notify::new(),
            });
            let action_port: Arc<dyn ActionPort> = port.clone();
            kovi::tokio::spawn(run_ingress(
                receiver,
                store,
                runtime_handle.clone(),
                None,
                None,
                None,
                Arc::clone(&blocked_users),
                Arc::clone(&blocked_groups),
                Some(Arc::clone(&arbiter)),
                Some(Arc::clone(&action_port)),
                Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            ));
            kovi::tokio::spawn(run_runtime(core_runtime, None, None, None));
            let bridge = Arc::new(CoreBridge {
                ingress,
                runtime: runtime_handle,
                action_arbiter: Some(arbiter),
                action_port: Some(action_port),
                blocked_users,
                blocked_groups,
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                group_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
                ambient_attention: Arc::new(StdMutex::new(super::AmbientAttentionRegistry::new())),
            });
            let action = ProposedAction::send_message(
                conversation_id,
                MessageContent::text("before erasure"),
            )
            .expect("valid action");
            let dispatch_bridge = Arc::clone(&bridge);
            let dispatch = kovi::tokio::spawn(async move {
                dispatch_bridge.dispatch_action(user_id, action).await
            });
            port.entered.notified().await;

            let begin_bridge = Arc::clone(&bridge);
            let mut begin =
                kovi::tokio::spawn(
                    async move { begin_bridge.begin_user_data_erasure(user_id).await },
                );
            kovi::tokio::time::timeout(StdDuration::from_secs(1), async {
                while !bridge.is_user_blocked(user_id) {
                    kovi::tokio::task::yield_now().await;
                }
            })
            .await
            .expect("begin task should close the synchronous ingress gate");
            assert!(
                kovi::tokio::time::timeout(StdDuration::from_millis(20), &mut begin)
                    .await
                    .is_err(),
                "begin must wait until the prior side effect has drained"
            );

            port.release.notify_one();
            assert!(matches!(
                dispatch.await.expect("dispatch task"),
                Ok(Some(ActionResult::Executed { .. }))
            ));
            let erasure = begin
                .await
                .expect("begin task")
                .expect("barrier should acknowledge");
            let blocked_action = ProposedAction::send_message(
                conversation_id,
                MessageContent::text("during erasure"),
            )
            .expect("valid action");
            assert!(
                bridge
                    .dispatch_action(user_id, blocked_action)
                    .await
                    .is_ok_and(|result| result.is_none())
            );
            assert_eq!(port.calls.load(Ordering::SeqCst), 1);

            erasure.finish().await.expect("barrier should resume");
            assert!(!bridge.is_user_blocked(user_id));
        });
    }

    #[test]
    fn ingress_route_tracker_is_bounded_and_keeps_only_direct_conversations() {
        let first_person = PersonId::new();
        let second_person = PersonId::new();
        let first_conversation = ConversationId::new();
        let second_conversation = ConversationId::new();
        let mut tracker = IngressRouteTracker::new(1, 1);
        tracker.record(1, first_person, Some(first_conversation));
        tracker.record(1, first_person, Some(second_conversation));
        let tracked = tracker.get(1).expect("first user should remain tracked");
        assert_eq!(tracked.person_id, Some(first_person));
        assert_eq!(
            tracked
                .direct_conversation_ids
                .into_iter()
                .collect::<Vec<_>>(),
            vec![second_conversation]
        );

        tracker.record(2, second_person, None);
        assert_eq!(tracker.len(), 1);
        assert!(tracker.get(1).is_none());
        assert_eq!(
            tracker.get(2).and_then(|routes| routes.person_id),
            Some(second_person)
        );
    }

    #[test]
    fn erasure_alias_blocks_are_atomic_and_resumed_together() {
        let mut local = HashSet::from([456]);
        let shared = StdMutex::new(HashSet::from([456]));
        block_user_aliases(456, &[456, 789, 999], &mut local, &shared)
            .expect("all aliases should fit the bounded gate");
        assert_eq!(local, HashSet::from([456, 789, 999]));
        assert_eq!(
            *shared.lock().expect("shared block state"),
            HashSet::from([456, 789, 999])
        );

        unblock_users(&mut local, &shared, &[456, 789, 999]);
        assert!(local.is_empty());
        assert!(shared.lock().expect("shared block state").is_empty());
    }

    #[test]
    fn erasure_alias_conflict_fails_without_partially_blocking_new_aliases() {
        let mut local = HashSet::from([456, 789]);
        let shared = StdMutex::new(HashSet::from([456, 789]));
        assert!(block_user_aliases(456, &[456, 789, 999], &mut local, &shared).is_err());
        assert_eq!(local, HashSet::from([456, 789]));
        assert_eq!(
            *shared.lock().expect("shared block state"),
            HashSet::from([456, 789])
        );
    }

    #[test]
    fn alias_handler_barrier_drains_an_active_legacy_handler() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Direct,
            });
            let bridge = CoreBridge::start(store);
            let active = bridge
                .capture_private_handler(789)
                .expect("alias handler token")
                .enter()
                .await
                .expect("alias handler permit");
            let gates = Arc::clone(&bridge.private_handler_gates);
            let mut draining = kovi::tokio::spawn(async move {
                acquire_alias_handler_barriers(456, &[456, 789], &gates).await
            });
            assert!(
                kovi::tokio::time::timeout(StdDuration::from_millis(20), &mut draining)
                    .await
                    .is_err(),
                "alias write permit must wait for the old handler"
            );
            assert!(bridge.is_user_blocked(789));

            drop(active);
            let permits = draining
                .await
                .expect("alias drain task")
                .expect("alias barriers");
            assert_eq!(permits.len(), 1);
            drop(permits);
            assert!(!bridge.is_user_blocked(789));
        });
    }

    #[test]
    fn conversation_only_erasure_uses_an_internal_runtime_person_scope() {
        let conversation_id = ConversationId::new();
        let targets = merge_data_erasure_targets(
            456,
            crate::yunxi::identity_store::QqPersonDomainTargets {
                person_id: None,
                qq_user_ids: vec![456],
                direct_conversation_ids: vec![conversation_id],
            },
            Default::default(),
        )
        .expect("legacy orphaned direct conversation must remain erasable");

        assert_eq!(targets.canonical_person_id, None);
        assert_eq!(targets.direct_conversation_ids, vec![conversation_id]);
        assert_ne!(
            targets.runtime_barrier_person_id.into_uuid(),
            uuid::Uuid::nil()
        );
    }

    #[test]
    fn idle_tick_is_global_low_priority() {
        let event = idle_tick_event(Utc::now());
        assert_eq!(event.scope(), yunxi_core::EventScope::Global);
        assert_eq!(event.priority(), EventPriority::Low);
        assert!(matches!(event.kind(), yunxi_core::WorldEventKind::IdleTick));
        assert!(event.validate(8).is_ok());
    }

    #[test]
    fn action_results_become_scoped_core_events() {
        let conversation_id = ConversationId::new();
        let action = ProposedAction::send_message(conversation_id, MessageContent::text("hello"))
            .expect("action should validate");
        let result = ActionResult::Executed {
            receipt: yunxi_core::ActionReceipt {
                action_id: action.action_id(),
                idempotency_key: action.idempotency_key().map(ToOwned::to_owned),
                admitted_at: Utc::now(),
            },
            outcome: ActionPortOutcome::Delivered {
                external_reference: Some("qq-message:42".to_owned()),
                message_id: None,
                conversation_id: Some(conversation_id),
            },
        };
        let event = action_result_event(&action, &result, Utc::now()).expect("event");
        assert_eq!(
            event.scope(),
            yunxi_core::EventScope::Conversation { conversation_id }
        );
        assert_eq!(event.priority(), EventPriority::High);
        assert!(matches!(
            event.kind(),
            yunxi_core::WorldEventKind::ActionSucceeded(payload)
                if action
                    .idempotency_key()
                    .is_some_and(|key| payload.idempotency_key == key)
        ));
        assert!(event.validate(8).is_ok());
    }

    #[test]
    fn rejected_action_result_preserves_person_scope_and_reason() {
        let person_id = PersonId::new();
        let action = ProposedAction::reach_out(
            person_id,
            MessageContent::text("hello"),
            yunxi_core::ProactiveMotive::CheckIn,
        )
        .expect("action should validate");
        let result = ActionResult::Rejected(ActionRejection::TargetUnavailable {
            action_id: action.action_id(),
            person_id,
        });
        let event = action_result_event(&action, &result, Utc::now()).expect("event");
        assert_eq!(event.scope(), yunxi_core::EventScope::Person { person_id });
        assert!(matches!(
            event.kind(),
            yunxi_core::WorldEventKind::ActionRejected(payload)
                if payload.reason.contains("no delivery route")
        ));
        assert!(event.validate(8).is_ok());
    }

    #[test]
    fn direct_and_addressed_messages_use_reliable_priority() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            for (address, addressed, expected_priority) in [
                (
                    ConversationAddress::Direct {
                        self_id: 111,
                        peer_user_id: 456,
                    },
                    false,
                    EventPriority::High,
                ),
                (
                    ConversationAddress::Group { group_id: 123 },
                    true,
                    EventPriority::High,
                ),
                (
                    ConversationAddress::Group { group_id: 123 },
                    false,
                    EventPriority::Normal,
                ),
            ] {
                let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                    person_id: PersonId::new(),
                    conversation_id: ConversationId::new(),
                    stored_kind: address.kind(),
                });
                let (handle, mut runtime) =
                    yunxi_core::CognitiveRuntime::new(RuntimeConfig::default())
                        .expect("valid runtime");
                resolve_and_submit(
                    &inbound(address, addressed),
                    store.as_ref(),
                    &handle,
                    &mut MessageReferenceCache::new(4),
                )
                .await
                .expect("fake mappings should resolve");
                let ProcessingOutcome::Observed(observation) = runtime
                    .process_next()
                    .await
                    .expect("submitted event should be processed")
                else {
                    panic!("event should be observed");
                };
                assert_eq!(observation.priority, expected_priority);
            }
        });
    }

    #[test]
    fn sampled_ambient_group_messages_use_reliable_priority() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let address = ConversationAddress::Group { group_id: 123 };
            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Group,
            });
            let (handle, mut runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("runtime");
            let mut message = inbound(address, false);
            message.planner_attention_requested = true;
            resolve_and_submit(
                &message,
                store.as_ref(),
                &handle,
                &mut MessageReferenceCache::new(4),
            )
            .await
            .expect("fake mappings should resolve");
            let ProcessingOutcome::Observed(observation) = runtime
                .process_next()
                .await
                .expect("submitted event should be processed")
            else {
                panic!("event should be observed");
            };
            assert_eq!(observation.priority, EventPriority::High);
            assert_eq!(
                observation.attention.disposition,
                AttentionDisposition::Attend
            );
        });
    }

    #[test]
    fn fake_store_preserves_direct_and_group_attention() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            for (address, kind, addressed, expected) in [
                (
                    ConversationAddress::Group { group_id: 123 },
                    ConversationKind::Group,
                    false,
                    AttentionDisposition::ObserveOnly,
                ),
                (
                    ConversationAddress::Group { group_id: 123 },
                    ConversationKind::Group,
                    true,
                    AttentionDisposition::MustHandle,
                ),
                (
                    ConversationAddress::Direct {
                        self_id: 111,
                        peer_user_id: 456,
                    },
                    ConversationKind::Direct,
                    true,
                    AttentionDisposition::MustHandle,
                ),
            ] {
                let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                    person_id: PersonId::new(),
                    conversation_id: ConversationId::new(),
                    stored_kind: kind,
                });
                let (handle, mut runtime) =
                    yunxi_core::CognitiveRuntime::new(RuntimeConfig::default())
                        .expect("valid runtime");
                resolve_and_submit(
                    &inbound(address, addressed),
                    store.as_ref(),
                    &handle,
                    &mut MessageReferenceCache::new(4),
                )
                .await
                .expect("fake mappings should resolve");
                let ProcessingOutcome::Observed(observation) = runtime
                    .process_next()
                    .await
                    .expect("submitted event should be processed")
                else {
                    panic!("event should be observed");
                };
                assert_eq!(observation.attention.disposition, expected);
            }
        });
    }

    #[test]
    fn conversation_kind_mismatch_is_dropped_before_core_submission() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let store = FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Direct,
            };
            let (handle, _runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let error = resolve_and_submit(
                &inbound(ConversationAddress::Group { group_id: 123 }, true),
                &store,
                &handle,
                &mut MessageReferenceCache::new(4),
            )
            .await
            .expect_err("kind mismatch must be rejected");
            assert!(error.to_string().contains("kind mismatch"));
        });
    }

    #[test]
    fn identity_store_failure_does_not_submit_or_cache_an_event() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (handle, _runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let mut references = MessageReferenceCache::new(4);
            let error = resolve_and_submit(
                &inbound(
                    ConversationAddress::Direct {
                        self_id: 111,
                        peer_user_id: 456,
                    },
                    true,
                ),
                &FailingIdentityStore,
                &handle,
                &mut references,
            )
            .await
            .expect_err("storage failure should be returned");
            assert!(
                error
                    .chain()
                    .any(|cause| cause.to_string().contains("identity lookup unavailable"))
            );
            assert_eq!(references.len(), 0);
        });
    }

    #[test]
    fn duplicate_external_message_ids_are_not_submitted_twice() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let store = FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Group,
            };
            let (handle, mut runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let message = inbound(ConversationAddress::Group { group_id: 123 }, false);
            let mut references = MessageReferenceCache::new(4);
            resolve_and_submit(&message, &store, &handle, &mut references)
                .await
                .expect("first message should resolve");
            resolve_and_submit(&message, &store, &handle, &mut references)
                .await
                .expect("duplicate should be ignored");
            assert_eq!(references.len(), 1);
            assert!(matches!(
                runtime.process_next().await,
                Some(ProcessingOutcome::Observed(_))
            ));
        });
    }

    #[test]
    fn onebot_media_segments_normalize_to_opaque_core_attachments() {
        let message = Message::from(vec![
            Segment::new(
                "image",
                json!({"file_unique": "image-hash", "file_id": "image-id", "url": "https://image"}),
            ),
            Segment::new("record", json!({"file": "voice.amr"})),
            Segment::new("audio", json!({"file_id": "voice-id"})),
            Segment::new("video", json!({"url": "https://video"})),
            Segment::new("file", json!({"file": "document.pdf"})),
            Segment::new("image", json!({"url": "   "})),
            Segment::new("text", json!({"text": "not an attachment"})),
            Segment::new("image", json!({"file_id": 1234})),
        ]);

        let attachments = normalize_attachments(&message);
        assert_eq!(attachments.len(), 5);
        assert_eq!(attachments[0].kind(), AttachmentKind::Image);
        assert_eq!(attachments[0].reference(), "image-hash");
        assert_eq!(attachments[1].kind(), AttachmentKind::Audio);
        assert_eq!(attachments[1].reference(), "voice.amr");
        assert_eq!(attachments[2].kind(), AttachmentKind::Audio);
        assert_eq!(attachments[2].reference(), "voice-id");
        assert_eq!(attachments[3].kind(), AttachmentKind::Video);
        assert_eq!(attachments[3].reference(), "https://video");
        assert_eq!(attachments[4].kind(), AttachmentKind::File);
        assert_eq!(attachments[4].reference(), "document.pdf");
    }

    #[test]
    fn onebot_attachment_normalization_is_bounded_and_drops_oversized_references() {
        let message = Message::from(
            (0..20)
                .map(|index| Segment::new("image", json!({"file_id": format!("asset-{index}")})))
                .collect::<Vec<_>>(),
        );
        let attachments = normalize_attachments(&message);
        assert_eq!(attachments.len(), 16);
        assert_eq!(
            attachments.first().map(|item| item.reference()),
            Some("asset-0")
        );
        assert_eq!(
            attachments.last().map(|item| item.reference()),
            Some("asset-15")
        );

        let oversized = Message::from(vec![Segment::new(
            "file",
            json!({"file_id": "x".repeat(5_000)}),
        )]);
        assert!(
            normalize_attachments(&oversized).is_empty(),
            "references outside Core bounds must not enter the event"
        );
    }
}
