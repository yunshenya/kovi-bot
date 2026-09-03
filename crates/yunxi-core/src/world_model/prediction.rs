//! Prediction: bounded, calibrated "what might happen if we do X" estimates
//! (v4 §46–52, §186).
//!
//! Predictions are short-lived, carry an uncertainty bound (a prediction can
//! never be more certain than the state it was derived from, v4 §82), and
//! never fake precision (v4 §224): probabilities are quantized to 0.05 steps
//! and kept inside the band they claim (Low/Medium/High).

use super::{
    WorldScope, WorldValidationError,
    common::{clamp_unit, validate_unit},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_PREDICTED_OUTCOMES: usize = 4;
pub const MAX_RUNTIME_PREDICTIONS: usize = 64;
pub const MAX_RUNTIME_PREDICTION_ERRORS: usize = 64;
/// Exact-step quantization: no 73.42% (v4 §224).
pub const PROBABILITY_STEP: f32 = 0.05;

/// Coarse probability band (v4 §49). Never claim more precision than this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbabilityBand {
    Low,
    Medium,
    High,
}

impl ProbabilityBand {
    /// Representative point of the band. Only these three values are ever
    /// reported as a prediction's nominal probability.
    #[must_use]
    pub const fn representative(self) -> f32 {
        match self {
            Self::Low => 0.30,
            Self::Medium => 0.60,
            Self::High => 0.85,
        }
    }

    #[must_use]
    pub fn from_probability(probability: f32) -> Self {
        if probability < 0.35 {
            Self::Low
        } else if probability < 0.70 {
            Self::Medium
        } else {
            Self::High
        }
    }

    fn contains(self, probability: f32) -> bool {
        match self {
            Self::Low => probability <= 0.34,
            Self::Medium => (0.35..=0.69).contains(&probability),
            Self::High => probability >= 0.70,
        }
    }
}

/// Quantize to 0.05 steps (no fake precision).
#[must_use]
pub fn quantize_probability(probability: f32) -> f32 {
    if !probability.is_finite() {
        return 0.0;
    }
    ((probability.clamp(0.0, 1.0) / PROBABILITY_STEP).round() * PROBABILITY_STEP).min(1.0)
}

/// Prediction horizon: shallow only (v4 §138).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionHorizon {
    Immediate,
    Short,
    TaskStep,
}

/// Semantic outcome class. One prediction lists 1..=4 of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Success,
    Failure,
    Neutral,
    Unknown,
}

/// One predicted outcome (v4 §48).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PredictedOutcome {
    description: OutcomeKind,
    band: ProbabilityBand,
    probability: f32,
    utility: f32,
    social_cost: f32,
    risk: f32,
    goal_progress: f32,
}

impl PredictedOutcome {
    pub fn new(
        description: OutcomeKind,
        probability: f32,
        utility: f32,
        social_cost: f32,
        risk: f32,
        goal_progress: f32,
    ) -> Result<Self, WorldValidationError> {
        let probability = quantize_probability(probability);
        let outcome = Self {
            description,
            band: ProbabilityBand::from_probability(probability),
            probability,
            utility: validate_unit((utility + 1.0) / 2.0, "predicted utility")?,
            social_cost: validate_unit(social_cost, "predicted social cost")?,
            risk: validate_unit(risk, "predicted risk")?,
            goal_progress: validate_unit((goal_progress + 1.0) / 2.0, "predicted goal progress")?,
        };
        outcome.validate()?;
        Ok(outcome)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if !self.band.contains(self.probability) {
            return Err(WorldValidationError::OutOfRange {
                field: "predicted probability band",
                minimum: 0.0,
                maximum: 1.0,
            });
        }
        if (quantize_probability(self.probability) - self.probability).abs() > f32::EPSILON {
            return Err(WorldValidationError::InvalidState {
                reason: "predicted probability not quantized",
            });
        }
        validate_unit(self.utility, "predicted utility")?;
        validate_unit(self.social_cost, "predicted social cost")?;
        validate_unit(self.risk, "predicted risk")?;
        validate_unit(self.goal_progress, "predicted goal progress")?;
        Ok(())
    }

    /// Nominal probability is the band's representative, never the raw input.
    #[must_use]
    pub fn nominal_probability(&self) -> f32 {
        self.band.representative()
    }

    #[must_use]
    pub const fn description(&self) -> OutcomeKind {
        self.description
    }

    #[must_use]
    pub const fn band(&self) -> ProbabilityBand {
        self.band
    }

    #[must_use]
    pub const fn probability(&self) -> f32 {
        self.probability
    }

    #[must_use]
    pub fn utility(&self) -> f32 {
        self.utility * 2.0 - 1.0
    }

    #[must_use]
    pub const fn social_cost(&self) -> f32 {
        self.social_cost
    }

    #[must_use]
    pub const fn risk(&self) -> f32 {
        self.risk
    }

    #[must_use]
    pub fn goal_progress(&self) -> f32 {
        self.goal_progress * 2.0 - 1.0
    }
}

/// A bounded prediction before a candidate action (v4 §47).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    id: super::PredictionId,
    source_candidate: String,
    scope: WorldScope,
    horizon: PredictionHorizon,
    possible_outcomes: Vec<PredictedOutcome>,
    confidence: f32,
    uncertainty_bound: f32,
    generated_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    version: u64,
}

impl Prediction {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: super::PredictionId,
        source_candidate: impl Into<String>,
        scope: WorldScope,
        horizon: PredictionHorizon,
        possible_outcomes: Vec<PredictedOutcome>,
        confidence: f32,
        uncertainty_bound: f32,
        generated_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Self, WorldValidationError> {
        let prediction = Self {
            id,
            source_candidate: super::common::validate_value(source_candidate, "candidate id")?,
            scope,
            horizon,
            possible_outcomes,
            confidence: clamp_unit(confidence),
            uncertainty_bound: clamp_unit(uncertainty_bound),
            generated_at,
            expires_at,
            version: 1,
        };
        prediction.validate()?;
        Ok(prediction)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.possible_outcomes.is_empty() {
            return Err(WorldValidationError::InvalidState {
                reason: "prediction has no outcomes",
            });
        }
        if self.possible_outcomes.len() > MAX_PREDICTED_OUTCOMES {
            return Err(WorldValidationError::TooManyItems {
                field: "predicted outcomes",
                length: self.possible_outcomes.len(),
                maximum: MAX_PREDICTED_OUTCOMES,
            });
        }
        let probability_sum: f32 = self
            .possible_outcomes
            .iter()
            .map(|outcome| outcome.probability())
            .sum();
        if probability_sum > 1.05 {
            return Err(WorldValidationError::InvalidState {
                reason: "outcome probabilities exceed 1",
            });
        }
        for outcome in &self.possible_outcomes {
            outcome.validate()?;
        }
        // v4 §82: a prediction may never be more certain than its inputs.
        if self.confidence > self.uncertainty_bound + 0.01 {
            return Err(WorldValidationError::InvalidState {
                reason: "prediction confidence exceeds its uncertainty bound",
            });
        }
        validate_unit(self.confidence, "prediction confidence")?;
        validate_unit(self.uncertainty_bound, "prediction uncertainty bound")?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        if let Some(expires_at) = self.expires_at
            && expires_at < self.generated_at
        {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "prediction expires before generation",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::PredictionId {
        self.id
    }

    #[must_use]
    pub fn source_candidate(&self) -> &str {
        &self.source_candidate
    }

    #[must_use]
    pub const fn scope(&self) -> WorldScope {
        self.scope
    }

    #[must_use]
    pub const fn horizon(&self) -> PredictionHorizon {
        self.horizon
    }

    #[must_use]
    pub fn possible_outcomes(&self) -> &[PredictedOutcome] {
        &self.possible_outcomes
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn uncertainty_bound(&self) -> f32 {
        self.uncertainty_bound
    }

    #[must_use]
    pub const fn generated_at(&self) -> DateTime<Utc> {
        self.generated_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn freshness_at(&self, now: DateTime<Utc>) -> super::Freshness {
        super::temporal::freshness_at(self.generated_at, self.expires_at, now)
    }
}

/// Prediction vs observed result: a calibration signal, never a judgement
/// about the agent (v4 §52, §202).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PredictionError {
    prediction_id: super::PredictionId,
    expected: OutcomeKind,
    observed: OutcomeKind,
    confidence_at_generation: f32,
    observed_at: DateTime<Utc>,
}

impl PredictionError {
    pub fn new(
        prediction_id: super::PredictionId,
        expected: OutcomeKind,
        observed: OutcomeKind,
        confidence_at_generation: f32,
        observed_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let error = Self {
            prediction_id,
            expected,
            observed,
            confidence_at_generation: validate_unit(confidence_at_generation, "confidence")?,
            observed_at,
        };
        error.validate()?;
        Ok(error)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.confidence_at_generation, "confidence")?;
        Ok(())
    }

    #[must_use]
    pub const fn prediction_id(&self) -> super::PredictionId {
        self.prediction_id
    }

    #[must_use]
    pub const fn expected(&self) -> OutcomeKind {
        self.expected
    }

    #[must_use]
    pub const fn observed(&self) -> OutcomeKind {
        self.observed
    }

    #[must_use]
    pub const fn confidence_at_generation(&self) -> f32 {
        self.confidence_at_generation
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    #[must_use]
    pub fn is_correct(&self) -> bool {
        self.expected == self.observed
    }
}

/// Small rolling calibration tracker (v4 §123).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct PredictionCalibration {
    total: u32,
    correct: u32,
}

impl PredictionCalibration {
    pub fn observe(&mut self, correct: bool) {
        self.total = self.total.saturating_add(1);
        if correct {
            self.correct = self.correct.saturating_add(1);
        }
    }

    #[must_use]
    pub fn total(&self) -> u32 {
        self.total
    }

    #[must_use]
    pub fn correct(&self) -> u32 {
        self.correct
    }

    #[must_use]
    pub fn accuracy(&self) -> Option<f32> {
        (self.total > 0).then(|| self.correct as f32 / self.total as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probability_is_quantized_to_steps() {
        assert_eq!(quantize_probability(0.734219), 0.75);
        assert_eq!(quantize_probability(0.4), 0.4);
        assert_eq!(quantize_probability(f32::NAN), 0.0);
        assert_eq!(quantize_probability(1.7), 1.0);
    }

    #[test]
    fn outcome_bands_are_coarse_not_precise() {
        let outcome =
            PredictedOutcome::new(OutcomeKind::Success, 0.734, 0.8, 0.1, 0.1, 0.9).expect("ok");
        assert_eq!(outcome.band(), ProbabilityBand::High);
        assert_eq!(outcome.probability(), 0.75);
        // Nominal probability = band representative (0.85), not the raw 0.75.
        assert_eq!(outcome.nominal_probability(), 0.85);
        assert!(outcome.band().contains(outcome.probability()));
    }

    #[test]
    fn prediction_cannot_exceed_uncertainty_bound() {
        let now = Utc::now();
        let outcome = PredictedOutcome::new(OutcomeKind::Failure, 0.4, -0.3, 0.0, 0.0, -0.2)
            .expect("outcome");
        assert!(
            Prediction::new(
                super::super::PredictionId::new(),
                "reach_out:a",
                WorldScope::Global,
                PredictionHorizon::Immediate,
                vec![outcome],
                0.95,
                0.4,
                now,
                None,
            )
            .is_err()
        );
        assert!(
            Prediction::new(
                super::super::PredictionId::new(),
                "reach_out:a",
                WorldScope::Global,
                PredictionHorizon::Immediate,
                vec![outcome],
                0.4,
                0.8,
                now,
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn outcome_probability_sum_is_bounded() {
        let now = Utc::now();
        let success =
            PredictedOutcome::new(OutcomeKind::Success, 0.8, 0.8, 0.1, 0.1, 0.9).expect("success");
        let failure = PredictedOutcome::new(OutcomeKind::Failure, 0.8, -0.8, 0.1, 0.1, -0.9)
            .expect("failure");
        assert!(
            Prediction::new(
                super::super::PredictionId::new(),
                "x",
                WorldScope::Global,
                PredictionHorizon::Immediate,
                vec![success, failure],
                0.4,
                0.8,
                now,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn calibration_tracks_accuracy() {
        let mut calibration = PredictionCalibration::default();
        assert_eq!(calibration.accuracy(), None);
        calibration.observe(true);
        calibration.observe(true);
        calibration.observe(false);
        assert_eq!(calibration.total(), 3);
        assert_eq!(calibration.accuracy(), Some(2.0 / 3.0));
    }

    #[test]
    fn prediction_error_records_calibration_signal() {
        let now = Utc::now();
        let error = PredictionError::new(
            super::super::PredictionId::new(),
            OutcomeKind::Success,
            OutcomeKind::Failure,
            0.85,
            now,
        )
        .expect("error");
        assert!(!error.is_correct());
        assert_eq!(error.confidence_at_generation(), 0.85);
    }
}
