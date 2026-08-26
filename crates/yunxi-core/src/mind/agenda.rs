use super::common::{
    MindScope, MindValidationError, SCHEMA_VERSION, normalized_key, validate_label, validate_unit,
};
use super::{CuriosityId, InterestId, OpenQuestionId};
use crate::{ConversationId, GoalId, MemoryId, OpenLoopId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashSet;

mind_id!(AgendaItemId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgendaItemKind {
    OpenLoop,
    Curiosity,
    OpenQuestion,
    Goal,
    Interest,
    SalientMemory,
    UnresolvedConversation,
    SocialMotive,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum AgendaSubject {
    OpenLoop(OpenLoopId),
    Curiosity(CuriosityId),
    OpenQuestion(OpenQuestionId),
    Goal(GoalId),
    Interest(InterestId),
    SalientMemory(MemoryId),
    UnresolvedConversation(ConversationId),
    SocialMotive(String),
}

impl AgendaSubject {
    #[must_use]
    pub const fn kind(&self) -> AgendaItemKind {
        match self {
            Self::OpenLoop(_) => AgendaItemKind::OpenLoop,
            Self::Curiosity(_) => AgendaItemKind::Curiosity,
            Self::OpenQuestion(_) => AgendaItemKind::OpenQuestion,
            Self::Goal(_) => AgendaItemKind::Goal,
            Self::Interest(_) => AgendaItemKind::Interest,
            Self::SalientMemory(_) => AgendaItemKind::SalientMemory,
            Self::UnresolvedConversation(_) => AgendaItemKind::UnresolvedConversation,
            Self::SocialMotive(_) => AgendaItemKind::SocialMotive,
        }
    }

    fn validate(&self) -> Result<(), MindValidationError> {
        if let Self::SocialMotive(label) = self {
            validate_label(label.clone(), "agenda social motive")?;
        }
        Ok(())
    }

    #[must_use]
    pub fn dedupe_key(&self) -> String {
        match self {
            Self::OpenLoop(id) => format!("open_loop:{id}"),
            Self::Curiosity(id) => format!("curiosity:{id}"),
            Self::OpenQuestion(id) => format!("open_question:{id}"),
            Self::Goal(id) => format!("goal:{id}"),
            Self::Interest(id) => format!("interest:{id}"),
            Self::SalientMemory(id) => format!("memory:{id}"),
            Self::UnresolvedConversation(id) => format!("conversation:{id}"),
            Self::SocialMotive(value) => format!("social:{}", normalized_key(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgendaSource {
    OpenLoop,
    Goal,
    Curiosity,
    OpenQuestion,
    Interest,
    Memory,
    Reflection,
    Interaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgendaStatus {
    Active,
    Deferred,
    Resolved,
    Dropped,
}

impl AgendaStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Dropped)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Deferred => "deferred",
            Self::Resolved => "resolved",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgendaItem {
    id: AgendaItemId,
    scope: MindScope,
    kind: AgendaItemKind,
    subject: AgendaSubject,
    salience: f32,
    activation: f32,
    stability: f32,
    source: AgendaSource,
    status: AgendaStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_activated_at: DateTime<Utc>,
    deferred_until: Option<DateTime<Utc>>,
    cooldown_until: Option<DateTime<Utc>>,
    version: u64,
    schema_version: u16,
}

impl AgendaItem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: AgendaItemId,
        scope: MindScope,
        subject: AgendaSubject,
        salience: f32,
        activation: f32,
        stability: f32,
        source: AgendaSource,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        let item = Self {
            id,
            scope,
            kind: subject.kind(),
            subject,
            salience: validate_unit(salience, "agenda salience")?,
            activation: validate_unit(activation, "agenda activation")?,
            stability: validate_unit(stability, "agenda stability")?,
            source,
            status: AgendaStatus::Active,
            created_at: now,
            updated_at: now,
            last_activated_at: now,
            deferred_until: None,
            cooldown_until: None,
            version: 1,
            schema_version: SCHEMA_VERSION,
        };
        item.validate()?;
        Ok(item)
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        self.subject.validate()?;
        if self.subject.kind() != self.kind {
            return Err(MindValidationError::InvalidProposal {
                reason: "agenda kind does not match its subject",
            });
        }
        validate_unit(self.salience, "agenda salience")?;
        validate_unit(self.activation, "agenda activation")?;
        validate_unit(self.stability, "agenda stability")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported agenda schema version",
            });
        }
        if self.updated_at < self.created_at || self.last_activated_at < self.created_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "agenda timestamps predate creation",
            });
        }
        if self.status == AgendaStatus::Deferred && self.deferred_until.is_none() {
            return Err(MindValidationError::InvalidProposal {
                reason: "deferred agenda item requires deferred_until",
            });
        }
        if self.status != AgendaStatus::Deferred && self.deferred_until.is_some() {
            return Err(MindValidationError::InvalidProposal {
                reason: "only deferred agenda items may have deferred_until",
            });
        }
        Ok(())
    }

    pub fn activate(
        &self,
        activation: f32,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        validate_unit(activation, "agenda activation")?;
        if self.status.is_terminal() {
            return Err(MindValidationError::InvalidTransition {
                from: self.status.as_str(),
                to: AgendaStatus::Active.as_str(),
            });
        }
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "agenda activation predates stored state",
            });
        }
        let mut updated = self.clone();
        updated.status = AgendaStatus::Active;
        updated.activation = updated.activation.max(activation);
        updated.last_activated_at = now;
        updated.updated_at = now;
        updated.deferred_until = None;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    pub fn defer(
        &self,
        until: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        if self.status.is_terminal() {
            return Err(MindValidationError::InvalidTransition {
                from: self.status.as_str(),
                to: AgendaStatus::Deferred.as_str(),
            });
        }
        if now < self.updated_at || until <= now {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "agenda deferral must be future-directed",
            });
        }
        let mut updated = self.clone();
        updated.status = AgendaStatus::Deferred;
        updated.deferred_until = Some(until);
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    pub fn transition(
        &self,
        next: AgendaStatus,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        if self.status.is_terminal() && self.status != next {
            return Err(MindValidationError::InvalidTransition {
                from: self.status.as_str(),
                to: next.as_str(),
            });
        }
        if next == AgendaStatus::Deferred {
            return Err(MindValidationError::InvalidProposal {
                reason: "use AgendaItem::defer with an explicit deadline",
            });
        }
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "agenda transition predates stored state",
            });
        }
        if self.status == next {
            return Ok(self.clone());
        }
        let mut updated = self.clone();
        updated.status = next;
        updated.deferred_until = None;
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    pub fn with_cooldown(
        &self,
        cooldown_until: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        if now < self.updated_at || cooldown_until.is_some_and(|until| until <= now) {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "agenda cooldown must follow the update",
            });
        }
        let mut updated = self.clone();
        updated.cooldown_until = cooldown_until;
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    pub fn decay(
        &self,
        now: DateTime<Utc>,
        half_life_seconds: f64,
    ) -> Result<Self, MindValidationError> {
        if !half_life_seconds.is_finite() || half_life_seconds <= 0.0 {
            return Err(MindValidationError::InvalidProposal {
                reason: "agenda half-life must be positive and finite",
            });
        }
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "agenda decay predates stored state",
            });
        }
        if self.status.is_terminal() {
            return Ok(self.clone());
        }
        let elapsed = (now - self.updated_at).num_milliseconds().max(0) as f64 / 1_000.0;
        let retention = (-std::f64::consts::LN_2 * elapsed / half_life_seconds).exp() as f32;
        let protection = self.salience.max(self.stability) * 0.25;
        let mut updated = self.clone();
        updated.activation = (updated.activation * retention).max(protection);
        if updated.status == AgendaStatus::Deferred
            && updated.deferred_until.is_some_and(|until| until <= now)
        {
            updated.status = AgendaStatus::Active;
            updated.deferred_until = None;
        }
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    #[must_use]
    pub const fn id(&self) -> AgendaItemId {
        self.id
    }

    #[must_use]
    pub const fn scope(&self) -> MindScope {
        self.scope
    }

    #[must_use]
    pub const fn kind(&self) -> AgendaItemKind {
        self.kind
    }

    #[must_use]
    pub const fn subject(&self) -> &AgendaSubject {
        &self.subject
    }

    #[must_use]
    pub const fn salience(&self) -> f32 {
        self.salience
    }

    #[must_use]
    pub const fn activation(&self) -> f32 {
        self.activation
    }

    #[must_use]
    pub const fn stability(&self) -> f32 {
        self.stability
    }

    #[must_use]
    pub const fn source(&self) -> AgendaSource {
        self.source
    }

    #[must_use]
    pub const fn status(&self) -> AgendaStatus {
        self.status
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    #[must_use]
    pub const fn cooldown_until(&self) -> Option<DateTime<Utc>> {
        self.cooldown_until
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn is_available_at(&self, now: DateTime<Utc>) -> bool {
        self.status == AgendaStatus::Active && self.cooldown_until.is_none_or(|until| until <= now)
    }

    #[must_use]
    pub fn rank_score(&self) -> f32 {
        self.salience * 0.45 + self.activation * 0.4 + self.stability * 0.15
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InnerAgendaLimits {
    pub max_total: usize,
    pub max_per_person: usize,
    pub max_per_conversation: usize,
}

impl Default for InnerAgendaLimits {
    fn default() -> Self {
        Self {
            max_total: 24,
            max_per_person: 12,
            max_per_conversation: 12,
        }
    }
}

impl InnerAgendaLimits {
    pub fn validate(self) -> Result<(), MindValidationError> {
        if self.max_total == 0 || self.max_total > 128 {
            return Err(MindValidationError::InvalidProposal {
                reason: "agenda max_total must be within 1..=128",
            });
        }
        if self.max_per_person == 0
            || self.max_per_conversation == 0
            || self.max_per_person > self.max_total
            || self.max_per_conversation > self.max_total
        {
            return Err(MindValidationError::InvalidProposal {
                reason: "agenda scoped limits must be non-zero and no larger than max_total",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InnerAgenda {
    items: Vec<AgendaItem>,
    version: u64,
    updated_at: DateTime<Utc>,
    schema_version: u16,
}

impl InnerAgenda {
    #[must_use]
    pub const fn empty(now: DateTime<Utc>) -> Self {
        Self {
            items: Vec::new(),
            version: 1,
            updated_at: now,
            schema_version: SCHEMA_VERSION,
        }
    }

    pub fn from_items(
        items: Vec<AgendaItem>,
        version: u64,
        updated_at: DateTime<Utc>,
        limits: InnerAgendaLimits,
    ) -> Result<Self, MindValidationError> {
        let agenda = Self {
            items,
            version,
            updated_at,
            schema_version: SCHEMA_VERSION,
        };
        agenda.validate(limits)?;
        Ok(agenda)
    }

    pub fn validate(&self, limits: InnerAgendaLimits) -> Result<(), MindValidationError> {
        limits.validate()?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported inner-agenda schema version",
            });
        }
        if self.items.len() > limits.max_total {
            return Err(MindValidationError::TooManyItems {
                field: "agenda items",
                length: self.items.len(),
                maximum: limits.max_total,
            });
        }
        let mut ids = HashSet::new();
        let mut active_keys = HashSet::new();
        for item in &self.items {
            item.validate()?;
            if !ids.insert(item.id()) {
                return Err(MindValidationError::Duplicate {
                    field: "agenda item id",
                });
            }
            if !item.status().is_terminal()
                && !active_keys.insert((item.scope(), item.subject().dedupe_key()))
            {
                return Err(MindValidationError::Duplicate {
                    field: "active agenda subject",
                });
            }
        }
        for scope in self.items.iter().map(AgendaItem::scope) {
            let scoped_count = self
                .items
                .iter()
                .filter(|item| item.scope() == scope && !item.status().is_terminal())
                .count();
            let maximum = match scope {
                MindScope::Person { .. } => limits.max_per_person,
                MindScope::Conversation { .. } => limits.max_per_conversation,
                MindScope::Global => limits.max_total,
            };
            if scoped_count > maximum {
                return Err(MindValidationError::TooManyItems {
                    field: "scoped agenda items",
                    length: scoped_count,
                    maximum,
                });
            }
        }
        Ok(())
    }

    pub fn upsert(
        &mut self,
        item: AgendaItem,
        limits: InnerAgendaLimits,
        now: DateTime<Utc>,
    ) -> Result<(), MindValidationError> {
        item.validate()?;
        if let Some(index) = self
            .items
            .iter()
            .position(|stored| stored.id() == item.id())
        {
            self.items[index] = item;
        } else if let Some(index) = self.items.iter().position(|stored| {
            !stored.status().is_terminal()
                && stored.scope() == item.scope()
                && stored.subject().dedupe_key() == item.subject().dedupe_key()
        }) {
            if self.items[index].version() > item.version() {
                return Err(MindValidationError::InvalidProposal {
                    reason: "agenda upsert is stale",
                });
            }
            self.items[index] = item;
        } else {
            self.items.push(item);
        }
        self.prune_to_limits(limits);
        self.updated_at = now;
        self.version = self.version.saturating_add(1);
        self.validate(limits)
    }

    pub fn decay(
        &mut self,
        now: DateTime<Utc>,
        half_life_seconds: f64,
        limits: InnerAgendaLimits,
    ) -> Result<(), MindValidationError> {
        self.items = self
            .items
            .iter()
            .map(|item| item.decay(now, half_life_seconds))
            .collect::<Result<_, _>>()?;
        self.items.retain(|item| {
            !item.status().is_terminal()
                || now.signed_duration_since(item.updated_at()).num_days() < 7
        });
        self.prune_to_limits(limits);
        self.updated_at = now;
        self.version = self.version.saturating_add(1);
        self.validate(limits)
    }

    fn prune_to_limits(&mut self, limits: InnerAgendaLimits) {
        self.items.sort_by(|left, right| {
            right
                .rank_score()
                .partial_cmp(&left.rank_score())
                .unwrap_or(Ordering::Equal)
                .then_with(|| right.updated_at().cmp(&left.updated_at()))
        });
        let mut person_counts = std::collections::HashMap::new();
        let mut conversation_counts = std::collections::HashMap::new();
        self.items.retain(|item| {
            if item.status().is_terminal() {
                return true;
            }
            match item.scope() {
                MindScope::Person { person_id } => {
                    let count = person_counts.entry(person_id).or_insert(0usize);
                    *count += 1;
                    *count <= limits.max_per_person
                }
                MindScope::Conversation { conversation_id } => {
                    let count = conversation_counts.entry(conversation_id).or_insert(0usize);
                    *count += 1;
                    *count <= limits.max_per_conversation
                }
                MindScope::Global => true,
            }
        });
        self.items.truncate(limits.max_total);
    }

    #[must_use]
    pub fn items(&self) -> &[AgendaItem] {
        &self.items
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }
}
