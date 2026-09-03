//! EntityState: platform-neutral abstraction over external entities.
//!
//! v1 understands the kinds that actually matter (Person, Conversation, Host,
//! Tool) plus a small closed set; it never grows into a psychographic
//! database of the user (v4 §15–20, §179).

use super::observation::ObservationSource;
use super::{
    WorldScope, WorldValidationError,
    common::{clamp_unit, dedupe, validate_unit, validate_value},
};
use crate::{ConversationId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_PROPERTIES_PER_ENTITY: usize = 32;
/// Runtime cap on entities kept in the in-memory index.
pub const MAX_ACTIVE_ENTITIES: usize = 1_024;
/// Cap per person / conversation scope.
pub const MAX_ENTITIES_PER_SCOPE: usize = 64;

/// External entity categories (v4 §16) — only the needed ones first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Person,
    Conversation,
    Host,
    Tool,
    Place,
    Topic,
    Resource,
    ExternalService,
    GoalContext,
    Unknown,
}

/// One property of an entity with its own confidence, source and validity
/// window. No property is ever a bare "fact" (v4 §19).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateProperty {
    key: String,
    value: String,
    confidence: f32,
    source: ObservationSource,
    valid_from: DateTime<Utc>,
    valid_until: Option<DateTime<Utc>>,
}

impl StateProperty {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: impl Into<String>,
        value: impl Into<String>,
        confidence: f32,
        source: ObservationSource,
        valid_from: DateTime<Utc>,
        valid_until: Option<DateTime<Utc>>,
    ) -> Result<Self, WorldValidationError> {
        let property = Self {
            key: validate_value(key, "property key")?,
            value: validate_value(value, "property value")?,
            confidence: clamp_unit(confidence),
            source,
            valid_from,
            valid_until,
        };
        property.validate()?;
        Ok(property)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_value(self.key.clone(), "property key")?;
        validate_value(self.value.clone(), "property value")?;
        validate_unit(self.confidence, "property confidence")?;
        if let Some(valid_until) = self.valid_until
            && valid_until < self.valid_from
        {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "property validity window is inverted",
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn source(&self) -> ObservationSource {
        self.source
    }

    #[must_use]
    pub const fn valid_from(&self) -> DateTime<Utc> {
        self.valid_from
    }

    #[must_use]
    pub const fn valid_until(&self) -> Option<DateTime<Utc>> {
        self.valid_until
    }

    /// Is this property still valid at `now` (within its window)?
    #[must_use]
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        self.valid_from <= now && self.valid_until.is_none_or(|until| now <= until)
    }

    #[must_use]
    pub fn freshness_at(&self, now: DateTime<Utc>) -> super::Freshness {
        super::temporal::freshness_at(self.valid_from, self.valid_until, now)
    }
}

/// Entity state itself (v4 §18).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityState {
    id: super::EntityId,
    kind: EntityKind,
    linked_person: Option<PersonId>,
    linked_conversation: Option<ConversationId>,
    properties: Vec<StateProperty>,
    confidence: f32,
    last_observed_at: DateTime<Utc>,
    version: u64,
}

impl EntityState {
    pub fn new(
        id: super::EntityId,
        kind: EntityKind,
        linked_person: Option<PersonId>,
        linked_conversation: Option<ConversationId>,
        confidence: f32,
        last_observed_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let state = Self {
            id,
            kind,
            linked_person,
            linked_conversation,
            properties: Vec::new(),
            confidence: clamp_unit(confidence),
            last_observed_at,
            version: 1,
        };
        state.validate()?;
        Ok(state)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.confidence, "entity confidence")?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        if self.properties.len() > MAX_PROPERTIES_PER_ENTITY {
            return Err(WorldValidationError::TooManyItems {
                field: "entity properties",
                length: self.properties.len(),
                maximum: MAX_PROPERTIES_PER_ENTITY,
            });
        }
        for property in &self.properties {
            property.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::EntityId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    #[must_use]
    pub const fn linked_person(&self) -> Option<PersonId> {
        self.linked_person
    }

    #[must_use]
    pub const fn linked_conversation(&self) -> Option<ConversationId> {
        self.linked_conversation
    }

    #[must_use]
    pub fn properties(&self) -> &[StateProperty] {
        &self.properties
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn last_observed_at(&self) -> DateTime<Utc> {
        self.last_observed_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn scope(&self) -> WorldScope {
        match (self.linked_person, self.linked_conversation) {
            (Some(person_id), _) => WorldScope::Person { person_id },
            (None, Some(conversation_id)) => WorldScope::Conversation { conversation_id },
            (None, None) => WorldScope::Global,
        }
    }

    pub fn property(&self, key: &str) -> Option<&StateProperty> {
        self.properties
            .iter()
            .find(|property| property.key() == key)
    }

    /// Apply one property action (set/clear) and bump the version.
    pub fn apply(
        &mut self,
        action: &EntityUpdateAction,
        observed_at: DateTime<Utc>,
    ) -> Result<(), WorldValidationError> {
        match action {
            EntityUpdateAction::Set(property) => {
                property.validate()?;
                if let Some(existing) = self
                    .properties
                    .iter_mut()
                    .find(|existing| existing.key() == property.key())
                {
                    *existing = property.clone();
                } else {
                    if self.properties.len() >= MAX_PROPERTIES_PER_ENTITY {
                        return Err(WorldValidationError::TooManyItems {
                            field: "entity properties",
                            length: self.properties.len(),
                            maximum: MAX_PROPERTIES_PER_ENTITY,
                        });
                    }
                    self.properties.push(property.clone());
                }
            }
            EntityUpdateAction::Clear { key } => {
                validate_value(key.clone(), "property key")?;
                self.properties.retain(|existing| existing.key() != key);
            }
        }
        if observed_at >= self.last_observed_at {
            self.last_observed_at = observed_at;
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub(crate) fn set_confidence(
        &mut self,
        confidence: f32,
        observed_at: DateTime<Utc>,
    ) -> Result<(), WorldValidationError> {
        self.confidence = clamp_unit(confidence);
        if observed_at >= self.last_observed_at {
            self.last_observed_at = observed_at;
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    #[must_use]
    pub fn matches_kind_and_links(
        &self,
        kind: EntityKind,
        linked_person: Option<PersonId>,
        linked_conversation: Option<ConversationId>,
    ) -> bool {
        self.kind == kind
            && self.linked_person == linked_person
            && self.linked_conversation == linked_conversation
    }
}

/// One mutation to an entity's properties (v4 §19 merge policy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum EntityUpdateAction {
    Set(StateProperty),
    Clear { key: String },
}

/// The internal "set/clear" sub-enum used by mutation loops.
pub type EntityUpdate = EntityUpdateAction;

/// Rust-validated proposal from the model/host layer (v4 §87).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityUpdateProposal {
    entity_id: Option<super::EntityId>,
    kind: EntityKind,
    linked_person: Option<PersonId>,
    linked_conversation: Option<ConversationId>,
    confidence: f32,
    actions: Vec<EntityUpdateAction>,
    observed_at: DateTime<Utc>,
}

impl EntityUpdateProposal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        entity_id: Option<super::EntityId>,
        kind: EntityKind,
        linked_person: Option<PersonId>,
        linked_conversation: Option<ConversationId>,
        confidence: f32,
        actions: Vec<EntityUpdateAction>,
        observed_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let mut proposal = Self {
            entity_id,
            kind,
            linked_person,
            linked_conversation,
            confidence: clamp_unit(confidence),
            actions,
            observed_at,
        };
        proposal.actions = dedupe(proposal.actions, "entity update actions", false)?;
        proposal.validate()?;
        Ok(proposal)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.confidence, "entity update confidence")?;
        if self.actions.is_empty() {
            return Err(WorldValidationError::InvalidState {
                reason: "entity update has no actions",
            });
        }
        if let Some(_) = self.entity_id
            && self.linked_person.is_none()
            && self.linked_conversation.is_none()
        {
            return Err(WorldValidationError::InvalidScope {
                reason: "entity update with a fixed id needs a scope link",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn entity_id(&self) -> Option<super::EntityId> {
        self.entity_id
    }

    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    #[must_use]
    pub const fn linked_person(&self) -> Option<PersonId> {
        self.linked_person
    }

    #[must_use]
    pub const fn linked_conversation(&self) -> Option<ConversationId> {
        self.linked_conversation
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub fn actions(&self) -> &[EntityUpdateAction] {
        &self.actions
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
}

/// Bounded in-memory index of entity states (v4 §133: active set + store).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EntityStateIndex {
    entities: Vec<EntityState>,
}

impl EntityStateIndex {
    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.entities.len() > MAX_ACTIVE_ENTITIES {
            return Err(WorldValidationError::TooManyItems {
                field: "entities",
                length: self.entities.len(),
                maximum: MAX_ACTIVE_ENTITIES,
            });
        }
        for entity in &self.entities {
            entity.validate()?;
        }
        let mut ids = Vec::new();
        for entity in &self.entities {
            if ids.contains(&entity.id()) {
                return Err(WorldValidationError::DuplicateItem {
                    field: "entity id",
                });
            }
            ids.push(entity.id());
        }
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &EntityState> {
        self.entities.iter()
    }

    #[must_use]
    pub fn get(&self, id: super::EntityId) -> Option<&EntityState> {
        self.entities.iter().find(|entity| entity.id() == id)
    }

    #[must_use]
    pub fn find(
        &self,
        kind: EntityKind,
        linked_person: Option<PersonId>,
        linked_conversation: Option<ConversationId>,
    ) -> Option<&EntityState> {
        self.entities
            .iter()
            .find(|entity| entity.matches_kind_and_links(kind, linked_person, linked_conversation))
    }

    /// Apply a validated update: create or merge the entity, return its id.
    pub fn apply(
        &mut self,
        proposal: EntityUpdateProposal,
    ) -> Result<super::EntityId, WorldValidationError> {
        proposal.validate()?;
        let id = match proposal.entity_id() {
            Some(entity_id) => match self
                .entities
                .iter_mut()
                .find(|entity| entity.id() == entity_id)
            {
                Some(entity) => {
                    entity.set_confidence(proposal.confidence(), proposal.observed_at())?;
                    for action in proposal.actions() {
                        entity.apply(action, proposal.observed_at())?;
                    }
                    entity.id()
                }
                None => self.create_or_merge(proposal)?,
            },
            None => self.create_or_merge(proposal)?,
        };
        Ok(id)
    }

    fn create_or_merge(
        &mut self,
        proposal: EntityUpdateProposal,
    ) -> Result<super::EntityId, WorldValidationError> {
        // A fixed id is deterministic: never merge by links, create it.
        let existing = if proposal.entity_id().is_none() {
            self.entities.iter_mut().find(|entity| {
                entity.matches_kind_and_links(
                    proposal.kind(),
                    proposal.linked_person(),
                    proposal.linked_conversation(),
                )
            })
        } else {
            None
        };
        match existing {
            Some(entity) => {
                entity.set_confidence(proposal.confidence(), proposal.observed_at())?;
                for action in proposal.actions() {
                    entity.apply(action, proposal.observed_at())?;
                }
                Ok(entity.id())
            }
            None => {
                // Per-scope cap: count every entity sharing the same person
                // or conversation link, not just the same kind.
                let per_scope = self
                    .entities
                    .iter()
                    .filter(|entity| {
                        (proposal.linked_person().is_some()
                            && entity.linked_person() == proposal.linked_person())
                            || (proposal.linked_conversation().is_some()
                                && entity.linked_conversation()
                                    == proposal.linked_conversation())
                    })
                    .count();
                if per_scope >= MAX_ENTITIES_PER_SCOPE {
                    return Err(WorldValidationError::TooManyItems {
                        field: "entities per scope",
                        length: per_scope,
                        maximum: MAX_ENTITIES_PER_SCOPE,
                    });
                }
                if self.entities.len() >= MAX_ACTIVE_ENTITIES {
                    return Err(WorldValidationError::TooManyItems {
                        field: "entities",
                        length: self.entities.len(),
                        maximum: MAX_ACTIVE_ENTITIES,
                    });
                }
                let id = proposal.entity_id().unwrap_or_default();
                let mut entity = EntityState::new(
                    id,
                    proposal.kind(),
                    proposal.linked_person(),
                    proposal.linked_conversation(),
                    proposal.confidence(),
                    proposal.observed_at(),
                )?;
                for action in proposal.actions() {
                    entity.apply(action, proposal.observed_at())?;
                }
                self.entities.push(entity);
                Ok(id)
            }
        }
    }

    pub fn erase_person(&mut self, person_id: PersonId) {
        self.entities
            .retain(|entity| entity.linked_person() != Some(person_id));
    }

    pub fn erase_conversation(&mut self, conversation_id: ConversationId) {
        self.entities
            .retain(|entity| entity.linked_conversation() != Some(conversation_id));
    }

    /// Rebuild an index from persisted entities (validated, bounded).
    pub fn from_entities(entities: Vec<EntityState>) -> Result<Self, WorldValidationError> {
        let index = Self { entities };
        index.validate()?;
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn property(key: &str, value: &str, confidence: f32, now: DateTime<Utc>) -> StateProperty {
        StateProperty::new(
            key,
            value,
            confidence,
            ObservationSource::DirectUserStatement,
            now,
            None,
        )
        .expect("property")
    }

    #[test]
    fn apply_bumps_version_and_sets_property() {
        let now = Utc::now();
        let mut entity = EntityState::new(
            super::super::EntityId::new(),
            EntityKind::Person,
            Some(PersonId::new()),
            None,
            0.5,
            now,
        )
        .expect("entity");
        assert_eq!(entity.version(), 1);
        entity
            .apply(&EntityUpdateAction::Set(property("employment", "interviewing", 0.8, now)), now)
            .expect("set");
        assert_eq!(entity.version(), 2);
        assert_eq!(entity.property("employment").expect("prop").value(), "interviewing");
        entity
            .apply(&EntityUpdateAction::Clear { key: "employment".into() }, now)
            .expect("clear");
        assert!(entity.property("employment").is_none());
        assert_eq!(entity.version(), 3);
    }

    #[test]
    fn property_ttl_marks_stale() {
        let now = Utc::now();
        let mut property =
            StateProperty::new("host", "online", 0.9, ObservationSource::SystemState, now, Some(now + Duration::seconds(100))).expect("property");
        assert_eq!(property.freshness_at(now + Duration::seconds(10)), super::super::Freshness::Fresh);
        property = StateProperty::new(
            "host",
            "online",
            0.9,
            ObservationSource::SystemState,
            now,
            Some(now + Duration::seconds(100)),
        )
        .expect("property");
        assert_eq!(
            property.freshness_at(now + Duration::seconds(85)),
            super::super::Freshness::Stale
        );
        assert_eq!(
            property.freshness_at(now + Duration::seconds(101)),
            super::super::Freshness::Expired
        );
        assert!(!property.is_valid_at(now + Duration::seconds(101)));
        assert!(property.is_valid_at(now + Duration::seconds(50)));
    }

    #[test]
    fn per_scope_entity_cap_is_enforced() {
        let now = Utc::now();
        let person_id = PersonId::new();
        let mut index = EntityStateIndex::default();
        // Distinct fixed ids for the same person-scope: no merges, cap at 64.
        for i in 0..MAX_ENTITIES_PER_SCOPE + 1 {
            let proposal = EntityUpdateProposal::new(
                Some(super::super::EntityId::new()),
                EntityKind::Tool,
                Some(person_id),
                None,
                0.5,
                vec![EntityUpdateAction::Set(property("tool", &format!("t{i}"), 0.5, now))],
                now,
            )
            .expect("proposal");
            let result = index.apply(proposal);
            if i < MAX_ENTITIES_PER_SCOPE {
                result.expect("within cap");
            } else {
                assert!(result.is_err());
            }
        }
        assert_eq!(index.len(), MAX_ENTITIES_PER_SCOPE);
    }

    #[test]
    fn merge_updates_existing_entity_by_links() {
        let now = Utc::now();
        let person_id = PersonId::new();
        let mut index = EntityStateIndex::default();
        let id = index
            .apply(
                EntityUpdateProposal::new(
                    None,
                    EntityKind::Person,
                    Some(person_id),
                    None,
                    0.4,
                    vec![EntityUpdateAction::Set(property("state", "busy", 0.6, now))],
                    now,
                )
                .expect("first"),
            )
            .expect("created");
        index
            .apply(
                EntityUpdateProposal::new(
                    None,
                    EntityKind::Person,
                    Some(person_id),
                    None,
                    0.7,
                    vec![EntityUpdateAction::Set(property("state", "free", 0.9, now))],
                    now,
                )
                .expect("second"),
            )
            .expect("merged");
        assert_eq!(index.len(), 1);
        let entity = index.get(id).expect("entity");
        assert_eq!(entity.property("state").expect("prop").value(), "free");
        assert_eq!(entity.confidence(), 0.7);
    }

    #[test]
    fn erase_person_cleans_linked_entities() {
        let now = Utc::now();
        let person_a = PersonId::new();
        let person_b = PersonId::new();
        let mut index = EntityStateIndex::default();
        for person in [person_a, person_b] {
            index
                .apply(
                    EntityUpdateProposal::new(
                        None,
                        EntityKind::Person,
                        Some(person),
                        None,
                        0.5,
                        vec![EntityUpdateAction::Set(property("state", "ok", 0.5, now))],
                        now,
                    )
                    .expect("proposal"),
                )
                .expect("applied");
        }
        index.erase_person(person_a);
        assert_eq!(index.len(), 1);
        assert!(index
            .find(EntityKind::Person, Some(person_a), None)
            .is_none());
        assert!(index
            .find(EntityKind::Person, Some(person_b), None)
            .is_some());
    }
}
