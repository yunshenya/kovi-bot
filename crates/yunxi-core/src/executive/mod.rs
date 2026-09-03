//! Executive control: bounded arbitration, budgets, plans, expectations and
//! model-tier selection. This module owns metadata and policy, never side
//! effects or hidden chain-of-thought.

macro_rules! executive_id {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(uuid::Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }

            #[must_use]
            pub const fn from_uuid(value: uuid::Uuid) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> &uuid::Uuid {
                &self.0
            }

            #[must_use]
            pub const fn into_uuid(self) -> uuid::Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<uuid::Uuid> for $name {
            fn from(value: uuid::Uuid) -> Self {
                Self::from_uuid(value)
            }
        }

        impl From<$name> for uuid::Uuid {
            fn from(value: $name) -> Self {
                value.into_uuid()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

executive_id!(ConflictId);
executive_id!(PlanId);
executive_id!(PlanStepId);
executive_id!(ExpectationId);
executive_id!(DecisionRecordId);

mod attention_budget;
mod confidence;
mod conflict;
mod consistency;
mod decision_record;
mod expectation;
mod outgoing;
mod persistence;
mod plan;
mod policy;
mod priority;
mod snapshot;

use chrono::Utc;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub use attention_budget::{
    AttentionBudget, AttentionBudgetSnapshot, AttentionCost, BudgetError, BudgetGrant,
    CognitiveBudget,
};
pub use confidence::{
    ConfidenceCalibration, ConfidenceLevel, ConfidenceUpdate, EvidencePolarity, EvidenceSource,
    EvidenceWeight, HypothesisState, confidence_level, hypothesis_state, update_confidence,
};
pub use conflict::{
    ConflictKind, ConflictMonitor, ConflictMonitorConfig, ConflictRef, ConflictStatus,
    ConflictValidationError, ExecutiveConflict, MAX_CONFLICT_PARTICIPANTS,
};
pub use consistency::{ConsistencyKind, ConsistencySeverity, SelfConsistencyConflict};
pub use decision_record::{
    DecisionActionKind, DecisionRecord, DecisionRecordRetention, DecisionRecordSnapshot,
    DecisionRecordStore, ExecutiveReasonTag, MAX_REASON_TAGS,
};
pub use expectation::{
    Expectation, ExpectationObservation, ExpectationSnapshot, ExpectationStatus,
    ExpectationTracker, ExpectationTrackerConfig, ExpectedEventPattern,
};
pub use outgoing::{OutgoingSource, PendingOutgoing};
pub use persistence::{
    DecisionRecordPersistence, ExecutivePersistenceError, ExecutiveScope, ExecutiveStore,
    ExecutiveStoreFuture, ExpectationStore, PlanStore,
};
pub use plan::{
    MAX_PLAN_REVISIONS, MAX_PLAN_STEPS, PlanError, PlanRevision, PlanSnapshot, PlanStaleReason,
    PlanState, PlanStatus, PlanStep, PlanStepKind, PlanStepStatus, PlanValidationError,
    RetryPolicy,
};
pub use policy::{ExecutivePolicy, ExecutivePolicyError, ExecutiveTierDecision, HardPriority};
pub use priority::GoalPrioritySnapshot;
pub use snapshot::{
    ConflictSnapshot, ExecutiveSnapshot, ExpectationSnapshot as SnapshotExpectation,
    MAX_SNAPSHOT_ITEMS, PlanSnapshot as SnapshotPlan,
};

/// Maximum number of simultaneously retained active plans across all scopes.
/// Each scope owns at most one plan; this second bound keeps a stream of
/// short-lived scopes from growing the process indefinitely.
pub const MAX_ACTIVE_PLANS: usize = 64;

#[derive(Debug)]
struct ExecutiveState {
    conflicts: ConflictMonitor,
    conflict_scopes: HashMap<ConflictId, ExecutiveScope>,
    goals: Vec<GoalPrioritySnapshot>,
    goal_scopes: HashMap<crate::GoalId, ExecutiveScope>,
    attention_budget: AttentionBudget,
    active_plans: HashMap<ExecutiveScope, PlanState>,
    expectations: VecDeque<Expectation>,
    expectation_scopes: HashMap<ExpectationId, ExecutiveScope>,
    decisions: DecisionRecordStore,
    decision_scopes: HashMap<DecisionRecordId, ExecutiveScope>,
    capability: crate::CognitiveCapabilitySnapshot,
    version: u64,
}

/// Thread-safe synchronous state holder. Callers take a clone snapshot before
/// any model await; no Executive lock is held while a backend runs.
#[derive(Debug, Clone)]
pub struct ExecutiveController {
    policy: ExecutivePolicy,
    state: Arc<Mutex<ExecutiveState>>,
}

pub type ExecutiveRuntime = ExecutiveController;

impl ExecutiveController {
    pub fn new(policy: ExecutivePolicy) -> Result<Self, ExecutivePolicyError> {
        policy.validate()?;
        let attention_budget = AttentionBudget::new(
            policy.attention_budget_capacity,
            policy.critical_attention_reserve,
            1.0,
        )
        .map_err(|_| ExecutivePolicyError::InvalidBudget)?;
        let conflicts = ConflictMonitor::new(ConflictMonitorConfig {
            threshold: policy.conflict_threshold,
            max_active: policy.max_active_conflicts,
            ..ConflictMonitorConfig::default()
        })
        .map_err(|_| ExecutivePolicyError::InvalidBound {
            field: "max_active_conflicts",
        })?;
        let decisions = DecisionRecordStore::new(DecisionRecordRetention {
            max_records: policy.decision_record_limit,
            ..DecisionRecordRetention::default()
        })
        .map_err(|_| ExecutivePolicyError::InvalidBound {
            field: "decision_record_limit",
        })?;
        Ok(Self {
            policy,
            state: Arc::new(Mutex::new(ExecutiveState {
                conflicts,
                conflict_scopes: HashMap::new(),
                goals: Vec::new(),
                goal_scopes: HashMap::new(),
                attention_budget,
                active_plans: HashMap::new(),
                expectations: VecDeque::new(),
                expectation_scopes: HashMap::new(),
                decisions,
                decision_scopes: HashMap::new(),
                capability: crate::CognitiveCapabilitySnapshot::default(),
                version: 0,
            })),
        })
    }

    #[must_use]
    pub fn policy(&self) -> ExecutivePolicy {
        self.policy
    }

    #[must_use]
    pub fn snapshot(&self) -> ExecutiveSnapshot {
        self.snapshot_for_scope(&ExecutiveScope::Global)
    }

    /// Return the bounded planner projection for one scope. The global plan
    /// is used as a fallback when the requested scope has no own plan.
    #[must_use]
    pub fn snapshot_for_scope(&self, scope: &ExecutiveScope) -> ExecutiveSnapshot {
        self.snapshot_for_scopes(std::slice::from_ref(scope))
    }

    /// Return a projection for an ordered list of scopes. The first matching
    /// plan wins, which lets a host prefer conversation/goal state and fall
    /// back to global policy without exposing a second active plan field.
    #[must_use]
    pub fn snapshot_for_scopes(&self, scopes: &[ExecutiveScope]) -> ExecutiveSnapshot {
        let state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        let mut conflicts = state.conflicts.active();
        conflicts.truncate(MAX_SNAPSHOT_ITEMS);
        let mut goals = state.goals.clone();
        goals.truncate(MAX_SNAPSHOT_ITEMS);
        let mut expectations = state.expectations.iter().cloned().collect::<Vec<_>>();
        expectations.truncate(MAX_SNAPSHOT_ITEMS);
        let mut decisions = state.decisions.recent();
        if decisions.len() > MAX_SNAPSHOT_ITEMS {
            decisions = decisions.split_off(decisions.len() - MAX_SNAPSHOT_ITEMS);
        }
        let active_plan = scopes
            .iter()
            .find_map(|scope| state.active_plans.get(scope).cloned())
            .or_else(|| state.active_plans.get(&ExecutiveScope::Global).cloned());
        ExecutiveSnapshot {
            active_conflicts: conflicts,
            prioritized_goals: goals,
            attention_budget: state.attention_budget.snapshot(),
            active_plan,
            pending_expectations: expectations,
            recent_decisions: decisions,
            cognitive_capability: state.capability.clone(),
            version: state.version,
        }
    }

    #[must_use]
    pub fn version(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .version
    }

    pub fn set_capability(
        &self,
        capability: crate::CognitiveCapabilitySnapshot,
    ) -> Result<(), &'static str> {
        capability.validate()?;
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        if state.capability != capability {
            state.capability = capability;
            bump(&mut state.version);
        }
        Ok(())
    }

    pub fn detect_conflict(
        &self,
        kind: conflict::ConflictKind,
        severity: f32,
        confidence: f32,
        participants: Vec<conflict::ConflictRef>,
    ) -> Option<ExecutiveConflict> {
        self.detect_conflict_for_scope(
            ExecutiveScope::Global,
            kind,
            severity,
            confidence,
            participants,
        )
    }

    /// Record a conflict with an explicit erasure scope. The original method
    /// remains global for compatibility with callers that do not carry a
    /// user/conversation owner yet.
    pub fn detect_conflict_for_scope(
        &self,
        scope: ExecutiveScope,
        kind: conflict::ConflictKind,
        severity: f32,
        confidence: f32,
        participants: Vec<conflict::ConflictRef>,
    ) -> Option<ExecutiveConflict> {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        let conflict = state
            .conflicts
            .detect(kind, severity, confidence, participants, Utc::now());
        if let Some(conflict) = &conflict {
            state.conflict_scopes.insert(conflict.id, scope);
            prune_scope_indexes(&mut state);
            bump(&mut state.version);
        }
        conflict
    }

    pub fn resolve_conflict(&self, id: ConflictId, status: conflict::ConflictStatus) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        let changed = state.conflicts.resolve(id, status);
        if changed {
            bump(&mut state.version);
        }
        changed
    }

    pub fn set_prioritized_goals(
        &self,
        goals: Vec<GoalPrioritySnapshot>,
    ) -> Result<(), &'static str> {
        self.set_prioritized_goals_for_scope(ExecutiveScope::Global, goals)
    }

    /// Replace the prioritized goal projection for one scope while retaining
    /// projections belonging to other scopes in the bounded controller.
    pub fn set_prioritized_goals_for_scope(
        &self,
        scope: ExecutiveScope,
        goals: Vec<GoalPrioritySnapshot>,
    ) -> Result<(), &'static str> {
        if goals.len() > MAX_SNAPSHOT_ITEMS {
            return Err("too many prioritized goals");
        }
        for goal in &goals {
            goal.validate()?;
        }
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        if matches!(scope, ExecutiveScope::Global) {
            state.goals = goals;
            state.goal_scopes.clear();
            let goal_ids = state
                .goals
                .iter()
                .map(|goal| goal.goal_id)
                .collect::<Vec<_>>();
            for goal_id in goal_ids {
                state.goal_scopes.insert(goal_id, ExecutiveScope::Global);
            }
        } else {
            let removed = state
                .goals
                .iter()
                .filter(|goal| {
                    state
                        .goal_scopes
                        .get(&goal.goal_id)
                        .is_some_and(|existing| existing == &scope)
                })
                .map(|goal| goal.goal_id)
                .collect::<Vec<_>>();
            state.goals.retain(|goal| !removed.contains(&goal.goal_id));
            for goal_id in removed {
                state.goal_scopes.remove(&goal_id);
            }
            for goal in goals {
                state.goal_scopes.insert(goal.goal_id, scope.clone());
                state.goals.push(goal);
            }
        }
        prune_scope_indexes(&mut state);
        bump(&mut state.version);
        Ok(())
    }

    pub fn set_active_plan(&self, plan: Option<PlanState>) -> Result<(), &'static str> {
        self.set_active_plan_for_scope(ExecutiveScope::Global, plan)
    }

    /// Install an active plan with an explicit owner for scoped erasure.
    pub fn set_active_plan_for_scope(
        &self,
        scope: ExecutiveScope,
        plan: Option<PlanState>,
    ) -> Result<(), &'static str> {
        if let Some(plan) = &plan {
            plan.validate().map_err(|_| "active plan is invalid")?;
        }
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        if plan.is_some()
            && !state.active_plans.contains_key(&scope)
            && state.active_plans.len() >= MAX_ACTIVE_PLANS
        {
            return Err("active plan capacity is exhausted");
        }
        if let Some(plan) = plan {
            state.active_plans.insert(scope, plan);
        } else {
            state.active_plans.remove(&scope);
        }
        bump(&mut state.version);
        Ok(())
    }

    pub fn register_expectation(&self, expectation: Expectation) -> Result<bool, &'static str> {
        self.register_expectation_for_scope(ExecutiveScope::Global, expectation)
    }

    /// Register an expectation with an explicit owner for scoped erasure.
    pub fn register_expectation_for_scope(
        &self,
        scope: ExecutiveScope,
        expectation: Expectation,
    ) -> Result<bool, &'static str> {
        expectation.validate()?;
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        if state.expectations.len() >= expectation::MAX_EXPECTATIONS {
            return Ok(false);
        }
        if state.expectations.iter().any(|existing| {
            existing.source_action_id == expectation.source_action_id
                && state
                    .expectation_scopes
                    .get(&existing.id)
                    .is_some_and(|existing_scope| existing_scope == &scope)
        }) {
            return Ok(false);
        }
        let pending_in_scope = state
            .expectations
            .iter()
            .filter(|existing| {
                state
                    .expectation_scopes
                    .get(&existing.id)
                    .is_some_and(|existing_scope| existing_scope == &scope)
            })
            .count();
        if pending_in_scope >= self.policy.expectation_limit {
            return Ok(false);
        }
        state.expectation_scopes.insert(expectation.id, scope);
        state.expectations.push_back(expectation);
        prune_scope_indexes(&mut state);
        bump(&mut state.version);
        Ok(true)
    }

    /// Observe one valid world event against all pending expectations. Terminal
    /// statuses are reported to the caller and removed from the pending
    /// projection, so satisfied/expired rows cannot consume future quota.
    pub fn observe_expectations(&self, event: &crate::WorldEvent) -> ExpectationObservation {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        let now = Utc::now();
        let mut observation = ExpectationObservation::default();
        for expectation in &mut state.expectations {
            match expectation.observe(event, now) {
                ExpectationStatus::Satisfied => observation.satisfied.push(expectation.id),
                ExpectationStatus::Expired => observation.expired.push(expectation.id),
                ExpectationStatus::Pending
                | ExpectationStatus::Violated
                | ExpectationStatus::Cancelled => {}
            }
        }
        if !observation.is_empty() {
            state
                .expectations
                .retain(|expectation| expectation.status == ExpectationStatus::Pending);
            prune_scope_indexes(&mut state);
            bump(&mut state.version);
        }
        observation
    }

    /// Expire pending expectations against the current wall clock and return
    /// their IDs. This is used by maintenance paths that have no event.
    pub fn expire_expectations(&self) -> Vec<ExpectationId> {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        let now = Utc::now();
        let mut expired = Vec::new();
        for expectation in &mut state.expectations {
            if expectation.expire_if_due(now) {
                expired.push(expectation.id);
            }
        }
        if !expired.is_empty() {
            state
                .expectations
                .retain(|expectation| expectation.status == ExpectationStatus::Pending);
            prune_scope_indexes(&mut state);
            bump(&mut state.version);
        }
        expired
    }

    pub fn record_decision(&self, record: DecisionRecord) -> Result<bool, &'static str> {
        self.record_decision_for_scope(ExecutiveScope::Global, record)
    }

    /// Record decision metadata with an explicit owner for scoped erasure.
    pub fn record_decision_for_scope(
        &self,
        scope: ExecutiveScope,
        record: DecisionRecord,
    ) -> Result<bool, &'static str> {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        let recorded = state.decisions.record(record, Utc::now())?;
        if recorded {
            if let Some(record) = state.decisions.recent().last() {
                state.decision_scopes.insert(record.id, scope);
            }
            prune_scope_indexes(&mut state);
            bump(&mut state.version);
        }
        Ok(recorded)
    }

    pub fn try_consume_attention(
        &self,
        cost: f32,
        critical: bool,
    ) -> Result<BudgetGrant, BudgetError> {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        let grant = state.attention_budget.try_consume(cost, critical)?;
        if grant.is_granted() {
            bump(&mut state.version);
        }
        Ok(grant)
    }

    pub fn replenish_attention(&self, elapsed: std::time::Duration) {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        state.attention_budget.replenish(elapsed);
        bump(&mut state.version);
    }

    /// Restore a bounded persisted snapshot without holding the state lock
    /// across any asynchronous boundary.  The persisted representation is
    /// validated before the live state is replaced, so a corrupt row leaves
    /// the current controller untouched and the host can safely discard it.
    pub fn restore_snapshot(&self, snapshot: ExecutiveSnapshot) -> Result<(), &'static str> {
        snapshot.validate()?;
        let attention_budget = AttentionBudget::from_snapshot(snapshot.attention_budget)
            .map_err(|_| "Executive snapshot contains an invalid attention budget")?;
        if attention_budget.total > self.policy.attention_budget_capacity
            || attention_budget.reserved_for_critical > self.policy.critical_attention_reserve
        {
            return Err("Executive snapshot exceeds the current policy budget");
        }
        let now = Utc::now();
        let mut conflicts = ConflictMonitor::new(ConflictMonitorConfig {
            threshold: self.policy.conflict_threshold,
            max_active: self.policy.max_active_conflicts,
            ..ConflictMonitorConfig::default()
        })
        .map_err(|_| "current Executive conflict policy is invalid")?;
        conflicts
            .restore(snapshot.active_conflicts.clone(), now)
            .map_err(|_| "Executive snapshot contains an invalid conflict")?;

        let mut decisions = DecisionRecordStore::new(DecisionRecordRetention {
            max_records: self.policy.decision_record_limit,
            ..DecisionRecordRetention::default()
        })
        .map_err(|_| "current Executive decision policy is invalid")?;
        decisions.restore(snapshot.recent_decisions.clone(), now)?;

        let mut goals = snapshot.prioritized_goals;
        goals.truncate(MAX_SNAPSHOT_ITEMS);
        for goal in &goals {
            goal.validate()?;
        }

        let mut expectations = VecDeque::new();
        for expectation in snapshot.pending_expectations {
            expectation.validate()?;
            if expectation.status != ExpectationStatus::Pending {
                continue;
            }
            if expectations
                .iter()
                .any(|existing: &Expectation| existing.id == expectation.id)
                || expectations.len() >= self.policy.expectation_limit
            {
                continue;
            }
            expectations.push_back(expectation);
        }

        if let Some(plan) = &snapshot.active_plan {
            plan.validate().map_err(|_| "active plan is invalid")?;
        }
        snapshot.cognitive_capability.validate()?;

        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        state.conflicts = conflicts;
        state.conflict_scopes.clear();
        for conflict in state.conflicts.active() {
            state
                .conflict_scopes
                .insert(conflict.id, ExecutiveScope::Global);
        }
        state.goals = goals;
        state.goal_scopes.clear();
        let goal_ids = state
            .goals
            .iter()
            .map(|goal| goal.goal_id)
            .collect::<Vec<_>>();
        for goal_id in goal_ids {
            state.goal_scopes.insert(goal_id, ExecutiveScope::Global);
        }
        state.attention_budget = attention_budget;
        state.active_plans.clear();
        if let Some(plan) = snapshot.active_plan {
            state.active_plans.insert(ExecutiveScope::Global, plan);
        }
        state.expectations = expectations;
        state.expectation_scopes.clear();
        let expectation_ids = state
            .expectations
            .iter()
            .map(|expectation| expectation.id)
            .collect::<Vec<_>>();
        for expectation_id in expectation_ids {
            state
                .expectation_scopes
                .insert(expectation_id, ExecutiveScope::Global);
        }
        state.decisions = decisions;
        state.decision_scopes.clear();
        for decision in state.decisions.recent() {
            state
                .decision_scopes
                .insert(decision.id, ExecutiveScope::Global);
        }
        state.capability = snapshot.cognitive_capability;
        state.version = snapshot.version.max(1);
        Ok(())
    }

    /// Clear all in-memory Executive state at a completed data-erasure
    /// barrier.  Capability and policy are installation metadata rather than
    /// user-scoped content, so they remain available for the next turn.
    pub fn clear_for_data_erasure(&self) {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        state.conflicts = ConflictMonitor::new(ConflictMonitorConfig {
            threshold: self.policy.conflict_threshold,
            max_active: self.policy.max_active_conflicts,
            ..ConflictMonitorConfig::default()
        })
        .expect("validated Executive conflict policy must remain valid");
        state.conflict_scopes.clear();
        state.goals.clear();
        state.goal_scopes.clear();
        state.attention_budget = AttentionBudget::new(
            self.policy.attention_budget_capacity,
            self.policy.critical_attention_reserve,
            1.0,
        )
        .expect("validated Executive budget policy must remain valid");
        state.active_plans.clear();
        state.expectations.clear();
        state.expectation_scopes.clear();
        state.decisions = DecisionRecordStore::new(DecisionRecordRetention {
            max_records: self.policy.decision_record_limit,
            ..DecisionRecordRetention::default()
        })
        .expect("validated Executive decision policy must remain valid");
        state.decision_scopes.clear();
        bump(&mut state.version);
    }

    /// Remove only Executive records attributable to one non-global scope.
    /// Explicitly scoped mutation APIs populate the ownership index; legacy
    /// calls remain global and therefore cannot be erased as another user.
    pub fn clear_for_scope_data_erasure(&self, scope: &ExecutiveScope) -> bool {
        if matches!(scope, ExecutiveScope::Global) {
            self.clear_for_data_erasure();
            return true;
        }
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        let conflict_ids = state
            .conflicts
            .active()
            .into_iter()
            .filter(|conflict| {
                state
                    .conflict_scopes
                    .get(&conflict.id)
                    .is_some_and(|existing| existing == scope)
                    || conflict_participates_in_scope(conflict, scope)
            })
            .map(|conflict| conflict.id)
            .collect::<Vec<_>>();
        let mut changed = state
            .conflicts
            .remove_where(|conflict| conflict_ids.contains(&conflict.id))
            > 0;
        for id in conflict_ids {
            state.conflict_scopes.remove(&id);
        }

        let goal_ids = state
            .goals
            .iter()
            .filter(|goal| {
                state
                    .goal_scopes
                    .get(&goal.goal_id)
                    .is_some_and(|existing| existing == scope)
                    || matches!(scope, ExecutiveScope::Goal { goal_id } if *goal_id == goal.goal_id)
            })
            .map(|goal| goal.goal_id)
            .collect::<Vec<_>>();
        if !goal_ids.is_empty() {
            state.goals.retain(|goal| !goal_ids.contains(&goal.goal_id));
            for goal_id in goal_ids {
                state.goal_scopes.remove(&goal_id);
            }
            changed = true;
        }

        let plan_scopes = state
            .active_plans
            .iter()
            .filter(|(plan_scope, plan)| {
                *plan_scope == scope
                    || matches!(scope, ExecutiveScope::Goal { goal_id } if *goal_id == plan.goal_id)
            })
            .map(|(plan_scope, _)| plan_scope.clone())
            .collect::<Vec<_>>();
        if !plan_scopes.is_empty() {
            for plan_scope in plan_scopes {
                state.active_plans.remove(&plan_scope);
            }
            changed = true;
        }

        let expectation_ids = state
            .expectations
            .iter()
            .filter(|expectation| {
                state
                    .expectation_scopes
                    .get(&expectation.id)
                    .is_some_and(|existing| existing == scope)
            })
            .map(|expectation| expectation.id)
            .collect::<Vec<_>>();
        if !expectation_ids.is_empty() {
            state
                .expectations
                .retain(|expectation| !expectation_ids.contains(&expectation.id));
            for id in expectation_ids {
                state.expectation_scopes.remove(&id);
            }
            changed = true;
        }

        let decision_ids = state
            .decisions
            .recent()
            .into_iter()
            .filter(|decision| {
                state
                    .decision_scopes
                    .get(&decision.id)
                    .is_some_and(|existing| existing == scope)
            })
            .map(|decision| decision.id)
            .collect::<Vec<_>>();
        let removed_decisions = state
            .decisions
            .remove_where(|decision| decision_ids.contains(&decision.id));
        if removed_decisions > 0 {
            for decision_id in decision_ids {
                state.decision_scopes.remove(&decision_id);
            }
            changed = true;
        }
        prune_scope_indexes(&mut state);
        if changed {
            bump(&mut state.version);
        }
        changed
    }
}

impl Default for ExecutiveController {
    fn default() -> Self {
        Self::new(ExecutivePolicy::default()).expect("default Executive policy is valid")
    }
}

fn bump(version: &mut u64) {
    *version = version.saturating_add(1).max(1);
}

fn conflict_participates_in_scope(conflict: &ExecutiveConflict, scope: &ExecutiveScope) -> bool {
    match scope {
        ExecutiveScope::Global | ExecutiveScope::Person { .. } => false,
        ExecutiveScope::Conversation { conversation_id } => conflict
            .participants
            .iter()
            .any(|participant| {
                matches!(participant, ConflictRef::Conversation(id) if id == conversation_id)
            }),
        ExecutiveScope::Goal { goal_id } => conflict
            .participants
            .iter()
            .any(|participant| matches!(participant, ConflictRef::Goal(id) if id == goal_id)),
    }
}

fn prune_scope_indexes(state: &mut ExecutiveState) {
    let conflict_ids = state
        .conflicts
        .active()
        .into_iter()
        .map(|conflict| conflict.id)
        .collect::<std::collections::HashSet<_>>();
    state
        .conflict_scopes
        .retain(|id, _| conflict_ids.contains(id));

    let goal_ids = state
        .goals
        .iter()
        .map(|goal| goal.goal_id)
        .collect::<std::collections::HashSet<_>>();
    state.goal_scopes.retain(|id, _| goal_ids.contains(id));

    let expectation_ids = state
        .expectations
        .iter()
        .map(|expectation| expectation.id)
        .collect::<std::collections::HashSet<_>>();
    state
        .expectation_scopes
        .retain(|id, _| expectation_ids.contains(id));

    let decision_ids = state
        .decisions
        .recent()
        .into_iter()
        .map(|decision| decision.id)
        .collect::<std::collections::HashSet<_>>();
    state
        .decision_scopes
        .retain(|id, _| decision_ids.contains(id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CognitiveCapabilitySnapshot, ModelHealth};
    use crate::{
        ActionId, ConversationId, DecisionDisposition, EventId, EventPriority, EventScope,
        GoalState, PlanStep, PlanStepKind, WorldEvent, WorldEventKind,
    };

    fn goal(goal_id: crate::GoalId) -> GoalPrioritySnapshot {
        GoalPrioritySnapshot {
            goal_id,
            score: 0.8,
            hard_priority: None,
            state: GoalState::Active,
        }
    }

    #[test]
    fn scoped_erasure_only_removes_owned_goals_and_conflicts() {
        let controller = ExecutiveController::default();
        let conversation_a = ConversationId::new();
        let conversation_b = ConversationId::new();
        let scope_a = ExecutiveScope::Conversation {
            conversation_id: conversation_a,
        };
        let scope_b = ExecutiveScope::Conversation {
            conversation_id: conversation_b,
        };
        let goal_a = crate::GoalId::new();
        let goal_b = crate::GoalId::new();

        controller
            .set_prioritized_goals_for_scope(scope_a.clone(), vec![goal(goal_a)])
            .expect("scope A goal is valid");
        controller
            .set_prioritized_goals_for_scope(scope_b.clone(), vec![goal(goal_b)])
            .expect("scope B goal is valid");
        let conflict_a = controller
            .detect_conflict_for_scope(
                scope_a.clone(),
                ConflictKind::GoalCompetition,
                0.9,
                0.8,
                vec![ConflictRef::Conversation(conversation_a)],
            )
            .expect("scope A conflict crosses the threshold");
        let conflict_b = controller
            .detect_conflict_for_scope(
                scope_b.clone(),
                ConflictKind::GoalCompetition,
                0.9,
                0.8,
                vec![ConflictRef::Conversation(conversation_b)],
            )
            .expect("scope B conflict crosses the threshold");

        assert!(controller.clear_for_scope_data_erasure(&scope_a));

        let snapshot = controller.snapshot();
        assert_eq!(snapshot.prioritized_goals.len(), 1);
        assert_eq!(snapshot.prioritized_goals[0].goal_id, goal_b);
        assert_eq!(snapshot.active_conflicts.len(), 1);
        assert_eq!(snapshot.active_conflicts[0].id, conflict_b.id);
        assert_ne!(snapshot.active_conflicts[0].id, conflict_a.id);
        assert!(!controller.clear_for_scope_data_erasure(&scope_a));
    }

    #[test]
    fn active_plans_are_isolated_by_scope_and_global_plan_is_only_a_fallback() {
        let controller = ExecutiveController::default();
        let scope_a = ExecutiveScope::Conversation {
            conversation_id: ConversationId::new(),
        };
        let scope_b = ExecutiveScope::Conversation {
            conversation_id: ConversationId::new(),
        };
        let plan_a = PlanState::new(
            PlanId::new(),
            crate::GoalId::new(),
            vec![PlanStep::new(PlanStepKind::Observe)],
            Utc::now(),
        )
        .expect("scope A plan is valid");
        let plan_b = PlanState::new(
            PlanId::new(),
            crate::GoalId::new(),
            vec![PlanStep::new(PlanStepKind::Wait)],
            Utc::now(),
        )
        .expect("scope B plan is valid");
        let plan_global = PlanState::new(
            PlanId::new(),
            crate::GoalId::new(),
            vec![PlanStep::new(PlanStepKind::Evaluate)],
            Utc::now(),
        )
        .expect("global plan is valid");

        controller
            .set_active_plan_for_scope(scope_a.clone(), Some(plan_a.clone()))
            .expect("scope A plan can be installed");
        controller
            .set_active_plan_for_scope(scope_b.clone(), Some(plan_b.clone()))
            .expect("scope B plan can be installed");
        assert_eq!(
            controller.snapshot_for_scope(&scope_a).active_plan,
            Some(plan_a.clone())
        );
        assert_eq!(
            controller.snapshot_for_scope(&scope_b).active_plan,
            Some(plan_b.clone())
        );
        assert!(controller.snapshot().active_plan.is_none());

        controller
            .set_active_plan(Some(plan_global.clone()))
            .expect("global plan can be installed");
        assert_eq!(
            controller
                .snapshot_for_scope(&scope_a)
                .active_plan
                .as_ref()
                .map(|plan| plan.id),
            Some(plan_a.id)
        );
        assert_eq!(controller.snapshot().active_plan, Some(plan_global));

        assert!(controller.clear_for_scope_data_erasure(&scope_a));
        assert!(
            controller
                .snapshot_for_scope(&scope_a)
                .active_plan
                .is_some()
        );
        assert_eq!(
            controller.snapshot_for_scope(&scope_b).active_plan,
            Some(plan_b)
        );
    }

    #[test]
    fn expectation_quota_is_per_scope_and_observation_releases_capacity() {
        let controller = ExecutiveController::new(ExecutivePolicy {
            expectation_limit: 2,
            ..ExecutivePolicy::default()
        })
        .expect("policy is valid");
        let scope_a = ExecutiveScope::Conversation {
            conversation_id: ConversationId::new(),
        };
        let scope_b = ExecutiveScope::Conversation {
            conversation_id: ConversationId::new(),
        };
        for scope in [&scope_a, &scope_a, &scope_b, &scope_b] {
            assert!(
                controller
                    .register_expectation_for_scope(
                        scope.clone(),
                        Expectation::new(
                            ActionId::new(),
                            ExpectedEventPattern::EventType(crate::EventType::IdleTick),
                            0.8,
                            None,
                        ),
                    )
                    .expect("expectation is valid")
            );
        }
        assert!(
            !controller
                .register_expectation_for_scope(
                    scope_a.clone(),
                    Expectation::new(
                        ActionId::new(),
                        ExpectedEventPattern::EventType(crate::EventType::IdleTick),
                        0.8,
                        None,
                    ),
                )
                .expect("expectation is valid")
        );

        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::IdleTick,
        );
        let observed = controller.observe_expectations(&event);
        assert_eq!(observed.satisfied.len(), 4);
        assert!(observed.expired.is_empty());
        assert!(controller.snapshot().pending_expectations.is_empty());
        assert!(
            controller
                .register_expectation_for_scope(
                    scope_a,
                    Expectation::new(
                        ActionId::new(),
                        ExpectedEventPattern::EventType(crate::EventType::IdleTick),
                        0.8,
                        None,
                    ),
                )
                .expect("released expectation capacity can be reused")
        );
    }

    #[test]
    fn global_erasure_clears_user_state_but_retains_capability() {
        let controller = ExecutiveController::default();
        let scope = ExecutiveScope::Conversation {
            conversation_id: ConversationId::new(),
        };
        let goal_id = crate::GoalId::new();
        let capability = CognitiveCapabilitySnapshot::intrinsic(ModelHealth::Healthy, None, true);

        controller
            .set_capability(capability.clone())
            .expect("capability is valid");
        controller
            .set_prioritized_goals_for_scope(scope.clone(), vec![goal(goal_id)])
            .expect("goal is valid");
        controller
            .detect_conflict_for_scope(
                scope.clone(),
                ConflictKind::CapabilityConflict,
                0.9,
                0.8,
                Vec::new(),
            )
            .expect("conflict crosses the threshold");
        controller
            .register_expectation_for_scope(
                scope.clone(),
                Expectation::new(
                    ActionId::new(),
                    ExpectedEventPattern::Custom("idle_tick".to_owned()),
                    0.7,
                    None,
                ),
            )
            .expect("expectation is valid")
            .then_some(())
            .expect("expectation was registered");
        controller
            .record_decision_for_scope(
                scope.clone(),
                DecisionRecord::new(EventId::new(), DecisionDisposition::Silent, Utc::now()),
            )
            .expect("decision is valid")
            .then_some(())
            .expect("decision was recorded");
        let plan = PlanState::new(
            PlanId::new(),
            goal_id,
            vec![PlanStep::new(PlanStepKind::Observe)],
            Utc::now(),
        )
        .expect("plan is valid");
        controller
            .set_active_plan_for_scope(scope, Some(plan))
            .expect("plan is valid");
        let before_version = controller.version();

        controller.clear_for_data_erasure();

        let snapshot = controller.snapshot();
        assert!(snapshot.active_conflicts.is_empty());
        assert!(snapshot.prioritized_goals.is_empty());
        assert!(snapshot.active_plan.is_none());
        assert!(snapshot.pending_expectations.is_empty());
        assert!(snapshot.recent_decisions.is_empty());
        assert_eq!(snapshot.cognitive_capability, capability);
        assert!(snapshot.version > before_version);
    }
}
