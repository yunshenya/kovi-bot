//! Structured self-consistency conflict type shared by the Executive conflict
//! model. The deterministic monitor that produced these was superseded by the
//! host-side belief/identity safeguards; this type remains as the bounded,
//! serializable conflict record the conflict snapshot carries.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsistencyKind {
    Identity,
    Value,
    HighConfidenceBelief,
    GoalCommitment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsistencySeverity {
    Informational,
    Warning,
    Severe,
    Blocking,
}

impl ConsistencySeverity {
    #[must_use]
    pub const fn score(self) -> f32 {
        match self {
            Self::Informational => 0.20,
            Self::Warning => 0.45,
            Self::Severe => 0.75,
            Self::Blocking => 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfConsistencyConflict {
    pub kind: ConsistencyKind,
    pub severity: ConsistencySeverity,
    pub score: f32,
    pub reason: String,
}
