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

/// Result of observing one event against the bounded pending set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpectationObservation {
    pub satisfied: Vec<ExpectationId>,
    pub expired: Vec<ExpectationId>,
}

impl ExpectationObservation {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.satisfied.is_empty() && self.expired.is_empty()
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
        }
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
}
