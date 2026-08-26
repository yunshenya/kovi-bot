use super::{
    AgendaUpdateProposal, BeliefUpdateProposal, Episode, InterestUpdateProposal, MindReasonTag,
    MindScope, MindSnapshot, MindValidationError, OpenQuestionUpdateProposal,
    PreferenceUpdateProposal,
};
use crate::{EventId, TraceContext};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Mutex;

pub const MAX_REFLECTION_EVENTS: usize = 32;
pub const MAX_REFLECTION_CONTEXT_ITEMS: usize = 32;
pub const MAX_REFLECTION_UPDATES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionTrigger {
    Idle,
    Maintenance,
    ConversationLikelyEnded,
    HighSalienceEvent,
    MemoryPressure,
    AgendaPressure,
    DayBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionDepth {
    Light,
    Deep,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflectionEvent {
    pub event_id: EventId,
    pub scope: MindScope,
    pub summary: String,
    pub salience: f32,
    pub occurred_at: DateTime<Utc>,
}

impl ReflectionEvent {
    pub fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_summary(self.summary.clone(), "reflection event summary")?;
        super::common::validate_unit(self.salience, "reflection event salience")?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflectionInput {
    pub trigger: ReflectionTrigger,
    pub depth: ReflectionDepth,
    pub scope: MindScope,
    pub recent_events: Vec<ReflectionEvent>,
    pub salient_memories: Vec<String>,
    pub open_loop_summaries: Vec<String>,
    pub goal_summaries: Vec<String>,
    pub mind: MindSnapshot,
    pub requested_at: DateTime<Utc>,
    pub trace: TraceContext,
}

impl ReflectionInput {
    pub fn validate(&self) -> Result<(), MindValidationError> {
        if self.recent_events.len() > MAX_REFLECTION_EVENTS {
            return Err(MindValidationError::TooManyItems {
                field: "reflection events",
                length: self.recent_events.len(),
                maximum: MAX_REFLECTION_EVENTS,
            });
        }
        for (field, values) in [
            ("reflection memories", &self.salient_memories),
            ("reflection open loops", &self.open_loop_summaries),
            ("reflection goals", &self.goal_summaries),
        ] {
            if values.len() > MAX_REFLECTION_CONTEXT_ITEMS {
                return Err(MindValidationError::TooManyItems {
                    field,
                    length: values.len(),
                    maximum: MAX_REFLECTION_CONTEXT_ITEMS,
                });
            }
            for value in values {
                super::common::validate_summary(value.clone(), field)?;
            }
        }
        for event in &self.recent_events {
            event.validate()?;
            if event.occurred_at > self.requested_at {
                return Err(MindValidationError::InvalidTimestamp {
                    reason: "reflection event occurs after request",
                });
            }
        }
        self.mind.validate()?;
        Ok(())
    }

    #[must_use]
    pub fn should_reflect(&self) -> bool {
        let high_salience = self.recent_events.iter().any(|event| event.salience >= 0.7);
        let unresolved = !self.mind.open_questions().is_empty()
            || !self.mind.agenda().is_empty()
            || !self.open_loop_summaries.is_empty()
            || !self.goal_summaries.is_empty();
        high_salience
            || unresolved
            || matches!(
                self.trigger,
                ReflectionTrigger::HighSalienceEvent
                    | ReflectionTrigger::MemoryPressure
                    | ReflectionTrigger::AgendaPressure
                    | ReflectionTrigger::DayBoundary
            )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflectionProposal {
    pub base_snapshot_version: u64,
    pub scope: MindScope,
    pub episodes: Vec<Episode>,
    pub belief_updates: Vec<BeliefUpdateProposal>,
    pub preference_updates: Vec<PreferenceUpdateProposal>,
    pub interest_updates: Vec<InterestUpdateProposal>,
    pub open_question_updates: Vec<OpenQuestionUpdateProposal>,
    pub agenda_updates: Vec<AgendaUpdateProposal>,
    pub reason_tags: Vec<MindReasonTag>,
    pub proposed_at: DateTime<Utc>,
    pub trace: TraceContext,
}

impl ReflectionProposal {
    #[must_use]
    pub fn empty(input: &ReflectionInput) -> Self {
        Self {
            base_snapshot_version: input.mind.version(),
            scope: input.scope,
            episodes: Vec::new(),
            belief_updates: Vec::new(),
            preference_updates: Vec::new(),
            interest_updates: Vec::new(),
            open_question_updates: Vec::new(),
            agenda_updates: Vec::new(),
            reason_tags: Vec::new(),
            proposed_at: input.requested_at,
            trace: input.trace,
        }
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        for (field, length) in [
            ("reflection episodes", self.episodes.len()),
            ("reflection belief updates", self.belief_updates.len()),
            (
                "reflection preference updates",
                self.preference_updates.len(),
            ),
            ("reflection interest updates", self.interest_updates.len()),
            (
                "reflection open-question updates",
                self.open_question_updates.len(),
            ),
            ("reflection agenda updates", self.agenda_updates.len()),
        ] {
            if length > MAX_REFLECTION_UPDATES {
                return Err(MindValidationError::TooManyItems {
                    field,
                    length,
                    maximum: MAX_REFLECTION_UPDATES,
                });
            }
        }
        if self.reason_tags.len() > MAX_REFLECTION_UPDATES {
            return Err(MindValidationError::TooManyItems {
                field: "reflection reason tags",
                length: self.reason_tags.len(),
                maximum: MAX_REFLECTION_UPDATES,
            });
        }
        for episode in &self.episodes {
            episode.validate()?;
        }
        for update in &self.belief_updates {
            update.validate()?;
        }
        for update in &self.preference_updates {
            update.validate()?;
        }
        for update in &self.interest_updates {
            update.validate()?;
        }
        for update in &self.open_question_updates {
            update.validate()?;
        }
        for update in &self.agenda_updates {
            update.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionQueueConfig {
    pub capacity: usize,
}

impl Default for ReflectionQueueConfig {
    fn default() -> Self {
        Self { capacity: 64 }
    }
}

#[derive(Debug)]
pub struct ReflectionQueue {
    capacity: usize,
    pending: Mutex<VecDeque<ReflectionInput>>,
}

impl ReflectionQueue {
    pub fn new(config: ReflectionQueueConfig) -> Result<Self, MindValidationError> {
        if config.capacity == 0 || config.capacity > 1_024 {
            return Err(MindValidationError::InvalidProposal {
                reason: "reflection queue capacity must be within 1..=1024",
            });
        }
        Ok(Self {
            capacity: config.capacity,
            pending: Mutex::new(VecDeque::new()),
        })
    }

    pub fn enqueue(&self, input: ReflectionInput) -> Result<bool, MindValidationError> {
        input.validate()?;
        let mut pending = self.pending.lock().unwrap_or_else(|lock| lock.into_inner());
        if let Some(existing) = pending
            .iter_mut()
            .find(|queued| queued.scope == input.scope)
        {
            if input.requested_at >= existing.requested_at {
                *existing = input;
            }
            return Ok(false);
        }
        if pending.len() == self.capacity {
            pending.pop_front();
        }
        pending.push_back(input);
        Ok(true)
    }

    #[must_use]
    pub fn dequeue(&self) -> Option<ReflectionInput> {
        self.pending
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .pop_front()
    }

    /// Invalidates queued work for scopes that are entering a data-erasure
    /// barrier. The caller must still block new producers before calling this
    /// method; purging alone is not a write barrier.
    pub fn purge_scopes(&self, scopes: &[MindScope]) -> usize {
        if scopes.is_empty() {
            return 0;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|lock| lock.into_inner());
        let before = pending.len();
        pending.retain(|input| !scopes.contains(&input.scope));
        before - pending.len()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
