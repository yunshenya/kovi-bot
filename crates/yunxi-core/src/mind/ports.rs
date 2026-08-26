use super::{
    AgendaItem, AgendaItemId, Belief, BeliefId, ConsolidationPlan, ConsolidationResult,
    CuriosityId, CuriosityItem, Episode, Interest, InterestId, MindScope, OpenQuestion,
    OpenQuestionId, Preference, PreferenceId, SelfModel,
};
use crate::{ConversationId, PersonId};
use chrono::{DateTime, Utc};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

pub type MindStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, MindStoreError>> + Send + 'a>>;
pub type MindDataErasureFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), MindDataErasureError>> + Send + 'a>>;

#[derive(Debug, Error)]
pub enum MindStoreError {
    #[error("mind storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("mind record is invalid: {0}")]
    Validation(#[from] super::MindValidationError),
    #[error("mind record {kind}:{id} was not found")]
    NotFound { kind: &'static str, id: String },
    #[error("mind record {kind}:{id} has version {actual}, expected {expected}")]
    VersionConflict {
        kind: &'static str,
        id: String,
        expected: u64,
        actual: u64,
    },
    #[error("mind request exceeds a bounded limit: {reason}")]
    InvalidRequest { reason: &'static str },
    #[error("mind persistence is unavailable")]
    Unavailable,
}

impl MindStoreError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

#[derive(Debug, Error)]
pub enum MindDataErasureError {
    #[error("mind data erasure failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("mind data erasure is unavailable")]
    Unavailable,
}

impl MindDataErasureError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

pub trait SelfModelStore: Send + Sync {
    fn get(&self) -> MindStoreFuture<'_, Option<SelfModel>>;

    fn put<'a>(
        &'a self,
        model: &'a SelfModel,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, SelfModel>;
}

pub trait BeliefStore: Send + Sync {
    fn get(&self, id: BeliefId) -> MindStoreFuture<'_, Option<Belief>>;

    fn find_by_key<'a>(
        &'a self,
        scope: MindScope,
        proposition_key: &'a str,
    ) -> MindStoreFuture<'a, Option<Belief>>;

    fn put<'a>(
        &'a self,
        belief: &'a Belief,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Belief>;

    fn relevant<'a>(
        &'a self,
        scopes: &'a [MindScope],
        query: &'a str,
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Belief>>;
}

pub trait PreferenceStore: Send + Sync {
    fn get(&self, id: PreferenceId) -> MindStoreFuture<'_, Option<Preference>>;

    fn find_by_key<'a>(&'a self, subject_key: &'a str) -> MindStoreFuture<'a, Option<Preference>>;

    fn put<'a>(
        &'a self,
        preference: &'a Preference,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Preference>;

    fn relevant<'a>(&'a self, query: &'a str, limit: usize)
    -> MindStoreFuture<'a, Vec<Preference>>;
}

pub trait InterestStore: Send + Sync {
    fn get(&self, id: InterestId) -> MindStoreFuture<'_, Option<Interest>>;

    fn find_by_key<'a>(&'a self, topic_key: &'a str) -> MindStoreFuture<'a, Option<Interest>>;

    fn put<'a>(
        &'a self,
        interest: &'a Interest,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, Interest>;

    fn relevant<'a>(&'a self, query: &'a str, limit: usize) -> MindStoreFuture<'a, Vec<Interest>>;
}

pub trait CuriosityStore: Send + Sync {
    fn get(&self, id: CuriosityId) -> MindStoreFuture<'_, Option<CuriosityItem>>;

    fn find_open_by_key<'a>(
        &'a self,
        scope: MindScope,
        question_key: &'a str,
    ) -> MindStoreFuture<'a, Option<CuriosityItem>>;

    fn put<'a>(
        &'a self,
        curiosity: &'a CuriosityItem,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, CuriosityItem>;

    fn list_open<'a>(
        &'a self,
        scopes: &'a [MindScope],
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<CuriosityItem>>;
}

pub trait OpenQuestionStore: Send + Sync {
    fn get(&self, id: OpenQuestionId) -> MindStoreFuture<'_, Option<OpenQuestion>>;

    fn find_open_by_key<'a>(
        &'a self,
        scope: MindScope,
        question_key: &'a str,
    ) -> MindStoreFuture<'a, Option<OpenQuestion>>;

    fn put<'a>(
        &'a self,
        question: &'a OpenQuestion,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, OpenQuestion>;

    fn list_open<'a>(
        &'a self,
        scopes: &'a [MindScope],
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<OpenQuestion>>;
}

pub trait AgendaStore: Send + Sync {
    fn get(&self, id: AgendaItemId) -> MindStoreFuture<'_, Option<AgendaItem>>;

    fn find_active_by_key<'a>(
        &'a self,
        scope: MindScope,
        subject_key: &'a str,
    ) -> MindStoreFuture<'a, Option<AgendaItem>>;

    fn put<'a>(
        &'a self,
        item: &'a AgendaItem,
        expected_version: Option<u64>,
    ) -> MindStoreFuture<'a, AgendaItem>;

    fn list_active<'a>(
        &'a self,
        scopes: &'a [MindScope],
        now: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<AgendaItem>>;
}

pub trait EpisodeStore: Send + Sync {
    fn put<'a>(&'a self, episode: &'a Episode) -> MindStoreFuture<'a, Episode>;

    fn list_recent<'a>(
        &'a self,
        scopes: &'a [MindScope],
        since: DateTime<Utc>,
        limit: usize,
    ) -> MindStoreFuture<'a, Vec<Episode>>;
}

/// Atomic boundary used after a model proposal has become a validated plan.
pub trait MindConsolidationStore: Send + Sync {
    fn apply<'a>(&'a self, plan: &'a ConsolidationPlan)
    -> MindStoreFuture<'a, ConsolidationResult>;

    fn current_version(&self) -> MindStoreFuture<'_, u64>;
}

pub trait MindDataErasure: Send + Sync {
    fn erase_person(&self, person_id: PersonId) -> MindDataErasureFuture<'_>;

    fn erase_conversation(&self, conversation_id: ConversationId) -> MindDataErasureFuture<'_>;
}

/// Companion service bundle. V1 [`crate::CoreServices`] remains unchanged.
#[derive(Clone)]
pub struct MindServices {
    pub self_model: Arc<dyn SelfModelStore>,
    pub beliefs: Arc<dyn BeliefStore>,
    pub preferences: Arc<dyn PreferenceStore>,
    pub interests: Arc<dyn InterestStore>,
    pub curiosities: Arc<dyn CuriosityStore>,
    pub open_questions: Arc<dyn OpenQuestionStore>,
    pub agenda: Arc<dyn AgendaStore>,
    pub episodes: Arc<dyn EpisodeStore>,
    pub consolidation: Arc<dyn MindConsolidationStore>,
    pub data_erasure: Arc<dyn MindDataErasure>,
}

impl std::fmt::Debug for MindServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MindServices")
            .finish_non_exhaustive()
    }
}

impl MindServices {
    #[must_use]
    pub fn from_store<T>(store: Arc<T>) -> Self
    where
        T: SelfModelStore
            + BeliefStore
            + PreferenceStore
            + InterestStore
            + CuriosityStore
            + OpenQuestionStore
            + AgendaStore
            + EpisodeStore
            + MindConsolidationStore
            + MindDataErasure
            + 'static,
    {
        Self {
            self_model: Arc::clone(&store) as Arc<dyn SelfModelStore>,
            beliefs: Arc::clone(&store) as Arc<dyn BeliefStore>,
            preferences: Arc::clone(&store) as Arc<dyn PreferenceStore>,
            interests: Arc::clone(&store) as Arc<dyn InterestStore>,
            curiosities: Arc::clone(&store) as Arc<dyn CuriosityStore>,
            open_questions: Arc::clone(&store) as Arc<dyn OpenQuestionStore>,
            agenda: Arc::clone(&store) as Arc<dyn AgendaStore>,
            episodes: Arc::clone(&store) as Arc<dyn EpisodeStore>,
            consolidation: Arc::clone(&store) as Arc<dyn MindConsolidationStore>,
            data_erasure: store as Arc<dyn MindDataErasure>,
        }
    }
}
