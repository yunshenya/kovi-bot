//! Persistent, platform-neutral Mind v2 domain state.
//!
//! Mind state is deliberately separate from [`crate::WorkingState`]. It is
//! durable, retrieved as a bounded snapshot, and never owns external actions.

macro_rules! mind_id {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(uuid::Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }

            #[must_use]
            pub const fn from_uuid(value: uuid::Uuid) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> &uuid::Uuid {
                &self.0
            }

            #[must_use]
            pub const fn into_uuid(self) -> uuid::Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<uuid::Uuid> for $name {
            fn from(value: uuid::Uuid) -> Self {
                Self::from_uuid(value)
            }
        }

        impl From<$name> for uuid::Uuid {
            fn from(value: $name) -> Self {
                value.into_uuid()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                uuid::Uuid::parse_str(value).map(Self::from_uuid)
            }
        }
    };
}

mod agenda;
mod belief;
mod common;
mod consolidation;
mod curiosity;
mod decision;
mod episode;
mod in_memory;
mod interest;
mod open_question;
mod ports;
mod preference;
mod reflection;
mod relevance;
mod self_model;
mod snapshot;

#[cfg(test)]
mod tests;

pub use agenda::{
    AgendaItem, AgendaItemId, AgendaItemKind, AgendaSource, AgendaStatus, AgendaSubject,
    InnerAgenda, InnerAgendaLimits,
};
pub use belief::{
    Belief, BeliefId, BeliefOperation, BeliefSource, BeliefUpdateProposal, EvidenceKind,
    EvidencePolarity, EvidenceRef,
};
pub use common::{
    MindInfluenceMode, MindReasonTag, MindScope, MindSource, MindValidationError, SCHEMA_VERSION,
};
pub use consolidation::{
    AgendaOperation, AgendaUpdateProposal, Consolidation, ConsolidationConfig, ConsolidationError,
    ConsolidationPlan, ConsolidationResult, InterestOperation, InterestUpdateProposal, MindUpsert,
    OpenQuestionOperation, OpenQuestionUpdateProposal, PreferenceOperation,
    PreferenceUpdateProposal,
};
pub use curiosity::{CuriosityId, CuriosityItem, CuriosityStatus};
pub use decision::{MindDecisionProjection, MindDecisionReference};
pub use episode::{Episode, EpisodeId};
pub use in_memory::InMemoryMindStore;
pub use interest::{Interest, InterestId};
pub use open_question::{OpenQuestion, OpenQuestionId, OpenQuestionStatus};
pub use ports::{
    AgendaStore, BeliefStore, CuriosityStore, EpisodeStore, InterestStore, MindConsolidationStore,
    MindDataErasure, MindDataErasureError, MindDataErasureFuture, MindServices, MindStoreError,
    MindStoreFuture, OpenQuestionStore, PreferenceStore, SelfModelStore,
};
pub use preference::{Preference, PreferenceId, PreferenceSource};
pub use reflection::{
    ReflectionDepth, ReflectionEvent, ReflectionInput, ReflectionProposal, ReflectionQueue,
    ReflectionQueueConfig, ReflectionTrigger,
};
pub use relevance::{MAX_LEXICAL_TERMS, lexical_relevance, lexical_terms};
pub use self_model::{SelfIdentity, SelfLimitation, SelfModel, SelfTrait, TraitName, ValueProfile};
pub use snapshot::{
    AgendaItemSnapshot, BeliefSnapshot, InterestSnapshot, MindSnapshot, MindSnapshotFuture,
    MindSnapshotLimits, MindSnapshotProvider, MindSnapshotRequest, MindSnapshotStoreProvider,
    OpenQuestionSnapshot, PreferenceSnapshot, SelfModelSnapshot, SnapshotProviderError,
};
