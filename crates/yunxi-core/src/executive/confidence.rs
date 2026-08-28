//! Bounded confidence calibration with explicit evidence provenance.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    VeryUncertain,
    Weak,
    Tentative,
    Likely,
    Strong,
    HighlyVerified,
}

#[must_use]
pub fn confidence_level(value: f32) -> ConfidenceLevel {
    if !value.is_finite() {
        return ConfidenceLevel::VeryUncertain;
    }
    match value.clamp(0.0, 1.0) {
        value if value < 0.20 => ConfidenceLevel::VeryUncertain,
        value if value < 0.40 => ConfidenceLevel::Weak,
        value if value < 0.60 => ConfidenceLevel::Tentative,
        value if value < 0.80 => ConfidenceLevel::Likely,
        value if value < 0.95 => ConfidenceLevel::Strong,
        _ => ConfidenceLevel::HighlyVerified,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EvidenceWeight {
    pub reliability: f32,
    pub relevance: f32,
    pub freshness: f32,
    pub directness: f32,
}

impl Default for EvidenceWeight {
    fn default() -> Self {
        Self {
            reliability: 0.5,
            relevance: 0.5,
            freshness: 0.5,
            directness: 0.5,
        }
    }
}

impl EvidenceWeight {
    pub fn validate(self) -> Result<(), &'static str> {
        for value in [
            self.reliability,
            self.relevance,
            self.freshness,
            self.directness,
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err("evidence weight must be within 0..=1");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn strength(self) -> f32 {
        (self.reliability * self.relevance * self.freshness * self.directness).sqrt()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    ToolResult,
    ExplicitUserCorrection,
    RepeatedDirectStatement,
    SingleDirectStatement,
    InferredImplication,
    ModelHypothesis,
}

impl EvidenceSource {
    #[must_use]
    pub const fn reliability(self) -> f32 {
        match self {
            Self::ToolResult => 1.0,
            Self::ExplicitUserCorrection => 0.90,
            Self::RepeatedDirectStatement => 0.75,
            Self::SingleDirectStatement => 0.60,
            Self::InferredImplication => 0.30,
            Self::ModelHypothesis => 0.15,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidencePolarity {
    Supports,
    Contradicts,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceUpdate {
    pub old: f32,
    pub new: f32,
    pub delta: f32,
    pub max_delta: f32,
    pub source: EvidenceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceCalibration {
    pub max_normal_delta: f32,
    pub max_reliable_delta: f32,
    pub stability_floor: f32,
}

impl Default for ConfidenceCalibration {
    fn default() -> Self {
        Self {
            max_normal_delta: 0.20,
            max_reliable_delta: 0.40,
            stability_floor: 0.05,
        }
    }
}

impl ConfidenceCalibration {
    pub fn validate(self) -> Result<(), &'static str> {
        for value in [
            self.max_normal_delta,
            self.max_reliable_delta,
            self.stability_floor,
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err("confidence calibration values must be within 0..=1");
            }
        }
        if self.max_reliable_delta < self.max_normal_delta {
            return Err("reliable confidence delta must not be smaller than normal delta");
        }
        Ok(())
    }

    #[must_use]
    pub fn update(
        self,
        old: f32,
        evidence_target: f32,
        source: EvidenceSource,
        weight: EvidenceWeight,
        polarity: EvidencePolarity,
        stability: f32,
    ) -> ConfidenceUpdate {
        let old = clamp(old);
        let target = match polarity {
            EvidencePolarity::Supports => clamp(evidence_target),
            EvidencePolarity::Contradicts => 1.0 - clamp(evidence_target),
        };
        let stability = clamp(stability).max(self.stability_floor);
        let source_factor = source.reliability();
        let magnitude = (weight.strength() * source_factor * stability).clamp(0.0, 1.0);
        let max_delta = if matches!(
            source,
            EvidenceSource::ToolResult | EvidenceSource::ExplicitUserCorrection
        ) {
            self.max_reliable_delta
        } else {
            self.max_normal_delta
        };
        let toward = (target - old) * magnitude;
        let delta = toward.clamp(-max_delta, max_delta);
        let new = clamp(old + delta);
        ConfidenceUpdate {
            old,
            new,
            delta: new - old,
            max_delta,
            source,
        }
    }
}

#[must_use]
pub fn update_confidence(
    old: f32,
    evidence_target: f32,
    source: EvidenceSource,
    weight: EvidenceWeight,
    polarity: EvidencePolarity,
    max_delta: f32,
) -> f32 {
    let calibration = ConfidenceCalibration {
        max_normal_delta: max_delta.clamp(0.0, 1.0),
        max_reliable_delta: max_delta.clamp(0.0, 1.0),
        ..ConfidenceCalibration::default()
    };
    calibration
        .update(old, evidence_target, source, weight, polarity, 1.0)
        .new
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisState {
    Hypothesis,
    Belief,
}

#[must_use]
pub const fn hypothesis_state(confidence: f32) -> HypothesisState {
    if confidence.is_nan() || confidence.is_infinite() || confidence < 0.60 {
        HypothesisState::Hypothesis
    } else {
        HypothesisState::Belief
    }
}

fn clamp(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_confidence_is_fail_safe() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(confidence_level(value), ConfidenceLevel::VeryUncertain);
            assert_eq!(hypothesis_state(value), HypothesisState::Hypothesis);
        }
    }

    #[test]
    fn finite_confidence_keeps_the_existing_thresholds() {
        assert_eq!(confidence_level(-1.0), ConfidenceLevel::VeryUncertain);
        assert_eq!(confidence_level(0.60), ConfidenceLevel::Likely);
        assert_eq!(confidence_level(1.0), ConfidenceLevel::HighlyVerified);
        assert_eq!(hypothesis_state(0.59), HypothesisState::Hypothesis);
        assert_eq!(hypothesis_state(0.60), HypothesisState::Belief);
    }
}
