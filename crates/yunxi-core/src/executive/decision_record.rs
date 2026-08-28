//! Bounded decision metadata. No prompt or hidden reasoning is retained.

use super::DecisionRecordId;
use crate::model::{CognitiveTier, IntrinsicModelVersion};
use crate::planner::DecisionDisposition;
use crate::{ActionId, EventId, GoalId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const MAX_REASON_TAGS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutiveReasonTag {
    GoalPreempted,
    GoalAged,
    ConflictHigh,
    ConfidenceLow,
    ExpectationPending,
    ExpectationViolated,
    PlanStale,
    PlanRevised,
    BudgetLow,
    BudgetReserved,
    SocialInterruptHigh,
    SelfConsistencyConflict,
    ReflectionDeferred,
    ReflectionRequired,
    CandidateDominated,
    CognitiveTierIntrinsic,
    CognitiveTierDowngraded,
    StrongModelUnavailable,
    IntrinsicModelUnavailable,
    IntrinsicFallbackUsed,
    ReflexOnly,
    DirectPreemptsProactive,
    OutgoingSuperseded,
    OutgoingDeferred,
    OutgoingRewritten,
    OutgoingMerged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionActionKind {
    SendMessage,
    ReachOut,
    UseTool,
    CreateOpenLoop,
    ResolveOpenLoop,
    StartGoal,
    CancelGoal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: DecisionRecordId,
    pub event_id: EventId,
    pub disposition: DecisionDisposition,
    pub selected_action: Option<DecisionActionKind>,
    pub selected_action_id: Option<ActionId>,
    pub reason_tags: Vec<ExecutiveReasonTag>,
    pub relevant_goals: Vec<GoalId>,
    pub relevant_agenda_items: Vec<crate::AgendaItemId>,
    pub relevant_conflicts: Vec<super::ConflictId>,
    pub confidence: f32,
    pub selected_cognitive_tier: CognitiveTier,
    pub fallback_used: bool,
    pub intrinsic_model_version: Option<IntrinsicModelVersion>,
    pub created_at: DateTime<Utc>,
}

pub type DecisionRecordSnapshot = DecisionRecord;

impl DecisionRecord {
    #[must_use]
    pub fn new(event_id: EventId, disposition: DecisionDisposition, now: DateTime<Utc>) -> Self {
        Self {
            id: DecisionRecordId::new(),
            event_id,
            disposition,
            selected_action: None,
            selected_action_id: None,
            reason_tags: Vec::new(),
            relevant_goals: Vec::new(),
            relevant_agenda_items: Vec::new(),
            relevant_conflicts: Vec::new(),
            confidence: 0.0,
            selected_cognitive_tier: CognitiveTier::Reflex,
            fallback_used: false,
            intrinsic_model_version: None,
            created_at: now,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err("decision confidence must be within 0..=1");
        }
        if self.reason_tags.len() > MAX_REASON_TAGS {
            return Err("decision reason tags exceed the bound");
        }
        if let Some(version) = &self.intrinsic_model_version {
            version.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionRecordRetention {
    pub max_records: usize,
    pub ttl: Duration,
}

impl Default for DecisionRecordRetention {
    fn default() -> Self {
        Self {
            max_records: 32,
            ttl: Duration::hours(24),
        }
    }
}

#[derive(Debug)]
pub struct DecisionRecordStore {
    retention: DecisionRecordRetention,
    records: VecDeque<DecisionRecord>,
}

impl Default for DecisionRecordStore {
    fn default() -> Self {
        Self::new(DecisionRecordRetention::default()).expect("default retention is valid")
    }
}

impl DecisionRecordStore {
    pub fn new(retention: DecisionRecordRetention) -> Result<Self, &'static str> {
        if retention.max_records == 0
            || retention.max_records > 256
            || retention.ttl <= Duration::zero()
        {
            return Err("decision retention is invalid");
        }
        Ok(Self {
            retention,
            records: VecDeque::new(),
        })
    }

    pub fn record(
        &mut self,
        record: DecisionRecord,
        now: DateTime<Utc>,
    ) -> Result<bool, &'static str> {
        record.validate()?;
        self.purge(now);
        if self
            .records
            .iter()
            .any(|existing| existing.event_id == record.event_id)
        {
            return Ok(false);
        }
        if self.records.len() >= self.retention.max_records {
            self.records.pop_front();
        }
        self.records.push_back(record);
        Ok(true)
    }

    pub fn purge(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.records.len();
        let ttl = self.retention.ttl;
        self.records.retain(|record| now - record.created_at < ttl);
        before - self.records.len()
    }

    #[must_use]
    pub fn recent(&self) -> Vec<DecisionRecord> {
        self.records.iter().cloned().collect()
    }

    /// Remove records selected by a bounded lifecycle or data-erasure
    /// predicate while preserving insertion order for the remaining records.
    pub fn remove_where(&mut self, mut predicate: impl FnMut(&DecisionRecord) -> bool) -> usize {
        let before = self.records.len();
        self.records.retain(|record| !predicate(record));
        before - self.records.len()
    }

    /// Restore only records that are still within the configured retention
    /// window.  Event-id deduplication is kept identical to the live append
    /// path so a replayed event cannot acquire a second decision.
    pub fn restore(
        &mut self,
        records: impl IntoIterator<Item = DecisionRecord>,
        now: DateTime<Utc>,
    ) -> Result<(), &'static str> {
        let mut restored = VecDeque::new();
        for record in records {
            record.validate()?;
            if now - record.created_at >= self.retention.ttl {
                continue;
            }
            if restored
                .iter()
                .any(|existing: &DecisionRecord| existing.event_id == record.event_id)
            {
                continue;
            }
            if restored.len() >= self.retention.max_records {
                restored.pop_front();
            }
            restored.push_back(record);
        }
        self.records = restored;
        Ok(())
    }
}
