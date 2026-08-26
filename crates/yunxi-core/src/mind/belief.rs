use super::common::{
    MAX_EVIDENCE_REFS, MindScope, MindValidationError, SCHEMA_VERSION, looks_sensitive,
    normalized_key, validate_mind_text, validate_signed_unit, validate_unit,
};
use super::episode::EpisodeId;
use crate::{EventId, MemoryId, MessageId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

mind_id!(BeliefId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeliefSource {
    Seed,
    Experience,
    Conversation,
    ToolResult,
    Reflection,
    Inference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidencePolarity {
    Supports,
    Contradicts,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum EvidenceKind {
    Event(EventId),
    Message(MessageId),
    Memory(MemoryId),
    Episode(EpisodeId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRef {
    kind: EvidenceKind,
    polarity: EvidencePolarity,
    reliability: f32,
    observed_at: DateTime<Utc>,
}

impl EvidenceRef {
    pub fn new(
        kind: EvidenceKind,
        polarity: EvidencePolarity,
        reliability: f32,
        observed_at: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        Ok(Self {
            kind,
            polarity,
            reliability: validate_unit(reliability, "evidence reliability")?,
            observed_at,
        })
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        validate_unit(self.reliability, "evidence reliability")?;
        Ok(())
    }

    #[must_use]
    pub const fn kind(&self) -> EvidenceKind {
        self.kind
    }

    #[must_use]
    pub const fn polarity(&self) -> EvidencePolarity {
        self.polarity
    }

    #[must_use]
    pub const fn reliability(&self) -> f32 {
        self.reliability
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Belief {
    id: BeliefId,
    scope: MindScope,
    proposition: String,
    proposition_key: String,
    confidence: f32,
    stability: f32,
    source: BeliefSource,
    evidence_refs: Vec<EvidenceRef>,
    contradiction_count: u32,
    last_contradicted_at: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    version: u64,
    schema_version: u16,
}

impl Belief {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: BeliefId,
        scope: MindScope,
        proposition: impl Into<String>,
        confidence: f32,
        stability: f32,
        source: BeliefSource,
        evidence_refs: Vec<EvidenceRef>,
        valid_until: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        let proposition = validate_mind_text(proposition, "belief proposition")?;
        let belief = Self {
            id,
            scope,
            proposition_key: normalized_key(&proposition),
            proposition,
            confidence: validate_unit(confidence, "belief confidence")?,
            stability: validate_unit(stability, "belief stability")?,
            source,
            evidence_refs,
            contradiction_count: 0,
            last_contradicted_at: None,
            valid_until,
            created_at: now,
            updated_at: now,
            version: 1,
            schema_version: SCHEMA_VERSION,
        };
        belief.validate()?;
        Ok(belief)
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        let proposition = validate_mind_text(self.proposition.clone(), "belief proposition")?;
        if normalized_key(&proposition) != self.proposition_key {
            return Err(MindValidationError::InvalidProposal {
                reason: "belief proposition key does not match its proposition",
            });
        }
        validate_unit(self.confidence, "belief confidence")?;
        validate_unit(self.stability, "belief stability")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported belief schema version",
            });
        }
        if self.updated_at < self.created_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "belief updated_at predates created_at",
            });
        }
        if self
            .valid_until
            .is_some_and(|until| until <= self.created_at)
        {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "belief valid_until must follow created_at",
            });
        }
        if self.evidence_refs.len() > MAX_EVIDENCE_REFS {
            return Err(MindValidationError::TooManyItems {
                field: "belief evidence",
                length: self.evidence_refs.len(),
                maximum: MAX_EVIDENCE_REFS,
            });
        }
        let mut evidence = HashSet::new();
        for item in &self.evidence_refs {
            item.validate()?;
            if !evidence.insert(item.kind()) {
                return Err(MindValidationError::Duplicate {
                    field: "belief evidence reference",
                });
            }
        }
        if matches!(self.scope, MindScope::Person { .. })
            && matches!(
                self.source,
                BeliefSource::Inference | BeliefSource::Reflection
            )
            && looks_sensitive(&self.proposition)
        {
            return Err(MindValidationError::SensitivePersonInference);
        }
        Ok(())
    }

    pub fn apply_delta(
        &self,
        confidence_delta: f32,
        stability_delta: f32,
        evidence_refs: &[EvidenceRef],
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        validate_signed_unit(confidence_delta, "belief confidence delta")?;
        validate_signed_unit(stability_delta, "belief stability delta")?;
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "belief update predates the stored version",
            });
        }
        let mut updated = self.clone();
        updated.confidence = (updated.confidence + confidence_delta).clamp(0.0, 1.0);
        updated.stability = (updated.stability + stability_delta).clamp(0.0, 1.0);
        if confidence_delta < 0.0
            || evidence_refs
                .iter()
                .any(|item| item.polarity() == EvidencePolarity::Contradicts)
        {
            updated.contradiction_count = updated.contradiction_count.saturating_add(1);
            updated.last_contradicted_at = Some(now);
        }
        for evidence in evidence_refs {
            if !updated
                .evidence_refs
                .iter()
                .any(|stored| stored.kind() == evidence.kind())
            {
                updated.evidence_refs.push(evidence.clone());
            }
        }
        if updated.evidence_refs.len() > MAX_EVIDENCE_REFS {
            updated
                .evidence_refs
                .sort_by_key(|evidence| evidence.observed_at());
            let excess = updated.evidence_refs.len() - MAX_EVIDENCE_REFS;
            updated.evidence_refs.drain(..excess);
        }
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    #[must_use]
    pub const fn id(&self) -> BeliefId {
        self.id
    }

    #[must_use]
    pub const fn scope(&self) -> MindScope {
        self.scope
    }

    #[must_use]
    pub fn proposition(&self) -> &str {
        &self.proposition
    }

    #[must_use]
    pub fn proposition_key(&self) -> &str {
        &self.proposition_key
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn stability(&self) -> f32 {
        self.stability
    }

    #[must_use]
    pub const fn source(&self) -> BeliefSource {
        self.source
    }

    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        &self.evidence_refs
    }

    #[must_use]
    pub const fn contradiction_count(&self) -> u32 {
        self.contradiction_count
    }

    #[must_use]
    pub const fn valid_until(&self) -> Option<DateTime<Utc>> {
        self.valid_until
    }

    #[must_use]
    pub const fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.valid_until.is_none_or(|until| until > now)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeliefOperation {
    Upsert,
    Reinforce,
    Contradict,
    Retract,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeliefUpdateProposal {
    pub operation: BeliefOperation,
    pub belief_id: Option<BeliefId>,
    pub expected_version: Option<u64>,
    pub scope: MindScope,
    pub proposition: String,
    pub confidence_delta: f32,
    pub stability_delta: f32,
    pub source: BeliefSource,
    pub evidence_refs: Vec<EvidenceRef>,
    pub valid_until: Option<DateTime<Utc>>,
}

impl BeliefUpdateProposal {
    pub fn validate(&self) -> Result<(), MindValidationError> {
        validate_mind_text(self.proposition.clone(), "belief proposal proposition")?;
        validate_signed_unit(self.confidence_delta, "belief confidence delta")?;
        validate_signed_unit(self.stability_delta, "belief stability delta")?;
        if self.evidence_refs.len() > MAX_EVIDENCE_REFS {
            return Err(MindValidationError::TooManyItems {
                field: "belief proposal evidence",
                length: self.evidence_refs.len(),
                maximum: MAX_EVIDENCE_REFS,
            });
        }
        if matches!(
            self.operation,
            BeliefOperation::Reinforce | BeliefOperation::Contradict
        ) && self.belief_id.is_none()
        {
            return Err(MindValidationError::InvalidProposal {
                reason: "belief update requires an existing belief id",
            });
        }
        if self.expected_version == Some(0) {
            return Err(MindValidationError::ZeroVersion);
        }
        for evidence in &self.evidence_refs {
            evidence.validate()?;
        }
        if matches!(self.scope, MindScope::Person { .. })
            && matches!(
                self.source,
                BeliefSource::Inference | BeliefSource::Reflection
            )
            && looks_sensitive(&self.proposition)
        {
            return Err(MindValidationError::SensitivePersonInference);
        }
        Ok(())
    }
}
