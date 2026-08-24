//! Delivery resolution boundary for high-level reach-outs.

use crate::{ConversationId, ConversationKind, PersonId};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

pub type DeliveryResolverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<DeliveryRoute, DeliveryResolutionError>> + Send + 'a>>;

/// A platform-neutral route selected by a host adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryRoute {
    pub conversation_id: ConversationId,
    pub conversation_kind: ConversationKind,
}

impl DeliveryRoute {
    #[must_use]
    pub const fn new(conversation_id: ConversationId, conversation_kind: ConversationKind) -> Self {
        Self {
            conversation_id,
            conversation_kind,
        }
    }
}

/// Resolves a Core person to a currently reachable Core conversation.
///
/// The implementation may consult platform identity mappings, but the trait
/// itself exposes no QQ/OneBot or other host-specific types.
pub trait DeliveryResolver: Send + Sync {
    fn resolve<'a>(&'a self, person_id: PersonId) -> DeliveryResolverFuture<'a>;
}

#[derive(Debug, Error)]
pub enum DeliveryResolutionError {
    #[error("no delivery route is currently available for person {person_id}")]
    Unavailable { person_id: PersonId },
    #[error("delivery resolution failed")]
    Failed {
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl DeliveryResolutionError {
    pub fn failed(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Failed {
            source: Box::new(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct FakeResolver {
        route: DeliveryRoute,
    }

    impl DeliveryResolver for FakeResolver {
        fn resolve<'a>(&'a self, _person_id: PersonId) -> DeliveryResolverFuture<'a> {
            Box::pin(async move { Ok(self.route) })
        }
    }

    #[tokio::test]
    async fn resolver_is_object_safe() {
        let route = DeliveryRoute::new(ConversationId::new(), ConversationKind::Direct);
        let resolver: Arc<dyn DeliveryResolver> = Arc::new(FakeResolver { route });
        assert_eq!(
            resolver.resolve(PersonId::new()).await.expect("route"),
            route
        );
    }
}
