use super::{
    AgendaItem, AgendaItemId, AgendaItemKind, AgendaStatus, AgendaSubject, Belief, BeliefId,
    BeliefSource, Interest, InterestId, MindInfluenceMode, MindReasonTag, MindScope, MindServices,
    MindStoreError, MindValidationError, OpenQuestion, OpenQuestionId, Preference, PreferenceId,
    PreferenceSource, SelfIdentity, SelfModel, SelfTrait, ValueProfile,
};
use crate::{
    ConversationId, ConversationKind, EventId, EventScope, PersonId, WorldEvent, WorldEventKind,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

pub const MAX_SNAPSHOT_BELIEFS: usize = 32;
pub const MAX_SNAPSHOT_PREFERENCES: usize = 32;
pub const MAX_SNAPSHOT_INTERESTS: usize = 32;
pub const MAX_SNAPSHOT_OPEN_QUESTIONS: usize = 24;
pub const MAX_SNAPSHOT_AGENDA_ITEMS: usize = 32;
pub const MAX_SNAPSHOT_REASON_TAGS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MindSnapshotLimits {
    pub beliefs: usize,
    pub preferences: usize,
    pub interests: usize,
    pub open_questions: usize,
    pub agenda_items: usize,
}

impl Default for MindSnapshotLimits {
    fn default() -> Self {
        Self {
            beliefs: 8,
            preferences: 8,
            interests: 8,
            open_questions: 6,
            agenda_items: 8,
        }
    }
}

impl MindSnapshotLimits {
    pub fn validate(self) -> Result<(), MindValidationError> {
        for (field, value, maximum) in [
            ("snapshot beliefs", self.beliefs, MAX_SNAPSHOT_BELIEFS),
            (
                "snapshot preferences",
                self.preferences,
                MAX_SNAPSHOT_PREFERENCES,
            ),
            ("snapshot interests", self.interests, MAX_SNAPSHOT_INTERESTS),
            (
                "snapshot open questions",
                self.open_questions,
                MAX_SNAPSHOT_OPEN_QUESTIONS,
            ),
            (
                "snapshot agenda items",
                self.agenda_items,
                MAX_SNAPSHOT_AGENDA_ITEMS,
            ),
        ] {
            if value > maximum {
                return Err(MindValidationError::TooManyItems {
                    field,
                    length: value,
                    maximum,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfModelSnapshot {
    identity: SelfIdentity,
    traits: Vec<SelfTrait>,
    values: ValueProfile,
    limitations: Vec<String>,
    version: u64,
}

impl SelfModelSnapshot {
    pub fn from_model(model: &SelfModel) -> Result<Self, MindValidationError> {
        model.validate()?;
        Ok(Self {
            identity: model.identity().clone(),
            traits: model.traits().to_vec(),
            values: model.values().clone(),
            limitations: model
                .limitations()
                .iter()
                .map(|limitation| limitation.description().to_owned())
                .collect(),
            version: model.version(),
        })
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        self.identity.validate()?;
        self.values.validate()?;
        if self.traits.len() > super::self_model::MAX_SELF_TRAITS {
            return Err(MindValidationError::TooManyItems {
                field: "self-model snapshot traits",
                length: self.traits.len(),
                maximum: super::self_model::MAX_SELF_TRAITS,
            });
        }
        if self.limitations.len() > super::self_model::MAX_SELF_LIMITATIONS {
            return Err(MindValidationError::TooManyItems {
                field: "self-model snapshot limitations",
                length: self.limitations.len(),
                maximum: super::self_model::MAX_SELF_LIMITATIONS,
            });
        }
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        for personality_trait in &self.traits {
            personality_trait.validate()?;
        }
        for limitation in &self.limitations {
            super::common::validate_mind_text(
                limitation.clone(),
                "self-model snapshot limitation",
            )?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn identity(&self) -> &SelfIdentity {
        &self.identity
    }

    #[must_use]
    pub fn traits(&self) -> &[SelfTrait] {
        &self.traits
    }

    #[must_use]
    pub const fn values(&self) -> &ValueProfile {
        &self.values
    }

    #[must_use]
    pub fn limitations(&self) -> &[String] {
        &self.limitations
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeliefSnapshot {
    pub id: BeliefId,
    pub scope: MindScope,
    pub proposition: String,
    pub confidence: f32,
    pub stability: f32,
    pub source: BeliefSource,
    pub updated_at: DateTime<Utc>,
    pub version: u64,
}

impl TryFrom<&Belief> for BeliefSnapshot {
    type Error = MindValidationError;

    fn try_from(value: &Belief) -> Result<Self, Self::Error> {
        value.validate()?;
        Ok(Self {
            id: value.id(),
            scope: value.scope(),
            proposition: value.proposition().to_owned(),
            confidence: value.confidence(),
            stability: value.stability(),
            source: value.source(),
            updated_at: value.updated_at(),
            version: value.version(),
        })
    }
}

impl BeliefSnapshot {
    fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_mind_text(self.proposition.clone(), "belief snapshot")?;
        super::common::validate_unit(self.confidence, "belief snapshot confidence")?;
        super::common::validate_unit(self.stability, "belief snapshot stability")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreferenceSnapshot {
    pub id: PreferenceId,
    pub subject: String,
    pub valence: f32,
    pub intensity: f32,
    pub confidence: f32,
    pub source: PreferenceSource,
    pub version: u64,
}

impl TryFrom<&Preference> for PreferenceSnapshot {
    type Error = MindValidationError;

    fn try_from(value: &Preference) -> Result<Self, Self::Error> {
        value.validate()?;
        Ok(Self {
            id: value.id(),
            subject: value.subject().to_owned(),
            valence: value.valence(),
            intensity: value.intensity(),
            confidence: value.confidence(),
            source: value.source(),
            version: value.version(),
        })
    }
}

impl PreferenceSnapshot {
    fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_mind_text(self.subject.clone(), "preference snapshot")?;
        super::common::validate_signed_unit(self.valence, "preference snapshot valence")?;
        super::common::validate_unit(self.intensity, "preference snapshot intensity")?;
        super::common::validate_unit(self.confidence, "preference snapshot confidence")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterestSnapshot {
    pub id: InterestId,
    pub topic: String,
    pub activation: f32,
    pub long_term_affinity: f32,
    pub novelty: f32,
    pub version: u64,
}

impl TryFrom<&Interest> for InterestSnapshot {
    type Error = MindValidationError;

    fn try_from(value: &Interest) -> Result<Self, Self::Error> {
        value.validate()?;
        Ok(Self {
            id: value.id(),
            topic: value.topic().to_owned(),
            activation: value.activation(),
            long_term_affinity: value.long_term_affinity(),
            novelty: value.novelty(),
            version: value.version(),
        })
    }
}

impl InterestSnapshot {
    fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_mind_text(self.topic.clone(), "interest snapshot")?;
        super::common::validate_unit(self.activation, "interest snapshot activation")?;
        super::common::validate_unit(
            self.long_term_affinity,
            "interest snapshot long-term affinity",
        )?;
        super::common::validate_unit(self.novelty, "interest snapshot novelty")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenQuestionSnapshot {
    pub id: OpenQuestionId,
    pub scope: MindScope,
    pub question: String,
    pub salience: f32,
    pub version: u64,
}

impl TryFrom<&OpenQuestion> for OpenQuestionSnapshot {
    type Error = MindValidationError;

    fn try_from(value: &OpenQuestion) -> Result<Self, Self::Error> {
        value.validate()?;
        Ok(Self {
            id: value.id(),
            scope: value.scope(),
            question: value.question().to_owned(),
            salience: value.salience(),
            version: value.version(),
        })
    }
}

impl OpenQuestionSnapshot {
    fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_mind_text(self.question.clone(), "open-question snapshot")?;
        super::common::validate_unit(self.salience, "open-question snapshot salience")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgendaItemSnapshot {
    pub id: AgendaItemId,
    pub scope: MindScope,
    pub kind: AgendaItemKind,
    pub summary_key: String,
    pub salience: f32,
    pub activation: f32,
    pub version: u64,
}

impl TryFrom<&AgendaItem> for AgendaItemSnapshot {
    type Error = MindValidationError;

    fn try_from(value: &AgendaItem) -> Result<Self, Self::Error> {
        value.validate()?;
        if value.status() != AgendaStatus::Active {
            return Err(MindValidationError::InvalidProposal {
                reason: "only active agenda items may enter a snapshot",
            });
        }
        Ok(Self {
            id: value.id(),
            scope: value.scope(),
            kind: value.kind(),
            summary_key: value.subject().dedupe_key(),
            salience: value.salience(),
            activation: value.activation(),
            version: value.version(),
        })
    }
}

impl AgendaItemSnapshot {
    fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_mind_text(self.summary_key.clone(), "agenda snapshot key")?;
        super::common::validate_unit(self.salience, "agenda snapshot salience")?;
        super::common::validate_unit(self.activation, "agenda snapshot activation")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MindSnapshot {
    #[serde(default)]
    self_model: Option<SelfModelSnapshot>,
    #[serde(default)]
    beliefs: Vec<BeliefSnapshot>,
    #[serde(default)]
    preferences: Vec<PreferenceSnapshot>,
    #[serde(default)]
    interests: Vec<InterestSnapshot>,
    #[serde(default)]
    open_questions: Vec<OpenQuestionSnapshot>,
    #[serde(default)]
    agenda: Vec<AgendaItemSnapshot>,
    #[serde(default)]
    reason_tags: Vec<MindReasonTag>,
    #[serde(default)]
    influence_mode: MindInfluenceMode,
    #[serde(default)]
    version: u64,
    generated_at: DateTime<Utc>,
    schema_version: u16,
}

impl MindSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        self_model: Option<SelfModelSnapshot>,
        beliefs: Vec<BeliefSnapshot>,
        preferences: Vec<PreferenceSnapshot>,
        interests: Vec<InterestSnapshot>,
        open_questions: Vec<OpenQuestionSnapshot>,
        agenda: Vec<AgendaItemSnapshot>,
        reason_tags: Vec<MindReasonTag>,
        influence_mode: MindInfluenceMode,
        version: u64,
        generated_at: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        let snapshot = Self {
            self_model,
            beliefs,
            preferences,
            interests,
            open_questions,
            agenda,
            reason_tags,
            influence_mode,
            version,
            generated_at,
            schema_version: super::SCHEMA_VERSION,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    #[must_use]
    pub fn empty() -> Self {
        Self {
            self_model: None,
            beliefs: Vec::new(),
            preferences: Vec::new(),
            interests: Vec::new(),
            open_questions: Vec::new(),
            agenda: Vec::new(),
            reason_tags: Vec::new(),
            influence_mode: MindInfluenceMode::Disabled,
            version: 0,
            generated_at: DateTime::from_timestamp(0, 0).expect("Unix epoch is representable"),
            schema_version: super::SCHEMA_VERSION,
        }
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        if self.schema_version != super::SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported mind snapshot schema version",
            });
        }
        for (field, length, maximum) in [
            ("snapshot beliefs", self.beliefs.len(), MAX_SNAPSHOT_BELIEFS),
            (
                "snapshot preferences",
                self.preferences.len(),
                MAX_SNAPSHOT_PREFERENCES,
            ),
            (
                "snapshot interests",
                self.interests.len(),
                MAX_SNAPSHOT_INTERESTS,
            ),
            (
                "snapshot open questions",
                self.open_questions.len(),
                MAX_SNAPSHOT_OPEN_QUESTIONS,
            ),
            (
                "snapshot agenda items",
                self.agenda.len(),
                MAX_SNAPSHOT_AGENDA_ITEMS,
            ),
            (
                "snapshot reason tags",
                self.reason_tags.len(),
                MAX_SNAPSHOT_REASON_TAGS,
            ),
        ] {
            if length > maximum {
                return Err(MindValidationError::TooManyItems {
                    field,
                    length,
                    maximum,
                });
            }
        }
        if let Some(self_model) = &self.self_model {
            self_model.validate()?;
        }
        for item in &self.beliefs {
            item.validate()?;
        }
        for item in &self.preferences {
            item.validate()?;
        }
        for item in &self.interests {
            item.validate()?;
        }
        for item in &self.open_questions {
            item.validate()?;
        }
        for item in &self.agenda {
            item.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn self_model(&self) -> Option<&SelfModelSnapshot> {
        self.self_model.as_ref()
    }

    #[must_use]
    pub fn beliefs(&self) -> &[BeliefSnapshot] {
        &self.beliefs
    }

    #[must_use]
    pub fn preferences(&self) -> &[PreferenceSnapshot] {
        &self.preferences
    }

    #[must_use]
    pub fn interests(&self) -> &[InterestSnapshot] {
        &self.interests
    }

    #[must_use]
    pub fn open_questions(&self) -> &[OpenQuestionSnapshot] {
        &self.open_questions
    }

    #[must_use]
    pub fn agenda(&self) -> &[AgendaItemSnapshot] {
        &self.agenda
    }

    #[must_use]
    pub fn reason_tags(&self) -> &[MindReasonTag] {
        &self.reason_tags
    }

    #[must_use]
    pub const fn influence_mode(&self) -> MindInfluenceMode {
        self.influence_mode
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn generated_at(&self) -> DateTime<Utc> {
        self.generated_at
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.self_model.is_none()
            && self.beliefs.is_empty()
            && self.preferences.is_empty()
            && self.interests.is_empty()
            && self.open_questions.is_empty()
            && self.agenda.is_empty()
    }
}

impl Default for MindSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MindSnapshotRequest {
    event_id: EventId,
    event_scope: EventScope,
    person_id: Option<PersonId>,
    conversation_id: Option<ConversationId>,
    query: String,
    topic: Option<String>,
    at: DateTime<Utc>,
    include_private_person_state: bool,
    limits: MindSnapshotLimits,
    influence_mode: MindInfluenceMode,
}

impl MindSnapshotRequest {
    pub fn for_event(
        event: &WorldEvent,
        topic: Option<&str>,
        limits: MindSnapshotLimits,
        influence_mode: MindInfluenceMode,
    ) -> Result<Self, MindValidationError> {
        limits.validate()?;
        let (person_id, conversation_id, query, include_private_person_state) = match event.kind() {
            WorldEventKind::MessageReceived(message) => (
                Some(message.sender),
                Some(message.conversation_id),
                message.content.as_text().to_owned(),
                message.conversation_kind == ConversationKind::Direct,
            ),
            WorldEventKind::InteractionCuesObserved(cues) => (
                Some(cues.person_id),
                event.scope().conversation_id(),
                String::new(),
                matches!(event.scope(), EventScope::Person { .. }),
            ),
            _ => (
                match event.scope() {
                    EventScope::Person { person_id } => Some(person_id),
                    _ => None,
                },
                event.scope().conversation_id(),
                String::new(),
                matches!(event.scope(), EventScope::Person { .. }),
            ),
        };
        let query = if query.trim().is_empty() {
            String::new()
        } else {
            super::common::validate_mind_text(query, "mind snapshot query")?
        };
        let topic = topic
            .map(|value| super::common::validate_mind_text(value, "mind snapshot topic"))
            .transpose()?;
        Ok(Self {
            event_id: event.id(),
            event_scope: event.scope(),
            person_id,
            conversation_id,
            query,
            topic,
            at: event.occurred_at(),
            include_private_person_state,
            limits,
            influence_mode,
        })
    }

    /// Builds the snapshot request for a host-generated autonomous heartbeat.
    /// Such an event is scoped to a conversation and therefore has no sender
    /// field; a direct conversation may provide its already-resolved person so
    /// the private Mind scope can be restored without guessing in group chat.
    pub fn for_autonomous_conversation(
        event: &WorldEvent,
        person_id: Option<PersonId>,
        topic: Option<&str>,
        limits: MindSnapshotLimits,
        influence_mode: MindInfluenceMode,
    ) -> Result<Self, MindValidationError> {
        let mut request = Self::for_event(event, topic, limits, influence_mode)?;
        if matches!(event.kind(), WorldEventKind::AutonomousConversationTick(_)) {
            request.person_id = person_id;
            request.include_private_person_state = person_id.is_some();
        }
        Ok(request)
    }

    #[must_use]
    pub fn scopes(&self) -> Vec<MindScope> {
        let mut scopes = vec![MindScope::Global];
        if let Some(conversation_id) = self.conversation_id {
            scopes.push(MindScope::Conversation { conversation_id });
        }
        if self.include_private_person_state
            && let Some(person_id) = self.person_id
        {
            scopes.push(MindScope::Person { person_id });
        }
        scopes
    }

    #[must_use]
    pub const fn event_id(&self) -> EventId {
        self.event_id
    }

    #[must_use]
    pub const fn event_scope(&self) -> EventScope {
        self.event_scope
    }

    #[must_use]
    pub const fn person_id(&self) -> Option<PersonId> {
        self.person_id
    }

    #[must_use]
    pub const fn conversation_id(&self) -> Option<ConversationId> {
        self.conversation_id
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub fn topic(&self) -> Option<&str> {
        self.topic.as_deref()
    }

    #[must_use]
    pub const fn at(&self) -> DateTime<Utc> {
        self.at
    }

    #[must_use]
    pub const fn limits(&self) -> MindSnapshotLimits {
        self.limits
    }

    #[must_use]
    pub const fn influence_mode(&self) -> MindInfluenceMode {
        self.influence_mode
    }
}

pub type MindSnapshotFuture<'a> =
    Pin<Box<dyn Future<Output = Result<MindSnapshot, SnapshotProviderError>> + Send + 'a>>;

pub trait MindSnapshotProvider: Send + Sync {
    fn snapshot<'a>(&'a self, request: &'a MindSnapshotRequest) -> MindSnapshotFuture<'a>;
}

#[derive(Debug, Error)]
pub enum SnapshotProviderError {
    #[error(transparent)]
    Store(#[from] MindStoreError),
    #[error(transparent)]
    Validation(#[from] MindValidationError),
}

#[derive(Debug, Clone)]
pub struct MindSnapshotStoreProvider {
    services: MindServices,
}

impl MindSnapshotStoreProvider {
    #[must_use]
    pub const fn new(services: MindServices) -> Self {
        Self { services }
    }

    #[must_use]
    pub const fn services(&self) -> &MindServices {
        &self.services
    }
}

impl MindSnapshotProvider for MindSnapshotStoreProvider {
    fn snapshot<'a>(&'a self, request: &'a MindSnapshotRequest) -> MindSnapshotFuture<'a> {
        Box::pin(async move {
            request.limits.validate()?;
            let scopes = request.scopes();
            let query = match request.topic() {
                Some(topic) if !request.query().is_empty() => {
                    format!("{} {topic}", request.query())
                }
                Some(topic) => topic.to_owned(),
                None => request.query().to_owned(),
            };
            let version = self.services.consolidation.current_version().await?;
            let self_model = self
                .services
                .self_model
                .get()
                .await?
                .as_ref()
                .map(SelfModelSnapshot::from_model)
                .transpose()?;
            let beliefs = self
                .services
                .beliefs
                .relevant(&scopes, &query, request.at, request.limits.beliefs)
                .await?
                .iter()
                .map(BeliefSnapshot::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            let preferences = self
                .services
                .preferences
                .relevant(&query, request.limits.preferences)
                .await?
                .iter()
                .map(PreferenceSnapshot::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            let agenda_items = self
                .services
                .agenda
                .list_active(&scopes, request.at, request.limits.agenda_items)
                .await?;
            let mut interest_values = self
                .services
                .interests
                .relevant(&query, request.limits.interests)
                .await?;
            for item in &agenda_items {
                if interest_values.len() == request.limits.interests {
                    break;
                }
                let AgendaSubject::Interest(interest_id) = item.subject() else {
                    continue;
                };
                if interest_values
                    .iter()
                    .any(|interest| interest.id() == *interest_id)
                {
                    continue;
                }
                if let Some(interest) = self.services.interests.get(*interest_id).await? {
                    interest_values.push(interest);
                }
            }
            let interests = interest_values
                .iter()
                .map(InterestSnapshot::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            let open_questions = self
                .services
                .open_questions
                .list_open(&scopes, request.limits.open_questions)
                .await?
                .iter()
                .map(OpenQuestionSnapshot::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            let agenda = agenda_items
                .iter()
                .map(AgendaItemSnapshot::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            MindSnapshot::new(
                self_model,
                beliefs,
                preferences,
                interests,
                open_questions,
                agenda,
                Vec::new(),
                request.influence_mode,
                version,
                request.at,
            )
            .map_err(Into::into)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{MindInfluenceMode, MindSnapshotLimits, MindSnapshotRequest};
    use crate::{
        AutonomousConversationTickEvent, ConversationId, EventPriority, EventScope, WorldEvent,
        WorldEventKind,
    };
    use chrono::Utc;

    #[test]
    fn autonomous_request_can_restore_private_direct_person_scope() {
        let conversation_id = ConversationId::new();
        let person_id = crate::PersonId::new();
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::Low,
            WorldEventKind::AutonomousConversationTick(AutonomousConversationTickEvent::default()),
        );

        let request = MindSnapshotRequest::for_autonomous_conversation(
            &event,
            Some(person_id),
            None,
            MindSnapshotLimits::default(),
            MindInfluenceMode::Active,
        )
        .expect("autonomous request should validate");

        assert_eq!(request.person_id(), Some(person_id));
        assert!(
            request
                .scopes()
                .contains(&super::MindScope::Person { person_id })
        );
    }
}
