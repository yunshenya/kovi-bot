use crate::goal::{Goal, GoalDraft, GoalOwner};
use crate::identity::GoalId;
use crate::identity::{
    ConversationId, ConversationKind, ConversationMember, ExternalConversation, ExternalIdentity,
    OpenLoopId, PersonId,
};
use crate::mind::MindSnapshotProvider;
use crate::open_loop::{OpenLoop, OpenLoopDraft, OpenLoopOwner};
use crate::planner::{AffectState, ModelBackend, RelationState};
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

    fn set<'a>(&'a self, state: RelationState) -> RelationStoreFuture<'a, RelationState>;
}

/// Persistence boundary for slow affect state. Implementations may bridge an
/// existing host mood system while keeping the Core-facing representation
/// platform-neutral.
pub type AffectStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, AffectStoreError>> + Send + 'a>>;

pub trait AffectStore: Send + Sync {
    fn get<'a>(&'a self, person_id: PersonId) -> AffectStoreFuture<'a, AffectState>;

    fn set<'a>(
        &'a self,
        person_id: PersonId,
        state: AffectState,
    ) -> AffectStoreFuture<'a, AffectState>;
}

#[derive(Debug, Error)]
pub enum AffectStoreError {
    #[error("affect storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("affect state is invalid")]
    InvalidState,
}

impl AffectStoreError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

#[derive(Debug, Error)]
pub enum RelationStoreError {
    #[error("relation storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("relation state is invalid")]
    InvalidState,
}

impl RelationStoreError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

/// Persistence boundary for long-running Core goals.
///
/// Every method has an unavailable default so existing hosts that only need a
/// marker service can continue to implement `GoalStore` with an empty `impl`.
/// Hosts adopting the goal phase override the CRUD methods they support.
pub type GoalStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, GoalStoreError>> + Send + 'a>>;

pub trait GoalStore: Send + Sync {
    fn create<'a>(&'a self, _draft: &'a GoalDraft) -> GoalStoreFuture<'a, Goal> {
        unavailable_goal_store()
    }

    fn get(&self, _id: GoalId) -> GoalStoreFuture<'_, Option<Goal>> {
        unavailable_goal_store()
    }

    fn list<'a>(&'a self, _owner: &'a GoalOwner, _limit: usize) -> GoalStoreFuture<'a, Vec<Goal>> {
        unavailable_goal_store()
    }

    fn update<'a>(&'a self, _goal: &'a Goal) -> GoalStoreFuture<'a, Goal> {
        unavailable_goal_store()
    }

    fn delete(&self, _id: GoalId) -> GoalStoreFuture<'_, bool> {
        unavailable_goal_store()
    }
}

#[derive(Debug, Error)]
pub enum GoalStoreError {
    #[error("goal storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("goal store is unavailable")]
    Unavailable,
    #[error("goal {id} was not found")]
    NotFound { id: GoalId },
    #[error("goal owner capacity exceeded (limit {limit})")]
    CapacityExceeded { owner: GoalOwner, limit: usize },
    #[error("goal operation conflicts with another update")]
    Conflict,
    #[error("invalid goal operation: {reason}")]
    InvalidRequest { reason: String },
}

impl GoalStoreError {
    pub fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

fn unavailable_goal_store<'a, T>() -> GoalStoreFuture<'a, T> {
    Box::pin(async { Err(GoalStoreError::Unavailable) })
}

/// Shared, platform-neutral dependencies used by a cognitive runtime.
///
/// The fields intentionally mirror the architecture document: every Core
/// boundary is present in the service container and can be replaced by a
/// host adapter. [`Self::with_model`] fills the not-yet-installed stores with
/// unavailable adapters so a standalone fake runtime only needs a model.
pub struct CoreServices {
    pub memory: Arc<dyn MemoryStore>,
    pub identity: Arc<dyn IdentityStore>,
    pub conversation_members: Arc<dyn ConversationMemberStore>,
    pub open_loops: Arc<dyn OpenLoopStore>,
    pub relations: Arc<dyn RelationStore>,
    pub affect: Arc<dyn AffectStore>,
    pub goals: Arc<dyn GoalStore>,
    /// Optional durable Mind retrieval shared by every host.
    pub mind_snapshot_provider: Option<Arc<dyn MindSnapshotProvider>>,
    pub model: Arc<dyn ModelBackend>,
}

impl std::fmt::Debug for CoreServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoreServices")
            .field("memory", &true)
            .field("identity", &true)
            .field("conversation_members", &true)
            .field("open_loops", &true)
            .field("relations", &true)
            .field("affect", &true)
            .field("goals", &true)
            .field(
                "mind_snapshot_provider",
                &self.mind_snapshot_provider.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl CoreServices {
    #[must_use]
    pub fn new(model: Arc<dyn ModelBackend>) -> Self {
        Self {
            memory: Arc::new(UnavailableMemoryStore),
            identity: Arc::new(UnavailableIdentityStore),
            conversation_members: Arc::new(UnavailableConversationMemberStore),
            open_loops: Arc::new(UnavailableOpenLoopStore),
            relations: Arc::new(UnavailableRelationStore),
            affect: Arc::new(UnavailableAffectStore),
            goals: Arc::new(UnavailableGoalStore),
            mind_snapshot_provider: None,
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
    pub fn with_conversation_members(
        mut self,
        conversation_members: Arc<dyn ConversationMemberStore>,
    ) -> Self {
        self.conversation_members = conversation_members;
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
    pub fn with_affect(mut self, affect: Arc<dyn AffectStore>) -> Self {
        self.affect = affect;
        self
    }

    #[must_use]
    pub fn with_goals(mut self, goals: Arc<dyn GoalStore>) -> Self {
        self.goals = goals;
        self
    }

    #[must_use]
    pub fn with_mind_snapshot_provider(mut self, provider: Arc<dyn MindSnapshotProvider>) -> Self {
        self.mind_snapshot_provider = Some(provider);
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

pub type ConversationMemberStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ConversationMemberStoreError>> + Send + 'a>>;

/// Persistence boundary for lazily discovered conversation membership.
pub trait ConversationMemberStore: Send + Sync {
    fn upsert<'a>(
        &'a self,
        member: &'a ConversationMember,
    ) -> ConversationMemberStoreFuture<'a, ConversationMember>;

    fn get(
        &self,
        conversation_id: ConversationId,
        person_id: PersonId,
    ) -> ConversationMemberStoreFuture<'_, Option<ConversationMember>>;

    fn list(
        &self,
        conversation_id: ConversationId,
        limit: usize,
    ) -> ConversationMemberStoreFuture<'_, Vec<ConversationMember>>;

    fn remove(
        &self,
        conversation_id: ConversationId,
        person_id: PersonId,
    ) -> ConversationMemberStoreFuture<'_, bool>;
}

#[derive(Debug, Error)]
pub enum ConversationMemberStoreError {
    #[error("conversation-member storage operation failed")]
    Storage {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    #[error("conversation-member store is unavailable")]
    Unavailable,
    #[error("invalid conversation-member operation: {reason}")]
    InvalidRequest { reason: String },
}

impl ConversationMemberStoreError {
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
struct UnavailableConversationMemberStore;

impl ConversationMemberStore for UnavailableConversationMemberStore {
    fn upsert<'a>(
        &'a self,
        _member: &'a ConversationMember,
    ) -> ConversationMemberStoreFuture<'a, ConversationMember> {
        Box::pin(async { Err(ConversationMemberStoreError::Unavailable) })
    }

    fn get(
        &self,
        _conversation_id: ConversationId,
        _person_id: PersonId,
    ) -> ConversationMemberStoreFuture<'_, Option<ConversationMember>> {
        Box::pin(async { Err(ConversationMemberStoreError::Unavailable) })
    }

    fn list(
        &self,
        _conversation_id: ConversationId,
        _limit: usize,
    ) -> ConversationMemberStoreFuture<'_, Vec<ConversationMember>> {
        Box::pin(async { Err(ConversationMemberStoreError::Unavailable) })
    }

    fn remove(
        &self,
        _conversation_id: ConversationId,
        _person_id: PersonId,
    ) -> ConversationMemberStoreFuture<'_, bool> {
        Box::pin(async { Err(ConversationMemberStoreError::Unavailable) })
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

    fn set<'a>(&'a self, _state: RelationState) -> RelationStoreFuture<'a, RelationState> {
        Box::pin(async {
            Err(RelationStoreError::storage(UnavailablePortError(
                "relation store is unavailable",
            )))
        })
    }
}

#[derive(Debug)]
struct UnavailableAffectStore;

impl AffectStore for UnavailableAffectStore {
    fn get<'a>(&'a self, _person_id: PersonId) -> AffectStoreFuture<'a, AffectState> {
        Box::pin(async {
            Err(AffectStoreError::storage(UnavailablePortError(
                "affect store is unavailable",
            )))
        })
    }

    fn set<'a>(
        &'a self,
        _person_id: PersonId,
        _state: AffectState,
    ) -> AffectStoreFuture<'a, AffectState> {
        Box::pin(async {
            Err(AffectStoreError::storage(UnavailablePortError(
                "affect store is unavailable",
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
        ConversationMemberStore, ConversationMemberStoreFuture, GoalStore, GoalStoreError,
        IdentityStore, IdentityStoreError, IdentityStoreFuture, MemoryStore, MemoryStoreError,
        MemoryStoreFuture,
    };
    use crate::{
        ConversationId, ConversationKind, ConversationMember, ExternalConversation,
        ExternalIdentity, GoalDraft, GoalId, GoalKind, GoalOwner, Memory, MemoryDraft, MemoryId,
        MemoryQuery, MemoryScope, PersonId, PlatformId,
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

    struct FakeConversationMemberStore;

    impl ConversationMemberStore for FakeConversationMemberStore {
        fn upsert<'a>(
            &'a self,
            member: &'a ConversationMember,
        ) -> ConversationMemberStoreFuture<'a, ConversationMember> {
            Box::pin(async move { Ok(member.clone()) })
        }

        fn get(
            &self,
            conversation_id: ConversationId,
            person_id: PersonId,
        ) -> ConversationMemberStoreFuture<'_, Option<ConversationMember>> {
            Box::pin(async move { Ok(Some(ConversationMember::new(conversation_id, person_id))) })
        }

        fn list(
            &self,
            _conversation_id: ConversationId,
            _limit: usize,
        ) -> ConversationMemberStoreFuture<'_, Vec<ConversationMember>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn remove(
            &self,
            _conversation_id: ConversationId,
            _person_id: PersonId,
        ) -> ConversationMemberStoreFuture<'_, bool> {
            Box::pin(async { Ok(true) })
        }
    }

    #[tokio::test]
    async fn conversation_member_store_is_usable_as_a_trait_object() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();
        let member = ConversationMember::new(conversation_id, person_id);
        let store: Arc<dyn ConversationMemberStore> = Arc::new(FakeConversationMemberStore);

        assert_eq!(store.upsert(&member).await.expect("upsert member"), member);
        assert_eq!(
            store
                .get(conversation_id, person_id)
                .await
                .expect("get member"),
            Some(member)
        );
        assert!(
            store
                .list(conversation_id, 16)
                .await
                .expect("list")
                .is_empty()
        );
        assert!(
            store
                .remove(conversation_id, person_id)
                .await
                .expect("remove")
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

    struct MarkerGoalStore;

    impl GoalStore for MarkerGoalStore {}

    #[tokio::test]
    async fn goal_store_defaults_remain_usable_as_a_trait_object() {
        let store: Arc<dyn GoalStore> = Arc::new(MarkerGoalStore);
        let draft = GoalDraft::new(GoalOwner::Global, GoalKind::Project, "ship core")
            .expect("valid goal draft");
        assert!(matches!(
            store.create(&draft).await,
            Err(GoalStoreError::Unavailable)
        ));
        assert!(matches!(
            store.get(GoalId::new()).await,
            Err(GoalStoreError::Unavailable)
        ));
        assert!(matches!(
            store.list(&GoalOwner::Global, 4).await,
            Err(GoalStoreError::Unavailable)
        ));
        let goal = crate::Goal::from_draft(GoalId::new(), &draft, Utc::now()).expect("valid goal");
        assert!(matches!(
            store.update(&goal).await,
            Err(GoalStoreError::Unavailable)
        ));
        assert!(matches!(
            store.delete(goal.id()).await,
            Err(GoalStoreError::Unavailable)
        ));
    }
}
