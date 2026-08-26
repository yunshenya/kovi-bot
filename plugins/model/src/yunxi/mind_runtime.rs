use crate::config::MindConfig;
use chrono::{DateTime, Duration, Utc};
use kovi::tokio::sync::{Mutex as AsyncMutex, RwLock, RwLockReadGuard};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use yunxi_core::{
    AgendaItemId, AgendaOperation, AgendaSource, AgendaSubject, AgendaUpdateProposal, Belief,
    BeliefId, BeliefOperation, BeliefSource, BeliefUpdateProposal, Consolidation,
    ConsolidationConfig, ConsolidationError, CuriosityId, CuriosityItem, CuriosityStatus, Episode,
    EpisodeId, EventId, EventPriority, EventScope, EvidenceKind, EvidencePolarity, EvidenceRef,
    Goal, GoalOwner, GoalState, GoalStore, Interest, InterestOperation, InterestUpdateProposal,
    Memory, MemoryQuery, MemoryScope, MemoryStore, MindDecisionProjection, MindDecisionReference,
    MindInfluenceMode, MindReasonTag, MindScope, MindServices, MindSnapshot, MindSnapshotFuture,
    MindSnapshotProvider, MindSnapshotRequest, MindSnapshotStoreProvider, MindSource, OpenLoop,
    OpenLoopKind, OpenLoopOwner, OpenLoopStore, OpenQuestion, OpenQuestionId,
    OpenQuestionOperation, OpenQuestionStatus, OpenQuestionUpdateProposal, PlannerInput,
    Preference, PreferenceOperation, PreferenceSource, PreferenceUpdateProposal, ReflectionDepth,
    ReflectionEvent, ReflectionInput, ReflectionProposal, ReflectionQueue, ReflectionQueueConfig,
    ReflectionTrigger, TraceContext, WorldEvent, WorldEventKind,
};

const MAX_TRACKED_SCOPES: usize = 512;
const MAX_PENDING_CANDIDATES: usize = 512;
const PENDING_CANDIDATE_TTL_MINUTES: i64 = 10;
const MAX_REFLECTIONS_PER_TICK: usize = 8;
const MAX_REFLECTION_SCOPES_PER_TICK: usize = 32;
const MAX_REFLECTION_DECAYS: usize = 8;
const MAX_EPISODE_SOURCE_EVENTS: usize = 16;
const CURIOSITY_TTL_DAYS: i64 = 30;
const MAX_OUTGOING_FENCES: usize = 512;
const OUTGOING_FENCE_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MindInterestCandidate {
    pub topic: String,
    pub novelty: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MindBeliefCandidate {
    pub proposition: String,
    pub confidence_delta: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MindPreferenceCandidate {
    pub subject: String,
    pub valence_delta: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MindCandidates {
    pub interest: Option<MindInterestCandidate>,
    pub curiosity: Option<String>,
    pub open_question: Option<String>,
    pub agenda: Option<String>,
    pub belief: Option<MindBeliefCandidate>,
    pub preference: Option<MindPreferenceCandidate>,
}

impl MindCandidates {
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.interest.is_none()
            && self.curiosity.is_none()
            && self.open_question.is_none()
            && self.agenda.is_none()
            && self.belief.is_none()
            && self.preference.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MindCandidateContext {
    person_id: yunxi_core::PersonId,
    conversation_id: yunxi_core::ConversationId,
    conversation_kind: yunxi_core::ConversationKind,
    event_id: EventId,
    occurred_at: DateTime<Utc>,
    trace: TraceContext,
}

impl MindCandidateContext {
    pub(crate) fn from_planner_input(input: &PlannerInput) -> Option<Self> {
        let WorldEventKind::MessageReceived(message) = input.event.kind() else {
            return None;
        };
        Some(Self {
            person_id: message.sender,
            conversation_id: message.conversation_id,
            conversation_kind: message.conversation_kind,
            event_id: input.event.id(),
            occurred_at: input.event.occurred_at(),
            trace: input.event.trace(),
        })
    }

    const fn scoped_state(self) -> MindScope {
        if matches!(self.conversation_kind, yunxi_core::ConversationKind::Direct) {
            MindScope::Person {
                person_id: self.person_id,
            }
        } else {
            MindScope::Conversation {
                conversation_id: self.conversation_id,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MindProactiveReference {
    Curiosity(CuriosityId),
    OpenQuestion(OpenQuestionId),
    Agenda(AgendaItemId),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MindProactiveSignals {
    pub salience: u8,
    pub topic: Option<String>,
    pub reference: Option<MindProactiveReference>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MindMetricsSnapshot {
    pub events_observed: u64,
    pub candidates_registered: u64,
    pub candidates_applied: u64,
    pub candidates_rejected: u64,
    pub reflections: u64,
    pub reflection_failures: u64,
    pub belief_updates: u64,
    pub preference_updates: u64,
    pub interest_updates: u64,
    pub agenda_updates: u64,
    pub proactive_uses: u64,
    pub erasures: u64,
    pub blocked_snapshots: u64,
    pub snapshot_requests: u64,
    pub snapshot_latency_total_micros: u64,
    pub snapshot_latency_max_micros: u64,
    pub snapshot_latency_last_micros: u64,
    pub decision_observations: u64,
    pub shadow_decision_deltas: u64,
    pub active_decision_deltas: u64,
    pub estimated_extra_prompt_tokens: u64,
    pub outgoing_fences_registered: u64,
    pub outgoing_fences_rejected: u64,
    pub outgoing_fences_stale: u64,
    pub last_reflection_unix_ms: i64,
}

#[derive(Debug, Default)]
struct MindMetrics {
    events_observed: AtomicU64,
    candidates_registered: AtomicU64,
    candidates_applied: AtomicU64,
    candidates_rejected: AtomicU64,
    reflections: AtomicU64,
    reflection_failures: AtomicU64,
    belief_updates: AtomicU64,
    preference_updates: AtomicU64,
    interest_updates: AtomicU64,
    agenda_updates: AtomicU64,
    proactive_uses: AtomicU64,
    erasures: AtomicU64,
    blocked_snapshots: AtomicU64,
    snapshot_requests: AtomicU64,
    snapshot_latency_total_micros: AtomicU64,
    snapshot_latency_max_micros: AtomicU64,
    snapshot_latency_last_micros: AtomicU64,
    decision_observations: AtomicU64,
    shadow_decision_deltas: AtomicU64,
    active_decision_deltas: AtomicU64,
    estimated_extra_prompt_tokens: AtomicU64,
    outgoing_fences_registered: AtomicU64,
    outgoing_fences_rejected: AtomicU64,
    outgoing_fences_stale: AtomicU64,
    last_reflection_unix_ms: AtomicI64,
}

impl MindMetrics {
    fn snapshot(&self) -> MindMetricsSnapshot {
        MindMetricsSnapshot {
            events_observed: self.events_observed.load(Ordering::Relaxed),
            candidates_registered: self.candidates_registered.load(Ordering::Relaxed),
            candidates_applied: self.candidates_applied.load(Ordering::Relaxed),
            candidates_rejected: self.candidates_rejected.load(Ordering::Relaxed),
            reflections: self.reflections.load(Ordering::Relaxed),
            reflection_failures: self.reflection_failures.load(Ordering::Relaxed),
            belief_updates: self.belief_updates.load(Ordering::Relaxed),
            preference_updates: self.preference_updates.load(Ordering::Relaxed),
            interest_updates: self.interest_updates.load(Ordering::Relaxed),
            agenda_updates: self.agenda_updates.load(Ordering::Relaxed),
            proactive_uses: self.proactive_uses.load(Ordering::Relaxed),
            erasures: self.erasures.load(Ordering::Relaxed),
            blocked_snapshots: self.blocked_snapshots.load(Ordering::Relaxed),
            snapshot_requests: self.snapshot_requests.load(Ordering::Relaxed),
            snapshot_latency_total_micros: self
                .snapshot_latency_total_micros
                .load(Ordering::Relaxed),
            snapshot_latency_max_micros: self.snapshot_latency_max_micros.load(Ordering::Relaxed),
            snapshot_latency_last_micros: self.snapshot_latency_last_micros.load(Ordering::Relaxed),
            decision_observations: self.decision_observations.load(Ordering::Relaxed),
            shadow_decision_deltas: self.shadow_decision_deltas.load(Ordering::Relaxed),
            active_decision_deltas: self.active_decision_deltas.load(Ordering::Relaxed),
            estimated_extra_prompt_tokens: self
                .estimated_extra_prompt_tokens
                .load(Ordering::Relaxed),
            outgoing_fences_registered: self.outgoing_fences_registered.load(Ordering::Relaxed),
            outgoing_fences_rejected: self.outgoing_fences_rejected.load(Ordering::Relaxed),
            outgoing_fences_stale: self.outgoing_fences_stale.load(Ordering::Relaxed),
            last_reflection_unix_ms: self.last_reflection_unix_ms.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MindReasonSnapshot {
    pub last_disposition: Option<yunxi_core::DecisionDisposition>,
    pub last_decision_reasons: Vec<MindReasonTag>,
    pub last_agenda_source: Option<AgendaSource>,
    pub last_belief_source: Option<BeliefSource>,
    pub last_proactive_kind: Option<yunxi_core::AgendaItemKind>,
}

#[derive(Debug, Clone)]
struct PendingMindFence {
    idempotency_key: String,
    request: MindSnapshotRequest,
    expected_signature: [u8; 32],
    projection: MindDecisionProjection,
    erasure_epoch: u64,
    registered_at: DateTime<Utc>,
}

#[derive(Debug)]
pub(crate) struct MindDeliveryPermit<'a> {
    _barrier: Option<RwLockReadGuard<'a, BarrierState>>,
}

impl MindDeliveryPermit<'_> {
    #[must_use]
    pub(crate) const fn untracked() -> Self {
        Self { _barrier: None }
    }
}

#[derive(Clone)]
pub(crate) struct MindContextServices {
    memory: Arc<dyn MemoryStore>,
    open_loops: Arc<dyn OpenLoopStore>,
    goals: Arc<dyn GoalStore>,
}

impl std::fmt::Debug for MindContextServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MindContextServices")
            .finish_non_exhaustive()
    }
}

impl MindContextServices {
    #[must_use]
    pub(crate) fn new(
        memory: Arc<dyn MemoryStore>,
        open_loops: Arc<dyn OpenLoopStore>,
        goals: Arc<dyn GoalStore>,
    ) -> Self {
        Self {
            memory,
            open_loops,
            goals,
        }
    }
}

#[derive(Debug, Default)]
struct V1MindContext {
    memories: Vec<Memory>,
    open_loops: Vec<OpenLoop>,
    goals: Vec<Goal>,
}

#[derive(Debug, Default)]
struct BarrierState {
    epoch: u64,
    persons: HashMap<yunxi_core::PersonId, usize>,
    conversations: HashMap<yunxi_core::ConversationId, usize>,
}

impl BarrierState {
    fn blocks_origin(
        &self,
        person_id: Option<yunxi_core::PersonId>,
        conversation_id: Option<yunxi_core::ConversationId>,
    ) -> bool {
        person_id.is_some_and(|id| self.persons.contains_key(&id))
            || conversation_id.is_some_and(|id| self.conversations.contains_key(&id))
    }

    fn blocks_scope(&self, scope: MindScope) -> bool {
        match scope {
            MindScope::Global => false,
            MindScope::Person { person_id } => self.persons.contains_key(&person_id),
            MindScope::Conversation { conversation_id } => {
                self.conversations.contains_key(&conversation_id)
            }
        }
    }

    fn add(
        &mut self,
        person_id: Option<yunxi_core::PersonId>,
        conversation_ids: &[yunxi_core::ConversationId],
    ) {
        self.epoch = self.epoch.wrapping_add(1).max(1);
        if let Some(person_id) = person_id {
            *self.persons.entry(person_id).or_default() += 1;
        }
        for conversation_id in conversation_ids {
            *self.conversations.entry(*conversation_id).or_default() += 1;
        }
    }

    fn remove(
        &mut self,
        person_id: Option<yunxi_core::PersonId>,
        conversation_ids: &[yunxi_core::ConversationId],
    ) {
        if let Some(person_id) = person_id {
            decrement_count(&mut self.persons, person_id);
        }
        for conversation_id in conversation_ids {
            decrement_count(&mut self.conversations, *conversation_id);
        }
    }
}

fn decrement_count<K: std::hash::Hash + Eq + Copy>(values: &mut HashMap<K, usize>, key: K) {
    let remove = values.get_mut(&key).is_some_and(|count| {
        *count = count.saturating_sub(1);
        *count == 0
    });
    if remove {
        values.remove(&key);
    }
}

#[derive(Debug, Clone)]
struct PendingCandidates {
    idempotency_key: String,
    context: MindCandidateContext,
    candidates: MindCandidates,
    erasure_epoch: u64,
    registered_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct ObservedReflectionEvent {
    event: ReflectionEvent,
    trace: TraceContext,
}

#[derive(Debug, Default)]
struct RecentEvents {
    by_scope: HashMap<MindScope, VecDeque<ObservedReflectionEvent>>,
}

impl RecentEvents {
    fn push(&mut self, observed: ObservedReflectionEvent, limit: usize) {
        if !self.by_scope.contains_key(&observed.event.scope)
            && self.by_scope.len() >= MAX_TRACKED_SCOPES
            && let Some(oldest) = self
                .by_scope
                .iter()
                .min_by_key(|(_, events)| events.back().map(|event| event.event.occurred_at))
                .map(|(scope, _)| *scope)
        {
            self.by_scope.remove(&oldest);
        }
        let events = self.by_scope.entry(observed.event.scope).or_default();
        if events
            .back()
            .is_some_and(|stored| stored.event.event_id == observed.event.event_id)
        {
            return;
        }
        events.push_back(observed);
        while events.len() > limit {
            events.pop_front();
        }
    }

    fn scopes(&self) -> Vec<MindScope> {
        self.by_scope.keys().copied().collect()
    }

    fn for_scope(&self, scope: MindScope) -> Vec<ObservedReflectionEvent> {
        self.by_scope
            .get(&scope)
            .map(|events| events.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn remove_through(&mut self, scope: MindScope, requested_at: DateTime<Utc>) {
        let remove_scope = self.by_scope.get_mut(&scope).is_some_and(|events| {
            events.retain(|event| event.event.occurred_at > requested_at);
            events.is_empty()
        });
        if remove_scope {
            self.by_scope.remove(&scope);
        }
    }

    fn purge(&mut self, scopes: &[MindScope]) {
        for scope in scopes {
            self.by_scope.remove(scope);
        }
    }
}

#[derive(Debug)]
pub(crate) struct MindRuntime {
    services: MindServices,
    snapshot_provider: MindSnapshotStoreProvider,
    config: MindConfig,
    context_services: Option<MindContextServices>,
    consolidation: Consolidation,
    reflection_queue: ReflectionQueue,
    barrier: RwLock<BarrierState>,
    pending_candidates: Mutex<VecDeque<PendingCandidates>>,
    outgoing_fences: Mutex<VecDeque<PendingMindFence>>,
    recent_events: Mutex<RecentEvents>,
    last_reflections: Mutex<HashMap<MindScope, DateTime<Utc>>>,
    reasons: Mutex<MindReasonSnapshot>,
    reflection_worker: AsyncMutex<()>,
    metrics: MindMetrics,
}

impl MindRuntime {
    pub(crate) fn new(services: MindServices, config: MindConfig) -> anyhow::Result<Self> {
        let snapshot_provider = MindSnapshotStoreProvider::new(services.clone());
        Ok(Self {
            services,
            snapshot_provider,
            config,
            context_services: None,
            consolidation: Consolidation::new(ConsolidationConfig::default())?,
            reflection_queue: ReflectionQueue::new(ReflectionQueueConfig::default())?,
            barrier: RwLock::new(BarrierState::default()),
            pending_candidates: Mutex::new(VecDeque::new()),
            outgoing_fences: Mutex::new(VecDeque::new()),
            recent_events: Mutex::new(RecentEvents::default()),
            last_reflections: Mutex::new(HashMap::new()),
            reasons: Mutex::new(MindReasonSnapshot::default()),
            reflection_worker: AsyncMutex::new(()),
            metrics: MindMetrics::default(),
        })
    }

    #[must_use]
    pub(crate) fn with_context_services(mut self, services: MindContextServices) -> Self {
        self.context_services = Some(services);
        self
    }

    #[must_use]
    pub(crate) const fn config(&self) -> &MindConfig {
        &self.config
    }

    #[must_use]
    pub(crate) fn metrics(&self) -> MindMetricsSnapshot {
        self.metrics.snapshot()
    }

    #[must_use]
    pub(crate) fn reasons(&self) -> MindReasonSnapshot {
        self.reasons
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
    }

    pub(crate) fn observe_decision(
        &self,
        projection: MindDecisionProjection,
        estimated_extra_tokens: usize,
    ) {
        self.metrics
            .decision_observations
            .fetch_add(1, Ordering::Relaxed);
        self.metrics.estimated_extra_prompt_tokens.fetch_add(
            u64::try_from(estimated_extra_tokens).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if projection.changes_baseline() {
            match self.config.influence_mode() {
                MindInfluenceMode::Shadow => {
                    self.metrics
                        .shadow_decision_deltas
                        .fetch_add(1, Ordering::Relaxed);
                }
                MindInfluenceMode::Active => {
                    self.metrics
                        .active_decision_deltas
                        .fetch_add(1, Ordering::Relaxed);
                }
                MindInfluenceMode::Disabled => {}
            }
        }
        let mut reasons = self.reasons.lock().unwrap_or_else(|lock| lock.into_inner());
        reasons.last_disposition = Some(projection.disposition());
        reasons.last_decision_reasons = projection.reason_tags().to_vec();
    }

    pub(crate) fn register_outgoing_fence(
        &self,
        idempotency_key: String,
        input: &PlannerInput,
        projection: MindDecisionProjection,
    ) -> bool {
        if self.config.influence_mode() != MindInfluenceMode::Active
            || input.mind.influence_mode() != MindInfluenceMode::Active
            || input.mind.is_empty()
            || input.mind.version() == 0
            || idempotency_key.is_empty()
        {
            self.metrics
                .outgoing_fences_rejected
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let topic = input
            .state
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.current_topic.as_deref());
        let Ok(request) = MindSnapshotRequest::for_event(
            &input.event,
            topic,
            self.config.snapshot_limits(),
            MindInfluenceMode::Active,
        ) else {
            self.metrics
                .outgoing_fences_rejected
                .fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let Ok(barrier) = self.barrier.try_read() else {
            self.metrics
                .outgoing_fences_rejected
                .fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if barrier.blocks_origin(request.person_id(), request.conversation_id())
            || request
                .scopes()
                .iter()
                .any(|scope| barrier.blocks_scope(*scope))
        {
            self.metrics
                .outgoing_fences_rejected
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let now = Utc::now();
        let mut fences = self
            .outgoing_fences
            .lock()
            .unwrap_or_else(|lock| lock.into_inner());
        fences.retain(|fence| {
            fence.idempotency_key != idempotency_key
                && now.signed_duration_since(fence.registered_at)
                    < Duration::minutes(OUTGOING_FENCE_TTL_MINUTES)
        });
        if fences.len() >= MAX_OUTGOING_FENCES {
            self.metrics
                .outgoing_fences_rejected
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        fences.push_back(PendingMindFence {
            idempotency_key,
            request,
            expected_signature: mind_snapshot_signature(&input.mind),
            projection,
            erasure_epoch: barrier.epoch,
            registered_at: now,
        });
        drop(fences);
        drop(barrier);
        self.metrics
            .outgoing_fences_registered
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    pub(crate) async fn pin_revalidated_outgoing_fence(
        &self,
        idempotency_key: &str,
    ) -> Option<MindDeliveryPermit<'_>> {
        let fence = self
            .outgoing_fences
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .iter()
            .find(|fence| fence.idempotency_key == idempotency_key)
            .cloned();
        let Some(fence) = fence else {
            return Some(MindDeliveryPermit::untracked());
        };
        if Utc::now().signed_duration_since(fence.registered_at)
            >= Duration::minutes(OUTGOING_FENCE_TTL_MINUTES)
        {
            self.discard_pending_candidates(idempotency_key);
            self.metrics
                .outgoing_fences_stale
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let barrier = self.barrier.read().await;
        if barrier.epoch != fence.erasure_epoch
            || barrier.blocks_origin(fence.request.person_id(), fence.request.conversation_id())
            || fence
                .request
                .scopes()
                .iter()
                .any(|scope| barrier.blocks_scope(*scope))
        {
            self.discard_pending_candidates(idempotency_key);
            self.metrics
                .outgoing_fences_stale
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let current = self.snapshot_provider.snapshot(&fence.request).await;
        let valid = current.is_ok_and(|snapshot| {
            !snapshot.is_empty() && mind_snapshot_signature(&snapshot) == fence.expected_signature
        });
        if !valid {
            self.discard_pending_candidates(idempotency_key);
            self.metrics
                .outgoing_fences_stale
                .fetch_add(1, Ordering::Relaxed);
            kovi::log::info!(
                "Yunxi Mind outgoing fence rejected: key={} disposition={:?} reasons={:?}",
                idempotency_key,
                fence.projection.disposition(),
                fence.projection.reason_tags(),
            );
        }
        valid.then_some(MindDeliveryPermit {
            _barrier: Some(barrier),
        })
    }

    fn discard_pending_candidates(&self, idempotency_key: &str) {
        self.pending_candidates
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .retain(|pending| pending.idempotency_key != idempotency_key);
    }

    pub(crate) fn register_candidates(
        &self,
        idempotency_key: String,
        context: MindCandidateContext,
        candidates: MindCandidates,
    ) -> bool {
        if !self.config.enabled() || candidates.is_empty() || idempotency_key.is_empty() {
            return false;
        }
        let Ok(barrier) = self.barrier.try_read() else {
            self.metrics
                .candidates_rejected
                .fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if barrier.blocks_origin(Some(context.person_id), Some(context.conversation_id)) {
            self.metrics
                .candidates_rejected
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let now = Utc::now();
        let mut pending = self
            .pending_candidates
            .lock()
            .unwrap_or_else(|lock| lock.into_inner());
        pending.retain(|item| {
            item.idempotency_key != idempotency_key
                && now.signed_duration_since(item.registered_at)
                    < Duration::minutes(PENDING_CANDIDATE_TTL_MINUTES)
        });
        pending.push_back(PendingCandidates {
            idempotency_key,
            context,
            candidates,
            erasure_epoch: barrier.epoch,
            registered_at: now,
        });
        while pending.len() > MAX_PENDING_CANDIDATES {
            pending.pop_front();
            self.metrics
                .candidates_rejected
                .fetch_add(1, Ordering::Relaxed);
        }
        self.metrics
            .candidates_registered
            .fetch_add(1, Ordering::Relaxed);
        drop(pending);
        drop(barrier);
        true
    }

    pub(crate) fn commit_candidates(self: &Arc<Self>, idempotency_key: &str) {
        let pending = {
            let mut queue = self
                .pending_candidates
                .lock()
                .unwrap_or_else(|lock| lock.into_inner());
            queue
                .iter()
                .position(|item| item.idempotency_key == idempotency_key)
                .and_then(|index| queue.remove(index))
        };
        let fence = {
            let mut fences = self
                .outgoing_fences
                .lock()
                .unwrap_or_else(|lock| lock.into_inner());
            fences
                .iter()
                .position(|item| item.idempotency_key == idempotency_key)
                .and_then(|index| fences.remove(index))
        };
        if pending.is_none() && fence.is_none() {
            return;
        }
        let runtime = Arc::clone(self);
        kovi::tokio::spawn(async move {
            if let Some(fence) = fence
                && let Some(reference) = fence.projection.reference()
                && let Err(error) = runtime.mark_decision_used_inner(reference).await
            {
                kovi::log::warn!("Yunxi Mind decision cooldown update failed: {error}");
            }
            if let Some(pending) = pending
                && let Err(error) = runtime.persist_candidates(pending).await
            {
                runtime
                    .metrics
                    .candidates_rejected
                    .fetch_add(1, Ordering::Relaxed);
                kovi::log::warn!("Yunxi Mind candidate consolidation failed: {error}");
            }
        });
    }

    pub(crate) async fn observe_event(&self, event: &WorldEvent) -> anyhow::Result<()> {
        if !self.config.enabled() {
            return Ok(());
        }
        if let WorldEventKind::MessageCollisionDetected(collision) = event.kind() {
            let scope = MindScope::Conversation {
                conversation_id: collision.conversation_id,
            };
            let barrier = self.barrier.read().await;
            if barrier.blocks_scope(scope) {
                return Ok(());
            }
            self.metrics.events_observed.fetch_add(1, Ordering::Relaxed);
            self.recent_events
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .push(
                    ObservedReflectionEvent {
                        event: ReflectionEvent {
                            event_id: event.id(),
                            scope,
                            summary: "message collision changed pending outgoing".to_string(),
                            salience: 0.45,
                            occurred_at: event.occurred_at(),
                        },
                        trace: event.trace(),
                    },
                    self.config.reflection_max_events(),
                );
            return Ok(());
        }
        let WorldEventKind::MessageReceived(message) = event.kind() else {
            return Ok(());
        };
        self.metrics.events_observed.fetch_add(1, Ordering::Relaxed);
        let scopes = if message.conversation_kind == yunxi_core::ConversationKind::Direct {
            vec![
                MindScope::Person {
                    person_id: message.sender,
                },
                MindScope::Conversation {
                    conversation_id: message.conversation_id,
                },
            ]
        } else {
            vec![MindScope::Conversation {
                conversation_id: message.conversation_id,
            }]
        };
        let barrier = self.barrier.read().await;
        if barrier.blocks_origin(Some(message.sender), Some(message.conversation_id)) {
            return Ok(());
        }
        let summary = bounded_summary(message.content.as_text());
        let salience = message_salience(message);
        {
            let mut recent = self
                .recent_events
                .lock()
                .unwrap_or_else(|lock| lock.into_inner());
            for scope in &scopes {
                recent.push(
                    ObservedReflectionEvent {
                        event: ReflectionEvent {
                            event_id: event.id(),
                            scope: *scope,
                            summary: summary.clone(),
                            salience,
                            occurred_at: event.occurred_at(),
                        },
                        trace: event.trace(),
                    },
                    self.config.reflection_max_events(),
                );
            }
        }
        self.resolve_answered_state(event, &scopes).await?;
        self.refresh_agenda_for_scopes(
            &scopes,
            message.content.as_text(),
            event.occurred_at(),
            event.trace(),
        )
        .await?;
        Ok(())
    }

    async fn load_v1_context(&self, scope: MindScope, query: &str) -> V1MindContext {
        let Some(services) = self.context_services.as_ref() else {
            return V1MindContext::default();
        };
        let (memory_scope, open_loop_owner, goal_owner) = match scope {
            MindScope::Global => (
                MemoryScope::Global,
                OpenLoopOwner::Global,
                GoalOwner::Global,
            ),
            MindScope::Person { person_id } => (
                MemoryScope::Person(person_id),
                OpenLoopOwner::Person(person_id),
                GoalOwner::Person(person_id),
            ),
            MindScope::Conversation { conversation_id } => (
                MemoryScope::Conversation(conversation_id),
                OpenLoopOwner::Conversation(conversation_id),
                GoalOwner::Conversation(conversation_id),
            ),
        };
        let memories = match MemoryQuery::new(memory_scope, query, 16) {
            Ok(memory_query) => {
                services
                    .memory
                    .recall(&memory_query)
                    .await
                    .unwrap_or_else(|error| {
                        kovi::log::warn!("Yunxi Mind V1 memory context failed soft: {error}");
                        Vec::new()
                    })
            }
            Err(error) => {
                kovi::log::warn!("Yunxi Mind V1 memory query rejected: {error}");
                Vec::new()
            }
        };
        let open_loops = services
            .open_loops
            .list(&open_loop_owner, 16)
            .await
            .unwrap_or_else(|error| {
                kovi::log::warn!("Yunxi Mind V1 open-loop context failed soft: {error}");
                Vec::new()
            });
        let goals = services
            .goals
            .list(&goal_owner, 16)
            .await
            .unwrap_or_else(|error| {
                kovi::log::warn!("Yunxi Mind V1 goal context failed soft: {error}");
                Vec::new()
            });
        V1MindContext {
            memories,
            open_loops,
            goals,
        }
    }

    async fn refresh_agenda_for_scopes(
        &self,
        scopes: &[MindScope],
        query: &str,
        now: DateTime<Utc>,
        trace: TraceContext,
    ) -> anyhow::Result<()> {
        if !self.config.agenda_enabled() {
            return Ok(());
        }
        for scope in scopes.iter().copied() {
            let context = self.load_v1_context(scope, query).await;
            for open_loop in context
                .open_loops
                .into_iter()
                .filter(|item| !item.status().is_terminal())
            {
                let due_bonus = open_loop
                    .due_at()
                    .is_some_and(|due| due <= now + Duration::days(1));
                let salience = (f32::from(open_loop.salience()) / 100.0).max(if due_bonus {
                    0.8
                } else {
                    0.45
                });
                if self
                    .ensure_agenda(
                        scope,
                        AgendaSubject::OpenLoop(open_loop.id()),
                        AgendaSource::OpenLoop,
                        salience,
                        0.8,
                        now,
                        trace,
                    )
                    .await?
                {
                    self.record_agenda_reason(AgendaSource::OpenLoop);
                }
            }
            for goal in context
                .goals
                .into_iter()
                .filter(|goal| goal.state() == GoalState::Active)
            {
                let salience = if goal
                    .due_at()
                    .is_some_and(|due| due <= now + Duration::days(7))
                {
                    0.8
                } else {
                    0.6
                };
                if self
                    .ensure_agenda(
                        scope,
                        AgendaSubject::Goal(goal.id()),
                        AgendaSource::Goal,
                        salience,
                        0.8,
                        now,
                        trace,
                    )
                    .await?
                {
                    self.record_agenda_reason(AgendaSource::Goal);
                }
            }
            for memory in context
                .memories
                .into_iter()
                .filter(|memory| memory.importance() >= 70)
            {
                let salience = f32::from(memory.importance()) / 100.0;
                if self
                    .ensure_agenda(
                        scope,
                        AgendaSubject::SalientMemory(memory.id()),
                        AgendaSource::Memory,
                        salience,
                        0.5,
                        now,
                        trace,
                    )
                    .await?
                {
                    self.record_agenda_reason(AgendaSource::Memory);
                }
            }
        }
        if self.config.interest_enabled() {
            for interest in self.services.interests.relevant(query, 8).await? {
                let salience = interest
                    .activation()
                    .max(interest.long_term_affinity() * 0.8);
                if salience < 0.5 {
                    continue;
                }
                if self
                    .ensure_agenda(
                        MindScope::Global,
                        AgendaSubject::Interest(interest.id()),
                        AgendaSource::Interest,
                        salience,
                        0.55,
                        now,
                        trace,
                    )
                    .await?
                {
                    self.record_agenda_reason(AgendaSource::Interest);
                }
            }
        }
        Ok(())
    }

    fn record_agenda_reason(&self, source: AgendaSource) {
        self.reasons
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .last_agenda_source = Some(source);
    }

    pub(crate) async fn observe_interaction_cues(
        &self,
        person_id: yunxi_core::PersonId,
        cues: yunxi_core::InteractionCues,
    ) -> anyhow::Result<()> {
        if !self.config.enabled() || cues == yunxi_core::InteractionCues::default() {
            return Ok(());
        }
        let barrier = self.barrier.read().await;
        if barrier.blocks_origin(Some(person_id), None) {
            return Ok(());
        }
        let bonus = (cues.gratitude_strength * 0.05
            + cues.sentiment_arousal.abs() * cues.sentiment_confidence * 0.02)
            .clamp(0.0, 0.08);
        if bonus <= 0.0 {
            return Ok(());
        }
        let now = Utc::now();
        if self.config.interest_enabled()
            && let Some(interest) = self
                .services
                .interests
                .relevant("", 1)
                .await?
                .into_iter()
                .next()
        {
            let mut proposal = self
                .empty_proposal(MindScope::Global, now, TraceContext::root(EventId::new()))
                .await?;
            proposal.interest_updates.push(InterestUpdateProposal {
                operation: InterestOperation::Activate,
                interest_id: Some(interest.id()),
                expected_version: None,
                topic: interest.topic().to_owned(),
                activation_delta: bonus,
                affinity_delta: 0.0,
                novelty: interest.novelty(),
                source: MindSource::Conversation,
            });
            self.consolidate_retry(proposal).await?;
            self.metrics
                .interest_updates
                .fetch_add(1, Ordering::Relaxed);
        }
        if self.config.agenda_enabled()
            && let Some(item) = self
                .services
                .agenda
                .list_active(&[MindScope::Person { person_id }], now, 1)
                .await?
                .into_iter()
                .next()
        {
            let mut proposal = self
                .empty_proposal(item.scope(), now, TraceContext::root(EventId::new()))
                .await?;
            proposal.agenda_updates.push(AgendaUpdateProposal {
                operation: AgendaOperation::Activate,
                item_id: Some(item.id()),
                expected_version: None,
                scope: item.scope(),
                subject: item.subject().clone(),
                salience: item.salience(),
                activation: (item.activation() + bonus).clamp(0.0, 1.0),
                stability: item.stability(),
                source: item.source(),
                defer_until: None,
            });
            self.consolidate_retry(proposal).await?;
            self.metrics.agenda_updates.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    pub(crate) async fn trigger_reflection(&self, trigger: ReflectionTrigger) {
        if !self.config.reflection_enabled() {
            return;
        }
        let Ok(_worker) = self.reflection_worker.try_lock() else {
            return;
        };
        let now = Utc::now();
        let mut scopes = {
            self.recent_events
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .scopes()
        };
        if !scopes.contains(&MindScope::Global) {
            scopes.push(MindScope::Global);
        }
        scopes.truncate(MAX_REFLECTION_SCOPES_PER_TICK);
        let mut processed = 0;
        while processed < MAX_REFLECTIONS_PER_TICK && self.process_next_reflection().await {
            processed += 1;
        }
        for scope in scopes {
            if processed >= MAX_REFLECTIONS_PER_TICK {
                break;
            }
            if !self.reflection_due(scope, now) {
                continue;
            }
            match self.reflection_input(scope, trigger, now).await {
                Ok(Some(input)) => match self.reflection_queue.enqueue(input) {
                    Ok(_) => {
                        if self.process_next_reflection().await {
                            processed += 1;
                        }
                    }
                    Err(error) => {
                        self.metrics
                            .reflection_failures
                            .fetch_add(1, Ordering::Relaxed);
                        kovi::log::warn!("Yunxi Mind reflection enqueue rejected: {error}");
                    }
                },
                Ok(None) => {}
                Err(error) => {
                    self.metrics
                        .reflection_failures
                        .fetch_add(1, Ordering::Relaxed);
                    kovi::log::warn!("Yunxi Mind reflection input failed: {error}");
                }
            }
        }
    }

    async fn process_next_reflection(&self) -> bool {
        let Some(input) = self.reflection_queue.dequeue() else {
            return false;
        };
        if let Err(error) = self.process_reflection(input).await {
            self.metrics
                .reflection_failures
                .fetch_add(1, Ordering::Relaxed);
            kovi::log::warn!("Yunxi Mind reflection failed: {error}");
        }
        true
    }

    pub(crate) async fn proactive_signals(
        &self,
        person_id: yunxi_core::PersonId,
    ) -> anyhow::Result<MindProactiveSignals> {
        if !self.config.enabled() || self.config.influence_mode() == MindInfluenceMode::Disabled {
            return Ok(MindProactiveSignals::default());
        }
        let barrier = self.barrier.read().await;
        if barrier.blocks_origin(Some(person_id), None) {
            return Ok(MindProactiveSignals::default());
        }
        let now = Utc::now();
        let scopes = [MindScope::Global, MindScope::Person { person_id }];
        self.refresh_agenda_for_scopes(&scopes, "", now, TraceContext::root(EventId::new()))
            .await?;
        let agenda = self.services.agenda.list_active(&scopes, now, 8).await?;
        let questions = self.services.open_questions.list_open(&scopes, 8).await?;
        let curiosities = self.services.curiosities.list_open(&scopes, now, 8).await?;
        let cooldown_boundary =
            now - Duration::minutes(self.config.question_cooldown_minutes() as i64);

        let mut best_score = 0.0_f32;
        let mut topic_score = 0.0_f32;
        let mut topic = None;
        let mut reference = None;

        for item in agenda {
            if matches!(
                item.subject(),
                AgendaSubject::Curiosity(_) | AgendaSubject::OpenQuestion(_)
            ) {
                continue;
            }
            let score = item.rank_score();
            best_score = best_score.max(score);
            if score >= topic_score
                && let Some(label) = self.agenda_topic(&item).await
                && safe_proactive_topic(&label)
            {
                topic_score = score;
                topic = Some(label);
                reference = Some(MindProactiveReference::Agenda(item.id()));
                self.reasons
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .last_proactive_kind = Some(item.kind());
            }
        }

        for question in questions {
            if !safe_proactive_topic(question.question())
                || question.updated_at() > cooldown_boundary
                || !self
                    .question_agenda_available(question.id(), question.scope(), now)
                    .await?
            {
                continue;
            }
            best_score = best_score.max(question.salience());
            if question.salience() >= topic_score {
                topic_score = question.salience();
                topic = Some(question.question().to_owned());
                reference = Some(MindProactiveReference::OpenQuestion(question.id()));
                self.reasons
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .last_proactive_kind = Some(yunxi_core::AgendaItemKind::OpenQuestion);
            }
        }
        for curiosity in curiosities {
            if curiosity.status() != CuriosityStatus::Open
                || !safe_proactive_topic(curiosity.question())
                || curiosity.updated_at() > cooldown_boundary
                || !self
                    .curiosity_agenda_available(curiosity.id(), curiosity.scope(), now)
                    .await?
            {
                continue;
            }
            best_score = best_score.max(curiosity.salience());
            if curiosity.salience() >= topic_score {
                topic_score = curiosity.salience();
                topic = Some(curiosity.question().to_owned());
                reference = Some(MindProactiveReference::Curiosity(curiosity.id()));
                self.reasons
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .last_proactive_kind = Some(yunxi_core::AgendaItemKind::Curiosity);
            }
        }
        let projected = MindProactiveSignals {
            salience: (best_score.clamp(0.0, 1.0) * 100.0).round() as u8,
            topic,
            reference,
        };
        if self.config.influence_mode() == MindInfluenceMode::Shadow {
            kovi::log::info!(
                "Yunxi Mind proactive shadow: person_id={} salience={} has_topic={} would_influence={}",
                person_id,
                projected.salience,
                projected.topic.is_some(),
                projected.salience > 0,
            );
            Ok(MindProactiveSignals::default())
        } else {
            Ok(projected)
        }
    }

    async fn agenda_topic(&self, item: &yunxi_core::AgendaItem) -> Option<String> {
        match item.subject() {
            AgendaSubject::OpenLoop(id) => self
                .context_services
                .as_ref()?
                .open_loops
                .get(*id)
                .await
                .ok()
                .flatten()
                .filter(|open_loop| !open_loop.status().is_terminal())
                .map(|open_loop| open_loop.summary().to_owned()),
            AgendaSubject::Goal(id) => self
                .context_services
                .as_ref()?
                .goals
                .get(*id)
                .await
                .ok()
                .flatten()
                .filter(|goal| goal.state() == GoalState::Active)
                .map(|goal| goal.title().to_owned()),
            AgendaSubject::Interest(id) => self
                .services
                .interests
                .get(*id)
                .await
                .ok()
                .flatten()
                .map(|interest| interest.topic().to_owned()),
            AgendaSubject::SalientMemory(id) => self
                .load_v1_context(item.scope(), "")
                .await
                .memories
                .into_iter()
                .find(|memory| memory.id() == *id)
                .map(|memory| memory.content().to_owned()),
            AgendaSubject::SocialMotive(label) => Some(label.clone()),
            AgendaSubject::Curiosity(_)
            | AgendaSubject::OpenQuestion(_)
            | AgendaSubject::UnresolvedConversation(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) async fn proactive_reference_current(
        &self,
        reference: MindProactiveReference,
    ) -> bool {
        self.pin_proactive_reference(reference).await.is_some()
    }

    pub(crate) async fn pin_proactive_reference(
        &self,
        reference: MindProactiveReference,
    ) -> Option<MindDeliveryPermit<'_>> {
        let barrier = self.barrier.read().await;
        let now = Utc::now();
        let current = match reference {
            MindProactiveReference::Curiosity(id) => {
                let curiosity = self
                    .services
                    .curiosities
                    .get(id)
                    .await
                    .ok()
                    .flatten()
                    .filter(|item| {
                        item.status() == CuriosityStatus::Open
                            && item.expires_at().is_none_or(|expires| expires > now)
                    })?;
                if barrier.blocks_origin(curiosity.subject(), curiosity.conversation_id()) {
                    return None;
                }
                self.curiosity_agenda_available(id, curiosity.scope(), now)
                    .await
                    .unwrap_or(false)
            }
            MindProactiveReference::OpenQuestion(id) => {
                let question = self
                    .services
                    .open_questions
                    .get(id)
                    .await
                    .ok()
                    .flatten()
                    .filter(|question| question.status() == OpenQuestionStatus::Open)?;
                if barrier.blocks_scope(question.scope()) {
                    return None;
                }
                self.question_agenda_available(id, question.scope(), now)
                    .await
                    .unwrap_or(false)
            }
            MindProactiveReference::Agenda(id) => {
                let item = self
                    .services
                    .agenda
                    .get(id)
                    .await
                    .ok()
                    .flatten()
                    .filter(|item| item.is_available_at(now))?;
                if barrier.blocks_scope(item.scope()) {
                    return None;
                }
                self.agenda_topic(&item).await.is_some()
            }
        };
        current.then_some(MindDeliveryPermit {
            _barrier: Some(barrier),
        })
    }

    pub(crate) fn mark_proactive_used(self: &Arc<Self>, reference: MindProactiveReference) {
        let runtime = Arc::clone(self);
        kovi::tokio::spawn(async move {
            if let Err(error) = runtime.mark_proactive_used_inner(reference).await {
                kovi::log::warn!("Yunxi Mind proactive state update failed: {error}");
            }
        });
    }

    pub(crate) async fn begin_erasure(
        self: &Arc<Self>,
        person_id: Option<yunxi_core::PersonId>,
        conversation_ids: &[yunxi_core::ConversationId],
    ) -> MindErasureGuard {
        let mut barrier = self.barrier.write().await;
        barrier.add(person_id, conversation_ids);
        let mut scopes = conversation_ids
            .iter()
            .copied()
            .map(|conversation_id| MindScope::Conversation { conversation_id })
            .collect::<Vec<_>>();
        if let Some(person_id) = person_id {
            scopes.push(MindScope::Person { person_id });
        }
        self.reflection_queue.purge_scopes(&scopes);
        self.recent_events
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .purge(&scopes);
        self.last_reflections
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .retain(|scope, _| !scopes.contains(scope));
        self.pending_candidates
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .retain(|pending| {
                person_id != Some(pending.context.person_id)
                    && !conversation_ids.contains(&pending.context.conversation_id)
            });
        self.metrics.erasures.fetch_add(1, Ordering::Relaxed);
        drop(barrier);
        MindErasureGuard {
            runtime: Arc::clone(self),
            person_id,
            conversation_ids: conversation_ids.to_vec(),
            finished: false,
        }
    }

    async fn finish_erasure(
        &self,
        person_id: Option<yunxi_core::PersonId>,
        conversation_ids: &[yunxi_core::ConversationId],
    ) {
        self.barrier
            .write()
            .await
            .remove(person_id, conversation_ids);
    }

    async fn persist_candidates(&self, pending: PendingCandidates) -> anyhow::Result<()> {
        let barrier = self.barrier.read().await;
        if barrier.epoch != pending.erasure_epoch
            || barrier.blocks_origin(
                Some(pending.context.person_id),
                Some(pending.context.conversation_id),
            )
        {
            return Ok(());
        }
        let now = Utc::now().max(pending.context.occurred_at);
        let mut applied = 0_u64;
        let interest_agenda_topic = pending
            .candidates
            .interest
            .as_ref()
            .map(|candidate| candidate.topic.clone());

        let mut global = self
            .empty_proposal(MindScope::Global, now, pending.context.trace)
            .await?;
        if self.config.belief_enabled()
            && let Some(candidate) = pending.candidates.belief
            && safe_global_state_text(&candidate.proposition)
            && self.can_upsert_belief(&candidate.proposition, now).await?
        {
            let confidence_delta = candidate.confidence_delta.clamp(-0.2, 0.2);
            let polarity = if confidence_delta < 0.0 {
                EvidencePolarity::Contradicts
            } else {
                EvidencePolarity::Supports
            };
            let evidence = EvidenceRef::new(
                EvidenceKind::Event(pending.context.event_id),
                polarity,
                0.55,
                pending.context.occurred_at,
            )?;
            global.belief_updates.push(BeliefUpdateProposal {
                operation: BeliefOperation::Upsert,
                belief_id: None,
                expected_version: None,
                scope: MindScope::Global,
                proposition: candidate.proposition,
                confidence_delta,
                stability_delta: 0.02,
                source: BeliefSource::Inference,
                evidence_refs: vec![evidence],
                valid_until: None,
            });
        }
        if self.config.preference_enabled()
            && let Some(candidate) = pending.candidates.preference
            && safe_global_state_text(&candidate.subject)
            && self.can_upsert_preference(&candidate.subject, now).await?
        {
            global.preference_updates.push(PreferenceUpdateProposal {
                operation: PreferenceOperation::Upsert,
                preference_id: None,
                expected_version: None,
                subject: candidate.subject,
                valence_delta: candidate.valence_delta.clamp(-0.1, 0.1),
                intensity_delta: 0.05,
                confidence_delta: 0.05,
                source: PreferenceSource::Experience,
            });
        }
        if self.config.interest_enabled()
            && let Some(candidate) = pending.candidates.interest
            && safe_global_state_text(&candidate.topic)
            && self.can_upsert_interest(&candidate.topic, now).await?
        {
            global.interest_updates.push(InterestUpdateProposal {
                operation: InterestOperation::Upsert,
                interest_id: None,
                expected_version: None,
                topic: candidate.topic,
                activation_delta: 0.2,
                affinity_delta: 0.03,
                novelty: candidate.novelty,
                source: MindSource::Inference,
            });
        }
        if !proposal_is_empty(&global) {
            let belief_count = global.belief_updates.len() as u64;
            let preference_count = global.preference_updates.len() as u64;
            let interest_count = global.interest_updates.len() as u64;
            self.consolidate_retry(global).await?;
            self.metrics
                .belief_updates
                .fetch_add(belief_count, Ordering::Relaxed);
            self.metrics
                .preference_updates
                .fetch_add(preference_count, Ordering::Relaxed);
            self.metrics
                .interest_updates
                .fetch_add(interest_count, Ordering::Relaxed);
            if belief_count > 0 {
                self.reasons
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .last_belief_source = Some(BeliefSource::Inference);
            }
            applied += belief_count + preference_count + interest_count;
        }
        if self.config.agenda_enabled()
            && let Some(topic) = interest_agenda_topic
            && let Some(interest) = self
                .services
                .interests
                .relevant(&topic, 4)
                .await?
                .into_iter()
                .find(|interest| interest.topic().eq_ignore_ascii_case(topic.trim()))
            && self
                .ensure_agenda(
                    MindScope::Global,
                    AgendaSubject::Interest(interest.id()),
                    AgendaSource::Interest,
                    interest
                        .activation()
                        .max(interest.long_term_affinity() * 0.8),
                    0.55,
                    now,
                    pending.context.trace,
                )
                .await?
        {
            self.record_agenda_reason(AgendaSource::Interest);
            applied += 1;
        }

        let scope = pending.context.scoped_state();
        let question = pending
            .candidates
            .open_question
            .filter(|question| safe_scoped_question_text(question));
        let curiosity = if question.is_none() {
            pending
                .candidates
                .curiosity
                .filter(|question| safe_scoped_question_text(question))
        } else {
            None
        };
        if self.config.curiosity_enabled()
            && let Some(question) = curiosity
            && let Some(curiosity) = self
                .create_curiosity(scope, pending.context.person_id, question, now)
                .await?
        {
            applied += 1;
            if self.config.agenda_enabled()
                && self
                    .ensure_agenda(
                        scope,
                        AgendaSubject::Curiosity(curiosity.id()),
                        AgendaSource::Curiosity,
                        curiosity.salience(),
                        0.35,
                        now,
                        pending.context.trace,
                    )
                    .await?
            {
                applied += 1;
            }
        }
        if self.config.curiosity_enabled()
            && let Some(question) = question
            && let Some(open_question) = self
                .create_open_question(scope, question, now, pending.context.trace)
                .await?
        {
            applied += 1;
            if self.config.agenda_enabled()
                && self
                    .ensure_agenda(
                        scope,
                        AgendaSubject::OpenQuestion(open_question.id()),
                        AgendaSource::OpenQuestion,
                        open_question.salience(),
                        0.4,
                        now,
                        pending.context.trace,
                    )
                    .await?
            {
                applied += 1;
            }
        }
        if self.config.agenda_enabled()
            && let Some(label) = pending
                .candidates
                .agenda
                .filter(|label| safe_scoped_question_text(label))
            && self
                .ensure_agenda(
                    scope,
                    AgendaSubject::SocialMotive(label),
                    AgendaSource::Interaction,
                    0.55,
                    0.3,
                    now,
                    pending.context.trace,
                )
                .await?
        {
            applied += 1;
        }

        if applied > 0 {
            self.metrics
                .candidates_applied
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.metrics
                .candidates_rejected
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn resolve_answered_state(
        &self,
        event: &WorldEvent,
        scopes: &[MindScope],
    ) -> anyhow::Result<()> {
        let WorldEventKind::MessageReceived(message) = event.kind() else {
            return Ok(());
        };
        let answer = message.content.as_text();
        if looks_like_question(answer) {
            return Ok(());
        }
        let now = Utc::now().max(event.occurred_at());
        let mut resolved_open_loops = Vec::new();
        if let Some(context_services) = self.context_services.as_ref() {
            for scope in scopes.iter().copied() {
                let context = self.load_v1_context(scope, answer).await;
                for open_loop in context.open_loops.into_iter().filter(|item| {
                    !item.status().is_terminal()
                        && matches!(
                            item.kind(),
                            OpenLoopKind::FollowUp
                                | OpenLoopKind::AwaitingOutcome
                                | OpenLoopKind::PendingQuestion
                        )
                        && answer_matches_question(item.summary(), answer)
                }) {
                    match context_services
                        .open_loops
                        .resolve(open_loop.id(), now)
                        .await
                    {
                        Ok(_) => resolved_open_loops.push(open_loop.id()),
                        Err(error) => {
                            let already_terminal = context_services
                                .open_loops
                                .get(open_loop.id())
                                .await
                                .ok()
                                .flatten()
                                .is_some_and(|current| current.status().is_terminal());
                            if already_terminal {
                                resolved_open_loops.push(open_loop.id());
                            } else {
                                kovi::log::warn!(
                                    "Yunxi Mind answered open-loop resolution failed soft: {error}"
                                );
                            }
                        }
                    }
                }
            }
        }
        let mut resolved_curiosities = Vec::new();
        for curiosity in self.services.curiosities.list_open(scopes, now, 16).await? {
            if answer_matches_question(curiosity.question(), answer)
                && let Ok(updated) = curiosity.transition(CuriosityStatus::Resolved, now)
                && self
                    .services
                    .curiosities
                    .put(&updated, Some(curiosity.version()))
                    .await
                    .is_ok()
            {
                resolved_curiosities.push(curiosity.id());
            }
        }

        let questions = self.services.open_questions.list_open(scopes, 16).await?;
        let agenda = self
            .services
            .agenda
            .list_active(scopes, DateTime::<Utc>::MAX_UTC, 32)
            .await?;
        let mut updates: HashMap<MindScope, ReflectionProposal> = HashMap::new();
        let mut resolved_questions = Vec::new();
        for question in questions {
            if !answer_matches_question(question.question(), answer) {
                continue;
            }
            resolved_questions.push(question.id());
            let proposal = updates.entry(question.scope()).or_insert(
                self.empty_proposal(question.scope(), now, event.trace())
                    .await?,
            );
            proposal
                .open_question_updates
                .push(OpenQuestionUpdateProposal {
                    operation: OpenQuestionOperation::Resolve,
                    question_id: Some(question.id()),
                    expected_version: None,
                    scope: question.scope(),
                    question: question.question().to_owned(),
                    related_beliefs: question.related_beliefs().to_vec(),
                    salience: question.salience(),
                });
        }
        for item in agenda {
            let resolved = match item.subject() {
                AgendaSubject::OpenLoop(id) => resolved_open_loops.contains(id),
                AgendaSubject::Curiosity(id) => resolved_curiosities.contains(id),
                AgendaSubject::OpenQuestion(id) => resolved_questions.contains(id),
                _ => false,
            };
            if !resolved {
                continue;
            }
            let proposal = updates.entry(item.scope()).or_insert(
                self.empty_proposal(item.scope(), now, event.trace())
                    .await?,
            );
            proposal.agenda_updates.push(AgendaUpdateProposal {
                operation: AgendaOperation::Resolve,
                item_id: Some(item.id()),
                expected_version: None,
                scope: item.scope(),
                subject: item.subject().clone(),
                salience: item.salience(),
                activation: item.activation(),
                stability: item.stability(),
                source: item.source(),
                defer_until: None,
            });
        }
        for proposal in updates.into_values() {
            let agenda_count = proposal.agenda_updates.len() as u64;
            self.consolidate_retry(proposal).await?;
            self.metrics
                .agenda_updates
                .fetch_add(agenda_count, Ordering::Relaxed);
        }
        if !resolved_open_loops.is_empty() {
            self.record_agenda_reason(AgendaSource::OpenLoop);
        }
        Ok(())
    }

    async fn reflection_input(
        &self,
        scope: MindScope,
        trigger: ReflectionTrigger,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<ReflectionInput>> {
        let barrier = self.barrier.read().await;
        if barrier.blocks_scope(scope) {
            return Ok(None);
        }
        let observed = self
            .recent_events
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .for_scope(scope);
        let context = self.load_v1_context(scope, "").await;
        self.refresh_agenda_for_scopes(&[scope], "", now, TraceContext::root(EventId::new()))
            .await?;
        let synthetic_scope = match scope {
            MindScope::Global => EventScope::Global,
            MindScope::Person { person_id } => EventScope::Person { person_id },
            MindScope::Conversation { conversation_id } => {
                EventScope::Conversation { conversation_id }
            }
        };
        let event = WorldEvent::new(
            now,
            synthetic_scope,
            EventPriority::Low,
            if trigger == ReflectionTrigger::Idle {
                WorldEventKind::IdleTick
            } else {
                WorldEventKind::MaintenanceTick
            },
        );
        let request = MindSnapshotRequest::for_event(
            &event,
            None,
            self.config.snapshot_limits(),
            self.config.influence_mode(),
        )?;
        let mind = self.snapshot_provider.snapshot(&request).await?;
        let effective_trigger = if trigger == ReflectionTrigger::Idle
            && observed.last().is_some_and(|item| {
                now.signed_duration_since(item.event.occurred_at)
                    >= Duration::minutes(self.config.conversation_end_idle_minutes() as i64)
            }) {
            ReflectionTrigger::ConversationLikelyEnded
        } else {
            trigger
        };
        let collision_count = observed
            .iter()
            .filter(|item| {
                item.event
                    .summary
                    .starts_with("message collision changed pending outgoing")
            })
            .count();
        let deep = observed.iter().any(|item| item.event.salience >= 0.85)
            || collision_count >= 2
            || matches!(
                effective_trigger,
                ReflectionTrigger::HighSalienceEvent
                    | ReflectionTrigger::MemoryPressure
                    | ReflectionTrigger::AgendaPressure
                    | ReflectionTrigger::DayBoundary
            );
        let input = ReflectionInput {
            trigger: effective_trigger,
            depth: if deep {
                ReflectionDepth::Deep
            } else {
                ReflectionDepth::Light
            },
            scope,
            recent_events: observed.iter().map(|item| item.event.clone()).collect(),
            salient_memories: context
                .memories
                .iter()
                .filter(|memory| memory.importance() >= 60)
                .take(16)
                .map(|memory| bounded_summary(memory.content()))
                .collect(),
            open_loop_summaries: context
                .open_loops
                .iter()
                .filter(|item| !item.status().is_terminal())
                .take(16)
                .map(|item| bounded_summary(item.summary()))
                .collect(),
            goal_summaries: context
                .goals
                .iter()
                .filter(|goal| goal.state() == GoalState::Active)
                .take(16)
                .map(|goal| bounded_summary(goal.title()))
                .collect(),
            mind,
            requested_at: now,
            trace: observed.last().map_or(event.trace(), |item| item.trace),
        };
        input.validate()?;
        drop(barrier);
        Ok(input.should_reflect().then_some(input))
    }

    async fn process_reflection(&self, input: ReflectionInput) -> anyhow::Result<()> {
        let barrier = self.barrier.read().await;
        if barrier.blocks_scope(input.scope) {
            return Ok(());
        }
        let current_version = self.services.consolidation.current_version().await?;
        if current_version != input.mind.version() {
            kovi::log::info!(
                "Yunxi Mind stale reflection discarded: scope={:?} expected_version={} actual_version={}",
                input.scope,
                input.mind.version(),
                current_version,
            );
            return Ok(());
        }
        let mut proposal = ReflectionProposal::empty(&input);
        proposal
            .reason_tags
            .push(MindReasonTag::ReflectionConsolidation);
        let max_event_salience = input
            .recent_events
            .iter()
            .map(|event| event.salience)
            .fold(0.0_f32, f32::max);
        let should_create_episode = !input.recent_events.is_empty()
            && (input.depth == ReflectionDepth::Deep
                || max_event_salience >= 0.7
                || input.recent_events.len() >= 2);
        if should_create_episode {
            let summary_event_limit = if input.depth == ReflectionDepth::Deep {
                6
            } else {
                3
            };
            let source_events = input
                .recent_events
                .iter()
                .map(|event| event.event_id)
                .take(if input.depth == ReflectionDepth::Deep {
                    MAX_EPISODE_SOURCE_EVENTS
                } else {
                    MAX_EPISODE_SOURCE_EVENTS / 2
                })
                .collect::<Vec<_>>();
            let summary = bounded_summary(
                &input
                    .recent_events
                    .iter()
                    .rev()
                    .take(summary_event_limit)
                    .map(|event| event.summary.as_str())
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
            let participants = input.scope.person_id().into_iter().collect::<Vec<_>>();
            proposal.episodes.push(Episode::new(
                EpisodeId::new(),
                input.scope,
                participants,
                source_events,
                summary,
                max_event_salience,
                0.0,
                !input.mind.open_questions().is_empty()
                    || !input.mind.agenda().is_empty()
                    || !input.open_loop_summaries.is_empty()
                    || !input.goal_summaries.is_empty(),
                MindSource::Reflection,
                input
                    .recent_events
                    .last()
                    .map_or(input.requested_at, |event| event.occurred_at),
                input.requested_at,
            )?);
        }
        if self.config.interest_enabled() {
            let decay_limit = if input.depth == ReflectionDepth::Deep {
                MAX_REFLECTION_DECAYS
            } else {
                MAX_REFLECTION_DECAYS / 2
            };
            for interest in input.mind.interests().iter().take(decay_limit) {
                proposal.interest_updates.push(InterestUpdateProposal {
                    operation: InterestOperation::Decay,
                    interest_id: Some(interest.id),
                    expected_version: Some(interest.version),
                    topic: interest.topic.clone(),
                    activation_delta: 0.0,
                    affinity_delta: 0.0,
                    novelty: interest.novelty,
                    source: MindSource::Reflection,
                });
            }
        }
        if !proposal_is_empty(&proposal) {
            self.consolidate_retry(proposal.clone()).await?;
        }
        if self.config.agenda_enabled() {
            let half_life = self.config.agenda_half_life_hours() as f64 * 60.0 * 60.0;
            for item in self
                .services
                .agenda
                .list_active(&[input.scope], input.requested_at, MAX_REFLECTION_DECAYS)
                .await?
            {
                let decayed = item.decay(input.requested_at, half_life)?;
                self.services
                    .agenda
                    .put(&decayed, Some(item.version()))
                    .await?;
            }
        }
        self.metrics.reflections.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .last_reflection_unix_ms
            .store(input.requested_at.timestamp_millis(), Ordering::Relaxed);
        self.record_reflection_success(input.scope, input.requested_at);
        self.recent_events
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .remove_through(input.scope, input.requested_at);
        kovi::log::info!(
            "Yunxi Mind reflection: scope={:?} trigger={:?} depth={:?} events={} memories={} open_loops={} goals={} episodes={} extra_model_calls=0",
            input.scope,
            input.trigger,
            input.depth,
            input.recent_events.len(),
            input.salient_memories.len(),
            input.open_loop_summaries.len(),
            input.goal_summaries.len(),
            proposal.episodes.len(),
        );
        Ok(())
    }

    fn reflection_due(&self, scope: MindScope, now: DateTime<Utc>) -> bool {
        self.last_reflections
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .get(&scope)
            .is_none_or(|last| {
                now.signed_duration_since(*last)
                    >= Duration::minutes(self.config.reflection_min_interval_minutes() as i64)
            })
    }

    fn record_reflection_success(&self, scope: MindScope, at: DateTime<Utc>) {
        let mut reflections = self
            .last_reflections
            .lock()
            .unwrap_or_else(|lock| lock.into_inner());
        if !reflections.contains_key(&scope)
            && reflections.len() >= MAX_TRACKED_SCOPES
            && let Some(oldest) = reflections
                .iter()
                .min_by_key(|(_, reflected_at)| **reflected_at)
                .map(|(scope, _)| *scope)
        {
            reflections.remove(&oldest);
        }
        reflections.insert(scope, at);
    }

    async fn empty_proposal(
        &self,
        scope: MindScope,
        proposed_at: DateTime<Utc>,
        trace: TraceContext,
    ) -> Result<ReflectionProposal, yunxi_core::MindStoreError> {
        Ok(ReflectionProposal {
            base_snapshot_version: self.services.consolidation.current_version().await?,
            scope,
            episodes: Vec::new(),
            belief_updates: Vec::new(),
            preference_updates: Vec::new(),
            interest_updates: Vec::new(),
            open_question_updates: Vec::new(),
            agenda_updates: Vec::new(),
            reason_tags: Vec::new(),
            proposed_at,
            trace,
        })
    }

    async fn consolidate_retry(
        &self,
        mut proposal: ReflectionProposal,
    ) -> Result<(), ConsolidationError> {
        for attempt in 0..2 {
            match self
                .consolidation
                .consolidate(&self.services, &proposal)
                .await
            {
                Ok(_) => return Ok(()),
                Err(ConsolidationError::StaleSnapshot { .. }) if attempt == 0 => {
                    proposal.base_snapshot_version =
                        self.services.consolidation.current_version().await?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded consolidation retry returns from each branch")
    }

    async fn can_upsert_belief(
        &self,
        proposition: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let draft = Belief::new(
            BeliefId::new(),
            MindScope::Global,
            proposition,
            0.5,
            0.25,
            BeliefSource::Inference,
            Vec::new(),
            None,
            now,
        )?;
        if self
            .services
            .beliefs
            .find_by_key(MindScope::Global, draft.proposition_key())
            .await?
            .is_some()
        {
            return Ok(true);
        }
        Ok(self
            .services
            .beliefs
            .relevant(
                &[MindScope::Global],
                "",
                now,
                self.config.max_learned_beliefs_per_scope(),
            )
            .await?
            .len()
            < self.config.max_learned_beliefs_per_scope())
    }

    async fn can_upsert_preference(
        &self,
        subject: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let draft = Preference::new(
            yunxi_core::PreferenceId::new(),
            subject,
            0.0,
            0.1,
            0.4,
            0.25,
            PreferenceSource::Experience,
            now,
        )?;
        if self
            .services
            .preferences
            .find_by_key(draft.subject_key())
            .await?
            .is_some()
        {
            return Ok(true);
        }
        Ok(self
            .services
            .preferences
            .relevant("", self.config.max_preferences())
            .await?
            .len()
            < self.config.max_preferences())
    }

    async fn can_upsert_interest(&self, topic: &str, now: DateTime<Utc>) -> anyhow::Result<bool> {
        let draft = Interest::new(
            yunxi_core::InterestId::new(),
            topic,
            0.2,
            0.03,
            0.5,
            MindSource::Inference,
            now,
        )?;
        if self
            .services
            .interests
            .find_by_key(draft.topic_key())
            .await?
            .is_some()
        {
            return Ok(true);
        }
        Ok(self
            .services
            .interests
            .relevant("", self.config.max_interests())
            .await?
            .len()
            < self.config.max_interests())
    }

    async fn create_curiosity(
        &self,
        scope: MindScope,
        person_id: yunxi_core::PersonId,
        question: String,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<CuriosityItem>> {
        let (subject, conversation_id) = match scope {
            MindScope::Person { .. } => (Some(person_id), None),
            MindScope::Conversation { conversation_id } => (None, Some(conversation_id)),
            MindScope::Global => (None, None),
        };
        let draft = CuriosityItem::new(
            CuriosityId::new(),
            question,
            subject,
            conversation_id,
            0.55,
            now,
            Some(now + Duration::days(CURIOSITY_TTL_DAYS)),
        )?;
        if let Some(existing) = self
            .services
            .curiosities
            .find_open_by_key(scope, draft.question_key())
            .await?
        {
            return Ok(Some(existing));
        }
        let open = self
            .services
            .curiosities
            .list_open(&[scope], now, self.config.max_curiosity_per_person())
            .await?;
        if open.len() >= self.config.max_curiosity_per_person()
            || open.iter().any(|item| {
                now.signed_duration_since(item.updated_at())
                    < Duration::minutes(self.config.question_cooldown_minutes() as i64)
            })
        {
            return Ok(None);
        }
        let stored = self.services.curiosities.put(&draft, None).await?;
        Ok(Some(stored))
    }

    async fn create_open_question(
        &self,
        scope: MindScope,
        question: String,
        now: DateTime<Utc>,
        trace: TraceContext,
    ) -> anyhow::Result<Option<OpenQuestion>> {
        let draft =
            OpenQuestion::new(OpenQuestionId::new(), scope, question, Vec::new(), 0.6, now)?;
        if let Some(existing) = self
            .services
            .open_questions
            .find_open_by_key(scope, draft.question_key())
            .await?
        {
            return Ok(Some(existing));
        }
        let open = self
            .services
            .open_questions
            .list_open(&[scope], self.config.max_open_questions_per_scope())
            .await?;
        if open.len() >= self.config.max_open_questions_per_scope()
            || open.iter().any(|item| {
                now.signed_duration_since(item.updated_at())
                    < Duration::minutes(self.config.question_cooldown_minutes() as i64)
            })
        {
            return Ok(None);
        }
        let mut proposal = self.empty_proposal(scope, now, trace).await?;
        proposal
            .open_question_updates
            .push(OpenQuestionUpdateProposal {
                operation: OpenQuestionOperation::Upsert,
                question_id: None,
                expected_version: None,
                scope,
                question: draft.question().to_owned(),
                related_beliefs: Vec::new(),
                salience: draft.salience(),
            });
        self.consolidate_retry(proposal).await?;
        self.services
            .open_questions
            .find_open_by_key(scope, draft.question_key())
            .await
            .map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    async fn ensure_agenda(
        &self,
        scope: MindScope,
        subject: AgendaSubject,
        source: AgendaSource,
        salience: f32,
        stability: f32,
        now: DateTime<Utc>,
        trace: TraceContext,
    ) -> anyhow::Result<bool> {
        if self
            .services
            .agenda
            .find_active_by_key(scope, &subject.dedupe_key())
            .await?
            .is_some()
        {
            return Ok(false);
        }
        if self
            .services
            .agenda
            .list_active(
                &[scope],
                DateTime::<Utc>::MAX_UTC,
                self.config.max_agenda_for_scope(scope),
            )
            .await?
            .len()
            >= self.config.max_agenda_for_scope(scope)
        {
            return Ok(false);
        }
        let mut proposal = self.empty_proposal(scope, now, trace).await?;
        proposal.agenda_updates.push(AgendaUpdateProposal {
            operation: AgendaOperation::Activate,
            item_id: None,
            expected_version: None,
            scope,
            subject,
            salience,
            activation: salience,
            stability,
            source,
            defer_until: None,
        });
        self.consolidate_retry(proposal).await?;
        self.metrics.agenda_updates.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    async fn question_agenda_available(
        &self,
        question_id: OpenQuestionId,
        scope: MindScope,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        Ok(self
            .services
            .agenda
            .find_active_by_key(
                scope,
                &AgendaSubject::OpenQuestion(question_id).dedupe_key(),
            )
            .await?
            .is_some_and(|item| item.is_available_at(now)))
    }

    async fn curiosity_agenda_available(
        &self,
        curiosity_id: CuriosityId,
        scope: MindScope,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        Ok(self
            .services
            .agenda
            .find_active_by_key(scope, &AgendaSubject::Curiosity(curiosity_id).dedupe_key())
            .await?
            .is_some_and(|item| item.is_available_at(now)))
    }

    async fn mark_proactive_used_inner(
        &self,
        reference: MindProactiveReference,
    ) -> anyhow::Result<()> {
        let now = Utc::now();
        // Pin the erasure epoch through the read and write. If deletion has
        // already started, the stored barrier rejects the late delivery
        // callback; if deletion starts later, it waits and then removes this
        // update with the rest of the scope.
        let barrier = self.barrier.read().await;
        match reference {
            MindProactiveReference::Curiosity(id) => {
                if let Some(item) = self.services.curiosities.get(id).await?
                    && item.status() == CuriosityStatus::Open
                    && !barrier.blocks_origin(item.subject(), item.conversation_id())
                {
                    let updated = item.transition(CuriosityStatus::Asked, now)?;
                    self.services
                        .curiosities
                        .put(&updated, Some(item.version()))
                        .await?;
                }
            }
            MindProactiveReference::OpenQuestion(id) => {
                let Some(question) = self.services.open_questions.get(id).await? else {
                    return Ok(());
                };
                if barrier.blocks_scope(question.scope()) {
                    return Ok(());
                }
                let key = AgendaSubject::OpenQuestion(id).dedupe_key();
                if let Some(item) = self
                    .services
                    .agenda
                    .find_active_by_key(question.scope(), &key)
                    .await?
                {
                    let updated = item.with_cooldown(
                        Some(
                            now + Duration::minutes(self.config.question_cooldown_minutes() as i64),
                        ),
                        now,
                    )?;
                    self.services
                        .agenda
                        .put(&updated, Some(item.version()))
                        .await?;
                }
            }
            MindProactiveReference::Agenda(id) => {
                if let Some(item) = self.services.agenda.get(id).await?
                    && !item.status().is_terminal()
                {
                    if barrier.blocks_scope(item.scope()) {
                        return Ok(());
                    }
                    let updated = item.with_cooldown(
                        Some(
                            now + Duration::minutes(self.config.question_cooldown_minutes() as i64),
                        ),
                        now,
                    )?;
                    self.services
                        .agenda
                        .put(&updated, Some(item.version()))
                        .await?;
                }
            }
        }
        self.metrics.proactive_uses.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn mark_decision_used_inner(
        &self,
        reference: MindDecisionReference,
    ) -> anyhow::Result<()> {
        match reference {
            MindDecisionReference::Agenda(id) => {
                self.mark_proactive_used_inner(MindProactiveReference::Agenda(id))
                    .await
            }
            MindDecisionReference::OpenQuestion(id) => {
                self.mark_proactive_used_inner(MindProactiveReference::OpenQuestion(id))
                    .await
            }
            MindDecisionReference::Interest(id) => {
                let now = Utc::now();
                let barrier = self.barrier.read().await;
                let subject = AgendaSubject::Interest(id);
                if let Some(item) = self
                    .services
                    .agenda
                    .find_active_by_key(MindScope::Global, &subject.dedupe_key())
                    .await?
                    && !barrier.blocks_scope(item.scope())
                {
                    let updated = item.with_cooldown(
                        Some(
                            now + Duration::minutes(self.config.question_cooldown_minutes() as i64),
                        ),
                        now,
                    )?;
                    self.services
                        .agenda
                        .put(&updated, Some(item.version()))
                        .await?;
                }
                Ok(())
            }
        }
    }
}

impl MindSnapshotProvider for MindRuntime {
    fn snapshot<'a>(&'a self, request: &'a MindSnapshotRequest) -> MindSnapshotFuture<'a> {
        Box::pin(async move {
            let barrier = self.barrier.read().await;
            if barrier.blocks_origin(request.person_id(), request.conversation_id())
                || request
                    .scopes()
                    .iter()
                    .any(|scope| barrier.blocks_scope(*scope))
            {
                self.metrics
                    .blocked_snapshots
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(MindSnapshot::empty());
            }
            let started = Instant::now();
            let result = self.snapshot_provider.snapshot(request).await;
            let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            self.metrics
                .snapshot_requests
                .fetch_add(1, Ordering::Relaxed);
            self.metrics
                .snapshot_latency_total_micros
                .fetch_add(elapsed, Ordering::Relaxed);
            self.metrics
                .snapshot_latency_max_micros
                .fetch_max(elapsed, Ordering::Relaxed);
            self.metrics
                .snapshot_latency_last_micros
                .store(elapsed, Ordering::Relaxed);
            result
        })
    }
}

#[derive(Debug)]
pub(crate) struct MindErasureGuard {
    runtime: Arc<MindRuntime>,
    person_id: Option<yunxi_core::PersonId>,
    conversation_ids: Vec<yunxi_core::ConversationId>,
    finished: bool,
}

impl MindErasureGuard {
    pub(crate) async fn finish(mut self) {
        self.runtime
            .finish_erasure(self.person_id, &self.conversation_ids)
            .await;
        self.finished = true;
    }
}

impl Drop for MindErasureGuard {
    fn drop(&mut self) {
        if !self.finished {
            kovi::log::warn!(
                "Yunxi Mind erasure guard dropped unfinished; affected scopes remain fail-closed"
            );
        }
    }
}

fn proposal_is_empty(proposal: &ReflectionProposal) -> bool {
    proposal.episodes.is_empty()
        && proposal.belief_updates.is_empty()
        && proposal.preference_updates.is_empty()
        && proposal.interest_updates.is_empty()
        && proposal.open_question_updates.is_empty()
        && proposal.agenda_updates.is_empty()
}

fn mind_snapshot_signature(snapshot: &MindSnapshot) -> [u8; 32] {
    let mut entries = Vec::new();
    if let Some(model) = snapshot.self_model() {
        entries.push(format!("self:{}", model.version()));
    }
    entries.extend(
        snapshot
            .beliefs()
            .iter()
            .map(|item| format!("belief:{}:{}", item.id, item.version)),
    );
    entries.extend(
        snapshot
            .preferences()
            .iter()
            .map(|item| format!("preference:{}:{}", item.id, item.version)),
    );
    entries.extend(
        snapshot
            .interests()
            .iter()
            .map(|item| format!("interest:{}:{}", item.id, item.version)),
    );
    entries.extend(
        snapshot
            .open_questions()
            .iter()
            .map(|item| format!("question:{}:{}", item.id, item.version)),
    );
    entries.extend(
        snapshot
            .agenda()
            .iter()
            .map(|item| format!("agenda:{}:{}", item.id, item.version)),
    );
    entries.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(format!("mode:{:?}", snapshot.influence_mode()));
    for entry in entries {
        hasher.update([0]);
        hasher.update(entry.as_bytes());
    }
    hasher.finalize().into()
}

fn message_salience(message: &yunxi_core::MessageReceivedEvent) -> f32 {
    if message.stop_requested {
        1.0
    } else if message.explicit_request || message.replies_to_agent {
        0.8
    } else if message.addressed_to_agent {
        0.7
    } else {
        0.35
    }
}

fn bounded_summary(value: &str) -> String {
    let value = value.trim();
    let mut summary = value.chars().take(512).collect::<String>();
    if summary.is_empty() {
        summary.push_str("non-text conversation event");
    }
    summary
}

fn safe_global_state_text(value: &str) -> bool {
    let normalized = value.trim().to_lowercase();
    !normalized.is_empty()
        && !normalized
            .chars()
            .any(|character| character.is_ascii_digit())
        && ![
            "你",
            "用户",
            "他",
            "她",
            "qq",
            "手机号",
            "住址",
            "身份证",
            "政治",
            "宗教",
            "性取向",
            "疾病",
            "诊断",
            "密码",
            "token",
            "secret",
        ]
        .iter()
        .any(|marker| normalized.contains(marker))
}

fn safe_scoped_question_text(value: &str) -> bool {
    let normalized = value.trim().to_lowercase();
    !normalized.is_empty()
        && ![
            "手机号",
            "住址",
            "身份证",
            "银行卡",
            "政治",
            "宗教",
            "性取向",
            "疾病",
            "诊断",
            "密码",
            "token",
            "secret",
        ]
        .iter()
        .any(|marker| normalized.contains(marker))
}

fn safe_proactive_topic(value: &str) -> bool {
    safe_scoped_question_text(value) && value.len() <= 1_024 && value.chars().count() <= 256
}

fn looks_like_question(value: &str) -> bool {
    let value = value.trim().to_lowercase();
    value.contains('?')
        || value.contains('？')
        || value.ends_with('吗')
        || value.ends_with('呢')
        || [
            "为什么",
            "怎么",
            "如何",
            "what",
            "why",
            "how",
            "when",
            "where",
        ]
        .iter()
        .any(|marker| value.contains(marker))
}

fn answer_matches_question(question: &str, answer: &str) -> bool {
    if answer.trim().chars().count() < 3 || looks_like_question(answer) {
        return false;
    }
    let question_terms = semantic_terms(question);
    let answer_terms = semantic_terms(answer);
    question_terms
        .iter()
        .any(|term| answer_terms.contains(term))
}

fn semantic_terms(value: &str) -> std::collections::HashSet<String> {
    let normalized = value
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&character) {
                character
            } else {
                ' '
            }
        })
        .collect::<String>();
    let mut terms = normalized
        .split_whitespace()
        .filter(|term| {
            term.chars()
                .all(|character| character.is_ascii_alphanumeric())
        })
        .filter(|term| term.chars().count() >= 3)
        .map(ToOwned::to_owned)
        .collect::<std::collections::HashSet<_>>();
    let chinese = normalized
        .chars()
        .filter(|character| ('\u{4e00}'..='\u{9fff}').contains(character))
        .collect::<Vec<_>>();
    for pair in chinese.windows(2) {
        let term = pair.iter().collect::<String>();
        if is_informative_chinese_term(&term) {
            terms.insert(term);
        }
    }
    terms
}

fn is_informative_chinese_term(term: &str) -> bool {
    !matches!(
        term,
        "今天"
            | "明天"
            | "昨天"
            | "最近"
            | "现在"
            | "刚才"
            | "这次"
            | "那次"
            | "这个"
            | "那个"
            | "这些"
            | "那些"
            | "怎么"
            | "什么"
            | "为什么"
            | "了吗"
            | "如何"
            | "怎样"
            | "哪里"
            | "哪个"
            | "可以"
            | "觉得"
            | "感觉"
            | "一下"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use yunxi_core::{
        AgendaItem, AgendaStore, BeliefStore, ConversationId, ConversationKind, CuriosityStore,
        EpisodeStore, GoalStoreFuture, InMemoryMindStore, InterestStore, MemoryDraft, MemoryId,
        MemoryStoreError, MemoryStoreFuture, MessageCollisionDetectedEvent, MessageContent,
        MessageId, MessageReceivedEvent, MindDataErasure, MindSnapshotLimits, OpenLoopDraft,
        OpenLoopStatus, OpenLoopStoreError, OpenLoopStoreFuture, OpenQuestionStore,
        PlannerStateSnapshot, PreferenceStore,
    };

    struct EmptyMemoryStore;

    impl MemoryStore for EmptyMemoryStore {
        fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
            Box::pin(async move {
                Memory::from_draft(MemoryId::new(), draft, Utc::now()).map_err(|error| {
                    MemoryStoreError::InvalidRequest {
                        reason: error.to_string(),
                    }
                })
            })
        }

        fn recall<'a>(&'a self, _query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn forget(&self, _scope: MemoryScope, _id: MemoryId) -> MemoryStoreFuture<'_, bool> {
            Box::pin(async { Ok(false) })
        }
    }

    struct BlockingRecallStore {
        entered: Mutex<Option<kovi::tokio::sync::oneshot::Sender<()>>>,
        release: Arc<kovi::tokio::sync::Semaphore>,
    }

    impl MemoryStore for BlockingRecallStore {
        fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
            Box::pin(async move {
                Memory::from_draft(MemoryId::new(), draft, Utc::now()).map_err(|error| {
                    MemoryStoreError::InvalidRequest {
                        reason: error.to_string(),
                    }
                })
            })
        }

        fn recall<'a>(&'a self, _query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>> {
            let entered = self
                .entered
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .take();
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                if let Some(entered) = entered {
                    let _ = entered.send(());
                    release
                        .acquire()
                        .await
                        .expect("test recall semaphore remains open")
                        .forget();
                }
                Ok(Vec::new())
            })
        }

        fn forget(&self, _scope: MemoryScope, _id: MemoryId) -> MemoryStoreFuture<'_, bool> {
            Box::pin(async { Ok(false) })
        }
    }

    struct EmptyGoalStore;

    impl GoalStore for EmptyGoalStore {
        fn get(&self, _id: yunxi_core::GoalId) -> GoalStoreFuture<'_, Option<Goal>> {
            Box::pin(async { Ok(None) })
        }

        fn list<'a>(
            &'a self,
            _owner: &'a GoalOwner,
            _limit: usize,
        ) -> GoalStoreFuture<'a, Vec<Goal>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[derive(Default)]
    struct StatefulOpenLoopStore {
        items: Mutex<HashMap<yunxi_core::OpenLoopId, OpenLoop>>,
    }

    impl StatefulOpenLoopStore {
        fn with_item(item: OpenLoop) -> Self {
            Self {
                items: Mutex::new(HashMap::from([(item.id(), item)])),
            }
        }

        fn transition(
            &self,
            id: yunxi_core::OpenLoopId,
            status: OpenLoopStatus,
            now: DateTime<Utc>,
        ) -> Result<OpenLoop, OpenLoopStoreError> {
            let mut items = self.items.lock().unwrap_or_else(|lock| lock.into_inner());
            let current = items
                .get(&id)
                .cloned()
                .ok_or(OpenLoopStoreError::NotFound { id })?;
            let updated = current.transition(status, now).map_err(|error| {
                OpenLoopStoreError::InvalidRequest {
                    reason: error.to_string(),
                }
            })?;
            items.insert(id, updated.clone());
            Ok(updated)
        }
    }

    impl OpenLoopStore for StatefulOpenLoopStore {
        fn create<'a>(&'a self, draft: &'a OpenLoopDraft) -> OpenLoopStoreFuture<'a, OpenLoop> {
            Box::pin(async move {
                let item = OpenLoop::from_draft(yunxi_core::OpenLoopId::new(), draft, Utc::now())
                    .map_err(|error| OpenLoopStoreError::InvalidRequest {
                    reason: error.to_string(),
                })?;
                self.items
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .insert(item.id(), item.clone());
                Ok(item)
            })
        }

        fn get<'a>(
            &'a self,
            id: yunxi_core::OpenLoopId,
        ) -> OpenLoopStoreFuture<'a, Option<OpenLoop>> {
            Box::pin(async move {
                Ok(self
                    .items
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .get(&id)
                    .cloned())
            })
        }

        fn list<'a>(
            &'a self,
            owner: &'a OpenLoopOwner,
            limit: usize,
        ) -> OpenLoopStoreFuture<'a, Vec<OpenLoop>> {
            Box::pin(async move {
                Ok(self
                    .items
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .values()
                    .filter(|item| item.owner() == *owner)
                    .take(limit)
                    .cloned()
                    .collect())
            })
        }

        fn claim_due(
            &self,
            _now: DateTime<Utc>,
            _limit: usize,
        ) -> OpenLoopStoreFuture<'_, Vec<OpenLoop>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn defer(
            &self,
            id: yunxi_core::OpenLoopId,
            _due_at: Option<DateTime<Utc>>,
            _now: DateTime<Utc>,
        ) -> OpenLoopStoreFuture<'_, OpenLoop> {
            Box::pin(async move {
                self.items
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .get(&id)
                    .cloned()
                    .ok_or(OpenLoopStoreError::NotFound { id })
            })
        }

        fn resolve(
            &self,
            id: yunxi_core::OpenLoopId,
            now: DateTime<Utc>,
        ) -> OpenLoopStoreFuture<'_, OpenLoop> {
            Box::pin(async move { self.transition(id, OpenLoopStatus::Resolved, now) })
        }

        fn cancel(
            &self,
            id: yunxi_core::OpenLoopId,
            now: DateTime<Utc>,
        ) -> OpenLoopStoreFuture<'_, OpenLoop> {
            Box::pin(async move { self.transition(id, OpenLoopStatus::Cancelled, now) })
        }

        fn recover_stale_triggered(
            &self,
            _now: DateTime<Utc>,
            _limit: usize,
        ) -> OpenLoopStoreFuture<'_, usize> {
            Box::pin(async { Ok(0) })
        }
    }

    fn test_runtime() -> (Arc<MindRuntime>, Arc<InMemoryMindStore>) {
        let store = Arc::new(InMemoryMindStore::new());
        let services = MindServices::from_store(Arc::clone(&store));
        let runtime = Arc::new(
            MindRuntime::new(services, MindConfig::default()).expect("valid test Mind runtime"),
        );
        (runtime, store)
    }

    fn active_config() -> MindConfig {
        serde_json::from_value(serde_json::json!({"influence_mode": "active"}))
            .expect("active Mind config")
    }

    fn active_test_runtime() -> (Arc<MindRuntime>, Arc<InMemoryMindStore>) {
        let store = Arc::new(InMemoryMindStore::new());
        let services = MindServices::from_store(Arc::clone(&store));
        let runtime = Arc::new(
            MindRuntime::new(services, active_config()).expect("valid active Mind runtime"),
        );
        (runtime, store)
    }

    fn active_runtime_with_open_loops(
        open_loops: Arc<StatefulOpenLoopStore>,
    ) -> (Arc<MindRuntime>, Arc<InMemoryMindStore>) {
        let store = Arc::new(InMemoryMindStore::new());
        let services = MindServices::from_store(Arc::clone(&store));
        let context = MindContextServices::new(
            Arc::new(EmptyMemoryStore),
            open_loops,
            Arc::new(EmptyGoalStore),
        );
        let runtime = Arc::new(
            MindRuntime::new(services, active_config())
                .expect("valid active Mind runtime")
                .with_context_services(context),
        );
        (runtime, store)
    }

    fn direct_message(
        person_id: yunxi_core::PersonId,
        conversation_id: ConversationId,
        content: &str,
    ) -> WorldEvent {
        WorldEvent::message_received(
            EventPriority::High,
            MessageReceivedEvent {
                message_id: MessageId::new(),
                conversation_id,
                sender: person_id,
                content: MessageContent::text(content),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind: ConversationKind::Direct,
                addressed_to_agent: true,
                replies_to_agent: false,
                stop_requested: false,
                explicit_request: true,
                visible_reply_allowed: true,
            },
        )
    }

    fn casual_direct_message(
        person_id: yunxi_core::PersonId,
        conversation_id: ConversationId,
        content: &str,
    ) -> WorldEvent {
        let WorldEventKind::MessageReceived(mut message) =
            direct_message(person_id, conversation_id, content)
                .kind()
                .clone()
        else {
            unreachable!()
        };
        message.addressed_to_agent = false;
        message.explicit_request = false;
        WorldEvent::message_received(EventPriority::Normal, message)
    }

    async fn active_planner_input(runtime: &MindRuntime, event: WorldEvent) -> PlannerInput {
        let request = MindSnapshotRequest::for_event(
            &event,
            None,
            runtime.config().snapshot_limits(),
            MindInfluenceMode::Active,
        )
        .expect("active snapshot request");
        let snapshot = MindSnapshotProvider::snapshot(runtime, &request)
            .await
            .expect("active snapshot");
        assert!(!snapshot.is_empty());
        assert_eq!(snapshot.influence_mode(), MindInfluenceMode::Active);
        PlannerInput::new(event, PlannerStateSnapshot::empty()).with_mind(snapshot)
    }

    fn candidate_context(event: &WorldEvent) -> MindCandidateContext {
        let WorldEventKind::MessageReceived(message) = event.kind() else {
            panic!("test event must be a message");
        };
        MindCandidateContext {
            person_id: message.sender,
            conversation_id: message.conversation_id,
            conversation_kind: message.conversation_kind,
            event_id: event.id(),
            occurred_at: event.occurred_at(),
            trace: event.trace(),
        }
    }

    #[test]
    fn answer_matching_is_conservative_but_handles_chinese_topics() {
        assert!(answer_matches_question(
            "你今天面试怎么样？",
            "我面试过了，结果还不错"
        ));
        assert!(!answer_matches_question(
            "你今天面试怎么样？",
            "你觉得面试怎么样？"
        ));
        assert!(!answer_matches_question(
            "你今天面试怎么样？",
            "今天天气很好"
        ));
    }

    #[test]
    fn global_candidate_filter_rejects_person_and_sensitive_inferences() {
        assert!(safe_global_state_text("我认为诚实比迎合更重要"));
        assert!(!safe_global_state_text("用户 123 患有某种疾病"));
        assert!(!safe_global_state_text("我觉得你更喜欢安静"));
        assert!(safe_scoped_question_text("你今天面试结果怎么样？"));
        assert!(!safe_scoped_question_text("想问用户的政治立场"));
    }

    #[test]
    fn negative_belief_candidate_records_contradicting_evidence() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let proposition = "静态类型总能提高开发效率";
            let supporting_event = direct_message(person_id, conversation_id, "最初我赞同这个观点");
            runtime
                .persist_candidates(PendingCandidates {
                    idempotency_key: "supporting-belief".to_string(),
                    context: candidate_context(&supporting_event),
                    candidates: MindCandidates {
                        belief: Some(MindBeliefCandidate {
                            proposition: proposition.to_string(),
                            confidence_delta: 0.2,
                        }),
                        ..MindCandidates::default()
                    },
                    erasure_epoch: 0,
                    registered_at: Utc::now(),
                })
                .await
                .expect("persist supporting belief");
            let before = BeliefStore::relevant(
                store.as_ref(),
                &[MindScope::Global],
                proposition,
                Utc::now(),
                1,
            )
            .await
            .expect("belief query")
            .into_iter()
            .next()
            .expect("stored belief");

            let contradicting_event =
                direct_message(person_id, conversation_id, "后来我发现这个观点并不总成立");
            runtime
                .persist_candidates(PendingCandidates {
                    idempotency_key: "contradicting-belief".to_string(),
                    context: candidate_context(&contradicting_event),
                    candidates: MindCandidates {
                        belief: Some(MindBeliefCandidate {
                            proposition: proposition.to_string(),
                            confidence_delta: -0.2,
                        }),
                        ..MindCandidates::default()
                    },
                    erasure_epoch: 0,
                    registered_at: Utc::now(),
                })
                .await
                .expect("persist contradicting belief");

            let updated = BeliefStore::get(store.as_ref(), before.id())
                .await
                .expect("belief lookup")
                .expect("updated belief");
            assert!(updated.confidence() < before.confidence());
            assert_eq!(updated.contradiction_count(), 1);
            assert!(updated.evidence_refs().iter().any(|evidence| {
                evidence.kind() == EvidenceKind::Event(contradicting_event.id())
                    && evidence.polarity() == EvidencePolarity::Contradicts
            }));
        });
    }

    #[test]
    fn erasure_purges_pending_candidates_before_they_can_commit() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let event = direct_message(person_id, conversation_id, "我最近在学 Rust");
            assert!(runtime.register_candidates(
                "pending-reply".to_string(),
                candidate_context(&event),
                MindCandidates {
                    interest: Some(MindInterestCandidate {
                        topic: "Rust".to_string(),
                        novelty: 0.8,
                    }),
                    ..MindCandidates::default()
                },
            ));

            let guard = runtime
                .begin_erasure(Some(person_id), &[conversation_id])
                .await;
            runtime.commit_candidates("pending-reply");
            kovi::tokio::task::yield_now().await;

            assert!(
                InterestStore::relevant(store.as_ref(), "", 8)
                    .await
                    .expect("interest query")
                    .is_empty()
            );
            assert!(
                runtime
                    .pending_candidates
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .is_empty()
            );
            guard.finish().await;
        });
    }

    #[test]
    fn completed_erasure_invalidates_candidate_already_removed_from_pending_queue() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let event = direct_message(person_id, conversation_id, "以后继续聊 Rust");
            let pending = PendingCandidates {
                idempotency_key: "already-claimed-before-erasure".to_string(),
                context: candidate_context(&event),
                candidates: MindCandidates {
                    agenda: Some("以后继续聊 Rust".to_string()),
                    ..MindCandidates::default()
                },
                erasure_epoch: 0,
                registered_at: Utc::now(),
            };

            let guard = runtime
                .begin_erasure(Some(person_id), &[conversation_id])
                .await;
            MindDataErasure::erase_person(store.as_ref(), person_id)
                .await
                .expect("person erasure");
            MindDataErasure::erase_conversation(store.as_ref(), conversation_id)
                .await
                .expect("conversation erasure");
            guard.finish().await;

            runtime
                .persist_candidates(pending)
                .await
                .expect("stale candidate should fail closed");
            assert!(
                AgendaStore::list_active(
                    store.as_ref(),
                    &[MindScope::Person { person_id }],
                    Utc::now(),
                    8,
                )
                .await
                .expect("agenda query")
                .is_empty()
            );
        });
    }

    #[test]
    fn unfinished_mind_erasure_guard_keeps_scope_fail_closed() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, _) = test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let event = direct_message(person_id, conversation_id, "屏障仍应关闭");
            let request = MindSnapshotRequest::for_event(
                &event,
                None,
                MindSnapshotLimits::default(),
                MindInfluenceMode::Shadow,
            )
            .expect("snapshot request");

            let guard = runtime
                .begin_erasure(Some(person_id), &[conversation_id])
                .await;
            drop(guard);

            let snapshot = MindSnapshotProvider::snapshot(runtime.as_ref(), &request)
                .await
                .expect("blocked snapshot fails soft");
            assert!(snapshot.is_empty());
            assert!(!runtime.register_candidates(
                "blocked-after-failed-erasure".to_string(),
                candidate_context(&event),
                MindCandidates {
                    agenda: Some("不应写入".to_string()),
                    ..MindCandidates::default()
                },
            ));
        });
    }

    #[test]
    fn reflection_snapshot_from_before_erasure_cannot_write_an_episode() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let scope = MindScope::Person { person_id };
            let event = direct_message(person_id, conversation_id, "请认真记住这次很重要的变化");
            runtime.observe_event(&event).await.expect("observe event");
            let input = runtime
                .reflection_input(scope, ReflectionTrigger::HighSalienceEvent, Utc::now())
                .await
                .expect("reflection input")
                .expect("high-salience reflection");

            let guard = runtime
                .begin_erasure(Some(person_id), &[conversation_id])
                .await;
            MindDataErasure::erase_person(store.as_ref(), person_id)
                .await
                .expect("person erasure");
            MindDataErasure::erase_conversation(store.as_ref(), conversation_id)
                .await
                .expect("conversation erasure");
            guard.finish().await;

            runtime
                .process_reflection(input)
                .await
                .expect("stale reflection should be discarded");
            assert!(
                EpisodeStore::list_recent(
                    store.as_ref(),
                    &[scope],
                    Utc::now() - Duration::days(1),
                    8,
                )
                .await
                .expect("episode query")
                .is_empty()
            );
        });
    }

    #[test]
    fn stale_outgoing_key_cannot_release_mind_candidates() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let event = direct_message(person_id, conversation_id, "这个话题挺有意思");
            assert!(runtime.register_candidates(
                "winning-outgoing".to_string(),
                candidate_context(&event),
                MindCandidates {
                    agenda: Some("以后继续这个话题".to_string()),
                    ..MindCandidates::default()
                },
            ));

            runtime.commit_candidates("superseded-outgoing");
            kovi::tokio::task::yield_now().await;

            assert!(
                AgendaStore::list_active(
                    store.as_ref(),
                    &[MindScope::Person { person_id }],
                    Utc::now(),
                    8,
                )
                .await
                .expect("agenda query")
                .is_empty()
            );
            assert_eq!(
                runtime
                    .pending_candidates
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .len(),
                1
            );

            let guard = runtime
                .begin_erasure(Some(person_id), &[conversation_id])
                .await;
            guard.finish().await;
        });
    }

    #[test]
    fn erasure_blocks_snapshot_retrieval_even_when_global_state_exists() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let now = Utc::now();
            InterestStore::put(
                store.as_ref(),
                &Interest::new(
                    yunxi_core::InterestId::new(),
                    "Rust",
                    0.8,
                    0.7,
                    0.6,
                    MindSource::Seed,
                    now,
                )
                .expect("valid interest"),
                None,
            )
            .await
            .expect("seed interest");
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let event = direct_message(person_id, conversation_id, "Rust");
            let request = MindSnapshotRequest::for_event(
                &event,
                None,
                MindSnapshotLimits::default(),
                MindInfluenceMode::Shadow,
            )
            .expect("valid snapshot request");
            let before = MindSnapshotProvider::snapshot(runtime.as_ref(), &request)
                .await
                .expect("snapshot before erasure");
            assert!(!before.interests().is_empty());

            let guard = runtime
                .begin_erasure(Some(person_id), &[conversation_id])
                .await;
            let blocked = MindSnapshotProvider::snapshot(runtime.as_ref(), &request)
                .await
                .expect("blocked snapshot fails soft");
            assert!(blocked.is_empty());
            assert_eq!(runtime.metrics().blocked_snapshots, 1);
            guard.finish().await;
        });
    }

    #[test]
    fn person_erasure_waits_for_pending_snapshot_retrieval() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let store = Arc::new(InMemoryMindStore::new());
            let services = MindServices::from_store(Arc::clone(&store));
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let scope = MindScope::Person { person_id };
            let event = direct_message(person_id, conversation_id, "请认真记住这次很重要的变化");
            let runtime =
                MindRuntime::new(services, MindConfig::default()).expect("valid test Mind runtime");
            runtime.observe_event(&event).await.expect("observe event");

            let (recall_entered, recall_pending) = kovi::tokio::sync::oneshot::channel();
            let recall_release = Arc::new(kovi::tokio::sync::Semaphore::new(0));
            let context = MindContextServices::new(
                Arc::new(BlockingRecallStore {
                    entered: Mutex::new(Some(recall_entered)),
                    release: Arc::clone(&recall_release),
                }),
                Arc::new(StatefulOpenLoopStore::default()),
                Arc::new(EmptyGoalStore),
            );
            let runtime = Arc::new(runtime.with_context_services(context));
            let snapshot_runtime = Arc::clone(&runtime);
            let pending_snapshot = kovi::tokio::spawn(async move {
                snapshot_runtime
                    .reflection_input(scope, ReflectionTrigger::HighSalienceEvent, Utc::now())
                    .await
            });
            recall_pending.await.expect("snapshot retrieval started");

            let (erase_started, erase_pending) = kovi::tokio::sync::oneshot::channel();
            let erasure_runtime = Arc::clone(&runtime);
            let pending_erasure = kovi::tokio::spawn(async move {
                let _ = erase_started.send(());
                erasure_runtime
                    .begin_erasure(Some(person_id), &[conversation_id])
                    .await
            });
            erase_pending.await.expect("erasure task started");
            kovi::tokio::task::yield_now().await;
            assert!(
                !pending_erasure.is_finished(),
                "erasure must wait while snapshot retrieval holds the read barrier"
            );

            recall_release.add_permits(1);
            let input = pending_snapshot
                .await
                .expect("snapshot task")
                .expect("reflection input retrieval")
                .expect("high-salience reflection input");
            let guard = pending_erasure
                .await
                .expect("erasure acquires barrier after retrieval");
            MindDataErasure::erase_person(store.as_ref(), person_id)
                .await
                .expect("person erasure");
            MindDataErasure::erase_conversation(store.as_ref(), conversation_id)
                .await
                .expect("conversation erasure");
            guard.finish().await;

            runtime
                .process_reflection(input)
                .await
                .expect("pre-erasure reflection is discarded");
            assert!(
                EpisodeStore::list_recent(
                    store.as_ref(),
                    &[scope],
                    Utc::now() - Duration::days(1),
                    8,
                )
                .await
                .expect("episode query")
                .is_empty()
            );
        });
    }

    #[test]
    fn outgoing_mind_permit_pins_erasure_through_delivery_window() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = active_test_runtime();
            let now = Utc::now();
            InterestStore::put(
                store.as_ref(),
                &Interest::new(
                    yunxi_core::InterestId::new(),
                    "Rust",
                    0.8,
                    0.7,
                    0.6,
                    MindSource::Seed,
                    now,
                )
                .expect("valid interest"),
                None,
            )
            .await
            .expect("seed interest");
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let input = active_planner_input(
                runtime.as_ref(),
                direct_message(person_id, conversation_id, "聊聊 Rust"),
            )
            .await;
            let projection =
                MindDecisionProjection::for_input(&input, yunxi_core::DecisionDisposition::Reply);
            let key = "pinned-mind-delivery";
            assert!(runtime.register_outgoing_fence(key.to_string(), &input, projection));
            let permit = runtime
                .pin_revalidated_outgoing_fence(key)
                .await
                .expect("current Mind snapshot should receive a delivery permit");

            let erasure_runtime = Arc::clone(&runtime);
            let pending_erasure = kovi::tokio::spawn(async move {
                erasure_runtime
                    .begin_erasure(Some(person_id), &[conversation_id])
                    .await
            });
            kovi::tokio::task::yield_now().await;
            assert!(
                !pending_erasure.is_finished(),
                "erasure must wait until the outgoing delivery permit is released"
            );

            drop(permit);
            pending_erasure.await.expect("erasure task").finish().await;
        });
    }

    #[test]
    fn proactive_mind_permit_pins_erasure_through_delivery_window() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = active_test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let scope = MindScope::Person { person_id };
            let now = Utc::now() - Duration::hours(3);
            let curiosity = CuriosityItem::new(
                CuriosityId::new(),
                "你为什么换工作？",
                Some(person_id),
                None,
                0.9,
                now,
                Some(now + Duration::days(30)),
            )
            .expect("valid curiosity");
            let agenda = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::Curiosity(curiosity.id()),
                0.9,
                0.9,
                0.5,
                AgendaSource::Curiosity,
                now,
            )
            .expect("valid curiosity agenda");
            CuriosityStore::put(store.as_ref(), &curiosity, None)
                .await
                .expect("seed curiosity");
            AgendaStore::put(store.as_ref(), &agenda, None)
                .await
                .expect("seed curiosity agenda");
            let permit = runtime
                .pin_proactive_reference(MindProactiveReference::Curiosity(curiosity.id()))
                .await
                .expect("current proactive reference should receive a delivery permit");

            let erasure_runtime = Arc::clone(&runtime);
            let pending_erasure = kovi::tokio::spawn(async move {
                erasure_runtime
                    .begin_erasure(Some(person_id), &[conversation_id])
                    .await
            });
            kovi::tokio::task::yield_now().await;
            assert!(
                !pending_erasure.is_finished(),
                "erasure must wait until the proactive delivery permit is released"
            );

            drop(permit);
            pending_erasure.await.expect("erasure task").finish().await;
        });
    }

    #[test]
    fn shadow_decision_metrics_capture_token_and_snapshot_latency_delta() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let now = Utc::now();
            let interest = Interest::new(
                yunxi_core::InterestId::new(),
                "Rust 类型系统",
                0.9,
                0.8,
                0.7,
                MindSource::Experience,
                now,
            )
            .expect("active interest");
            InterestStore::put(store.as_ref(), &interest, None)
                .await
                .expect("seed interest");
            AgendaStore::put(
                store.as_ref(),
                &AgendaItem::new(
                    AgendaItemId::new(),
                    MindScope::Global,
                    AgendaSubject::Interest(interest.id()),
                    0.9,
                    0.9,
                    0.5,
                    AgendaSource::Interest,
                    now,
                )
                .expect("interest agenda"),
                None,
            )
            .await
            .expect("seed interest agenda");
            let event = casual_direct_message(
                yunxi_core::PersonId::new(),
                ConversationId::new(),
                "这个话题已经聊完了。",
            );
            let request = MindSnapshotRequest::for_event(
                &event,
                None,
                runtime.config().snapshot_limits(),
                MindInfluenceMode::Shadow,
            )
            .expect("shadow snapshot request");
            let snapshot = MindSnapshotProvider::snapshot(runtime.as_ref(), &request)
                .await
                .expect("shadow snapshot");
            let input = PlannerInput::new(event, PlannerStateSnapshot::empty()).with_mind(snapshot);
            let projection =
                MindDecisionProjection::for_input(&input, yunxi_core::DecisionDisposition::Reply);
            assert!(projection.changes_baseline());

            runtime.observe_decision(projection, 37);
            let metrics = runtime.metrics();
            assert_eq!(metrics.snapshot_requests, 1);
            assert!(metrics.snapshot_latency_total_micros >= metrics.snapshot_latency_last_micros);
            assert!(metrics.snapshot_latency_max_micros >= metrics.snapshot_latency_last_micros);
            assert_eq!(metrics.decision_observations, 1);
            assert_eq!(metrics.shadow_decision_deltas, 1);
            assert_eq!(metrics.active_decision_deltas, 0);
            assert_eq!(metrics.estimated_extra_prompt_tokens, 37);
        });
    }

    #[test]
    fn interaction_cues_never_create_beliefs_or_preferences() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let now = Utc::now();
            InterestStore::put(
                store.as_ref(),
                &Interest::new(
                    yunxi_core::InterestId::new(),
                    "系统设计",
                    0.3,
                    0.4,
                    0.5,
                    MindSource::Seed,
                    now,
                )
                .expect("valid interest"),
                None,
            )
            .await
            .expect("seed interest");

            runtime
                .observe_interaction_cues(
                    yunxi_core::PersonId::new(),
                    yunxi_core::InteractionCues {
                        sentiment_valence: 0.8,
                        sentiment_arousal: 0.7,
                        sentiment_confidence: 1.0,
                        gratitude_strength: 1.0,
                    },
                )
                .await
                .expect("cue update");

            assert!(
                BeliefStore::relevant(store.as_ref(), &[MindScope::Global], "", Utc::now(), 8,)
                    .await
                    .expect("belief query")
                    .is_empty()
            );
            assert!(
                PreferenceStore::relevant(store.as_ref(), "", 8)
                    .await
                    .expect("preference query")
                    .is_empty()
            );
            assert_eq!(runtime.metrics().interest_updates, 1);
        });
    }

    #[test]
    fn open_question_agenda_obeys_persistent_cooldown() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let now = Utc::now();
            let question_id = OpenQuestionId::new();
            let scope = MindScope::Person {
                person_id: yunxi_core::PersonId::new(),
            };
            let item = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::OpenQuestion(question_id),
                0.8,
                0.8,
                0.5,
                AgendaSource::OpenQuestion,
                now,
            )
            .expect("valid agenda");
            AgendaStore::put(store.as_ref(), &item, None)
                .await
                .expect("seed agenda");
            assert!(
                runtime
                    .question_agenda_available(question_id, scope, now)
                    .await
                    .expect("available agenda")
            );

            let until = now + Duration::minutes(120);
            let cooled = item
                .with_cooldown(Some(until), now)
                .expect("valid cooldown");
            AgendaStore::put(store.as_ref(), &cooled, Some(item.version()))
                .await
                .expect("persist cooldown");
            assert!(
                !runtime
                    .question_agenda_available(question_id, scope, now)
                    .await
                    .expect("cooldown query")
            );
            assert!(
                runtime
                    .question_agenda_available(question_id, scope, until)
                    .await
                    .expect("expired cooldown query")
            );
        });
    }

    #[test]
    fn cooldown_agenda_items_still_count_toward_scope_capacity() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let store = Arc::new(InMemoryMindStore::new());
            let services = MindServices::from_store(Arc::clone(&store));
            let config: MindConfig =
                serde_json::from_value(serde_json::json!({"max_agenda_per_person": 1}))
                    .expect("bounded agenda config");
            let runtime = MindRuntime::new(services, config).expect("valid Mind runtime");
            let now = Utc::now();
            let scope = MindScope::Person {
                person_id: yunxi_core::PersonId::new(),
            };
            let item = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::OpenQuestion(OpenQuestionId::new()),
                0.8,
                0.8,
                0.5,
                AgendaSource::OpenQuestion,
                now,
            )
            .expect("valid agenda");
            AgendaStore::put(store.as_ref(), &item, None)
                .await
                .expect("seed agenda");
            let cooled = item
                .with_cooldown(Some(now + Duration::hours(2)), now)
                .expect("valid cooldown");
            AgendaStore::put(store.as_ref(), &cooled, Some(item.version()))
                .await
                .expect("persist cooldown");

            assert!(
                !runtime
                    .ensure_agenda(
                        scope,
                        AgendaSubject::Curiosity(CuriosityId::new()),
                        AgendaSource::Curiosity,
                        0.7,
                        0.4,
                        now,
                        TraceContext::root(EventId::new()),
                    )
                    .await
                    .expect("capacity check")
            );
            assert_eq!(
                AgendaStore::list_active(store.as_ref(), &[scope], DateTime::<Utc>::MAX_UTC, 8)
                    .await
                    .expect("all active agenda query")
                    .len(),
                1
            );
        });
    }

    #[test]
    fn curiosity_proactive_requires_an_available_agenda_item() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = active_test_runtime();
            let now = Utc::now();
            let person_id = yunxi_core::PersonId::new();
            let scope = MindScope::Person { person_id };
            let curiosity = CuriosityItem::new(
                CuriosityId::new(),
                "你换工作主要是出于什么考虑？",
                Some(person_id),
                None,
                0.8,
                now - Duration::hours(3),
                Some(now + Duration::days(1)),
            )
            .expect("valid curiosity");
            CuriosityStore::put(store.as_ref(), &curiosity, None)
                .await
                .expect("seed curiosity");

            assert_eq!(
                runtime
                    .proactive_signals(person_id)
                    .await
                    .expect("signals without agenda"),
                MindProactiveSignals::default()
            );
            assert!(
                !runtime
                    .proactive_reference_current(MindProactiveReference::Curiosity(curiosity.id()))
                    .await
            );

            let agenda = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::Curiosity(curiosity.id()),
                0.8,
                0.8,
                0.5,
                AgendaSource::Curiosity,
                now,
            )
            .expect("valid curiosity agenda");
            AgendaStore::put(store.as_ref(), &agenda, None)
                .await
                .expect("seed curiosity agenda");

            let signals = runtime
                .proactive_signals(person_id)
                .await
                .expect("signals with agenda");
            assert_eq!(signals.topic.as_deref(), Some(curiosity.question()));
            assert_eq!(
                signals.reference,
                Some(MindProactiveReference::Curiosity(curiosity.id()))
            );
            assert!(
                runtime
                    .proactive_reference_current(MindProactiveReference::Curiosity(curiosity.id()))
                    .await
            );
        });
    }

    #[test]
    fn incoming_answer_resolves_question_and_agenda_before_next_plan() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let source = direct_message(person_id, conversation_id, "等面试结束后再问问结果");
            runtime
                .persist_candidates(PendingCandidates {
                    idempotency_key: "prepared-question".to_string(),
                    context: candidate_context(&source),
                    candidates: MindCandidates {
                        open_question: Some("你今天面试结果怎么样？".to_string()),
                        ..MindCandidates::default()
                    },
                    erasure_epoch: 0,
                    registered_at: Utc::now(),
                })
                .await
                .expect("persist question candidate");
            let scope = MindScope::Person { person_id };
            let question = OpenQuestionStore::list_open(store.as_ref(), &[scope], 8)
                .await
                .expect("open-question query")
                .into_iter()
                .next()
                .expect("stored open question");
            assert_eq!(
                AgendaStore::list_active(store.as_ref(), &[scope], Utc::now(), 8)
                    .await
                    .expect("agenda query")
                    .len(),
                1
            );
            runtime
                .mark_decision_used_inner(MindDecisionReference::OpenQuestion(question.id()))
                .await
                .expect("mark question as used");
            assert!(
                AgendaStore::list_active(store.as_ref(), &[scope], Utc::now(), 8)
                    .await
                    .expect("cooled agenda query")
                    .is_empty()
            );

            let answer = direct_message(person_id, conversation_id, "我面试过了，结果还不错");
            runtime
                .observe_event(&answer)
                .await
                .expect("observe answer");

            assert!(
                OpenQuestionStore::list_open(store.as_ref(), &[scope], 8)
                    .await
                    .expect("resolved question query")
                    .is_empty()
            );
            assert!(
                AgendaStore::list_active(store.as_ref(), &[scope], DateTime::<Utc>::MAX_UTC, 8,)
                    .await
                    .expect("resolved cooled agenda query")
                    .is_empty()
            );
        });
    }

    #[test]
    fn curiosity_resolved_before_pending_question_commit() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = active_test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let scope = MindScope::Person { person_id };
            let created_at = Utc::now() - Duration::hours(3);
            let curiosity = CuriosityItem::new(
                CuriosityId::new(),
                "你为什么换工作？",
                Some(person_id),
                None,
                0.8,
                created_at,
                Some(created_at + Duration::days(30)),
            )
            .expect("curiosity");
            let agenda = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::Curiosity(curiosity.id()),
                0.8,
                0.8,
                0.5,
                AgendaSource::Curiosity,
                created_at,
            )
            .expect("curiosity agenda");
            CuriosityStore::put(store.as_ref(), &curiosity, None)
                .await
                .expect("seed curiosity");
            AgendaStore::put(store.as_ref(), &agenda, None)
                .await
                .expect("seed agenda");

            let input = active_planner_input(
                runtime.as_ref(),
                casual_direct_message(person_id, conversation_id, "刚才的话题说完了。"),
            )
            .await;
            let projection =
                MindDecisionProjection::for_input(&input, yunxi_core::DecisionDisposition::Reply);
            assert!(runtime.register_outgoing_fence(
                "pending-curiosity-question".to_string(),
                &input,
                projection,
            ));
            assert!(
                runtime
                    .proactive_reference_current(MindProactiveReference::Curiosity(curiosity.id()))
                    .await
            );

            runtime
                .observe_event(&direct_message(
                    person_id,
                    conversation_id,
                    "换工作主要是因为上一份工作太远了。",
                ))
                .await
                .expect("observe natural answer");

            assert!(
                !runtime
                    .proactive_reference_current(MindProactiveReference::Curiosity(curiosity.id()))
                    .await
            );
            assert!(
                !runtime
                    .pin_revalidated_outgoing_fence("pending-curiosity-question")
                    .await
                    .is_some()
            );
            assert_eq!(
                CuriosityStore::get(store.as_ref(), curiosity.id())
                    .await
                    .expect("curiosity lookup")
                    .expect("stored curiosity")
                    .status(),
                CuriosityStatus::Resolved
            );
            assert!(
                AgendaStore::list_active(store.as_ref(), &[scope], Utc::now(), 8)
                    .await
                    .expect("resolved agenda query")
                    .is_empty()
            );
        });
    }

    #[test]
    fn open_question_resolved_before_pending_question_commit() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = active_test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let scope = MindScope::Person { person_id };
            let created_at = Utc::now() - Duration::hours(3);
            let question = OpenQuestion::new(
                OpenQuestionId::new(),
                scope,
                "你今天面试结果怎么样？",
                Vec::new(),
                0.8,
                created_at,
            )
            .expect("open question");
            let agenda = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::OpenQuestion(question.id()),
                0.8,
                0.8,
                0.5,
                AgendaSource::OpenQuestion,
                created_at,
            )
            .expect("question agenda");
            OpenQuestionStore::put(store.as_ref(), &question, None)
                .await
                .expect("seed question");
            AgendaStore::put(store.as_ref(), &agenda, None)
                .await
                .expect("seed agenda");

            let input = active_planner_input(
                runtime.as_ref(),
                casual_direct_message(person_id, conversation_id, "之前那个话题先到这里。"),
            )
            .await;
            let projection =
                MindDecisionProjection::for_input(&input, yunxi_core::DecisionDisposition::Reply);
            assert_eq!(
                projection.disposition(),
                yunxi_core::DecisionDisposition::AskQuestion
            );
            assert!(runtime.register_outgoing_fence(
                "pending-open-question".to_string(),
                &input,
                projection,
            ));

            runtime
                .observe_event(&direct_message(
                    person_id,
                    conversation_id,
                    "我今天面试过了，结果还不错。",
                ))
                .await
                .expect("observe answer");

            assert!(
                !runtime
                    .pin_revalidated_outgoing_fence("pending-open-question")
                    .await
                    .is_some()
            );
            assert!(
                OpenQuestionStore::list_open(store.as_ref(), &[scope], 8)
                    .await
                    .expect("resolved question query")
                    .is_empty()
            );
            assert!(
                AgendaStore::list_active(store.as_ref(), &[scope], Utc::now(), 8)
                    .await
                    .expect("resolved agenda query")
                    .is_empty()
            );
        });
    }

    #[test]
    fn open_loop_resolved_before_proactive_commit() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let scope = MindScope::Person { person_id };
            let created_at = Utc::now() - Duration::hours(3);
            let open_loop = OpenLoop::new(
                yunxi_core::OpenLoopId::new(),
                OpenLoopOwner::Person(person_id),
                OpenLoopKind::FollowUp,
                "你今天面试结果怎么样？",
                created_at,
            )
            .expect("open loop");
            let open_loop_id = open_loop.id();
            let open_loops = Arc::new(StatefulOpenLoopStore::with_item(open_loop));
            let (runtime, store) = active_runtime_with_open_loops(Arc::clone(&open_loops));
            let agenda = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::OpenLoop(open_loop_id),
                0.9,
                0.9,
                0.7,
                AgendaSource::OpenLoop,
                created_at,
            )
            .expect("open-loop agenda");
            let agenda_id = agenda.id();
            AgendaStore::put(store.as_ref(), &agenda, None)
                .await
                .expect("seed agenda");

            assert!(
                runtime
                    .proactive_reference_current(MindProactiveReference::Agenda(agenda_id))
                    .await
            );
            let input = active_planner_input(
                runtime.as_ref(),
                casual_direct_message(person_id, conversation_id, "技术问题已经聊完了。"),
            )
            .await;
            let projection =
                MindDecisionProjection::for_input(&input, yunxi_core::DecisionDisposition::Reply);
            assert_eq!(
                projection.disposition(),
                yunxi_core::DecisionDisposition::ResumeAgenda
            );
            assert!(runtime.register_outgoing_fence(
                "pending-open-loop-proactive".to_string(),
                &input,
                projection,
            ));

            runtime
                .observe_event(&direct_message(
                    person_id,
                    conversation_id,
                    "我今天面试过了，结果还不错。",
                ))
                .await
                .expect("observe open-loop answer");

            assert!(
                !runtime
                    .proactive_reference_current(MindProactiveReference::Agenda(agenda_id))
                    .await
            );
            assert!(
                !runtime
                    .pin_revalidated_outgoing_fence("pending-open-loop-proactive")
                    .await
                    .is_some()
            );
            assert!(
                OpenLoopStore::get(open_loops.as_ref(), open_loop_id)
                    .await
                    .expect("open-loop lookup")
                    .expect("stored open loop")
                    .status()
                    .is_terminal()
            );
            assert!(
                AgendaStore::list_active(store.as_ref(), &[scope], Utc::now(), 8)
                    .await
                    .expect("resolved agenda query")
                    .is_empty()
            );
        });
    }

    #[test]
    fn inner_agenda_changes_affect_revalidation() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = active_test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let scope = MindScope::Person { person_id };
            let created_at = Utc::now() - Duration::hours(1);
            let agenda = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::SocialMotive("继续聊之前的系统设计".to_string()),
                0.8,
                0.8,
                0.6,
                AgendaSource::Interaction,
                created_at,
            )
            .expect("agenda");
            AgendaStore::put(store.as_ref(), &agenda, None)
                .await
                .expect("seed agenda");
            let input = active_planner_input(
                runtime.as_ref(),
                casual_direct_message(person_id, conversation_id, "现在空下来了。"),
            )
            .await;
            let projection =
                MindDecisionProjection::for_input(&input, yunxi_core::DecisionDisposition::Reply);
            assert!(runtime.register_outgoing_fence(
                "pending-agenda-resume".to_string(),
                &input,
                projection,
            ));

            let changed = agenda
                .activate(1.0, Utc::now())
                .expect("agenda activation update");
            AgendaStore::put(store.as_ref(), &changed, Some(agenda.version()))
                .await
                .expect("persist agenda update");

            assert!(
                !runtime
                    .pin_revalidated_outgoing_fence("pending-agenda-resume")
                    .await
                    .is_some()
            );
            assert_eq!(runtime.metrics().outgoing_fences_stale, 1);
        });
    }

    #[test]
    fn collision_does_not_duplicate_curiosity() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let source = direct_message(person_id, conversation_id, "我最近换工作了");
            let candidates = MindCandidates {
                curiosity: Some("你为什么换工作？".to_string()),
                ..MindCandidates::default()
            };
            runtime
                .persist_candidates(PendingCandidates {
                    idempotency_key: "first-curiosity".to_string(),
                    context: candidate_context(&source),
                    candidates: candidates.clone(),
                    erasure_epoch: 0,
                    registered_at: Utc::now(),
                })
                .await
                .expect("persist first curiosity");
            let collision = WorldEvent::new(
                Utc::now(),
                EventScope::Conversation { conversation_id },
                EventPriority::High,
                WorldEventKind::MessageCollisionDetected(MessageCollisionDetectedEvent {
                    conversation_id,
                    outgoing_generation: 1,
                    conversation_version: 2,
                    fingerprint: 3,
                }),
            );
            runtime
                .observe_event(&collision)
                .await
                .expect("observe collision");
            runtime
                .persist_candidates(PendingCandidates {
                    idempotency_key: "replacement-curiosity".to_string(),
                    context: candidate_context(&source),
                    candidates,
                    erasure_epoch: 0,
                    registered_at: Utc::now(),
                })
                .await
                .expect("persist replacement curiosity");

            let scope = MindScope::Person { person_id };
            assert_eq!(
                CuriosityStore::list_open(store.as_ref(), &[scope], Utc::now(), 8)
                    .await
                    .expect("curiosity query")
                    .len(),
                1
            );
            assert_eq!(
                AgendaStore::list_active(store.as_ref(), &[scope], Utc::now(), 8)
                    .await
                    .expect("agenda query")
                    .len(),
                1
            );
            assert_eq!(
                runtime
                    .recent_events
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .for_scope(MindScope::Conversation { conversation_id })
                    .len(),
                1
            );
        });
    }

    #[test]
    fn stale_pending_outgoing_does_not_write_wrong_episode() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, store) = active_test_runtime();
            let person_id = yunxi_core::PersonId::new();
            let conversation_id = ConversationId::new();
            let scope = MindScope::Conversation { conversation_id };
            let created_at = Utc::now() - Duration::hours(1);
            let agenda = AgendaItem::new(
                AgendaItemId::new(),
                scope,
                AgendaSubject::SocialMotive("继续问面试结果".to_string()),
                0.8,
                0.8,
                0.5,
                AgendaSource::Interaction,
                created_at,
            )
            .expect("agenda");
            AgendaStore::put(store.as_ref(), &agenda, None)
                .await
                .expect("seed agenda");
            let source = casual_direct_message(person_id, conversation_id, "之前的话题结束了。");
            let input = active_planner_input(runtime.as_ref(), source.clone()).await;
            let projection =
                MindDecisionProjection::for_input(&input, yunxi_core::DecisionDisposition::Reply);
            let key = "stale-pending-with-candidate";
            assert!(runtime.register_outgoing_fence(key.to_string(), &input, projection));
            assert!(runtime.register_candidates(
                key.to_string(),
                candidate_context(&source),
                MindCandidates {
                    curiosity: Some("你为什么换工作？".to_string()),
                    ..MindCandidates::default()
                },
            ));
            let changed = agenda.activate(1.0, Utc::now()).expect("agenda update");
            AgendaStore::put(store.as_ref(), &changed, Some(agenda.version()))
                .await
                .expect("persist agenda update");
            runtime
                .observe_event(&WorldEvent::new(
                    Utc::now(),
                    EventScope::Conversation { conversation_id },
                    EventPriority::High,
                    WorldEventKind::MessageCollisionDetected(MessageCollisionDetectedEvent {
                        conversation_id,
                        outgoing_generation: 5,
                        conversation_version: 6,
                        fingerprint: 7,
                    }),
                ))
                .await
                .expect("observe collision");

            assert!(runtime.pin_revalidated_outgoing_fence(key).await.is_none());
            assert!(
                runtime
                    .pending_candidates
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .iter()
                    .all(|pending| pending.idempotency_key != key)
            );
            runtime.trigger_reflection(ReflectionTrigger::Idle).await;

            assert!(
                CuriosityStore::list_open(
                    store.as_ref(),
                    &[MindScope::Person { person_id }],
                    Utc::now(),
                    8,
                )
                .await
                .expect("curiosity query")
                .is_empty()
            );
            assert!(
                EpisodeStore::list_recent(
                    store.as_ref(),
                    &[scope, MindScope::Person { person_id }],
                    Utc::now() - Duration::days(1),
                    8,
                )
                .await
                .expect("episode query")
                .is_empty()
            );
        });
    }

    #[test]
    fn reflection_is_low_frequency_and_uses_no_model_boundary() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let (runtime, _) = test_runtime();
            let event = direct_message(
                yunxi_core::PersonId::new(),
                ConversationId::new(),
                "请帮我认真记住这件重要的事",
            );
            runtime
                .observe_event(&event)
                .await
                .expect("observe message");
            runtime.trigger_reflection(ReflectionTrigger::Idle).await;
            let first = runtime.metrics();
            assert!(first.reflections > 0);
            assert_eq!(first.reflection_failures, 0);

            runtime.trigger_reflection(ReflectionTrigger::Idle).await;
            let second = runtime.metrics();
            assert_eq!(second.reflections, first.reflections);
            assert_eq!(second.reflection_failures, 0);
        });
    }
}
