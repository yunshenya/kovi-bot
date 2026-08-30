//! Platform adapter boundary for Yunxi Core.
//!
//! A channel adapter is the only object a host needs to provide for visible
//! delivery. It combines the platform-neutral delivery resolver and action
//! port with a small capability descriptor. QQ, WeChat, a desktop client, or
//! a test host can implement the same trait without changing Core state or
//! planner code.

use crate::{
    ActionArbiter, ActionArbiterConfig, ActionPort, Admission, CognitiveRuntime, CoreServices,
    DeliveryResolver, EnvironmentCapabilities, ModelBackend, PlannedProcessingOutcome,
    PlannerError, PlatformId, RuntimeConfig, SubmitError, WorldEvent,
};
use std::sync::Arc;

/// Host implementation of one external conversation channel.
pub trait ChannelAdapter: ActionPort + DeliveryResolver + Send + Sync {
    /// Stable lower-case channel name such as `qq` or `wechat`.
    fn platform_id(&self) -> PlatformId;

    /// Actions currently available on this channel. Core uses this snapshot
    /// to expose only actions the adapter can actually execute.
    fn capabilities(&self) -> EnvironmentCapabilities;
}

/// A reusable Core host runner. It owns no platform types: adapters translate
/// normalized Core actions into concrete API calls at the ActionPort boundary.
pub struct CoreHost {
    handle: crate::RuntimeHandle,
    runtime: CognitiveRuntime,
    arbiter: ActionArbiter,
    adapter: Arc<dyn ChannelAdapter>,
}

impl std::fmt::Debug for CoreHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoreHost")
            .field("platform", &self.adapter.platform_id())
            .field("capabilities", &self.adapter.capabilities())
            .finish_non_exhaustive()
    }
}

impl CoreHost {
    /// Installs one model-backed runtime and one channel adapter. The
    /// resulting host can be embedded in a QQ, WeChat, desktop, or CLI event
    /// loop with identical Core behavior.
    pub fn new(
        config: RuntimeConfig,
        services: CoreServices,
        adapter: Arc<dyn ChannelAdapter>,
    ) -> Result<Self, crate::RuntimeConfigError> {
        let (handle, runtime) = CognitiveRuntime::new_with_services(config, services)?;
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(adapter.capabilities()),
        )
        .with_delivery_resolver(adapter.clone());
        Ok(Self {
            handle,
            runtime,
            arbiter,
            adapter,
        })
    }

    /// Submit an event through Core's bounded ingress queue.
    pub async fn submit(&self, event: WorldEvent) -> Result<Admission, SubmitError> {
        self.handle.submit(event).await
    }

    /// Process one already-normalized event, including planning, arbitration,
    /// delivery, and derived feedback events.
    pub async fn process_event(
        &mut self,
        event: WorldEvent,
    ) -> Result<PlannedProcessingOutcome, PlannerError> {
        self.runtime
            .process_event_with_planner_and_actions(event, &self.arbiter, self.adapter.as_ref())
            .await
    }

    /// Drain the next queued event. Returns `None` after the ingress channel
    /// is closed and all pending tool follow-ups have been consumed.
    pub async fn process_next(&mut self) -> Option<Result<PlannedProcessingOutcome, PlannerError>> {
        self.runtime
            .process_next_with_planner_and_actions(&self.arbiter, self.adapter.as_ref())
            .await
    }

    #[must_use]
    pub fn platform_id(&self) -> PlatformId {
        self.adapter.platform_id()
    }

    #[must_use]
    pub fn capabilities(&self) -> EnvironmentCapabilities {
        self.adapter.capabilities()
    }

    #[must_use]
    pub fn adapter(&self) -> &Arc<dyn ChannelAdapter> {
        &self.adapter
    }

    #[must_use]
    pub fn runtime(&self) -> &CognitiveRuntime {
        &self.runtime
    }
}

/// Convenience constructor for callers that already hold an `Arc` model and
/// want Core to install the standard service container.
pub fn services_with_model(model: Arc<dyn ModelBackend>) -> CoreServices {
    CoreServices::new(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActionPortError, ActionPortFuture, ActionPortOutcome, DeliveryResolutionError,
        DeliveryResolverFuture, MessageContent, PersonId, ProposedAction,
    };
    #[derive(Debug)]
    struct FakeAdapter;

    impl ActionPort for FakeAdapter {
        fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a> {
            let conversation_id = match action {
                ProposedAction::SendMessage(message) => Some(message.conversation_id),
                _ => None,
            };
            Box::pin(async move {
                Ok(ActionPortOutcome::Delivered {
                    external_reference: Some("fake".to_owned()),
                    message_id: Some(crate::MessageId::new()),
                    conversation_id,
                })
            })
        }
    }

    impl DeliveryResolver for FakeAdapter {
        fn resolve<'a>(&'a self, _person_id: PersonId) -> DeliveryResolverFuture<'a> {
            Box::pin(async {
                Err(DeliveryResolutionError::failed(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "fake has no reach-out route",
                )))
            })
        }
    }

    impl ChannelAdapter for FakeAdapter {
        fn platform_id(&self) -> PlatformId {
            PlatformId::new("fake").expect("valid platform")
        }

        fn capabilities(&self) -> EnvironmentCapabilities {
            EnvironmentCapabilities::empty().with_action(crate::ActionDescriptor::new(
                crate::ActionCapability::SendMessage,
            ))
        }
    }

    #[test]
    fn adapter_boundary_exposes_platform_without_host_types() {
        let adapter = FakeAdapter;
        assert_eq!(adapter.platform_id().as_str(), "fake");
        assert!(adapter.capabilities().supports(
            crate::ActionCapability::SendMessage,
            crate::ActionScope::Global
        ));
    }

    #[test]
    fn platform_id_rejects_host_specific_uppercase_names() {
        assert!(PlatformId::new("QQ").is_err());
    }

    #[allow(dead_code)]
    fn _send_message_is_a_core_value() {
        let _ = MessageContent::text("hello");
        let _ = ActionPortError::new("unused", false);
    }
}
