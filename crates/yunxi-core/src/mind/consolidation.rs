use super::{
    AgendaItem, AgendaItemId, AgendaSource, AgendaStatus, AgendaSubject, Belief, BeliefId,
    BeliefOperation, BeliefSource, BeliefUpdateProposal, Episode, Interest, InterestId,
    MindReasonTag, MindScope, MindServices, MindStoreError, MindValidationError, OpenQuestion,
    OpenQuestionId, OpenQuestionStatus, Preference, PreferenceId, PreferenceSource,
    ReflectionProposal,
};
use crate::TraceContext;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceOperation {
    Upsert,
    Reinforce,
    Weaken,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreferenceUpdateProposal {
    pub operation: PreferenceOperation,
    pub preference_id: Option<PreferenceId>,
    pub expected_version: Option<u64>,
    pub subject: String,
    pub valence_delta: f32,
    pub intensity_delta: f32,
    pub confidence_delta: f32,
    pub source: PreferenceSource,
}

impl PreferenceUpdateProposal {
    pub fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_mind_text(self.subject.clone(), "preference proposal subject")?;
        super::common::validate_signed_unit(
            self.valence_delta,
            "preference proposal valence delta",
        )?;
        super::common::validate_signed_unit(
            self.intensity_delta,
            "preference proposal intensity delta",
        )?;
        super::common::validate_signed_unit(
            self.confidence_delta,
            "preference proposal confidence delta",
        )?;
        if self.expected_version == Some(0) {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.operation != PreferenceOperation::Upsert && self.preference_id.is_none() {
            return Err(MindValidationError::InvalidProposal {
                reason: "preference mutation requires an existing id",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterestOperation {
    Upsert,
    Activate,
    Decay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterestUpdateProposal {
    pub operation: InterestOperation,
    pub interest_id: Option<InterestId>,
    pub expected_version: Option<u64>,
    pub topic: String,
    pub activation_delta: f32,
    pub affinity_delta: f32,
    pub novelty: f32,
    pub source: super::MindSource,
}

impl InterestUpdateProposal {
    pub fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_mind_text(self.topic.clone(), "interest proposal topic")?;
        super::common::validate_signed_unit(
            self.activation_delta,
            "interest proposal activation delta",
        )?;
        super::common::validate_signed_unit(
            self.affinity_delta,
            "interest proposal affinity delta",
        )?;
        super::common::validate_unit(self.novelty, "interest proposal novelty")?;
        if self.expected_version == Some(0) {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.operation != InterestOperation::Upsert && self.interest_id.is_none() {
            return Err(MindValidationError::InvalidProposal {
                reason: "interest mutation requires an existing id",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenQuestionOperation {
    Upsert,
    Resolve,
    Drop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenQuestionUpdateProposal {
    pub operation: OpenQuestionOperation,
    pub question_id: Option<OpenQuestionId>,
    pub expected_version: Option<u64>,
    pub scope: MindScope,
    pub question: String,
    pub related_beliefs: Vec<BeliefId>,
    pub salience: f32,
}

impl OpenQuestionUpdateProposal {
    pub fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_mind_text(self.question.clone(), "open-question proposal")?;
        super::common::validate_unit(self.salience, "open-question proposal salience")?;
        if self.related_beliefs.len() > super::common::MAX_RELATED_IDS {
            return Err(MindValidationError::TooManyItems {
                field: "open-question proposal related beliefs",
                length: self.related_beliefs.len(),
                maximum: super::common::MAX_RELATED_IDS,
            });
        }
        if self.expected_version == Some(0) {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.operation != OpenQuestionOperation::Upsert && self.question_id.is_none() {
            return Err(MindValidationError::InvalidProposal {
                reason: "open-question mutation requires an existing id",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgendaOperation {
    Activate,
    Defer,
    Resolve,
    Drop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgendaUpdateProposal {
    pub operation: AgendaOperation,
    pub item_id: Option<AgendaItemId>,
    pub expected_version: Option<u64>,
    pub scope: MindScope,
    pub subject: AgendaSubject,
    pub salience: f32,
    pub activation: f32,
    pub stability: f32,
    pub source: AgendaSource,
    pub defer_until: Option<DateTime<Utc>>,
}

impl AgendaUpdateProposal {
    pub fn validate(&self) -> Result<(), MindValidationError> {
        super::common::validate_unit(self.salience, "agenda proposal salience")?;
        super::common::validate_unit(self.activation, "agenda proposal activation")?;
        super::common::validate_unit(self.stability, "agenda proposal stability")?;
        if self.expected_version == Some(0) {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.operation == AgendaOperation::Defer && self.defer_until.is_none() {
            return Err(MindValidationError::InvalidProposal {
                reason: "agenda defer proposal requires defer_until",
            });
        }
        if self.operation != AgendaOperation::Defer && self.defer_until.is_some() {
            return Err(MindValidationError::InvalidProposal {
                reason: "only agenda defer proposals may set defer_until",
            });
        }
        if matches!(
            self.operation,
            AgendaOperation::Resolve | AgendaOperation::Drop
        ) && self.item_id.is_none()
        {
            return Err(MindValidationError::InvalidProposal {
                reason: "terminal agenda proposal requires an existing id",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationConfig {
    pub max_belief_delta: f32,
    pub max_preference_delta: f32,
    pub max_interest_affinity_delta: f32,
    pub max_updates_per_reflection: usize,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            max_belief_delta: 0.2,
            max_preference_delta: 0.1,
            max_interest_affinity_delta: 0.1,
            max_updates_per_reflection: 32,
        }
    }
}

impl ConsolidationConfig {
    pub fn validate(self) -> Result<(), MindValidationError> {
        super::common::validate_unit(self.max_belief_delta, "maximum belief delta")?;
        super::common::validate_unit(self.max_preference_delta, "maximum preference delta")?;
        super::common::validate_unit(
            self.max_interest_affinity_delta,
            "maximum interest affinity delta",
        )?;
        if self.max_updates_per_reflection == 0 || self.max_updates_per_reflection > 128 {
            return Err(MindValidationError::InvalidProposal {
                reason: "consolidation update limit must be within 1..=128",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MindUpsert<T> {
    pub value: T,
    pub expected_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationPlan {
    pub base_mind_version: u64,
    pub beliefs: Vec<MindUpsert<Belief>>,
    pub preferences: Vec<MindUpsert<Preference>>,
    pub interests: Vec<MindUpsert<Interest>>,
    pub open_questions: Vec<MindUpsert<OpenQuestion>>,
    pub agenda: Vec<MindUpsert<AgendaItem>>,
    pub episodes: Vec<Episode>,
    pub reason_tags: Vec<MindReasonTag>,
    pub created_at: DateTime<Utc>,
    pub trace: TraceContext,
}

impl ConsolidationPlan {
    pub fn validate(&self, config: ConsolidationConfig) -> Result<(), MindValidationError> {
        config.validate()?;
        let update_count = self.beliefs.len()
            + self.preferences.len()
            + self.interests.len()
            + self.open_questions.len()
            + self.agenda.len()
            + self.episodes.len();
        if update_count > config.max_updates_per_reflection {
            return Err(MindValidationError::TooManyItems {
                field: "consolidation updates",
                length: update_count,
                maximum: config.max_updates_per_reflection,
            });
        }
        for upsert in &self.beliefs {
            upsert.value.validate()?;
        }
        for upsert in &self.preferences {
            upsert.value.validate()?;
        }
        for upsert in &self.interests {
            upsert.value.validate()?;
        }
        for upsert in &self.open_questions {
            upsert.value.validate()?;
        }
        for upsert in &self.agenda {
            upsert.value.validate()?;
        }
        for episode in &self.episodes {
            episode.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidationResult {
    pub applied_updates: usize,
    pub new_mind_version: u64,
}

#[derive(Debug, Error)]
pub enum ConsolidationError {
    #[error(transparent)]
    Validation(#[from] MindValidationError),
    #[error(transparent)]
    Store(#[from] MindStoreError),
    #[error("reflection snapshot is stale: expected mind version {expected}, actual {actual}")]
    StaleSnapshot { expected: u64, actual: u64 },
    #[error("proposal scope does not match the stored record")]
    ScopeMismatch,
}

#[derive(Debug, Clone, Copy)]
pub struct Consolidation {
    config: ConsolidationConfig,
}

impl Consolidation {
    pub fn new(config: ConsolidationConfig) -> Result<Self, MindValidationError> {
        config.validate()?;
        Ok(Self { config })
    }

    #[must_use]
    pub const fn config(&self) -> ConsolidationConfig {
        self.config
    }

    pub async fn prepare(
        &self,
        services: &MindServices,
        proposal: &ReflectionProposal,
    ) -> Result<ConsolidationPlan, ConsolidationError> {
        proposal.validate()?;
        let current_version = services.consolidation.current_version().await?;
        if current_version != proposal.base_snapshot_version {
            return Err(ConsolidationError::StaleSnapshot {
                expected: proposal.base_snapshot_version,
                actual: current_version,
            });
        }

        let mut plan = ConsolidationPlan {
            base_mind_version: current_version,
            beliefs: Vec::new(),
            preferences: Vec::new(),
            interests: Vec::new(),
            open_questions: Vec::new(),
            agenda: Vec::new(),
            episodes: proposal.episodes.clone(),
            reason_tags: proposal.reason_tags.clone(),
            created_at: proposal.proposed_at,
            trace: proposal.trace,
        };

        for episode in &plan.episodes {
            ensure_scope(proposal.scope, episode.scope())?;
        }
        for update in &proposal.belief_updates {
            let existing = if let Some(id) = update.belief_id {
                services.beliefs.get(id).await?
            } else {
                services
                    .beliefs
                    .find_by_key(
                        update.scope,
                        &super::common::normalized_key(&update.proposition),
                    )
                    .await?
            };
            let upsert = self.prepare_belief(update, existing, proposal.proposed_at)?;
            ensure_scope(proposal.scope, upsert.value.scope())?;
            plan.beliefs.push(upsert);
        }
        for update in &proposal.preference_updates {
            let existing = if let Some(id) = update.preference_id {
                services.preferences.get(id).await?
            } else {
                services
                    .preferences
                    .find_by_key(&super::common::normalized_key(&update.subject))
                    .await?
            };
            plan.preferences.push(self.prepare_preference(
                update,
                existing,
                proposal.proposed_at,
            )?);
        }
        for update in &proposal.interest_updates {
            let existing = if let Some(id) = update.interest_id {
                services.interests.get(id).await?
            } else {
                services
                    .interests
                    .find_by_key(&super::common::normalized_key(&update.topic))
                    .await?
            };
            plan.interests
                .push(self.prepare_interest(update, existing, proposal.proposed_at)?);
        }
        for update in &proposal.open_question_updates {
            let existing = if let Some(id) = update.question_id {
                services.open_questions.get(id).await?
            } else {
                services
                    .open_questions
                    .find_open_by_key(
                        update.scope,
                        &super::common::normalized_key(&update.question),
                    )
                    .await?
            };
            let upsert = self.prepare_open_question(update, existing, proposal.proposed_at)?;
            ensure_scope(proposal.scope, upsert.value.scope())?;
            plan.open_questions.push(upsert);
        }
        for update in &proposal.agenda_updates {
            let existing = if let Some(id) = update.item_id {
                services.agenda.get(id).await?
            } else {
                services
                    .agenda
                    .find_active_by_key(update.scope, &update.subject.dedupe_key())
                    .await?
            };
            let upsert = self.prepare_agenda(update, existing, proposal.proposed_at)?;
            ensure_scope(proposal.scope, upsert.value.scope())?;
            plan.agenda.push(upsert);
        }

        plan.validate(self.config)?;
        Ok(plan)
    }

    pub async fn consolidate(
        &self,
        services: &MindServices,
        proposal: &ReflectionProposal,
    ) -> Result<ConsolidationResult, ConsolidationError> {
        let plan = self.prepare(services, proposal).await?;
        Ok(services.consolidation.apply(&plan).await?)
    }

    fn prepare_belief(
        &self,
        proposal: &BeliefUpdateProposal,
        existing: Option<Belief>,
        now: DateTime<Utc>,
    ) -> Result<MindUpsert<Belief>, ConsolidationError> {
        proposal.validate()?;
        let directed_delta = match proposal.operation {
            BeliefOperation::Reinforce => proposal.confidence_delta.abs(),
            BeliefOperation::Contradict | BeliefOperation::Retract => {
                -proposal.confidence_delta.abs().max(0.01)
            }
            BeliefOperation::Upsert => proposal.confidence_delta,
        }
        .clamp(-self.config.max_belief_delta, self.config.max_belief_delta);
        let stability_delta = proposal
            .stability_delta
            .clamp(-self.config.max_belief_delta, self.config.max_belief_delta);
        match existing {
            Some(existing) => {
                check_expected(
                    "belief",
                    existing.id().to_string(),
                    proposal.expected_version,
                    existing.version(),
                )?;
                if existing.scope() != proposal.scope
                    || existing.proposition_key()
                        != super::common::normalized_key(&proposal.proposition)
                {
                    return Err(ConsolidationError::ScopeMismatch);
                }
                let expected = existing.version();
                let value = existing.apply_delta(
                    directed_delta,
                    stability_delta,
                    &proposal.evidence_refs,
                    now,
                )?;
                Ok(MindUpsert {
                    value,
                    expected_version: Some(expected),
                })
            }
            None => {
                if proposal.operation != BeliefOperation::Upsert {
                    return Err(MindStoreError::NotFound {
                        kind: "belief",
                        id: proposal
                            .belief_id
                            .map_or_else(|| "dedupe-key".to_owned(), |id| id.to_string()),
                    }
                    .into());
                }
                let value = Belief::new(
                    proposal.belief_id.unwrap_or_default(),
                    proposal.scope,
                    proposal.proposition.clone(),
                    (0.5 + directed_delta).clamp(0.0, 1.0),
                    (0.25 + stability_delta).clamp(0.0, 1.0),
                    proposal.source,
                    proposal.evidence_refs.clone(),
                    proposal.valid_until,
                    now,
                )?;
                Ok(MindUpsert {
                    value,
                    expected_version: None,
                })
            }
        }
    }

    fn prepare_preference(
        &self,
        proposal: &PreferenceUpdateProposal,
        existing: Option<Preference>,
        now: DateTime<Utc>,
    ) -> Result<MindUpsert<Preference>, ConsolidationError> {
        proposal.validate()?;
        let direction = if proposal.operation == PreferenceOperation::Weaken {
            -1.0
        } else {
            1.0
        };
        let clamp = |value: f32| {
            (value.abs() * direction).clamp(
                -self.config.max_preference_delta,
                self.config.max_preference_delta,
            )
        };
        match existing {
            Some(existing) => {
                check_expected(
                    "preference",
                    existing.id().to_string(),
                    proposal.expected_version,
                    existing.version(),
                )?;
                if existing.subject_key() != super::common::normalized_key(&proposal.subject) {
                    return Err(ConsolidationError::ScopeMismatch);
                }
                let expected = existing.version();
                let value = existing.apply_delta(
                    if proposal.operation == PreferenceOperation::Weaken {
                        -proposal.valence_delta.signum()
                            * proposal
                                .valence_delta
                                .abs()
                                .min(self.config.max_preference_delta)
                    } else {
                        proposal.valence_delta.clamp(
                            -self.config.max_preference_delta,
                            self.config.max_preference_delta,
                        )
                    },
                    clamp(proposal.intensity_delta),
                    clamp(proposal.confidence_delta),
                    now,
                )?;
                Ok(MindUpsert {
                    value,
                    expected_version: Some(expected),
                })
            }
            None => {
                if proposal.operation != PreferenceOperation::Upsert {
                    return Err(MindStoreError::NotFound {
                        kind: "preference",
                        id: proposal
                            .preference_id
                            .map_or_else(|| "dedupe-key".to_owned(), |id| id.to_string()),
                    }
                    .into());
                }
                let value = Preference::new(
                    proposal.preference_id.unwrap_or_default(),
                    proposal.subject.clone(),
                    proposal.valence_delta.clamp(
                        -self.config.max_preference_delta,
                        self.config.max_preference_delta,
                    ),
                    proposal
                        .intensity_delta
                        .abs()
                        .min(self.config.max_preference_delta),
                    (0.4 + proposal.confidence_delta).clamp(0.0, 1.0),
                    0.25,
                    proposal.source,
                    now,
                )?;
                Ok(MindUpsert {
                    value,
                    expected_version: None,
                })
            }
        }
    }

    fn prepare_interest(
        &self,
        proposal: &InterestUpdateProposal,
        existing: Option<Interest>,
        now: DateTime<Utc>,
    ) -> Result<MindUpsert<Interest>, ConsolidationError> {
        proposal.validate()?;
        match existing {
            Some(existing) => {
                check_expected(
                    "interest",
                    existing.id().to_string(),
                    proposal.expected_version,
                    existing.version(),
                )?;
                if existing.topic_key() != super::common::normalized_key(&proposal.topic) {
                    return Err(ConsolidationError::ScopeMismatch);
                }
                let expected = existing.version();
                let value = match proposal.operation {
                    InterestOperation::Decay => existing.decay(now, 6.0 * 60.0 * 60.0)?,
                    InterestOperation::Upsert | InterestOperation::Activate => existing.activate(
                        proposal.activation_delta.clamp(-0.5, 0.5),
                        proposal.affinity_delta.clamp(
                            -self.config.max_interest_affinity_delta,
                            self.config.max_interest_affinity_delta,
                        ),
                        proposal.novelty,
                        now,
                    )?,
                };
                Ok(MindUpsert {
                    value,
                    expected_version: Some(expected),
                })
            }
            None => {
                if proposal.operation != InterestOperation::Upsert {
                    return Err(MindStoreError::NotFound {
                        kind: "interest",
                        id: proposal
                            .interest_id
                            .map_or_else(|| "dedupe-key".to_owned(), |id| id.to_string()),
                    }
                    .into());
                }
                let value = Interest::new(
                    proposal.interest_id.unwrap_or_default(),
                    proposal.topic.clone(),
                    proposal.activation_delta.clamp(0.0, 1.0),
                    proposal
                        .affinity_delta
                        .clamp(0.0, self.config.max_interest_affinity_delta),
                    proposal.novelty,
                    proposal.source,
                    now,
                )?;
                Ok(MindUpsert {
                    value,
                    expected_version: None,
                })
            }
        }
    }

    fn prepare_open_question(
        &self,
        proposal: &OpenQuestionUpdateProposal,
        existing: Option<OpenQuestion>,
        now: DateTime<Utc>,
    ) -> Result<MindUpsert<OpenQuestion>, ConsolidationError> {
        proposal.validate()?;
        match existing {
            Some(existing) => {
                check_expected(
                    "open_question",
                    existing.id().to_string(),
                    proposal.expected_version,
                    existing.version(),
                )?;
                if existing.scope() != proposal.scope
                    || existing.question_key() != super::common::normalized_key(&proposal.question)
                {
                    return Err(ConsolidationError::ScopeMismatch);
                }
                let expected = existing.version();
                let value = match proposal.operation {
                    OpenQuestionOperation::Upsert => existing.refresh(
                        proposal.related_beliefs.clone(),
                        proposal.salience,
                        now,
                    )?,
                    OpenQuestionOperation::Resolve => {
                        existing.transition(OpenQuestionStatus::Resolved, now)?
                    }
                    OpenQuestionOperation::Drop => {
                        existing.transition(OpenQuestionStatus::Dropped, now)?
                    }
                };
                Ok(MindUpsert {
                    value,
                    expected_version: Some(expected),
                })
            }
            None => {
                if proposal.operation != OpenQuestionOperation::Upsert {
                    return Err(MindStoreError::NotFound {
                        kind: "open_question",
                        id: proposal
                            .question_id
                            .map_or_else(|| "dedupe-key".to_owned(), |id| id.to_string()),
                    }
                    .into());
                }
                let value = OpenQuestion::new(
                    proposal.question_id.unwrap_or_default(),
                    proposal.scope,
                    proposal.question.clone(),
                    proposal.related_beliefs.clone(),
                    proposal.salience,
                    now,
                )?;
                Ok(MindUpsert {
                    value,
                    expected_version: None,
                })
            }
        }
    }

    fn prepare_agenda(
        &self,
        proposal: &AgendaUpdateProposal,
        existing: Option<AgendaItem>,
        now: DateTime<Utc>,
    ) -> Result<MindUpsert<AgendaItem>, ConsolidationError> {
        proposal.validate()?;
        match existing {
            Some(existing) => {
                check_expected(
                    "agenda",
                    existing.id().to_string(),
                    proposal.expected_version,
                    existing.version(),
                )?;
                if existing.scope() != proposal.scope
                    || existing.subject().dedupe_key() != proposal.subject.dedupe_key()
                {
                    return Err(ConsolidationError::ScopeMismatch);
                }
                let expected = existing.version();
                let value = match proposal.operation {
                    AgendaOperation::Activate => existing.activate(proposal.activation, now)?,
                    AgendaOperation::Defer => existing.defer(
                        proposal
                            .defer_until
                            .ok_or(MindValidationError::InvalidProposal {
                                reason: "agenda defer proposal requires defer_until",
                            })?,
                        now,
                    )?,
                    AgendaOperation::Resolve => existing.transition(AgendaStatus::Resolved, now)?,
                    AgendaOperation::Drop => existing.transition(AgendaStatus::Dropped, now)?,
                };
                Ok(MindUpsert {
                    value,
                    expected_version: Some(expected),
                })
            }
            None => {
                if proposal.operation != AgendaOperation::Activate {
                    return Err(MindStoreError::NotFound {
                        kind: "agenda",
                        id: proposal
                            .item_id
                            .map_or_else(|| "dedupe-key".to_owned(), |id| id.to_string()),
                    }
                    .into());
                }
                let value = AgendaItem::new(
                    proposal.item_id.unwrap_or_default(),
                    proposal.scope,
                    proposal.subject.clone(),
                    proposal.salience,
                    proposal.activation,
                    proposal.stability,
                    proposal.source,
                    now,
                )?;
                Ok(MindUpsert {
                    value,
                    expected_version: None,
                })
            }
        }
    }
}

fn ensure_scope(expected: MindScope, actual: MindScope) -> Result<(), ConsolidationError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ConsolidationError::ScopeMismatch)
    }
}

fn check_expected(
    kind: &'static str,
    id: String,
    expected: Option<u64>,
    actual: u64,
) -> Result<(), MindStoreError> {
    if let Some(expected) = expected
        && expected != actual
    {
        return Err(MindStoreError::VersionConflict {
            kind,
            id,
            expected,
            actual,
        });
    }
    Ok(())
}

#[allow(dead_code)]
fn _sources_are_intentionally_distinct(_: BeliefSource, _: PreferenceSource) {}
