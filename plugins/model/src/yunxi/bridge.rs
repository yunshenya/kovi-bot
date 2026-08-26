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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use yunxi_core::{
    ActionArbiter, ActionArbiterConfig, ActionPort, ActionResult, Admission, Attachment,
    AttachmentKind, CognitiveRuntime, ConversationId, ConversationKind, ConversationMemberStore,
    CoreServices, EnvironmentCapabilities, EventPriority, EventScope, ExternalConversation,
    IdentityStore, MessageCollisionDetectedEvent, MessageContent, MessageId, MessageReceivedEvent,
    ModelBackend, OpenLoopStore, PlannedProcessingOutcome, ProcessingOutcome, ProposedAction,
    RuntimeConfig, RuntimeHandle, WorldEvent, WorldEventKind,
};

pub(crate) const CORE_INGRESS_CAPACITY: usize = 256;
pub(crate) const MESSAGE_REFERENCE_CAPACITY: usize = 4_096;
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
        let model = super::core_model::KoviModelBackend::new(Arc::clone(&bot), Arc::clone(&store));
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
            let arbiter = ActionArbiter::new(ActionArbiterConfig::default().with_capabilities(
                EnvironmentCapabilities::new([
                    yunxi_core::ActionDescriptor::new(yunxi_core::ActionCapability::SendMessage),
                    yunxi_core::ActionDescriptor::new(yunxi_core::ActionCapability::ReachOut),
                    yunxi_core::ActionDescriptor::new(yunxi_core::ActionCapability::UseTool),
                    yunxi_core::ActionDescriptor::new(yunxi_core::ActionCapability::CreateOpenLoop),
                    yunxi_core::ActionDescriptor::new(
                        yunxi_core::ActionCapability::ResolveOpenLoop,
                    ),
                    yunxi_core::ActionDescriptor::new(yunxi_core::ActionCapability::StartGoal),
                    yunxi_core::ActionDescriptor::new(yunxi_core::ActionCapability::CancelGoal),
                ]),
            ))
            .with_delivery_resolver(resolver);
            let port: Arc<dyn ActionPort> = adapter;
            (Some(Arc::new(arbiter)), Some(port))
        });

        let scheduler_runtime = runtime_handle.clone();
        let bridge_runtime = runtime_handle.clone();
        let incoming_releaser = model_backend
            .as_ref()
            .map(|backend| Arc::clone(backend) as Arc<dyn IncomingAdmissionReleaser>);
        kovi::tokio::spawn(run_ingress(
            receiver,
            store,
            runtime_handle,
            model_backend,
            message_store,
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

    /// Submit an already-canonical host event directly to the Core runtime.
    /// Callers remain responsible for choosing priority and bounding any wait
    /// for reliable-event backpressure.
    pub(crate) async fn submit_event(
        &self,
        event: WorldEvent,
    ) -> Result<Admission, yunxi_core::SubmitError> {
        self.runtime.submit(event).await
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
        self.ingress
            .send(IngressCommand::ProjectDestination {
                destination,
                priority,
                kind,
                acknowledge,
            })
            .await
            .map_err(|_| "Yunxi ingress is closed".to_string())?;
        acknowledged
            .await
            .map_err(|_| "Yunxi projection acknowledgement was dropped".to_string())?
    }

    pub(crate) fn enqueue_group(
        &self,
        event: &GroupMsgEvent,
        incoming_admission: IncomingAdmission,
    ) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_group(event, true) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.incoming_admission = Some(incoming_admission);
        self.try_enqueue(message)
    }

    pub(crate) fn enqueue_group_observation(&self, event: &GroupMsgEvent) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_group(event, false) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.visible_reply_allowed = false;
        self.try_enqueue(message)
    }

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

    pub(crate) fn enqueue_private_observation(&self, event: &PrivateMsgEvent) -> EnqueueOutcome {
        let Some(mut message) = InboundMessage::from_private(event) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        message.visible_reply_allowed = false;
        self.try_enqueue(message)
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
        self.ingress
            .send(IngressCommand::FlushMessageCollisions {
                sender_user_id,
                address,
                acknowledge,
            })
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Yunxi collision flush ingress closed; collision records remain queued"
                )
            })?;
        acknowledged.await.map_err(|_| {
            anyhow::anyhow!("Yunxi collision flush worker stopped before acknowledgement")
        })?
    }

    /// Whether this private event is owned by the Core direct-conversation
    /// path. Ordinary images are supported; control commands and other media
    /// stay on the Host handler so their specialized behavior remains.
    pub(crate) fn handles_private(&self, event: &PrivateMsgEvent) -> bool {
        self.action_arbiter.is_some()
            && self.action_port.is_some()
            && core_private_payload_is_supported(&event.message, event.borrow_text())
            && InboundMessage::from_private(event).is_some()
    }

    /// Core owns ordinary group text/images as bounded observations. Only an
    /// explicit address or a locally sampled ambient candidate receives a
    /// reply admission; background chatter cannot supersede an active reply.
    pub(crate) fn supports_group(&self, event: &GroupMsgEvent) -> bool {
        self.action_arbiter.is_some()
            && self.action_port.is_some()
            && core_group_payload_is_supported(&event.message, event.borrow_text())
            && InboundMessage::from_group(event, false).is_some()
    }

    pub(crate) fn classify_group(&self, event: &GroupMsgEvent) -> GroupCoreHandling {
        if !self.supports_group(event) {
            return GroupCoreHandling::Unsupported;
        }
        let addressed = message_at_self(&event.message, event.self_id)
            || event.borrow_text().is_some_and(text_mentions_agent);
        if addressed || self.should_request_ambient_attention(event) {
            GroupCoreHandling::Decide
        } else {
            GroupCoreHandling::Observe
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
        let attachments = normalize_attachments(&event.message);
        let vision_attachments = crate::vision::extract_image_attachments(&event.message);
        Some(Self {
            address: ConversationAddress::Group {
                group_id: event.group_id,
            },
            sender_user_id: event.user_id,
            external_message_id: positive_message_id(event.message_id),
            reply_to_external_message_id: reply_message_id(&event.message),
            addressed_to_agent: message_at_self(&event.message, event.self_id)
                || text_mentions_agent(&text),
            visible_reply_allowed: true,
            explicit_request: false,
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
                if let Err(error) = resolve_and_submit_inner(
                    &message,
                    store.as_ref(),
                    &runtime,
                    &mut references,
                    model_backend.as_deref(),
                    message_store.as_deref(),
                    Some(&mut routes),
                )
                .await
                {
                    if let Some(admission) = message.incoming_admission {
                        crate::model::ConversationCoordinator::abandon_incoming(admission).await;
                    }
                    eprintln!(
                        "[WARN] Yunxi Core message dropped during identity resolution: {error}"
                    );
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
                    resolve_projected_destination(
                        destination,
                        priority,
                        kind,
                        store.as_ref(),
                        &runtime,
                    )
                    .await
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
                acknowledge,
            } => {
                let blocked_conversation = match action.scope() {
                    yunxi_core::ActionScope::Conversation(conversation_id) => {
                        group_erasure_conversations
                            .values()
                            .any(|blocked| blocked.contains(&conversation_id))
                    }
                    yunxi_core::ActionScope::Person(_) | yunxi_core::ActionScope::Global => false,
                };
                if blocked_at_ingress.contains(&user_id) || blocked_conversation {
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
                if result.is_err() {
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
            let _ = model_backend
                .purge_private_message_contexts(&targets.blocked_user_ids)
                .await;
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
    let cleared_references = references.remove_conversations(&conversation_ids);
    if let Some(model_backend) = model_backend {
        let _ = model_backend.purge_group_message_contexts(group_id).await;
    }
    active.insert(group_id, conversation_ids.clone());
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
        while let Some(outcome) = runtime
            .process_next_with_planner_and_actions(arbiter, port)
            .await
        {
            match outcome {
                Ok(PlannedProcessingOutcome::Planned {
                    observation,
                    plan,
                    actions,
                    ..
                }) => {
                    let has_action_failure = actions.iter().any(|action| !action.is_success());
                    if actions.is_empty() || has_action_failure {
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
                        kovi::log::debug!(
                            "Yunxi Core turn completed: event_id={} type={:?} scope={:?} disposition={:?} intents={} actions={}",
                            observation.event_id,
                            observation.event_type,
                            observation.scope,
                            plan.disposition,
                            plan.intents.len(),
                            actions.len(),
                        );
                    }
                }
                Ok(PlannedProcessingOutcome::RejectedEvent { event, .. })
                | Ok(PlannedProcessingOutcome::RejectedState { event, .. }) => {
                    release_rejected_incoming(&event, incoming_releaser.as_deref()).await;
                    kovi::log::warn!("Yunxi Core planner rejected an event");
                }
                Err(error) => {
                    kovi::log::error!("Yunxi Core planner failed before action outcome: {error}")
                }
            }
        }
        return;
    }
    while let Some(outcome) = runtime.process_next().await {
        match outcome {
            ProcessingOutcome::Observed(observation) => {
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
                release_rejected_incoming(&event, incoming_releaser.as_deref()).await;
                kovi::log::warn!("Yunxi Core runtime rejected an event");
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
    resolve_and_submit_inner(message, store, runtime, references, None, None, None).await
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
    runtime
        .submit(WorldEvent::new(Utc::now(), scope, priority, kind))
        .await
        .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn resolve_and_submit_inner(
    message: &InboundMessage,
    store: &dyn IdentityStore,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
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

    if let Some(model_backend) = model_backend {
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
        || message.planner_attention_requested
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
            visible_reply_allowed: message.visible_reply_allowed,
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
    let registered_incoming = if let (Some(model_backend), Some(incoming_admission)) =
        (model_backend, message.incoming_admission)
    {
        if priority != EventPriority::High {
            crate::model::ConversationCoordinator::abandon_incoming(incoming_admission).await;
            false
        } else if incoming_admission.ticket.scope() == reply_scope {
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
            false
        }
    } else {
        if let Some(incoming_admission) = message.incoming_admission {
            crate::model::ConversationCoordinator::abandon_incoming(incoming_admission).await;
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
    let admission = match runtime.submit(event).await {
        Ok(admission) => admission,
        Err(error) => {
            if registered_incoming && let Some(model_backend) = model_backend {
                model_backend.discard_incoming(message_id).await;
            }
            return Err(anyhow::anyhow!(error));
        }
    };
    if registered_incoming
        && !matches!(admission, Admission::Accepted)
        && let Some(model_backend) = model_backend
    {
        model_backend.discard_incoming(message_id).await;
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
    let conversation_id = if matches!(address, ConversationAddress::Direct { .. })
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
            .map_err(|error| anyhow::anyhow!(error))?
    } else {
        store
            .resolve_external_conversation(&external_conversation)
            .await
            .map_err(|error| anyhow::anyhow!(error))?
    };
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
        let admission = runtime.submit(collision_event).await;
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
                Err(error) => Err(anyhow::anyhow!(error)),
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
    let segments_supported = message.iter().all(|segment| {
        matches!(segment.type_.as_str(), "text" | "image") || (group && segment.type_ == "at")
    });
    segments_supported
        && (!text.is_empty() || image_segments > 0)
        && !text.starts_with('#')
        && crate::vision::image_segments_are_resolvable(message)
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

fn ambient_group_payload_can_be_sampled(message: &Message) -> bool {
    !message
        .iter()
        .any(|segment| matches!(segment.type_.as_str(), "at" | "reply"))
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationAddress, CoreBridge, EnqueueOutcome, InboundMessage,
        IncomingAdmissionReleaseFuture, IncomingAdmissionReleaser, IngressRouteTracker,
        MessageReference, MessageReferenceCache, MessageReferenceKey,
        acquire_alias_handler_barriers, action_result_event, ambient_group_payload_can_be_sampled,
        block_user_aliases, bounded_text, core_group_payload_is_supported,
        core_private_payload_is_supported, idle_tick_event, merge_data_erasure_targets,
        message_at_self, normalize_attachments, reply_message_id, resolve_and_submit, run_ingress,
        run_runtime, submit_message_collisions, text_mentions_agent, unblock_users,
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
        assert!(!ambient_group_payload_can_be_sampled(&at));
        let reply = Message::from(vec![Segment::new("reply", json!({"id": "456"}))]);
        assert!(!ambient_group_payload_can_be_sampled(&reply));
        assert!(ambient_group_payload_can_be_sampled(&Message::from(
            "大家觉得这个怎么样"
        )));
        assert!(text_mentions_agent("芸汐，看看这个"));
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
