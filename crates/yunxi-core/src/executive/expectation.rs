//! Bounded workflow expectations, distinct from OpenLoop memory.

use super::ExpectationId;
use crate::{ActionId, EventType, WorldEvent, WorldEventKind};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const MAX_EXPECTATIONS: usize = 64;
pub const MAX_EXPECTATION_TEXT_BYTES: usize = 2 * 1_024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ExpectedEventPattern {
    EventType(EventType),
    MessageContains(String),
    ToolCompleted { operation: String },
    ToolFailed { operation: String },
    ActionSucceeded { idempotency_key: String },
    Custom(String),
}

impl ExpectedEventPattern {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::MessageContains(value)
            | Self::Custom(value)
            | Self::ToolCompleted { operation: value }
            | Self::ToolFailed { operation: value }
            | Self::ActionSucceeded {
                idempotency_key: value,
            } if value.is_empty() || value.len() > MAX_EXPECTATION_TEXT_BYTES => {
                Err("expectation pattern text is out of bounds")
            }
            _ => Ok(()),
        }
    }

    #[must_use]
    pub fn matches(&self, event: &WorldEvent) -> bool {
        match self {
            Self::EventType(event_type) => event.kind().event_type() == *event_type,
            Self::MessageContains(needle) => match event.kind() {
                WorldEventKind::MessageReceived(message) => {
                    message.content.as_text().contains(needle)
                }
                WorldEventKind::MessageSent(message) => message
                    .content
                    .as_ref()
                    .is_some_and(|content| content.as_text().contains(needle)),
                _ => false,
            },
            Self::ToolCompleted { operation } => match event.kind() {
                WorldEventKind::ToolCompleted(result) => &result.operation == operation,
                _ => false,
            },
            Self::ToolFailed { operation } => match event.kind() {
                WorldEventKind::ToolFailed(result) => &result.operation == operation,
                _ => false,
            },
            Self::ActionSucceeded { idempotency_key } => match event.kind() {
                WorldEventKind::ActionSucceeded(result) => {
                    &result.idempotency_key == idempotency_key
                }
                _ => false,
            },
            Self::Custom(value) => {
                format!("{:?}", event.kind().event_type()).eq_ignore_ascii_case(value)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectationStatus {
    Pending,
    Satisfied,
    Violated,
    Expired,
    Cancelled,
}

impl ExpectationStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One expectation that reached a terminal status, with the expectation itself.
///
/// [`ExpectationObservation`] reports identifiers only, which is enough to
/// count outcomes but not to act on them: a consumer that wants to tell a task
/// "the thing you expected did not happen" needs the pattern. The tracker drops
/// terminal expectations in the same pass, so this is the last moment the
/// expectation can be read.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedExpectation {
    pub expectation: Expectation,
    pub status: ExpectationStatus,
}

/// Result of observing one event against the bounded pending set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpectationObservation {
    pub satisfied: Vec<ExpectationId>,
    pub expired: Vec<ExpectationId>,
    /// 被显式判定为"没发生"的预期。
    ///
    /// `Expectation::observe` 自己只会产生 Pending/Satisfied/Expired，这两个终态来自
    /// `violate()`/`cancel()`。把它们一并报出来，是因为 `observe_expectations` 观察完
    /// 会 `retain(Pending)`：如果它们落进空分支，既不会被上报、也不会当场清理（要等
    /// 下一条别的预期变动才顺带删掉）——将来谁真的接上这两个状态，就会变成"静默消失"。
    pub violated: Vec<ExpectationId>,
    pub cancelled: Vec<ExpectationId>,
}

impl ExpectationObservation {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.satisfied.is_empty()
            && self.expired.is_empty()
            && self.violated.is_empty()
            && self.cancelled.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expectation {
    pub id: ExpectationId,
    pub source_action_id: ActionId,
    pub expected_event: ExpectedEventPattern,
    pub confidence: f32,
    pub expires_at: Option<DateTime<Utc>>,
    pub status: ExpectationStatus,
    /// The task (trace root) this expectation belongs to, when Core registered
    /// it on a turn's behalf.
    ///
    /// An expectation describes what a turn expected to happen next, so when it
    /// resolves the result has to reach *that* task's working memory. Without
    /// this the runtime could only report that some expectation somewhere
    /// ended, which no follow-up round can act on.
    ///
    /// Never serialized: a trace root identifies an event in *this* process's
    /// queue, and a host persists expectations. Restoring a stale root after a
    /// restart would route a resolution into a task that no longer exists,
    /// creating working memory nothing can ever release. A restored
    /// expectation therefore has no task, which is the honest answer — its task
    /// did not survive the process either.
    #[serde(skip)]
    pub trace_root: Option<crate::EventId>,
}

pub type ExpectationSnapshot = Expectation;

impl Expectation {
    #[must_use]
    pub fn new(
        source_action_id: ActionId,
        expected_event: ExpectedEventPattern,
        confidence: f32,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id: ExpectationId::new(),
            source_action_id,
            expected_event,
            confidence: if confidence.is_finite() {
                confidence.clamp(0.0, 1.0)
            } else {
                0.0
            },
            expires_at,
            status: ExpectationStatus::Pending,
            trace_root: None,
        }
    }

    /// Binds this expectation to the task that formed it.
    #[must_use]
    pub const fn for_trace(mut self, trace_root: crate::EventId) -> Self {
        self.trace_root = Some(trace_root);
        self
    }

    /// The task that formed this expectation, when Core registered it.
    #[must_use]
    pub const fn trace_root(&self) -> Option<crate::EventId> {
        self.trace_root
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err("expectation confidence must be within 0..=1");
        }
        self.expected_event.validate()
    }

    #[must_use]
    pub fn observe(&mut self, event: &WorldEvent, now: DateTime<Utc>) -> ExpectationStatus {
        if self.status != ExpectationStatus::Pending {
            return self.status;
        }
        if self.expires_at.is_some_and(|expires| expires <= now) {
            self.status = ExpectationStatus::Expired;
        } else if self.expected_event.matches(event) {
            self.status = ExpectationStatus::Satisfied;
        }
        self.status
    }

    pub fn expire_if_due(&mut self, now: DateTime<Utc>) -> bool {
        if self.status == ExpectationStatus::Pending
            && self.expires_at.is_some_and(|expires| expires <= now)
        {
            self.status = ExpectationStatus::Expired;
            true
        } else {
            false
        }
    }

    pub fn cancel(&mut self) -> bool {
        if self.status != ExpectationStatus::Pending {
            return false;
        }
        self.status = ExpectationStatus::Cancelled;
        true
    }

    pub fn violate(&mut self) -> bool {
        if self.status != ExpectationStatus::Pending {
            return false;
        }
        self.status = ExpectationStatus::Violated;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectationTrackerConfig {
    pub max_pending: usize,
}

impl Default for ExpectationTrackerConfig {
    fn default() -> Self {
        Self { max_pending: 8 }
    }
}

#[derive(Debug, Default)]
pub struct ExpectationTracker {
    config: ExpectationTrackerConfig,
    pending: VecDeque<Expectation>,
}

impl ExpectationTracker {
    pub fn new(config: ExpectationTrackerConfig) -> Result<Self, &'static str> {
        if config.max_pending == 0 || config.max_pending > MAX_EXPECTATIONS {
            return Err("expectation capacity is out of bounds");
        }
        Ok(Self {
            config,
            pending: VecDeque::new(),
        })
    }

    pub fn register(&mut self, expectation: Expectation) -> Result<bool, &'static str> {
        expectation.validate()?;
        if self.pending.iter().any(|item| {
            item.source_action_id == expectation.source_action_id
                && item.status == ExpectationStatus::Pending
        }) {
            return Ok(false);
        }
        if self.pending.len() >= self.config.max_pending {
            return Ok(false);
        }
        self.pending.push_back(expectation);
        Ok(true)
    }

    pub fn observe(&mut self, event: &WorldEvent, now: DateTime<Utc>) -> Vec<ExpectationId> {
        let mut satisfied = Vec::new();
        for expectation in &mut self.pending {
            if expectation.observe(event, now) == ExpectationStatus::Satisfied {
                satisfied.push(expectation.id);
            }
        }
        self.prune_terminal();
        satisfied
    }

    pub fn expire(&mut self, now: DateTime<Utc>) -> usize {
        let mut expired = 0;
        for expectation in &mut self.pending {
            expired += usize::from(expectation.expire_if_due(now));
        }
        self.prune_terminal();
        expired
    }

    pub fn cancel(&mut self, id: ExpectationId) -> bool {
        let Some(expectation) = self.pending.iter_mut().find(|item| item.id == id) else {
            return false;
        };
        let changed = expectation.cancel();
        self.prune_terminal();
        changed
    }

    fn prune_terminal(&mut self) {
        self.pending
            .retain(|expectation| expectation.status == ExpectationStatus::Pending);
    }

    #[must_use]
    pub fn pending(&self) -> Vec<Expectation> {
        self.pending.iter().cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_root_never_survives_serialization() {
        // The root names an event in this process's queue, and hosts persist
        // expectations. A restored stale root would route a resolution into a
        // task that no longer exists — working memory nothing can release.
        let expectation = Expectation::new(
            ActionId::new(),
            ExpectedEventPattern::EventType(crate::EventType::IdleTick),
            0.8,
            None,
        )
        .for_trace(crate::EventId::new());
        assert!(expectation.trace_root().is_some());
        let encoded = serde_json::to_value(&expectation).expect("serializes");
        assert!(
            encoded.get("trace_root").is_none(),
            "trace root leaked into the persisted form: {encoded}"
        );
        let restored: Expectation = serde_json::from_value(encoded).expect("deserializes");
        assert_eq!(restored.trace_root(), None);
        // Everything the host durably needs is still there.
        assert_eq!(restored.id, expectation.id);
        assert_eq!(restored.expected_event, expectation.expected_event);
    }
}
