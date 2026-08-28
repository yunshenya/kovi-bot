//! Bounded conflict detection and lifecycle.

use super::ConflictId;
use crate::{ActionId, ConversationId, EventId, GoalId, OpenLoopId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use thiserror::Error;

pub const MAX_CONFLICT_PARTICIPANTS: usize = 8;
pub const MAX_CONFLICTS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    BeliefContradiction,
    GoalCompetition,
    GoalConstraintConflict,
    AgendaCompetition,
    ValueConflict,
    SelfConsistencyConflict,
    CapabilityConflict,
    TemporalConflict,
    DuplicateIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStatus {
    Open,
    Deferred,
    Resolved,
    Ignored,
    Expired,
}

impl ConflictStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Ignored | Self::Expired)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum ConflictRef {
    Belief(crate::BeliefId),
    Goal(GoalId),
    OpenLoop(OpenLoopId),
    Action(ActionId),
    Event(EventId),
    Conversation(ConversationId),
    Label(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutiveConflict {
    pub id: ConflictId,
    pub kind: ConflictKind,
    pub severity: f32,
    pub confidence: f32,
    pub participants: Vec<ConflictRef>,
    pub detected_at: DateTime<Utc>,
    pub status: ConflictStatus,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl ExecutiveConflict {
    #[must_use]
    pub fn new(
        kind: ConflictKind,
        severity: f32,
        confidence: f32,
        participants: Vec<ConflictRef>,
        detected_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: ConflictId::new(),
            kind,
            severity: clamp_unit(severity),
            confidence: clamp_unit(confidence),
            participants: participants
                .into_iter()
                .take(MAX_CONFLICT_PARTICIPANTS)
                .collect(),
            detected_at,
            status: ConflictStatus::Open,
            expires_at: None,
        }
    }

    #[must_use]
    pub fn with_expiry(mut self, expires_at: Option<DateTime<Utc>>) -> Self {
        self.expires_at = expires_at;
        self
    }

    pub fn validate(&self) -> Result<(), ConflictValidationError> {
        if !self.severity.is_finite() || !(0.0..=1.0).contains(&self.severity) {
            return Err(ConflictValidationError::OutOfRange { field: "severity" });
        }
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(ConflictValidationError::OutOfRange {
                field: "confidence",
            });
        }
        if self.participants.len() > MAX_CONFLICT_PARTICIPANTS {
            return Err(ConflictValidationError::TooManyParticipants);
        }
        if self
            .expires_at
            .is_some_and(|expires| expires <= self.detected_at)
        {
            return Err(ConflictValidationError::InvalidExpiry);
        }
        Ok(())
    }

    #[must_use]
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.status == ConflictStatus::Open && self.expires_at.is_none_or(|expires| expires > now)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConflictMonitorConfig {
    pub threshold: f32,
    pub max_active: usize,
    pub ttl: Duration,
}

impl Default for ConflictMonitorConfig {
    fn default() -> Self {
        Self {
            threshold: 0.60,
            max_active: 16,
            ttl: Duration::hours(24),
        }
    }
}

impl ConflictMonitorConfig {
    pub fn validate(self) -> Result<(), ConflictValidationError> {
        if !self.threshold.is_finite() || !(0.0..=1.0).contains(&self.threshold) {
            return Err(ConflictValidationError::OutOfRange { field: "threshold" });
        }
        if self.max_active == 0 || self.max_active > MAX_CONFLICTS {
            return Err(ConflictValidationError::InvalidCapacity);
        }
        if self.ttl <= Duration::zero() {
            return Err(ConflictValidationError::InvalidExpiry);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ConflictMonitor {
    config: ConflictMonitorConfig,
    active: VecDeque<ExecutiveConflict>,
}

impl Default for ConflictMonitor {
    fn default() -> Self {
        Self::new(ConflictMonitorConfig::default()).expect("default conflict policy is valid")
    }
}

impl ConflictMonitor {
    pub fn new(config: ConflictMonitorConfig) -> Result<Self, ConflictValidationError> {
        config.validate()?;
        Ok(Self {
            config,
            active: VecDeque::new(),
        })
    }

    /// Record only material conflicts. Equal kind/participants are coalesced
    /// instead of creating an unbounded stream of nearly identical alarms.
    pub fn detect(
        &mut self,
        kind: ConflictKind,
        severity: f32,
        confidence: f32,
        participants: Vec<ConflictRef>,
        now: DateTime<Utc>,
    ) -> Option<ExecutiveConflict> {
        self.purge_expired(now);
        let severity = clamp_unit(severity);
        if severity < self.config.threshold {
            return None;
        }
        let mut participants = participants;
        participants.truncate(MAX_CONFLICT_PARTICIPANTS);
        if let Some(existing) = self.active.iter_mut().find(|existing| {
            existing.status == ConflictStatus::Open
                && existing.expires_at.is_none_or(|expires| expires > now)
                && existing.kind == kind
                && same_participants(&existing.participants, &participants)
        }) {
            existing.severity = existing.severity.max(severity);
            existing.confidence = existing.confidence.max(clamp_unit(confidence));
            existing.detected_at = now;
            existing.expires_at = Some(now + self.config.ttl);
            return Some(existing.clone());
        }
        if self.active.len() >= self.config.max_active {
            self.active.pop_front();
        }
        let conflict = ExecutiveConflict::new(kind, severity, confidence, participants, now)
            .with_expiry(Some(now + self.config.ttl));
        self.active.push_back(conflict.clone());
        Some(conflict)
    }

    pub fn resolve(&mut self, id: ConflictId, status: ConflictStatus) -> bool {
        if !matches!(
            status,
            ConflictStatus::Deferred
                | ConflictStatus::Resolved
                | ConflictStatus::Ignored
                | ConflictStatus::Expired
        ) {
            return false;
        }
        self.active
            .iter_mut()
            .find(|conflict| conflict.id == id)
            .map(|conflict| {
                conflict.status = status;
                true
            })
            .unwrap_or(false)
    }

    pub fn purge_expired(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.active.len();
        for conflict in &mut self.active {
            if conflict.expires_at.is_some_and(|expires| expires <= now)
                && !conflict.status.is_terminal()
            {
                conflict.status = ConflictStatus::Expired;
            }
        }
        self.active
            .retain(|conflict| !conflict.status.is_terminal());
        before - self.active.len()
    }

    #[must_use]
    pub fn active(&self) -> Vec<ExecutiveConflict> {
        self.active_at(Utc::now())
    }

    /// Return only conflicts that are still open at the supplied time. This
    /// keeps deferred/history records available to lifecycle code without
    /// leaking them into planner-facing `active_conflicts`.
    #[must_use]
    pub fn active_at(&self, now: DateTime<Utc>) -> Vec<ExecutiveConflict> {
        self.active
            .iter()
            .filter(|conflict| conflict.is_active_at(now))
            .cloned()
            .collect()
    }

    /// Remove bounded conflict records selected by an erasure or lifecycle
    /// predicate. The monitor owns the queue so callers cannot mutate a
    /// conflict in place without preserving its capacity invariant.
    pub fn remove_where(&mut self, mut predicate: impl FnMut(&ExecutiveConflict) -> bool) -> usize {
        let before = self.active.len();
        self.active.retain(|conflict| !predicate(conflict));
        before - self.active.len()
    }

    /// Replace live conflicts with a validated bounded snapshot.  Only
    /// non-terminal conflicts are meaningful to the controller after a
    /// restart; callers pass the current time so expired persisted entries do
    /// not become active again.
    pub fn restore(
        &mut self,
        conflicts: impl IntoIterator<Item = ExecutiveConflict>,
        now: DateTime<Utc>,
    ) -> Result<(), ConflictValidationError> {
        let mut restored = VecDeque::new();
        for conflict in conflicts {
            conflict.validate()?;
            if conflict.status.is_terminal() || !conflict.is_active_at(now) {
                continue;
            }
            if restored.iter().any(|existing: &ExecutiveConflict| {
                existing.kind == conflict.kind
                    && same_participants(&existing.participants, &conflict.participants)
            }) {
                continue;
            }
            if restored.len() >= self.config.max_active {
                break;
            }
            restored.push_back(conflict);
        }
        self.active = restored;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConflictValidationError {
    #[error("conflict {field} is outside 0..=1")]
    OutOfRange { field: &'static str },
    #[error("conflict has too many participants")]
    TooManyParticipants,
    #[error("conflict capacity is invalid")]
    InvalidCapacity,
    #[error("conflict expiry is invalid")]
    InvalidExpiry,
}

fn clamp_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn same_participants(left: &[ConflictRef], right: &[ConflictRef]) -> bool {
    left.len() == right.len() && left.iter().all(|item| right.contains(item))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_conflicts_are_not_reported_as_active_or_coalesced() {
        let now = Utc::now();
        let mut monitor = ConflictMonitor::new(ConflictMonitorConfig {
            ttl: Duration::hours(1),
            ..ConflictMonitorConfig::default()
        })
        .expect("valid conflict config");
        let first = monitor
            .detect(
                ConflictKind::GoalCompetition,
                0.9,
                0.8,
                vec![ConflictRef::Label("same".to_owned())],
                now,
            )
            .expect("material conflict");
        assert!(monitor.resolve(first.id, ConflictStatus::Deferred));
        assert!(monitor.active_at(now).is_empty());

        let second = monitor
            .detect(
                ConflictKind::GoalCompetition,
                0.9,
                0.8,
                vec![ConflictRef::Label("same".to_owned())],
                now,
            )
            .expect("a deferred conflict does not block a fresh one");
        assert_ne!(first.id, second.id);
        assert_eq!(monitor.active_at(now).len(), 1);
        assert_eq!(monitor.active_at(now)[0].id, second.id);
    }

    #[test]
    fn conflicts_expire_at_ttl_and_are_removed_by_purge() {
        let now = Utc::now();
        let mut monitor = ConflictMonitor::new(ConflictMonitorConfig {
            ttl: Duration::seconds(5),
            ..ConflictMonitorConfig::default()
        })
        .expect("valid conflict config");
        monitor
            .detect(ConflictKind::CapabilityConflict, 0.9, 0.8, Vec::new(), now)
            .expect("material conflict");
        assert_eq!(monitor.active_at(now).len(), 1);
        let after_expiry = now + Duration::seconds(6);
        assert!(monitor.active_at(after_expiry).is_empty());
        assert_eq!(monitor.purge_expired(after_expiry), 1);
        assert!(monitor.active().is_empty());
    }
}
