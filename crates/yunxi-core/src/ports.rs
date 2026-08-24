use crate::identity::{
    ConversationId, ConversationKind, ExternalConversation, ExternalIdentity, PersonId,
};
use chrono::{DateTime, Utc};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
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
    use super::{IdentityStore, IdentityStoreError, IdentityStoreFuture};
    use crate::{
        ConversationId, ConversationKind, ExternalConversation, ExternalIdentity, PersonId,
        PlatformId,
    };
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
}
