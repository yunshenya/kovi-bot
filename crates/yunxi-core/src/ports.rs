use crate::identity::{
    ConversationId, ConversationKind, ExternalConversation, ExternalIdentity, OpenLoopId, PersonId,
};
use crate::open_loop::{OpenLoop, OpenLoopDraft, OpenLoopOwner};
use crate::planner::{ModelBackend, RelationState};
use crate::{Memory, MemoryDraft, MemoryId, MemoryQuery, MemoryScope};
use chrono::{DateTime, Utc};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Core persistence boundary for relation context. A host can provide this
/// store when relation persistence is available; the planner also accepts an
/// inline snapshot for read-only or lightweight hosts.
pub type RelationStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RelationStoreError>> + Send + 'a>>;

pub trait RelationStore: Send + Sync {
    fn get<'a>(&'a self, person_id: PersonId) -> RelationStoreFuture<'a, Option<RelationState>>;
}

#[derive(Debug, Error)]
pub enum RelationStoreError {
    #[error("relation storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl RelationStoreError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

/// Goal persistence is intentionally a narrow marker at this phase.  Goal
/// entities and mutation commands will be introduced by the Goal phase; the
/// service container can already carry a host implementation without making
/// the current Core depend on a platform goal schema.
pub trait GoalStore: Send + Sync {}

/// Shared, platform-neutral dependencies used by a cognitive runtime.
///
/// The fields intentionally mirror the architecture document: every Core
/// boundary is present in the service container and can be replaced by a
/// host adapter. [`Self::with_model`] fills the not-yet-installed stores with
/// unavailable adapters so a standalone fake runtime only needs a model.
pub struct CoreServices {
    pub memory: Arc<dyn MemoryStore>,
    pub identity: Arc<dyn IdentityStore>,
    pub open_loops: Arc<dyn OpenLoopStore>,
    pub relations: Arc<dyn RelationStore>,
    pub goals: Arc<dyn GoalStore>,
    pub model: Arc<dyn ModelBackend>,
}

impl std::fmt::Debug for CoreServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoreServices")
            .field("memory", &true)
            .field("identity", &true)
            .field("open_loops", &true)
            .field("relations", &true)
            .field("goals", &true)
            .finish_non_exhaustive()
    }
}

impl CoreServices {
    #[must_use]
    pub fn new(model: Arc<dyn ModelBackend>) -> Self {
        Self {
            memory: Arc::new(UnavailableMemoryStore),
            identity: Arc::new(UnavailableIdentityStore),
            open_loops: Arc::new(UnavailableOpenLoopStore),
            relations: Arc::new(UnavailableRelationStore),
            goals: Arc::new(UnavailableGoalStore),
            model,
        }
    }

    #[must_use]
    pub fn with_model<M>(model: M) -> Self
    where
        M: ModelBackend + 'static,
    {
        Self::new(Arc::new(model))
    }

    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = memory;
        self
    }

    #[must_use]
    pub fn with_identity(mut self, identity: Arc<dyn IdentityStore>) -> Self {
        self.identity = identity;
        self
    }

    #[must_use]
    pub fn with_open_loops(mut self, open_loops: Arc<dyn OpenLoopStore>) -> Self {
        self.open_loops = open_loops;
        self
    }

    #[must_use]
    pub fn with_relations(mut self, relations: Arc<dyn RelationStore>) -> Self {
        self.relations = relations;
        self
    }

    #[must_use]
    pub fn with_goals(mut self, goals: Arc<dyn GoalStore>) -> Self {
        self.goals = goals;
        self
    }
}

pub type IdentityStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, IdentityStoreError>> + Send + 'a>>;

/// Persistence boundary for resolving opaque platform references to Core IDs.
pub trait IdentityStore: Send + Sync {
    fn resolve_external_identity<'a>(
        &'a self,
        external: &'a ExternalIdentity,
    ) -> IdentityStoreFuture<'a, PersonId>;

    fn resolve_external_conversation<'a>(
        &'a self,
        external: &'a ExternalConversation,
    ) -> IdentityStoreFuture<'a, ConversationId>;
}

#[derive(Debug, Error)]
pub enum IdentityStoreError {
    #[error("identity storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error(
        "external conversation kind mismatch: requested {requested}, but stored mapping is {stored}"
    )]
    ConversationKindMismatch {
        requested: ConversationKind,
        stored: ConversationKind,
    },
}

impl IdentityStoreError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

pub type OpenLoopStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, OpenLoopStoreError>> + Send + 'a>>;

/// Persistence boundary for prospective memory. Implementations must make
/// `claim_due` an atomic claim, so two scheduler instances cannot emit the
/// same due item concurrently.
pub trait OpenLoopStore: Send + Sync {
    fn create<'a>(&'a self, draft: &'a OpenLoopDraft) -> OpenLoopStoreFuture<'a, OpenLoop>;

    fn get<'a>(&'a self, id: OpenLoopId) -> OpenLoopStoreFuture<'a, Option<OpenLoop>>;

    fn list<'a>(
        &'a self,
        owner: &'a OpenLoopOwner,
        limit: usize,
    ) -> OpenLoopStoreFuture<'a, Vec<OpenLoop>>;

    fn claim_due(&self, now: DateTime<Utc>, limit: usize)
    -> OpenLoopStoreFuture<'_, Vec<OpenLoop>>;

    fn defer(
        &self,
        id: OpenLoopId,
        due_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> OpenLoopStoreFuture<'_, OpenLoop>;

    fn resolve(&self, id: OpenLoopId, now: DateTime<Utc>) -> OpenLoopStoreFuture<'_, OpenLoop>;

    fn cancel(&self, id: OpenLoopId, now: DateTime<Utc>) -> OpenLoopStoreFuture<'_, OpenLoop>;

    /// Re-open leases left in `Triggered` after a host crash. The operation
    /// is bounded and must use the indexed triggered/lease columns.
    fn recover_stale_triggered(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> OpenLoopStoreFuture<'_, usize>;

    /// Store-specific lease duration used by `claim_due`. This is exposed so
    /// a host scheduler can choose a polling interval without duplicating the
    /// persistence policy. Core does not interpret the duration.
    fn claim_lease(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }
}

#[derive(Debug, Error)]
pub enum OpenLoopStoreError {
    #[error("open-loop storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("open loop {id} was not found")]
    NotFound { id: OpenLoopId },
    #[error("open-loop owner capacity exceeded (limit {limit})")]
    CapacityExceeded { owner: OpenLoopOwner, limit: usize },
    #[error("open-loop operation conflicts with another update")]
    Conflict,
    #[error("invalid open-loop operation: {reason}")]
    InvalidRequest { reason: String },
    #[error("invalid open-loop status transition from {from} to {to}")]
    InvalidTransition {
        from: crate::OpenLoopStatus,
        to: crate::OpenLoopStatus,
    },
}

impl OpenLoopStoreError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

pub type MemoryStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, MemoryStoreError>> + Send + 'a>>;

/// Platform-neutral memory boundary. Implementations must enforce the scope
/// in every read and delete; a query may never widen to another scope.
pub trait MemoryStore: Send + Sync {
    fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory>;

    fn recall<'a>(&'a self, query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>>;

    fn forget(&self, scope: MemoryScope, id: MemoryId) -> MemoryStoreFuture<'_, bool>;
}

#[derive(Debug, Error)]
pub enum MemoryStoreError {
    #[error("memory storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("memory scope is not available in this adapter: {scope:?}")]
    UnsupportedScope { scope: MemoryScope },
    #[error("memory operation is invalid: {reason}")]
    InvalidRequest { reason: String },
}

impl MemoryStoreError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

#[derive(Debug)]
struct UnavailablePortError(&'static str);

impl std::fmt::Display for UnavailablePortError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl StdError for UnavailablePortError {}

#[derive(Debug)]
struct UnavailableMemoryStore;

impl MemoryStore for UnavailableMemoryStore {
    fn remember<'a>(&'a self, _draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
        Box::pin(async {
            Err(MemoryStoreError::storage(UnavailablePortError(
                "memory store is unavailable",
            )))
        })
    }

    fn recall<'a>(&'a self, _query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>> {
        Box::pin(async {
            Err(MemoryStoreError::storage(UnavailablePortError(
                "memory store is unavailable",
            )))
        })
    }

    fn forget(&self, _scope: MemoryScope, _id: MemoryId) -> MemoryStoreFuture<'_, bool> {
        Box::pin(async {
            Err(MemoryStoreError::storage(UnavailablePortError(
                "memory store is unavailable",
            )))
        })
    }
}

#[derive(Debug)]
struct UnavailableIdentityStore;

impl IdentityStore for UnavailableIdentityStore {
    fn resolve_external_identity<'a>(
        &'a self,
        _external: &'a ExternalIdentity,
    ) -> IdentityStoreFuture<'a, PersonId> {
        Box::pin(async {
            Err(IdentityStoreError::storage(UnavailablePortError(
                "identity store is unavailable",
            )))
        })
    }

    fn resolve_external_conversation<'a>(
        &'a self,
        _external: &'a ExternalConversation,
    ) -> IdentityStoreFuture<'a, ConversationId> {
        Box::pin(async {
            Err(IdentityStoreError::storage(UnavailablePortError(
                "identity store is unavailable",
            )))
        })
    }
}

#[derive(Debug)]
struct UnavailableOpenLoopStore;

impl OpenLoopStore for UnavailableOpenLoopStore {
    fn create<'a>(&'a self, _draft: &'a OpenLoopDraft) -> OpenLoopStoreFuture<'a, OpenLoop> {
        Box::pin(async {
            Err(OpenLoopStoreError::storage(UnavailablePortError(
                "open-loop store is unavailable",
            )))
        })
    }

    fn get<'a>(&'a self, _id: OpenLoopId) -> OpenLoopStoreFuture<'a, Option<OpenLoop>> {
        Box::pin(async {
            Err(OpenLoopStoreError::storage(UnavailablePortError(
                "open-loop store is unavailable",
            )))
        })
    }

    fn list<'a>(
        &'a self,
        _owner: &'a OpenLoopOwner,
        _limit: usize,
    ) -> OpenLoopStoreFuture<'a, Vec<OpenLoop>> {
        Box::pin(async {
            Err(OpenLoopStoreError::storage(UnavailablePortError(
                "open-loop store is unavailable",
            )))
        })
    }

    fn claim_due(
        &self,
        _now: DateTime<Utc>,
        _limit: usize,
    ) -> OpenLoopStoreFuture<'_, Vec<OpenLoop>> {
        Box::pin(async {
            Err(OpenLoopStoreError::storage(UnavailablePortError(
                "open-loop store is unavailable",
            )))
        })
    }

    fn defer(
        &self,
        _id: OpenLoopId,
        _due_at: Option<DateTime<Utc>>,
        _now: DateTime<Utc>,
    ) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async {
            Err(OpenLoopStoreError::storage(UnavailablePortError(
                "open-loop store is unavailable",
            )))
        })
    }

    fn resolve(&self, _id: OpenLoopId, _now: DateTime<Utc>) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async {
            Err(OpenLoopStoreError::storage(UnavailablePortError(
                "open-loop store is unavailable",
            )))
        })
    }

    fn cancel(&self, _id: OpenLoopId, _now: DateTime<Utc>) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async {
            Err(OpenLoopStoreError::storage(UnavailablePortError(
                "open-loop store is unavailable",
            )))
        })
    }

    fn recover_stale_triggered(
        &self,
        _now: DateTime<Utc>,
        _limit: usize,
    ) -> OpenLoopStoreFuture<'_, usize> {
        Box::pin(async {
            Err(OpenLoopStoreError::storage(UnavailablePortError(
                "open-loop store is unavailable",
            )))
        })
    }
}

#[derive(Debug)]
struct UnavailableRelationStore;

impl RelationStore for UnavailableRelationStore {
    fn get<'a>(&'a self, _person_id: PersonId) -> RelationStoreFuture<'a, Option<RelationState>> {
        Box::pin(async {
            Err(RelationStoreError::storage(UnavailablePortError(
                "relation store is unavailable",
            )))
        })
    }
}

#[derive(Debug)]
struct UnavailableGoalStore;

impl GoalStore for UnavailableGoalStore {}

/// Time source used by domain services that need deterministic tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IdentityStore, IdentityStoreError, IdentityStoreFuture, MemoryStore, MemoryStoreError,
        MemoryStoreFuture,
    };
    use crate::{
        ConversationId, ConversationKind, ExternalConversation, ExternalIdentity, Memory,
        MemoryDraft, MemoryId, MemoryQuery, MemoryScope, PersonId, PlatformId,
    };
    use chrono::Utc;
    use std::sync::Arc;

    struct FakeIdentityStore {
        person_id: PersonId,
        conversation_id: ConversationId,
    }

    impl IdentityStore for FakeIdentityStore {
        fn resolve_external_identity<'a>(
            &'a self,
            _external: &'a ExternalIdentity,
        ) -> IdentityStoreFuture<'a, PersonId> {
            Box::pin(async move { Ok(self.person_id) })
        }

        fn resolve_external_conversation<'a>(
            &'a self,
            external: &'a ExternalConversation,
        ) -> IdentityStoreFuture<'a, ConversationId> {
            Box::pin(async move {
                if external.kind() != ConversationKind::Direct {
                    return Err(IdentityStoreError::ConversationKindMismatch {
                        requested: external.kind(),
                        stored: ConversationKind::Direct,
                    });
                }
                Ok(self.conversation_id)
            })
        }
    }

    #[tokio::test]
    async fn identity_store_is_usable_as_a_trait_object() {
        let expected_person = PersonId::new();
        let expected_conversation = ConversationId::new();
        let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
            person_id: expected_person,
            conversation_id: expected_conversation,
        });
        let platform = PlatformId::new("provider").expect("valid platform");
        let identity = ExternalIdentity::new(platform.clone(), "person").expect("valid identity");
        let conversation = ExternalConversation::new(platform, "direct", ConversationKind::Direct)
            .expect("valid conversation");

        assert_eq!(
            store
                .resolve_external_identity(&identity)
                .await
                .expect("identity should resolve"),
            expected_person
        );
        assert_eq!(
            store
                .resolve_external_conversation(&conversation)
                .await
                .expect("conversation should resolve"),
            expected_conversation
        );
    }

    struct FakeMemoryStore;

    impl MemoryStore for FakeMemoryStore {
        fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
            Box::pin(async move {
                Memory::from_draft(MemoryId::new(), draft, Utc::now()).map_err(|error| {
                    MemoryStoreError::InvalidRequest {
                        reason: error.to_string(),
                    }
                })
            })
        }

        fn recall<'a>(&'a self, query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>> {
            Box::pin(async move {
                let draft = MemoryDraft::new(
                    query.scope(),
                    crate::MemoryKind::Fact,
                    "remembered",
                    Utc::now(),
                )
                .map_err(|error| MemoryStoreError::InvalidRequest {
                    reason: error.to_string(),
                })?;
                Ok(vec![
                    Memory::from_draft(MemoryId::new(), &draft, Utc::now()).map_err(|error| {
                        MemoryStoreError::InvalidRequest {
                            reason: error.to_string(),
                        }
                    })?,
                ])
            })
        }

        fn forget(&self, _scope: MemoryScope, _id: MemoryId) -> MemoryStoreFuture<'_, bool> {
            Box::pin(async { Ok(true) })
        }
    }

    #[tokio::test]
    async fn memory_store_is_usable_as_a_trait_object() {
        let scope = MemoryScope::Person(PersonId::new());
        let store: Arc<dyn MemoryStore> = Arc::new(FakeMemoryStore);
        let draft = MemoryDraft::new(scope, crate::MemoryKind::Fact, "likes tea", Utc::now())
            .expect("valid draft");
        let memory = store.remember(&draft).await.expect("memory should persist");
        assert_eq!(memory.scope(), scope);
        let query = MemoryQuery::new(scope, "tea", 4).expect("valid query");
        assert_eq!(
            store
                .recall(&query)
                .await
                .expect("memory should recall")
                .len(),
            1
        );
        assert!(
            store
                .forget(scope, memory.id())
                .await
                .expect("memory should be forgotten")
        );
    }
}
