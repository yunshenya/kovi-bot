//! Counterfactual simulation: bounded, side-effect-free "what if" evaluation
//! (v4 §53–62, §120–122, §219).
//!
//! The hard guarantee here is *by construction*: every type in this module is
//! a pure value. The core never holds a simulation result against a live
//! model or an action port; a sim can only consume snapshots, so it can
//! never execute a real side effect (v4 §56–§58, §161).

use super::prediction::{PredictedOutcome, PredictionHorizon};
use super::snapshot::WorldModelSnapshot;
use super::{
    WorldValidationError,
    common::{dedupe, validate_unit, validate_value},
};
use crate::EventId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_SIMULATION_CANDIDATES: usize = 3;
pub const MAX_SIMULATIONS_PER_ROOT_TRACE: usize = 2;
pub const MAX_SIMULATION_CACHE_ENTRIES: usize = 64;
/// Short TTL for a simulation cache entry (v4 §122, §127).
pub const SIMULATION_CACHE_TTL_SECS: u64 = 300;

/// Infrastructure side effects only accept `Real` (v4 §57). The core
/// simulator only ever produces `Simulated` results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Simulated,
    Real,
}

/// One candidate action to evaluate (v4 §54).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationCandidate {
    id: String,
    description: String,
}

impl SimulationCandidate {
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, WorldValidationError> {
        let candidate = Self {
            id: validate_value(id, "candidate id")?,
            description: validate_value(description, "candidate description")?,
        };
        candidate.validate()?;
        Ok(candidate)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_value(self.id.clone(), "candidate id")?;
        validate_value(self.description.clone(), "candidate description")?;
        Ok(())
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// Simulation input: snapshots, never live pointers (v4 §54, §63, §67).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationInput {
    source_event_id: EventId,
    candidate: SimulationCandidate,
    horizon: PredictionHorizon,
    world: WorldModelSnapshot,
    world_version: u64,
    generated_at: DateTime<Utc>,
}

impl SimulationInput {
    pub fn new(
        source_event_id: EventId,
        candidate: SimulationCandidate,
        horizon: PredictionHorizon,
        world: WorldModelSnapshot,
        generated_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let input = Self {
            source_event_id,
            candidate,
            horizon,
            world_version: world.version(),
            world,
            generated_at,
        };
        input.validate()?;
        Ok(input)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        self.candidate.validate()?;
        self.world.validate()?;
        if self.world_version != self.world.version() {
            return Err(WorldValidationError::InvalidState {
                reason: "simulation world version mismatch",
            });
        }
        if self.generated_at < DateTime::<Utc>::MIN_UTC {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "simulation generated_at invalid",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn source_event_id(&self) -> EventId {
        self.source_event_id
    }

    #[must_use]
    pub fn candidate(&self) -> &SimulationCandidate {
        &self.candidate
    }

    #[must_use]
    pub const fn horizon(&self) -> PredictionHorizon {
        self.horizon
    }

    #[must_use]
    pub fn world(&self) -> &WorldModelSnapshot {
        &self.world
    }

    #[must_use]
    pub const fn world_version(&self) -> u64 {
        self.world_version
    }

    #[must_use]
    pub const fn generated_at(&self) -> DateTime<Utc> {
        self.generated_at
    }
}

/// Simulation result (v4 §55): bounded outcomes + uncertainty + the world
/// version it was computed against, so staleness can be detected (v4 §67).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationResult {
    candidate_id: String,
    predicted_outcomes: Vec<PredictedOutcome>,
    uncertainty: f32,
    world_version: u64,
    mode: ExecutionMode,
}

impl SimulationResult {
    pub fn new(
        candidate_id: impl Into<String>,
        predicted_outcomes: Vec<PredictedOutcome>,
        uncertainty: f32,
        world_version: u64,
        mode: ExecutionMode,
    ) -> Result<Self, WorldValidationError> {
        let result = Self {
            candidate_id: validate_value(candidate_id, "candidate id")?,
            predicted_outcomes,
            uncertainty: validate_unit(uncertainty, "simulation uncertainty")?,
            world_version,
            mode,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_value(self.candidate_id.clone(), "candidate id")?;
        if self.predicted_outcomes.is_empty() {
            return Err(WorldValidationError::InvalidState {
                reason: "simulation has no outcomes",
            });
        }
        if self.predicted_outcomes.len() > super::prediction::MAX_PREDICTED_OUTCOMES {
            return Err(WorldValidationError::TooManyItems {
                field: "simulation outcomes",
                length: self.predicted_outcomes.len(),
                maximum: super::prediction::MAX_PREDICTED_OUTCOMES,
            });
        }
        for outcome in &self.predicted_outcomes {
            outcome.validate()?;
        }
        validate_unit(self.uncertainty, "simulation uncertainty")?;
        if self.world_version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        // The core simulator never produces an infrastructure-writable mode.
        if self.mode != ExecutionMode::Simulated {
            return Err(WorldValidationError::InvalidState {
                reason: "core simulation must be Simulated",
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn candidate_id(&self) -> &str {
        &self.candidate_id
    }

    #[must_use]
    pub fn predicted_outcomes(&self) -> &[PredictedOutcome] {
        &self.predicted_outcomes
    }

    #[must_use]
    pub const fn uncertainty(&self) -> f32 {
        self.uncertainty
    }

    #[must_use]
    pub const fn world_version(&self) -> u64 {
        self.world_version
    }

    #[must_use]
    pub const fn mode(&self) -> ExecutionMode {
        self.mode
    }

    /// v4 §67: is this result stale against the given live version?
    #[must_use]
    pub fn is_stale_against(&self, current_world_version: u64) -> bool {
        self.world_version != current_world_version
    }
}

/// Per-root budgets (v4 §61, §137): total 1–3 candidates per decision, at
/// most 2 simulations per root trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationBudget {
    max_candidates: usize,
    max_per_root_trace: usize,
}

impl Default for SimulationBudget {
    fn default() -> Self {
        Self {
            max_candidates: MAX_SIMULATION_CANDIDATES,
            max_per_root_trace: MAX_SIMULATIONS_PER_ROOT_TRACE,
        }
    }
}

impl SimulationBudget {
    pub fn new(
        max_candidates: usize,
        max_per_root_trace: usize,
    ) -> Result<Self, WorldValidationError> {
        let budget = Self {
            max_candidates,
            max_per_root_trace,
        };
        budget.validate()?;
        Ok(budget)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.max_candidates == 0 || self.max_candidates > MAX_SIMULATION_CANDIDATES {
            return Err(WorldValidationError::OutOfRange {
                field: "max_candidates",
                minimum: 1.0,
                maximum: MAX_SIMULATION_CANDIDATES as f32,
            });
        }
        if self.max_per_root_trace == 0
            || self.max_per_root_trace > MAX_SIMULATIONS_PER_ROOT_TRACE
        {
            return Err(WorldValidationError::OutOfRange {
                field: "max_per_root_trace",
                minimum: 1.0,
                maximum: MAX_SIMULATIONS_PER_ROOT_TRACE as f32,
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn max_candidates(&self) -> usize {
        self.max_candidates
    }

    #[must_use]
    pub const fn max_per_root_trace(&self) -> usize {
        self.max_per_root_trace
    }
}

/// Short-lived simulation cache keyed by (candidate, world version) with a
/// 5-minute TTL (v4 §122). Bounded; cache loss is harmless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SimulationCache {
    entries: Vec<SimulationCacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationCacheEntry {
    candidate_id: String,
    world_version: u64,
    result: SimulationResult,
    created_at: DateTime<Utc>,
}

impl SimulationCache {
    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.entries.len() > MAX_SIMULATION_CACHE_ENTRIES {
            return Err(WorldValidationError::TooManyItems {
                field: "simulation cache entries",
                length: self.entries.len(),
                maximum: MAX_SIMULATION_CACHE_ENTRIES,
            });
        }
        for entry in &self.entries {
            entry.result.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a fresh cache hit; expired entries are ignored.
    #[must_use]
    pub fn get(&self, candidate_id: &str, world_version: u64, now: DateTime<Utc>) -> Option<&SimulationResult> {
        let ttl = chrono::Duration::seconds(SIMULATION_CACHE_TTL_SECS as i64);
        self.entries.iter().find(|entry| {
            entry.candidate_id == candidate_id
                && entry.world_version == world_version
                && now <= entry.created_at + ttl
        }).map(|entry| &entry.result)
    }

    /// Insert with eviction of the oldest entry at capacity.
    pub fn insert(
        &mut self,
        candidate_id: impl Into<String>,
        world_version: u64,
        result: SimulationResult,
        created_at: DateTime<Utc>,
    ) -> Result<(), WorldValidationError> {
        result.validate()?;
        let candidate_id = candidate_id.into();
        self.entries.retain(|entry| entry.candidate_id != candidate_id || entry.world_version != world_version);
        if self.entries.len() >= MAX_SIMULATION_CACHE_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(SimulationCacheEntry {
            candidate_id,
            world_version,
            result,
            created_at,
        });
        Ok(())
    }
}

/// A bounded batch of simulation results for one root decision. Keep this in
/// the domain so "how many simulations ran" is a counted fact, not a guess.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationBatch {
    root_event_id: EventId,
    results: Vec<SimulationResult>,
}

impl SimulationBatch {
    pub fn new(
        root_event_id: EventId,
        results: Vec<SimulationResult>,
    ) -> Result<Self, WorldValidationError> {
        let batch = Self {
            root_event_id,
            results,
        };
        batch.validate()?;
        Ok(batch)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.results.len() > MAX_SIMULATIONS_PER_ROOT_TRACE {
            return Err(WorldValidationError::TooManyItems {
                field: "simulations per root trace",
                length: self.results.len(),
                maximum: MAX_SIMULATIONS_PER_ROOT_TRACE,
            });
        }
        for result in &self.results {
            result.validate()?;
        }
        let ids = self.results.iter().map(|r| r.candidate_id()).collect();
        dedupe(ids, "simulation candidate ids", false)?;
        Ok(())
    }

    #[must_use]
    pub const fn root_event_id(&self) -> EventId {
        self.root_event_id
    }

    #[must_use]
    pub fn results(&self) -> &[SimulationResult] {
        &self.results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn empty_snapshot() -> WorldModelSnapshot {
        // Build a minimal valid snapshot directly from parts (empty world).
        let world = crate::world_model::WorldModel::new();
        let context = crate::world_model::WorldSnapshotContext::new(Utc::now());
        world.snapshot_for(&context).expect("snapshot")
    }

    fn sample_result(candidate: &str, version: u64) -> SimulationResult {
        SimulationResult::new(
            candidate,
            vec![PredictedOutcome::new(
                crate::world_model::OutcomeKind::Success,
                0.8,
                0.5,
                0.1,
                0.1,
                0.7,
            )
            .expect("outcome")],
            0.3,
            version,
            ExecutionMode::Simulated,
        )
        .expect("result")
    }

    #[test]
    fn simulated_mode_is_enforced_by_construction() {
        assert!(SimulationResult::new(
            "a",
            vec![PredictedOutcome::new(
                crate::world_model::OutcomeKind::Success,
                0.7,
                0.5,
                0.1,
                0.1,
                0.7,
            )
            .expect("outcome")],
            0.2,
            1,
            ExecutionMode::Real,
        )
        .is_err());
    }

    #[test]
    fn stale_simulation_is_detectable() {
        let result = sample_result("candidate_a", 20);
        assert!(!result.is_stale_against(20));
        assert!(result.is_stale_against(28));
    }

    #[test]
    fn simulation_batch_bounds_and_dedupes() {
        let event_id = EventId::new();
        let a = sample_result("a", 1);
        let b = sample_result("b", 1);
        let duplicate = sample_result("a", 1);
        // Two results with duplicate candidates are rejected.
        assert!(SimulationBatch::new(event_id, vec![a.clone(), duplicate]).is_err());
        // At most 2 per root trace.
        assert!(SimulationBatch::new(event_id, vec![a, b]).is_ok());
    }

    #[test]
    fn cache_hits_only_within_ttl_and_version() {
        let now = Utc::now();
        let mut cache = SimulationCache::default();
        cache
            .insert("a", 1, sample_result("a", 1), now)
            .expect("insert");
        assert!(cache.get("a", 1, now + Duration::minutes(4)).is_some());
        // Expired after 5 minutes.
        assert!(cache.get("a", 1, now + Duration::minutes(6)).is_none());
        // World version mismatch is a miss (v4 §67).
        assert!(cache.get("a", 2, now).is_none());
        cache.validate().expect("valid");
    }

    #[test]
    fn input_must_match_snapshot_version() {
        let now = Utc::now();
        let snapshot = empty_snapshot();
        let candidate = SimulationCandidate::new("a", "ask now").expect("candidate");
        assert!(SimulationInput::new(EventId::new(), candidate, PredictionHorizon::Immediate, snapshot, now).is_ok());
    }

    #[test]
    fn budgets_are_bounded() {
        assert!(SimulationBudget::default().validate().is_ok());
        assert!(SimulationBudget::new(4, 2).is_err());
        assert!(SimulationBudget::new(3, 0).is_err());
    }
}
