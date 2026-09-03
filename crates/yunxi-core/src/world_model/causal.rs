//! Causal relations: local, limited, evidence-backed cause→effect tendencies
//! (v4 §39–45, §96–100, §188).
//!
//! This is never "world truth" and never human psychology: v1 only supports
//! Tool / Host / Environment / deterministic-domain patterns, and
//! person-specific relations are gated behind an explicit rule or seed.

use super::environment::HostId;
use super::{
    ObservationId, WorldValidationError,
    common::{clamp_unit, dedupe, validate_unit, validate_value},
};
use crate::{ConversationId, PersonId};
use serde::{Deserialize, Serialize};

pub const MAX_CAUSAL_RELATIONS: usize = 64;
pub const MAX_CAUSAL_CANDIDATES: usize = 128;
/// Repeated evidence required before a candidate may become active
/// (v4 §98); domain rules and seeds bypass the count.
pub const MIN_EVIDENCE_OCCURRENCES: u32 = 3;

/// What part of the world a pattern matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    Tool,
    Host,
    Environment,
    User,
    Situation,
    Unknown,
}

/// A small, bounded pattern vocabulary (v4 §40).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldPattern {
    kind: PatternKind,
    label: String,
}

impl WorldPattern {
    pub fn new(kind: PatternKind, label: impl Into<String>) -> Result<Self, WorldValidationError> {
        let label = validate_value(label, "pattern label")?;
        Ok(Self { kind, label })
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_value(self.label.clone(), "pattern label")?;
        Ok(())
    }

    #[must_use]
    pub const fn kind(&self) -> PatternKind {
        self.kind
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// How a causal relation came to be known (v4 §41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalSource {
    Seed,
    ObservedRepeatedPattern,
    ToolBehavior,
    Reflection,
    DomainRule,
}

/// Where the relation is allowed to be applied (v4 §42).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CausalScope {
    Global,
    ToolSpecific { tool: String },
    PersonSpecific { person_id: PersonId },
    ConversationSpecific { conversation_id: ConversationId },
    HostSpecific { host: HostId },
}

/// One active causal relation (v4 §40).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CausalRelation {
    id: super::CausalRelationId,
    cause: WorldPattern,
    effect: WorldPattern,
    strength: f32,
    confidence: f32,
    source: CausalSource,
    scope: CausalScope,
    evidence_occurrences: u32,
    version: u64,
}

impl CausalRelation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: super::CausalRelationId,
        cause: WorldPattern,
        effect: WorldPattern,
        strength: f32,
        confidence: f32,
        source: CausalSource,
        scope: CausalScope,
        evidence_occurrences: u32,
    ) -> Result<Self, WorldValidationError> {
        let relation = Self {
            id,
            cause,
            effect,
            strength: clamp_unit(strength),
            confidence: clamp_unit(confidence),
            source,
            scope,
            evidence_occurrences,
            version: 1,
        };
        relation.validate()?;
        Ok(relation)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        self.cause.validate()?;
        self.effect.validate()?;
        validate_unit(self.strength, "causal strength")?;
        validate_unit(self.confidence, "causal confidence")?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        if self.evidence_occurrences == 0 {
            return Err(WorldValidationError::InvalidState {
                reason: "causal relation has no occurrences",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::CausalRelationId {
        self.id
    }

    #[must_use]
    pub fn cause(&self) -> &WorldPattern {
        &self.cause
    }

    #[must_use]
    pub fn effect(&self) -> &WorldPattern {
        &self.effect
    }

    #[must_use]
    pub const fn strength(&self) -> f32 {
        self.strength
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn source(&self) -> CausalSource {
        self.source
    }

    #[must_use]
    pub fn scope(&self) -> CausalScope {
        self.scope.clone()
    }

    #[must_use]
    pub const fn evidence_occurrences(&self) -> u32 {
        self.evidence_occurrences
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Does this relation match a scope (for retrieval)?
    #[must_use]
    pub fn matches_scope(&self, person_id: Option<PersonId>, conversation_id: Option<ConversationId>) -> bool {
        match self.scope {
            CausalScope::Global | CausalScope::ToolSpecific { .. } | CausalScope::HostSpecific { .. } => true,
            CausalScope::PersonSpecific { person_id: scope_person } => Some(scope_person) == person_id,
            CausalScope::ConversationSpecific { conversation_id: scope_conversation } => {
                Some(scope_conversation) == conversation_id
            }
        }
    }
}

/// A not-yet-promoted causal candidate (v4 §97).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CausalRelationProposal {
    cause: WorldPattern,
    effect: WorldPattern,
    confidence: f32,
    evidence_refs: Vec<ObservationId>,
    proposed_scope: CausalScope,
}

impl CausalRelationProposal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cause: WorldPattern,
        effect: WorldPattern,
        confidence: f32,
        evidence_refs: Vec<ObservationId>,
        proposed_scope: CausalScope,
    ) -> Result<Self, WorldValidationError> {
        let proposal = Self {
            cause,
            effect,
            confidence: clamp_unit(confidence),
            evidence_refs: dedupe(evidence_refs, "causal evidence", true)?,
            proposed_scope,
        };
        proposal.validate()?;
        Ok(proposal)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        self.validate_without_scope_policy()?;
        self.validate_scope_policy()
    }

    /// Validate everything except the person-specific scope policy: used at
    /// promotion, where an explicit domain rule may legitimately authorize
    /// person-specific relations (v4 §99).
    pub fn validate_without_scope_policy(&self) -> Result<(), WorldValidationError> {
        self.cause.validate()?;
        self.effect.validate()?;
        validate_unit(self.confidence, "causal proposal confidence")?;
        Ok(())
    }

    /// v4 §99: person-specific causal learning is restricted; only an
    /// explicit domain rule or seed may promote it. Checked at construction
    /// and again at promotion.
    pub fn validate_scope_policy(&self) -> Result<(), WorldValidationError> {
        match self.proposed_scope {
            CausalScope::PersonSpecific { .. } => Err(WorldValidationError::InvalidState {
                reason: "person-specific causal relations require an explicit domain rule",
            }),
            _ => Ok(()),
        }
    }

    #[must_use]
    pub fn cause(&self) -> &WorldPattern {
        &self.cause
    }

    #[must_use]
    pub fn effect(&self) -> &WorldPattern {
        &self.effect
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub fn evidence_refs(&self) -> &[ObservationId] {
        &self.evidence_refs
    }

    #[must_use]
    pub fn proposed_scope(&self) -> CausalScope {
        self.proposed_scope.clone()
    }

    /// Dedupe key: same cause+effect+scope is one candidate.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}",
            self.cause,
            self.effect,
            self.proposed_scope
        )
    }
}

/// Promotion rules (v4 §98): repeated evidence OR an explicit domain rule /
/// seed. Person-specific promotion additionally requires the explicit rule.
pub fn promote_candidate(
    proposal: CausalRelationProposal,
    occurrences: u32,
    domain_rule: bool,
    source: CausalSource,
    id: super::CausalRelationId,
) -> Result<CausalRelation, WorldValidationError> {
    proposal.validate_without_scope_policy()?;
    let authenticated = matches!(source, CausalSource::Seed | CausalSource::DomainRule);
    if !occurrences_qualify(occurrences, domain_rule || authenticated) {
        return Err(WorldValidationError::InvalidState {
            reason: "causal candidate lacks required repeated evidence",
        });
    }
    if let CausalScope::PersonSpecific { .. } = proposal.proposed_scope()
        && !domain_rule && !authenticated {
            return Err(WorldValidationError::InvalidState {
                reason: "person-specific causal relations require an explicit domain rule",
            });
        }
    CausalRelation::new(
        id,
        proposal.cause().clone(),
        proposal.effect().clone(),
        proposal.confidence().clamp(0.0, 1.0),
        proposal.confidence(),
        source,
        proposal.proposed_scope(),
        occurrences.max(1),
    )
}

#[must_use]
pub fn occurrences_qualify(occurrences: u32, domain_rule: bool) -> bool {
    domain_rule || occurrences >= MIN_EVIDENCE_OCCURRENCES
}

/// Bounded causal knowledge index (v4 §136: only high-confidence active
/// relations enter retrieval; candidates stay separate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CausalKnowledge {
    relations: Vec<CausalRelation>,
    candidates: Vec<CausalRelationProposal>,
}

impl CausalKnowledge {
    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.relations.len() > MAX_CAUSAL_RELATIONS {
            return Err(WorldValidationError::TooManyItems {
                field: "causal relations",
                length: self.relations.len(),
                maximum: MAX_CAUSAL_RELATIONS,
            });
        }
        if self.candidates.len() > MAX_CAUSAL_CANDIDATES {
            return Err(WorldValidationError::TooManyItems {
                field: "causal candidates",
                length: self.candidates.len(),
                maximum: MAX_CAUSAL_CANDIDATES,
            });
        }
        for relation in &self.relations {
            relation.validate()?;
        }
        for candidate in &self.candidates {
            candidate.validate()?;
        }
        let mut ids = Vec::new();
        for relation in &self.relations {
            if ids.contains(&relation.id()) {
                return Err(WorldValidationError::DuplicateItem {
                    field: "causal relation id",
                });
            }
            ids.push(relation.id());
        }
        Ok(())
    }

    #[must_use]
    pub fn relations(&self) -> &[CausalRelation] {
        &self.relations
    }

    #[must_use]
    pub fn candidates(&self) -> &[CausalRelationProposal] {
        &self.candidates
    }

    /// Record a proposal: dedupe by fingerprint, merges evidence.
    pub fn add_proposal(
        &mut self,
        proposal: CausalRelationProposal,
    ) -> Result<(), WorldValidationError> {
        proposal.validate()?;
        if let Some(existing) = self
            .candidates
            .iter_mut()
            .find(|existing| existing.fingerprint() == proposal.fingerprint())
        {
            let mut evidence = existing.evidence_refs.to_vec();
            evidence.extend(proposal.evidence_refs.to_vec());
            evidence = dedupe(evidence, "causal evidence", true)?;
            let confidence = existing.confidence.max(proposal.confidence);
            *existing = CausalRelationProposal {
                cause: existing.cause.clone(),
                effect: existing.effect.clone(),
                confidence,
                evidence_refs: evidence,
                proposed_scope: existing.proposed_scope.clone(),
            };
        } else if self.candidates.len() >= MAX_CAUSAL_CANDIDATES {
            return Err(WorldValidationError::TooManyItems {
                field: "causal candidates",
                length: self.candidates.len(),
                maximum: MAX_CAUSAL_CANDIDATES,
            });
        } else {
            self.candidates.push(proposal);
        }
        Ok(())
    }

    /// Promote a candidate or create a relation directly (dedupe by
    /// cause+effect+scope). Returns the promoted id.
    pub fn promote(
        &mut self,
        relation: CausalRelation,
        proposal_fingerprint: Option<&str>,
    ) -> Result<(), WorldValidationError> {
        relation.validate()?;
        let duplicate = self.relations.iter().any(|existing| {
            existing.cause() == relation.cause()
                && existing.effect() == relation.effect()
                && existing.scope() == relation.scope()
        });
        if duplicate {
            return Err(WorldValidationError::DuplicateItem {
                field: "causal relation",
            });
        }
        if self.relations.len() >= MAX_CAUSAL_RELATIONS {
            return Err(WorldValidationError::TooManyItems {
                field: "causal relations",
                length: self.relations.len(),
                maximum: MAX_CAUSAL_RELATIONS,
            });
        }
        if let Some(fingerprint) = proposal_fingerprint {
            self.candidates.retain(|candidate| candidate.fingerprint() != fingerprint);
        }
        self.relations.push(relation);
        Ok(())
    }

    /// Active relations relevant to a scope, confidence-descending (bounded
    /// by `limit`); only high-confidence entries quality (v4 §136).
    pub fn relevant(
        &self,
        person_id: Option<PersonId>,
        conversation_id: Option<ConversationId>,
        limit: usize,
    ) -> Vec<&CausalRelation> {
        let mut matched: Vec<_> = self
            .relations
            .iter()
            .filter(|relation| relation.confidence() >= 0.6)
            .filter(|relation| relation.matches_scope(person_id, conversation_id))
            .collect();
        matched.sort_by(|a, b| b.confidence().total_cmp(&a.confidence()));
        matched.truncate(limit);
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_pattern(tool: &str) -> WorldPattern {
        WorldPattern::new(PatternKind::Tool, tool).expect("pattern")
    }

    fn rate_kind() -> WorldPattern {
        WorldPattern::new(PatternKind::Environment, "rate_limited").expect("pattern")
    }

    fn proposal(scope: CausalScope) -> CausalRelationProposal {
        CausalRelationProposal::new(
            rate_kind(),
            tool_pattern("retry_now_likely_fail"),
            0.8,
            vec![ObservationId::new(), ObservationId::new()],
            scope,
        )
        .expect("proposal")
    }

    #[test]
    fn promotion_requires_repeated_evidence_or_domain_rule() {
        // 2 occurrences, no rule → rejected (v4 §98).
        assert!(promote_candidate(
            proposal(CausalScope::Global),
            2,
            false,
            CausalSource::ObservedRepeatedPattern,
            super::super::CausalRelationId::new(),
        )
        .is_err());
        // 3 occurrences → promoted.
        let relation = promote_candidate(
            proposal(CausalScope::Global),
            3,
            false,
            CausalSource::ObservedRepeatedPattern,
            super::super::CausalRelationId::new(),
        )
        .expect("promoted");
        assert_eq!(relation.evidence_occurrences(), 3);
        // Domain rule promotes without repeated evidence.
        assert!(promote_candidate(
            proposal(CausalScope::Global),
            0,
            true,
            CausalSource::DomainRule,
            super::super::CausalRelationId::new(),
        )
        .is_ok());
    }

    #[test]
    fn person_specific_causal_learning_is_restricted() {
        // Proposal refuses person-specific scope at construction (v4 §99).
        let proposal = CausalRelationProposal::new(
            rate_kind(),
            tool_pattern("maybe_upset"),
            0.6,
            vec![],
            CausalScope::PersonSpecific {
                person_id: PersonId::new(),
            },
        );
        assert!(proposal.is_err());
        // Even a hand-built proposal (bypassing new) is rejected at
        // promotion unless an explicit domain rule exists.
        let direct = CausalRelationProposal {
            cause: rate_kind(),
            effect: tool_pattern("maybe_upset"),
            confidence: 0.6,
            evidence_refs: vec![],
            proposed_scope: CausalScope::PersonSpecific {
                person_id: PersonId::new(),
            },
        };
        assert!(promote_candidate(
            direct.clone(),
            3,
            false,
            CausalSource::ObservedRepeatedPattern,
            super::super::CausalRelationId::new(),
        )
        .is_err());
        assert!(promote_candidate(
            direct,
            1,
            true,
            CausalSource::DomainRule,
            super::super::CausalRelationId::new(),
        )
        .is_ok());
    }

    #[test]
    fn causal_knowledge_dedupes_candidates_and_bounds_relations() {
        let mut knowledge = CausalKnowledge::default();
        let scope = CausalScope::Global;
        knowledge
            .add_proposal(proposal(scope.clone()))
            .expect("first proposal");
        knowledge
            .add_proposal(proposal(scope))
            .expect("merge instead of duplicate");
        assert_eq!(knowledge.candidates().len(), 1);
        // Promote from the merged candidate (fingerprint match removes it).
        let candidate = knowledge.candidates()[0].clone();
        let relation = promote_candidate(
            candidate.clone(),
            3,
            true,
            CausalSource::DomainRule,
            super::super::CausalRelationId::new(),
        )
        .expect("promote");
        knowledge
            .promote(relation, Some(&candidate.fingerprint()))
            .expect("stored");
        assert_eq!(knowledge.relations().len(), 1);
        assert!(knowledge.candidates().is_empty());
        // Duplicate relation rejected.
        let duplicate = promote_candidate(
            candidate.clone(),
            3,
            true,
            CausalSource::DomainRule,
            super::super::CausalRelationId::new(),
        )
        .expect("promote");
        assert!(knowledge.promote(duplicate, None).is_err());
    }

    #[test]
    fn relevant_relations_are_confidence_scoped_and_bounded() {
        let mut knowledge = CausalKnowledge::default();
        let person = PersonId::new();
        for i in 0..4 {
            let relation = CausalRelation::new(
                super::super::CausalRelationId::new(),
                rate_kind(),
                tool_pattern(&format!("outcome_{i}")),
                0.7,
                0.8,
                CausalSource::DomainRule,
                CausalScope::PersonSpecific { person_id: person },
                1,
            )
            .expect("relation");
            knowledge.promote(relation, None).expect("stored");
        }
        let relevant = knowledge.relevant(Some(person), None, 2);
        assert_eq!(relevant.len(), 2);
        assert!(relevant.iter().all(|r| r.confidence() >= 0.6));
        // Another person sees nothing.
        assert!(knowledge.relevant(Some(PersonId::new()), None, 4).is_empty());
    }
}
