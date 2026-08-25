//! Bounded QQ -> Yunxi Core shadow bridge.
//!
//! The bridge deliberately sits beside the existing Kovi handlers. It copies
//! only the small set of fields needed by Core, then resolves platform
//! identities on a single background worker. The legacy handlers remain the
//! owner of all model calls and QQ side effects.

use super::qq;
use crate::model::{ReplyScope, is_recent_bot_message};
use chrono::{DateTime, Duration, TimeZone, Utc};
use kovi::RuntimeBot;
use kovi::bot::message::Message;
use kovi::event::{GroupMsgEvent, PrivateMsgEvent};
use kovi::tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, mpsc, oneshot};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use yunxi_core::{
    ActionArbiter, ActionArbiterConfig, ActionPort, ActionResult, Admission, CognitiveRuntime,
    ConversationId, ConversationKind, CoreServices, EnvironmentCapabilities, EventPriority,
    EventScope, ExternalConversation, IdentityStore, MessageContent, MessageId,
    MessageReceivedEvent, ModelBackend, OpenLoopDraft, OpenLoopKind, OpenLoopOwner, OpenLoopStore,
    PlannedProcessingOutcome, ProcessingOutcome, ProposedAction, RuntimeConfig, RuntimeHandle,
    WorldEvent, WorldEventKind,
};

pub(crate) const SHADOW_INGRESS_CAPACITY: usize = 256;
pub(crate) const MESSAGE_REFERENCE_CAPACITY: usize = 4_096;
const MAX_TRACKED_USERS: usize = 256;
const MAX_TRACKED_DIRECT_CONVERSATIONS_PER_USER: usize = 256;
const MAX_BLOCKED_USERS: usize = 256;
const MAX_PRIVATE_HANDLER_GATES: usize = 1_024;
const MAX_MESSAGE_CHARS: usize = 8_192;
const MAX_MESSAGE_BYTES: usize = 32 * 1_024;

/// The result of the synchronous, non-blocking ingress operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    Accepted,
    DroppedAtCapacity,
    Blocked,
    SkippedInvalid,
}

enum IngressCommand {
    Message(InboundMessage),
    DispatchAction {
        user_id: i64,
        action: ProposedAction,
        acknowledge: oneshot::Sender<Option<ActionResult>>,
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
}

#[derive(Debug, Clone)]
struct DataErasureAck {
    canonical_person_id: Option<yunxi_core::PersonId>,
    runtime_barrier_person_id: yunxi_core::PersonId,
    blocked_user_ids: Vec<i64>,
    purged_conversations: usize,
    cleared_references: usize,
    cleared_person_routes: usize,
    cleared_conversation_routes: usize,
    cleared_tracked_routes: usize,
}

pub(crate) struct UserDataErasure {
    bridge: Arc<ShadowBridge>,
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
pub(crate) struct ShadowBridge {
    ingress: mpsc::Sender<IngressCommand>,
    runtime: RuntimeHandle,
    action_arbiter: Option<Arc<ActionArbiter>>,
    action_port: Option<Arc<dyn ActionPort>>,
    blocked_users: Arc<StdMutex<HashSet<i64>>>,
    private_handler_gates: Arc<PrivateHandlerGateRegistry>,
}

impl fmt::Debug for ShadowBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShadowBridge")
            .field("action_arbiter", &self.action_arbiter.is_some())
            .field("action_port", &self.action_port.is_some())
            .field(
                "blocked_users",
                &self
                    .blocked_users
                    .lock()
                    .map_or(MAX_BLOCKED_USERS, |users| users.len()),
            )
            .field("private_handler_gates", &"bounded per-user epochs")
            .finish_non_exhaustive()
    }
}

impl ShadowBridge {
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
        let model = super::core_model::KoviModelBackend::new(Arc::clone(&bot), Arc::clone(&store));
        let adapter = super::delivery::QqActionAdapter::new(bot, Arc::clone(&store));
        let mut services = CoreServices::new(Arc::clone(&model) as Arc<dyn ModelBackend>)
            .with_identity(Arc::clone(&store) as Arc<dyn IdentityStore>)
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
        let (ingress, receiver) = mpsc::channel(SHADOW_INGRESS_CAPACITY);
        let blocked_users = Arc::new(StdMutex::new(HashSet::with_capacity(
            MAX_BLOCKED_USERS.min(32),
        )));
        let private_handler_gates =
            Arc::new(PrivateHandlerGateRegistry::new(MAX_PRIVATE_HANDLER_GATES));
        let (runtime_handle, runtime) = services.map_or_else(
            || {
                CognitiveRuntime::new(RuntimeConfig::default())
                    .expect("default Yunxi runtime configuration must be valid")
            },
            |services| {
                CognitiveRuntime::new_with_services(RuntimeConfig::default(), services)
                    .expect("default Yunxi runtime configuration must be valid")
            },
        );

        let (action_arbiter, action_port): (
            Option<Arc<ActionArbiter>>,
            Option<Arc<dyn ActionPort>>,
        ) = action_adapter.map_or((None, None), |adapter| {
            let resolver: Arc<dyn yunxi_core::DeliveryResolver> = adapter.clone();
            let arbiter = ActionArbiter::new(
                ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
            )
            .with_delivery_resolver(resolver);
            let port: Arc<dyn ActionPort> = adapter;
            (Some(Arc::new(arbiter)), Some(port))
        });

        let scheduler_runtime = runtime_handle.clone();
        let bridge_runtime = runtime_handle.clone();
        kovi::tokio::spawn(run_ingress(
            receiver,
            store,
            runtime_handle,
            open_loop_store.clone(),
            model_backend,
            message_store,
            Arc::clone(&blocked_users),
            action_arbiter.clone(),
            action_port.clone(),
            Arc::clone(&private_handler_gates),
        ));
        kovi::tokio::spawn(run_runtime(
            runtime,
            action_arbiter.clone(),
            action_port.clone(),
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
            private_handler_gates,
        })
    }

    /// Dispatch an admitted Core action through the configured host adapter,
    /// then feed the result back into the same runtime event stream. Hosts
    /// using the compatibility constructor receive `None` and keep their
    /// existing observe-only behavior.
    #[allow(dead_code)]
    pub(crate) async fn dispatch_action(
        &self,
        user_id: i64,
        action: ProposedAction,
    ) -> Option<ActionResult> {
        if !valid_qq_id(user_id)
            || self.is_user_blocked(user_id)
            || self.action_arbiter.is_none()
            || self.action_port.is_none()
        {
            return None;
        }
        let (acknowledge, acknowledged) = oneshot::channel();
        self.ingress
            .send(IngressCommand::DispatchAction {
                user_id,
                action,
                acknowledge,
            })
            .await
            .ok()?;
        acknowledged.await.ok().flatten()
    }

    pub(crate) fn enqueue_group(&self, event: &GroupMsgEvent) -> EnqueueOutcome {
        let Some(message) = InboundMessage::from_group(event) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        self.try_enqueue(message)
    }

    pub(crate) fn enqueue_private(&self, event: &PrivateMsgEvent) -> EnqueueOutcome {
        let Some(message) = InboundMessage::from_private(event) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        self.try_enqueue(message)
    }

    /// Whether this private event is now owned by the Core direct-conversation
    /// path. Control commands and stop requests stay on the legacy handler so
    /// their specialized host behavior remains available during migration.
    pub(crate) fn handles_private(&self, event: &PrivateMsgEvent) -> bool {
        self.action_arbiter.is_some()
            && self.action_port.is_some()
            && event.message.iter().all(|segment| segment.type_ == "text")
            && InboundMessage::from_private(event).is_some_and(|message| {
                !message.text.trim().is_empty()
                    && !message.stop_requested
                    && !message.text.trim_start().starts_with('#')
            })
    }

    fn try_enqueue(&self, message: InboundMessage) -> EnqueueOutcome {
        if self.is_user_blocked(message.sender_user_id) {
            return EnqueueOutcome::Blocked;
        }
        match self.ingress.try_send(IngressCommand::Message(message)) {
            Ok(()) => EnqueueOutcome::Accepted,
            Err(mpsc::error::TrySendError::Full(_)) => EnqueueOutcome::DroppedAtCapacity,
            Err(mpsc::error::TrySendError::Closed(_)) => EnqueueOutcome::SkippedInvalid,
        }
    }

    pub(crate) fn is_user_blocked(&self, user_id: i64) -> bool {
        is_blocked(self.blocked_users.as_ref(), user_id)
            || self.private_handler_gates.deletion_pending(user_id)
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

    fn unblock_user(&self, user_id: i64) {
        if let Ok(mut blocked) = self.blocked_users.lock() {
            blocked.remove(&user_id);
        }
    }

    /// Submit a best-effort global idle observation without waiting on the
    /// ingress or runtime queue. Low-priority runtime admission is bounded and
    /// may be dropped when the host is busy.
    pub(crate) fn observe_idle_tick(&self) {
        let runtime = self.runtime.clone();
        kovi::tokio::spawn(async move {
            let event = idle_tick_event(Utc::now());
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
            outcome: yunxi_core::ActionPortOutcome::Delivered { .. },
            ..
        } => WorldEventKind::ActionSucceeded(yunxi_core::ActionSucceededEvent { idempotency_key }),
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
    Some(WorldEvent::new(
        occurred_at,
        scope,
        EventPriority::High,
        kind,
    ))
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
    text: String,
    timestamp: DateTime<Utc>,
    addressed_to_agent: bool,
    explicit_request: bool,
    stop_requested: bool,
}

impl InboundMessage {
    fn from_group(event: &GroupMsgEvent) -> Option<Self> {
        valid_qq_id(event.self_id)
            .then_some(())
            .and_then(|()| valid_qq_id(event.group_id).then_some(()))
            .and_then(|()| valid_qq_id(event.user_id).then_some(()))?;
        if event.user_id == event.self_id {
            return None;
        }
        let text = bounded_text(event.borrow_text().unwrap_or_default());
        Some(Self {
            address: ConversationAddress::Group {
                group_id: event.group_id,
            },
            sender_user_id: event.user_id,
            external_message_id: positive_message_id(event.message_id),
            reply_to_external_message_id: reply_message_id(&event.message),
            addressed_to_agent: message_at_self(&event.message, event.self_id)
                || text_mentions_agent(&text),
            explicit_request: false,
            stop_requested: looks_like_stop_request(&text),
            text,
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
        Some(Self {
            address: ConversationAddress::Direct {
                self_id: event.self_id,
                peer_user_id: event.user_id,
            },
            sender_user_id: event.user_id,
            external_message_id: positive_message_id(event.message_id),
            reply_to_external_message_id: reply_message_id(&event.message),
            addressed_to_agent: true,
            explicit_request: true,
            stop_requested: looks_like_stop_request(&text),
            text,
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
    open_loop_store: Option<Arc<dyn OpenLoopStore>>,
    model_backend: Option<Arc<super::core_model::KoviModelBackend>>,
    message_store: Option<Arc<super::identity_store::PostgresIdentityStore>>,
    blocked_users: Arc<StdMutex<HashSet<i64>>>,
    action_arbiter: Option<Arc<ActionArbiter>>,
    action_port: Option<Arc<dyn ActionPort>>,
    private_handler_gates: Arc<PrivateHandlerGateRegistry>,
) {
    let mut references = MessageReferenceCache::new(MESSAGE_REFERENCE_CAPACITY);
    let mut routes =
        IngressRouteTracker::new(MAX_TRACKED_USERS, MAX_TRACKED_DIRECT_CONVERSATIONS_PER_USER);
    let mut blocked_at_ingress = HashSet::with_capacity(MAX_BLOCKED_USERS.min(32));
    let mut alias_handler_barriers: HashMap<i64, Vec<PrivateDataErasurePermit>> = HashMap::new();
    while let Some(command) = receiver.recv().await {
        match command {
            IngressCommand::Message(message) => {
                if blocked_at_ingress.contains(&message.sender_user_id) {
                    continue;
                }
                if let Err(error) = resolve_and_submit_inner(
                    &message,
                    store.as_ref(),
                    &runtime,
                    &mut references,
                    open_loop_store.as_deref(),
                    model_backend.as_deref(),
                    message_store.as_deref(),
                    Some(&mut routes),
                )
                .await
                {
                    eprintln!(
                        "[WARN] Yunxi shadow message dropped during identity resolution: {error}"
                    );
                }
            }
            IngressCommand::DispatchAction {
                user_id,
                action,
                acknowledge,
            } => {
                if blocked_at_ingress.contains(&user_id) {
                    let _ = acknowledge.send(None);
                    continue;
                }
                let result = if let (Some(arbiter), Some(port)) =
                    (action_arbiter.as_deref(), action_port.as_deref())
                {
                    let result = arbiter.dispatch(action.clone(), port).await;
                    if let Some(event) = action_result_event(&action, &result, Utc::now())
                        && let Err(error) = runtime.submit(event).await
                    {
                        kovi::log::warn!("Yunxi action result could not enter runtime: {error}");
                    }
                    Some(result)
                } else {
                    None
                };
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
                if result.is_err() {
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
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn begin_data_erasure_at_ingress_barrier(
    user_id: i64,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
    routes: &mut IngressRouteTracker,
    model_backend: Option<&super::core_model::KoviModelBackend>,
    message_store: Option<&super::identity_store::PostgresIdentityStore>,
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
    let cleared_references = references.remove_conversations(&targets.direct_conversation_ids);
    let (cleared_person_routes, cleared_conversation_routes) =
        if let Some(model_backend) = model_backend {
            model_backend
                .purge_routes(
                    targets.canonical_person_id,
                    &targets.direct_conversation_ids,
                )
                .await
        } else {
            (0, 0)
        };
    let cleared_tracked_routes = targets
        .blocked_user_ids
        .iter()
        .filter(|user_id| routes.remove(**user_id))
        .count();
    alias_handler_barriers.insert(user_id, alias_permits);
    Ok(DataErasureAck {
        canonical_person_id: targets.canonical_person_id,
        runtime_barrier_person_id: targets.runtime_barrier_person_id,
        blocked_user_ids: targets.blocked_user_ids,
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

async fn run_runtime(
    mut runtime: CognitiveRuntime,
    action_arbiter: Option<Arc<ActionArbiter>>,
    action_port: Option<Arc<dyn ActionPort>>,
) {
    let planned = runtime.planner().is_some();
    if planned
        && let (Some(arbiter), Some(port)) = (action_arbiter.as_deref(), action_port.as_deref())
    {
        while let Some(outcome) = runtime
            .process_next_with_planner_and_actions(arbiter, port)
            .await
        {
            match outcome {
                Ok(PlannedProcessingOutcome::Planned {
                    observation,
                    actions,
                    ..
                }) => {
                    kovi::log::debug!(
                        "Yunxi Core turn completed: id={} type={:?} actions={}",
                        observation.event_id,
                        observation.event_type,
                        actions.len(),
                    );
                }
                Ok(PlannedProcessingOutcome::RejectedEvent { .. })
                | Ok(PlannedProcessingOutcome::RejectedState { .. }) => {
                    kovi::log::warn!("Yunxi Core planner rejected an event");
                }
                Err(error) => kovi::log::warn!("Yunxi Core planner failed: {error}"),
            }
        }
        return;
    }
    while let Some(outcome) = runtime.process_next().await {
        match outcome {
            ProcessingOutcome::Observed(observation) => {
                kovi::log::debug!(
                    "Yunxi shadow event observed: id={} type={:?} scope={:?} priority={:?} attention={:?} state={:?}",
                    observation.event_id,
                    observation.event_type,
                    observation.scope,
                    observation.priority,
                    observation.attention,
                    observation.state,
                );
            }
            ProcessingOutcome::RejectedEvent { .. } | ProcessingOutcome::RejectedState { .. } => {
                kovi::log::warn!("Yunxi shadow runtime rejected an event");
            }
        }
    }
}

#[allow(dead_code)]
async fn resolve_and_submit(
    message: &InboundMessage,
    store: &dyn IdentityStore,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
) -> anyhow::Result<()> {
    resolve_and_submit_inner(message, store, runtime, references, None, None, None, None).await
}

#[allow(clippy::too_many_arguments)]
async fn resolve_and_submit_inner(
    message: &InboundMessage,
    store: &dyn IdentityStore,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
    open_loop_store: Option<&dyn OpenLoopStore>,
    model_backend: Option<&super::core_model::KoviModelBackend>,
    message_store: Option<&super::identity_store::PostgresIdentityStore>,
    route_tracker: Option<&mut IngressRouteTracker>,
) -> anyhow::Result<()> {
    let external_identity = qq::person(message.sender_user_id)?;
    let external_conversation = message.address.external()?;
    let person_id = store
        .resolve_external_identity(&external_identity)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    let conversation_id = store
        .resolve_external_conversation(&external_conversation)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;

    if let Some(route_tracker) = route_tracker {
        route_tracker.record(
            message.sender_user_id,
            person_id,
            matches!(message.address, ConversationAddress::Direct { .. })
                .then_some(conversation_id),
        );
    }

    if let Some(model_backend) = model_backend {
        let conversation = match message.address {
            ConversationAddress::Group { group_id } => {
                super::core_model::LegacyConversation::Group { group_id }
            }
            ConversationAddress::Direct { peer_user_id, .. } => {
                super::core_model::LegacyConversation::Private {
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
    let recent_agent_reply = if reply_reference.is_some_and(|reference| reference.from_agent) {
        true
    } else {
        recent_bot_message(message.address, message.reply_to_external_message_id).await
    };

    let message_id = MessageId::new();
    let priority = if message.address.kind() == ConversationKind::Direct
        || message.addressed_to_agent
        || recent_agent_reply
        || message.stop_requested
        || message.explicit_request
    {
        EventPriority::High
    } else {
        EventPriority::Normal
    };
    let event = WorldEvent::message_received(
        priority,
        MessageReceivedEvent {
            message_id,
            conversation_id,
            sender: person_id,
            content: MessageContent::text(message.text.clone()),
            reply_to: reply_reference.map(|reference| reference.message_id),
            timestamp: message.timestamp,
            conversation_kind: message.address.kind(),
            addressed_to_agent: message.addressed_to_agent,
            replies_to_agent: recent_agent_reply,
            stop_requested: message.stop_requested,
            explicit_request: message.explicit_request,
        },
    );
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
    let admission = runtime
        .submit(event)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    if matches!(admission, Admission::Accepted)
        && let Some(open_loop_store) = open_loop_store
    {
        process_open_loop_candidate(
            open_loop_store,
            message,
            person_id,
            conversation_id,
            message_id,
        )
        .await?;
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
    Ok(())
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

async fn process_open_loop_candidate(
    open_loop_store: &dyn OpenLoopStore,
    message: &InboundMessage,
    person_id: yunxi_core::PersonId,
    conversation_id: ConversationId,
    message_id: MessageId,
) -> anyhow::Result<()> {
    let owner = match message.address {
        ConversationAddress::Direct { .. } => OpenLoopOwner::Person(person_id),
        ConversationAddress::Group { .. } => OpenLoopOwner::Conversation(conversation_id),
    };

    if looks_like_outcome_completion(&message.text) {
        resolve_matching_open_loop(open_loop_store, owner, &message.text).await;
        return Ok(());
    }

    let Some(external_message_id) = message.external_message_id else {
        return Ok(());
    };
    let Some(draft) = detect_open_loop_candidate(
        owner,
        &message.text,
        message.timestamp,
        message_id,
        format!("qq-message:{conversation_id}:{external_message_id}"),
    )?
    else {
        return Ok(());
    };
    if let Err(error) = open_loop_store.create(&draft).await {
        kovi::log::warn!("Yunxi open-loop candidate was not persisted: {error}");
    }
    Ok(())
}

fn detect_open_loop_candidate(
    owner: OpenLoopOwner,
    text: &str,
    timestamp: DateTime<Utc>,
    source_message_id: MessageId,
    dedupe_key: String,
) -> anyhow::Result<Option<OpenLoopDraft>> {
    let text = text.trim();
    if text.is_empty()
        || crate::reminders::looks_like_reminder_request(text)
        || !has_future_marker(text)
        || !has_future_event_marker(text)
    {
        return Ok(None);
    }

    let due_at = infer_future_due_at(text, timestamp);
    let expires_at = due_at.map(|value| value + Duration::days(14));
    let kind = if has_outcome_marker(text) {
        OpenLoopKind::AwaitingOutcome
    } else {
        OpenLoopKind::FutureEvent
    };
    let draft = OpenLoopDraft::new(owner, kind, text.to_owned())?
        .with_source_message_id(Some(source_message_id))
        .with_due_at(due_at)
        .with_expires_at(expires_at)
        .with_dedupe_key(Some(dedupe_key))?;
    Ok(Some(draft))
}

fn has_future_marker(text: &str) -> bool {
    [
        "明天",
        "后天",
        "下周",
        "下个月",
        "下星期",
        "下礼拜",
        "之后",
        "以后",
        "月底",
        "周末",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || text.contains('日') && text.chars().any(|character| character.is_ascii_digit())
}

fn has_future_event_marker(text: &str) -> bool {
    [
        "面试", "考试", "面谈", "结果", "回复", "答复", "申请", "会议", "比赛", "旅行", "出差",
        "生日", "发布", "上线", "项目", "约会", "演出", "手术", "预约",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn has_outcome_marker(text: &str) -> bool {
    [
        "面试", "考试", "面谈", "结果", "回复", "答复", "申请", "审核", "比赛",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn infer_future_due_at(text: &str, timestamp: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let offset_days = if text.contains("后天") {
        2
    } else if text.contains("明天") {
        1
    } else if text.contains("下周") || text.contains("下星期") || text.contains("下礼拜") {
        7
    } else if text.contains("下个月") {
        30
    } else {
        return None;
    };
    let date = timestamp.date_naive() + Duration::days(offset_days);
    let hour = number_before(text, '点').unwrap_or(9);
    let hour = if (text.contains("下午") || text.contains("晚上")) && hour < 12 {
        hour + 12
    } else {
        hour
    };
    let minute = if text.contains("点半") {
        30
    } else {
        number_after(text, '点')
            .filter(|value| *value < 60)
            .unwrap_or(0)
    };
    let time = chrono::NaiveTime::from_hms_opt(hour.min(23), minute, 0)?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(
        date.and_time(time),
        Utc,
    ))
}

fn number_before(text: &str, marker: char) -> Option<u32> {
    let position = text.find(marker)?;
    let digits: String = text[..position]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn number_after(text: &str, marker: char) -> Option<u32> {
    let position = text.find(marker)? + marker.len_utf8();
    let digits: String = text[position..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn looks_like_outcome_completion(text: &str) -> bool {
    ["过了", "完成了", "结束了", "有结果了", "收到回复", "答复了"]
        .iter()
        .any(|marker| text.contains(marker))
}

async fn resolve_matching_open_loop(
    open_loop_store: &dyn OpenLoopStore,
    owner: OpenLoopOwner,
    text: &str,
) {
    let Ok(items) = open_loop_store.list(&owner, 32).await else {
        return;
    };
    let Some(item) = items.into_iter().find(|item| {
        matches!(
            item.kind(),
            OpenLoopKind::AwaitingOutcome | OpenLoopKind::FutureEvent | OpenLoopKind::FollowUp
        ) && outcome_terms_overlap(item.summary(), text)
    }) else {
        return;
    };
    if let Err(error) = open_loop_store.resolve(item.id(), Utc::now()).await {
        kovi::log::warn!(
            "Yunxi open-loop completion could not resolve {}: {error}",
            item.id()
        );
    }
}

fn outcome_terms_overlap(summary: &str, text: &str) -> bool {
    [
        "面试", "考试", "面谈", "申请", "审核", "比赛", "结果", "回复", "答复",
    ]
    .iter()
    .any(|term| summary.contains(term) && text.contains(term))
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

fn message_at_self(message: &Message, self_id: i64) -> bool {
    message.iter().any(|segment| {
        segment.type_ == "at"
            && segment
                .data
                .get("qq")
                .and_then(value_as_i64)
                .is_some_and(|value| value == self_id)
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

fn looks_like_stop_request(message: &str) -> bool {
    let normalized = message
        .trim()
        .trim_matches(|character: char| {
            character.is_ascii_punctuation() || "，。！？…".contains(character)
        })
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "别说了"
            | "不要说了"
            | "别回复了"
            | "不要回复了"
            | "停下"
            | "停止回复"
            | "闭嘴"
            | "stop"
            | "stop replying"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationAddress, EnqueueOutcome, InboundMessage, IngressRouteTracker, MessageReference,
        MessageReferenceCache, MessageReferenceKey, ShadowBridge, acquire_alias_handler_barriers,
        action_result_event, block_user_aliases, bounded_text, detect_open_loop_candidate,
        idle_tick_event, looks_like_stop_request, merge_data_erasure_targets, message_at_self,
        reply_message_id, resolve_and_submit, run_ingress, run_runtime, text_mentions_agent,
        unblock_users,
    };
    use chrono::Utc;
    use kovi::bot::message::{Message, Segment};
    use kovi::tokio::sync::{Notify, mpsc};
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration as StdDuration;
    use yunxi_core::{
        ActionArbiter, ActionArbiterConfig, ActionPort, ActionPortFuture, ActionPortOutcome,
        ActionRejection, ActionResult, AttentionDisposition, ConversationId, ConversationKind,
        EnvironmentCapabilities, EventPriority, IdentityStore, IdentityStoreError,
        IdentityStoreFuture, MessageContent, MessageId, OpenLoopKind, OpenLoopOwner, PersonId,
        ProcessingOutcome, ProposedAction, RuntimeConfig,
    };

    struct FakeIdentityStore {
        person_id: PersonId,
        conversation_id: ConversationId,
        stored_kind: ConversationKind,
    }

    struct FailingIdentityStore;

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
            text: "hello".to_string(),
            timestamp: Utc::now(),
            addressed_to_agent,
            explicit_request: false,
            stop_requested: false,
        }
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
    fn structured_at_and_stop_detection_are_conservative() {
        let at = Message::from(vec![Segment::new("at", json!({"qq": "123"}))]);
        assert!(message_at_self(&at, 123));
        assert!(!message_at_self(&at, 456));
        assert!(text_mentions_agent("芸汐，看看这个"));
        assert!(looks_like_stop_request("STOP！"));
        assert!(!looks_like_stop_request("他说‘别说了’，然后离开了"));
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
        let bridge = ShadowBridge {
            ingress,
            runtime,
            action_arbiter: None,
            action_port: None,
            blocked_users: Arc::new(StdMutex::new(HashSet::new())),
            private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
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
    fn lost_begin_acknowledgement_keeps_host_ingress_fail_closed() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (ingress, mut receiver) = mpsc::channel(1);
            let (runtime_handle, _consumer) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let bridge = Arc::new(ShadowBridge {
                ingress,
                runtime: runtime_handle,
                action_arbiter: None,
                action_port: None,
                blocked_users: Arc::new(StdMutex::new(HashSet::new())),
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
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
    fn private_handler_epoch_drains_old_work_and_rejects_waiters_across_erasure() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Direct,
            });
            let bridge = ShadowBridge::start(store);
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
            let bridge = ShadowBridge::start(store);
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
            let (ingress, receiver) = mpsc::channel(super::SHADOW_INGRESS_CAPACITY);
            let (runtime_handle, core_runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let blocked_users = Arc::new(StdMutex::new(HashSet::new()));
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
                Some(Arc::clone(&arbiter)),
                Some(Arc::clone(&action_port)),
                Arc::new(super::PrivateHandlerGateRegistry::new(4)),
            ));
            kovi::tokio::spawn(run_runtime(core_runtime, None, None));
            let bridge = Arc::new(ShadowBridge {
                ingress,
                runtime: runtime_handle,
                action_arbiter: Some(arbiter),
                action_port: Some(action_port),
                blocked_users,
                private_handler_gates: Arc::new(super::PrivateHandlerGateRegistry::new(4)),
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
                Some(ActionResult::Executed { .. })
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
                    .is_none()
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
            let bridge = ShadowBridge::start(store);
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
    fn open_loop_candidate_detector_keeps_future_event_distinct_from_reminder() {
        let now = Utc::now();
        let owner = OpenLoopOwner::Person(PersonId::new());
        let source = MessageId::new();
        let candidate = detect_open_loop_candidate(
            owner,
            "我明天下午3点面试",
            now,
            source,
            "message:1".to_string(),
        )
        .expect("detector should not fail")
        .expect("future event should create a candidate");
        assert_eq!(candidate.kind(), OpenLoopKind::AwaitingOutcome);
        assert!(candidate.due_at().is_some());
        assert!(
            detect_open_loop_candidate(
                owner,
                "我喜欢看面试节目",
                now,
                source,
                "message:2".to_string(),
            )
            .expect("detector should not fail")
            .is_none()
        );
        assert!(
            detect_open_loop_candidate(
                owner,
                "明天下午3点提醒我面试",
                now,
                source,
                "message:3".to_string(),
            )
            .expect("detector should not fail")
            .is_none()
        );
    }
}
