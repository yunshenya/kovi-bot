//! Hypothesis: an explicit "we are not sure" that is never silently upgraded
//! to fact (v4 §27–33, §182).
//!
//! Known / Suspected / Unknown are distinct. A hypothesis carries evidence
//! both for and against, a status, a TTL, and merges with a sibling sharing
//! the same proposition key instead of multiplying.

use super::ObservationId;
use super::{
    WorldScope, WorldValidationError,
    common::{
        MAX_EVIDENCE_REFS, MAX_WORLD_VALUE_CHARS, clamp_unit, dedupe, validate_text, validate_unit,
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_HYPOTHESIS_TEXT_BYTES: usize = MAX_WORLD_VALUE_CHARS * 2;
pub const MAX_HYPOTHESIS_TEXT_CHARS: usize = MAX_WORLD_VALUE_CHARS;
pub const MAX_ACTIVE_HYPOTHESES_PER_PERSON: usize = 16;
pub const MAX_ACTIVE_HYPOTHESES_PER_CONVERSATION: usize = 16;
/// Minimum confidence for a hypothesis to be created at all (v4 §31).
pub const MIN_HYPOTHESIS_CREATE_CONFIDENCE: f32 = 0.20;

/// Lifecycle status of a hypothesis (v4 §29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisStatus {
    Active,
    Supported,
    Rejected,
    Superseded,
    Expired,
    Unknown,
}

impl HypothesisStatus {
    #[must_use]
    pub const fn is_resolved(self) -> bool {
        matches!(
            self,
            Self::Supported | Self::Rejected | Self::Superseded | Self::Expired
        )
    }
}

/// A proposition with a normalization key for dedupe/merge (v4 §148).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldProposition {
    text: String,
    key: String,
}

impl WorldProposition {
    pub fn new(text: impl Into<String>) -> Result<Self, WorldValidationError> {
        let text = validate_text(text, "hypothesis proposition")?;
        let key = normalized_proposition(&text);
        Ok(Self { text, key })
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_text(self.text.clone(), "hypothesis proposition")?;
        if normalized_proposition(&self.text) != self.key {
            return Err(WorldValidationError::InvalidState {
                reason: "proposition normalized key mismatch",
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Is `other` the direct negation of this proposition ("not ..." pair)?
    #[must_use]
    pub fn is_negation(&self, other: &WorldProposition) -> bool {
        let a = self.key.strip_prefix("not").unwrap_or(&self.key);
        let b = other.key.strip_prefix("not").unwrap_or(&other.key);
        !a.is_empty() && !b.is_empty() && a == b && self.key != other.key
    }
}

/// Normalize a proposition into a stable dedupe key: lowercase with all
/// whitespace removed (so "用户 生气" and "用户生气" are the same key).
#[must_use]
pub fn normalized_proposition(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("")
}

/// A single hypothesis (v4 §28).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hypothesis {
    id: super::HypothesisId,
    proposition: WorldProposition,
    scope: WorldScope,
    confidence: f32,
    evidence_for: Vec<ObservationId>,
    evidence_against: Vec<ObservationId>,
    status: HypothesisStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    version: u64,
}

impl Hypothesis {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: super::HypothesisId,
        proposition: WorldProposition,
        scope: WorldScope,
        confidence: f32,
        created_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Self, WorldValidationError> {
        let confidence = clamp_unit(confidence);
        if confidence < MIN_HYPOTHESIS_CREATE_CONFIDENCE {
            return Err(WorldValidationError::InvalidState {
                reason: "hypothesis confidence below creation threshold",
            });
        }
        let hypothesis = Self {
            id,
            proposition,
            scope,
            confidence,
            evidence_for: Vec::new(),
            evidence_against: Vec::new(),
            status: HypothesisStatus::Active,
            created_at,
            updated_at: created_at,
            expires_at,
            version: 1,
        };
        hypothesis.validate()?;
        Ok(hypothesis)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        self.proposition.validate()?;
        validate_unit(self.confidence, "hypothesis confidence")?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        if self.evidence_for.len() > MAX_EVIDENCE_REFS {
            return Err(WorldValidationError::TooManyItems {
                field: "evidence for",
                length: self.evidence_for.len(),
                maximum: MAX_EVIDENCE_REFS,
            });
        }
        if self.evidence_against.len() > MAX_EVIDENCE_REFS {
            return Err(WorldValidationError::TooManyItems {
                field: "evidence against",
                length: self.evidence_against.len(),
                maximum: MAX_EVIDENCE_REFS,
            });
        }
        for id in self.evidence_for.iter().chain(self.evidence_against.iter()) {
            if self.evidence_for.contains(id) && self.evidence_against.contains(id) {
                return Err(WorldValidationError::InvalidState {
                    reason: "the same observation counts for and against one hypothesis",
                });
            }
        }
        if self.updated_at < self.created_at {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "hypothesis update predates its creation",
            });
        }
        if let Some(expires_at) = self.expires_at
            && expires_at < self.created_at
        {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "hypothesis expires before creation",
            });
        }
        if self.status == HypothesisStatus::Supported && self.evidence_for.is_empty() {
            return Err(WorldValidationError::InvalidState {
                reason: "supported hypothesis has no evidence",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::HypothesisId {
        self.id
    }

    #[must_use]
    pub fn proposition(&self) -> &WorldProposition {
        &self.proposition
    }

    #[must_use]
    pub const fn scope(&self) -> WorldScope {
        self.scope
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub fn evidence_for(&self) -> &[ObservationId] {
        &self.evidence_for
    }

    #[must_use]
    pub fn evidence_against(&self) -> &[ObservationId] {
        &self.evidence_against
    }

    #[must_use]
    pub const fn status(&self) -> HypothesisStatus {
        self.status
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
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Same proposition key (dedupe/merge target).
    #[must_use]
    pub fn same_proposition(&self, other: &Hypothesis) -> bool {
        self.proposition.key() == other.proposition.key()
    }

    /// Can this hypothesis coexist with `other`? Contradictory pairs may
    /// coexist until evidence grows (v4 §149), but duplicated keys cannot.
    #[must_use]
    pub fn can_coexist(&self, other: &Hypothesis) -> bool {
        if self.scope != other.scope {
            return true;
        }
        !self.same_proposition(other) || self.proposition.is_negation(&other.proposition)
    }

    /// Merge another hypothesis with the same proposition: evidence union,
    /// latest timestamps, confidence = max (v4 §88, §148).
    pub fn merge(&mut self, other: Hypothesis) -> Result<(), WorldValidationError> {
        other.validate()?;
        if !self.same_proposition(&other) {
            return Err(WorldValidationError::DuplicateItem {
                field: "hypothesis merge mismatch",
            });
        }
        let mut evidence_for = self.evidence_for.clone();
        evidence_for.extend(other.evidence_for);
        evidence_for = dedupe(evidence_for, "evidence for", true)?;
        let mut evidence_against = self.evidence_against.clone();
        evidence_against.extend(other.evidence_against);
        evidence_against = dedupe(evidence_against, "evidence against", true)?;
        proof_disjoint(&evidence_for, &evidence_against)?;
        self.evidence_for = evidence_for;
        self.evidence_against = evidence_against;
        if other.confidence > self.confidence {
            self.confidence = other.confidence;
        }
        if other.updated_at > self.updated_at {
            self.updated_at = other.updated_at;
        }
        self.expires_at = self.expires_at.max(other.expires_at);
        self.version = self.version.saturating_add(1);
        self.validate()
    }

    /// Attach one observation as evidence (for/against) and bump version.
    pub fn add_evidence(
        &mut self,
        observation_id: ObservationId,
        for_hypothesis: bool,
        now: DateTime<Utc>,
    ) -> Result<(), WorldValidationError> {
        if now < self.updated_at {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "evidence predates hypothesis update",
            });
        }
        let target = if for_hypothesis {
            &mut self.evidence_for
        } else {
            &mut self.evidence_against
        };
        if !target.contains(&observation_id) {
            if target.len() >= MAX_EVIDENCE_REFS {
                return Err(WorldValidationError::TooManyItems {
                    field: "evidence refs",
                    length: target.len() + 1,
                    maximum: MAX_EVIDENCE_REFS,
                });
            }
            target.push(observation_id);
        }
        self.updated_at = now;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Resolve this hypothesis (validate the status transition).
    pub fn resolve(
        &mut self,
        status: HypothesisStatus,
        now: DateTime<Utc>,
    ) -> Result<(), WorldValidationError> {
        if !status.is_resolved() {
            return Err(WorldValidationError::InvalidTransition {
                from: hypothesis_status_label(self.status),
                to: hypothesis_status_label(status),
            });
        }
        if self.status != HypothesisStatus::Active {
            return Err(WorldValidationError::InvalidTransition {
                from: hypothesis_status_label(self.status),
                to: hypothesis_status_label(status),
            });
        }
        if now < self.updated_at {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "resolution predates hypothesis update",
            });
        }
        self.status = status;
        self.updated_at = now;
        self.version = self.version.saturating_add(1);
        self.validate()
    }

    #[must_use]
    pub fn freshness_at(&self, now: DateTime<Utc>) -> super::Freshness {
        super::temporal::freshness_at(self.updated_at, self.expires_at, now)
    }

    #[must_use]
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        matches!(self.freshness_at(now), super::Freshness::Expired)
    }
}

fn hypothesis_status_label(status: HypothesisStatus) -> &'static str {
    match status {
        HypothesisStatus::Active => "active",
        HypothesisStatus::Supported => "supported",
        HypothesisStatus::Rejected => "rejected",
        HypothesisStatus::Superseded => "superseded",
        HypothesisStatus::Expired => "expired",
        HypothesisStatus::Unknown => "unknown",
    }
}

fn proof_disjoint(
    evidence_for: &[ObservationId],
    evidence_against: &[ObservationId],
) -> Result<(), WorldValidationError> {
    for id in evidence_for {
        if evidence_against.contains(id) {
            return Err(WorldValidationError::InvalidState {
                reason: "the same observation counts for and against one hypothesis",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PersonId;
    use chrono::Duration;

    fn hypothesis(text: &str, confidence: f32, now: DateTime<Utc>) -> Hypothesis {
        Hypothesis::new(
            super::super::HypothesisId::new(),
            WorldProposition::new(text).expect("proposition"),
            WorldScope::Person {
                person_id: PersonId::new(),
            },
            confidence,
            now,
            None,
        )
        .expect("hypothesis")
    }

    #[test]
    fn creation_threshold_prevents_low_quality_hypotheses() {
        let now = Utc::now();
        assert!(
            Hypothesis::new(
                super::super::HypothesisId::new(),
                WorldProposition::new("可能忙").expect("proposition"),
                WorldScope::Global,
                0.1,
                now,
                None,
            )
            .is_err()
        );
        assert!(
            Hypothesis::new(
                super::super::HypothesisId::new(),
                WorldProposition::new("可能忙").expect("proposition"),
                WorldScope::Global,
                MIN_HYPOTHESIS_CREATE_CONFIDENCE,
                now,
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn merge_unions_evidence_and_keeps_max_confidence() {
        let now = Utc::now();
        let mut a = hypothesis("tool A 可能恢复", 0.4, now);
        let b = hypothesis("tool A 可能恢复", 0.7, now);
        let obs = ObservationId::new();
        a.add_evidence(obs, true, now).expect("evidence");
        a.merge(b).expect("merge");
        assert_eq!(a.confidence(), 0.7);
        assert_eq!(a.evidence_for(), &[obs]);
        assert_eq!(a.version(), 3); // new + evidence + merge
    }

    #[test]
    fn negation_pairs_coexist_and_dedupe_by_key() {
        let now = Utc::now();
        let person_id = PersonId::new();
        let make = |text: &str, confidence: f32| {
            Hypothesis::new(
                super::super::HypothesisId::new(),
                WorldProposition::new(text).expect("proposition"),
                WorldScope::Person { person_id },
                confidence,
                now,
                None,
            )
            .expect("hypothesis")
        };
        let a = make("用户生气", 0.25);
        let b = make("not 用户生气", 0.3);
        assert!(a.proposition().is_negation(b.proposition()));
        assert!(a.can_coexist(&b));
        let c = make("用户 生气", 0.5);
        assert!(a.same_proposition(&c));
        assert!(!a.can_coexist(&c));
    }

    #[test]
    fn contradiction_and_resolution_workflow() {
        let now = Utc::now();
        let mut h = hypothesis("任务会失败", 0.5, now);
        let obs_a = ObservationId::new();
        let obs_b = ObservationId::new();
        h.add_evidence(obs_a, true, now).expect("for");
        h.add_evidence(obs_b, false, now).expect("against");
        assert_eq!(h.evidence_for(), &[obs_a]);
        assert_eq!(h.evidence_against(), &[obs_b]);

        h.resolve(HypothesisStatus::Supported, now)
            .expect("supported");
        assert_eq!(h.status(), HypothesisStatus::Supported);
        // Resolved hypotheses cannot resolve again.
        assert!(h.resolve(HypothesisStatus::Rejected, now).is_err());
    }

    #[test]
    fn expiry_is_tracked() {
        let now = Utc::now();
        let h = Hypothesis::new(
            super::super::HypothesisId::new(),
            WorldProposition::new("可能忙").expect("proposition"),
            WorldScope::Global,
            0.3,
            now,
            Some(now + Duration::seconds(60)),
        )
        .expect("hypothesis");
        assert!(!h.is_expired_at(now + Duration::seconds(59)));
        assert!(h.is_expired_at(now + Duration::seconds(61)));
    }

    #[test]
    fn normalization_is_case_and_space_insensitive() {
        assert_eq!(normalized_proposition("  用户 生气 "), "用户生气");
        assert_eq!(normalized_proposition("USER  busy"), "userbusy");
    }
}
