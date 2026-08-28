//! Small persistence ports for Executive state.
//!
//! These traits intentionally stop at Core domain values. PostgreSQL, Redis,
//! migrations, and transaction policy belong to host infrastructure.

use super::{ExecutiveSnapshot, Expectation, PlanState};
use crate::{ActionId, ConversationId, GoalId, PersonId};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

pub type ExecutiveStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ExecutivePersistenceError>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutiveScope {
    Global,
    Person { person_id: PersonId },
    Conversation { conversation_id: ConversationId },
    Goal { goal_id: GoalId },
}

#[derive(Debug, Error)]
pub enum ExecutivePersistenceError {
    #[error("executive persistence is unavailable")]
    Unavailable,
    #[error("executive persistence operation failed")]
    Storage {
        #[source]
        source: Arc<dyn StdError + Send + Sync>,
    },
    #[error("executive persistence request is invalid: {reason}")]
    InvalidRequest { reason: String },
    #[error("executive state version conflicts with the stored version")]
    Conflict,
    #[error("executive record was not found")]
    NotFound,
}

impl ExecutivePersistenceError {
    #[must_use]
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Arc::new(source),
        }
    }
}

/// Composite state port for implementations that persist a complete bounded
/// snapshot. Hosts may instead implement the narrower Plan/Expectation ports.
pub trait ExecutiveStore: Send + Sync {
    fn load<'a>(
        &'a self,
        scope: &'a ExecutiveScope,
    ) -> ExecutiveStoreFuture<'a, Option<ExecutiveSnapshot>>;

    fn save<'a>(
        &'a self,
        scope: &'a ExecutiveScope,
        snapshot: &'a ExecutiveSnapshot,
    ) -> ExecutiveStoreFuture<'a, ()>;

    fn erase<'a>(&'a self, scope: &'a ExecutiveScope) -> ExecutiveStoreFuture<'a, usize>;
}

pub trait PlanStore: Send + Sync {
    fn create<'a>(&'a self, plan: &'a PlanState) -> ExecutiveStoreFuture<'a, PlanState>;
    fn get(&self, id: super::PlanId) -> ExecutiveStoreFuture<'_, Option<PlanState>>;
    fn update<'a>(&'a self, plan: &'a PlanState) -> ExecutiveStoreFuture<'a, PlanState>;
    fn delete(&self, id: super::PlanId) -> ExecutiveStoreFuture<'_, bool>;
}

pub trait ExpectationStore: Send + Sync {
    fn create<'a>(&'a self, expectation: &'a Expectation) -> ExecutiveStoreFuture<'a, Expectation>;
    fn list_for_action(
        &self,
        action_id: ActionId,
        limit: usize,
    ) -> ExecutiveStoreFuture<'_, Vec<Expectation>>;
    fn update<'a>(&'a self, expectation: &'a Expectation) -> ExecutiveStoreFuture<'a, Expectation>;
    fn delete(&self, id: super::ExpectationId) -> ExecutiveStoreFuture<'_, bool>;
}

/// Optional narrower port for hosts that store decision records separately.
pub trait DecisionRecordPersistence: Send + Sync {
    fn append<'a>(&'a self, record: &'a super::DecisionRecord) -> ExecutiveStoreFuture<'a, ()>;
    fn recent(&self, limit: usize) -> ExecutiveStoreFuture<'_, Vec<super::DecisionRecord>>;
    fn purge(&self) -> ExecutiveStoreFuture<'_, usize>;
}
