use crate::identity::{
    ConversationId, ConversationKind, ExternalConversation, ExternalIdentity, OpenLoopId, PersonId,
};
use crate::open_loop::{OpenLoop, OpenLoopDraft, OpenLoopOwner};
use crate::{Memory, MemoryDraft, MemoryId, MemoryQuery, MemoryScope};
use chrono::{DateTime, Utc};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use thiserror::Error;

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
