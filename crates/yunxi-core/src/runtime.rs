use crate::arbiter::{ActionArbiter, ActionPort, ActionResult};
use crate::attention::{AttentionResult, AttentionSystem};
use crate::event::{EventPriority, EventScope, EventType, EventValidationError, WorldEvent};
use crate::identity::EventId;
use crate::planner::{
    DecisionPlan, Planner, PlannerError, PlannerInput, PlannerOutputValidationError,
    PlannerStateSnapshot,
};
use crate::ports::CoreServices;
use crate::working_state::{
    StateUpdate, WorkingState, WorkingStateConfig, WorkingStateConfigError, WorkingStateError,
};
use chrono::Utc;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;

const MAX_EVENT_QUEUE_CAPACITY: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub event_queue_capacity: usize,
    pub max_trace_depth: u8,
    pub working_state: WorkingStateConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            event_queue_capacity: 256,
            max_trace_depth: 8,
            working_state: WorkingStateConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RuntimeConfigError {
    #[error("event queue capacity must be greater than zero")]
    ZeroQueueCapacity,
    #[error(
        "event queue capacity {value} is above maximum {maximum}",
        maximum = MAX_EVENT_QUEUE_CAPACITY
    )]
    QueueCapacityTooLarge { value: usize },
    #[error(transparent)]
    WorkingState(#[from] WorkingStateConfigError),
}

#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    sender: mpsc::Sender<WorldEvent>,
    max_trace_depth: u8,
}

impl RuntimeHandle {
    pub async fn submit(&self, event: WorldEvent) -> Result<Admission, SubmitError> {
        if let Err(error) = event.validate(self.max_trace_depth) {
            return Err(SubmitError::InvalidEvent { event, error });
        }
        if event.priority().requires_backpressure() {
            self.sender
                .send(event)
                .await
                .map(|()| Admission::Accepted)
                .map_err(|error| SubmitError::RuntimeClosed(error.0))
        } else {
            match self.sender.try_send(event) {
                Ok(()) => Ok(Admission::Accepted),
                Err(mpsc::error::TrySendError::Full(_)) => Ok(Admission::DroppedAtCapacity),
                Err(mpsc::error::TrySendError::Closed(event)) => {
                    Err(SubmitError::RuntimeClosed(event))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    DroppedAtCapacity,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SubmitError {
    #[error("cognitive runtime rejected an invalid event: {error}")]
    InvalidEvent {
        event: WorldEvent,
        error: EventValidationError,
    },
    #[error("cognitive runtime is closed")]
    RuntimeClosed(WorldEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeObservation {
    pub event_id: EventId,
    pub event_type: EventType,
    pub scope: EventScope,
    pub priority: EventPriority,
    pub attention: AttentionResult,
    pub state: StateUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingOutcome {
    Observed(RuntimeObservation),
    RejectedEvent {
        event: WorldEvent,
        error: EventValidationError,
    },
    RejectedState {
        event: WorldEvent,
        error: WorkingStateError,
    },
}

/// Result of a runtime turn that also invoked the optional Core Planner.
/// Planning remains declarative: the returned plan still needs intent
/// conversion and ActionArbiter admission before a host side effect occurs.
#[derive(Debug)]
pub enum PlannedProcessingOutcome {
    Planned {
        observation: RuntimeObservation,
        plan: DecisionPlan,
        actions: Vec<ActionResult>,
        feedback: Vec<RuntimeObservation>,
    },
    RejectedEvent {
        event: WorldEvent,
        error: EventValidationError,
    },
    RejectedState {
        event: WorldEvent,
        error: WorkingStateError,
    },
}

#[derive(Debug)]
pub struct CognitiveRuntime {
    receiver: mpsc::Receiver<WorldEvent>,
    state: WorkingState,
    attention: AttentionSystem,
    max_trace_depth: u8,
    planner: Option<Planner>,
    services: Option<Arc<CoreServices>>,
}

impl CognitiveRuntime {
    pub fn new(config: RuntimeConfig) -> Result<(RuntimeHandle, Self), RuntimeConfigError> {
        if config.event_queue_capacity == 0 {
            return Err(RuntimeConfigError::ZeroQueueCapacity);
        }
        if config.event_queue_capacity > MAX_EVENT_QUEUE_CAPACITY {
            return Err(RuntimeConfigError::QueueCapacityTooLarge {
                value: config.event_queue_capacity,
            });
        }
        let state = WorkingState::new(config.working_state)?;
        let (sender, receiver) = mpsc::channel(config.event_queue_capacity);
        Ok((
            RuntimeHandle {
                sender,
                max_trace_depth: config.max_trace_depth,
            },
            Self {
                receiver,
                state,
                attention: AttentionSystem,
                max_trace_depth: config.max_trace_depth,
                planner: None,
                services: None,
            },
        ))
    }

    /// Creates a runtime with the model and optional Core service ports.  The
    /// original [`Self::new`] constructor remains observe-only for hosts that
    /// have not installed a model backend yet.
    pub fn new_with_services(
        config: RuntimeConfig,
        services: CoreServices,
    ) -> Result<(RuntimeHandle, Self), RuntimeConfigError> {
        let (handle, mut runtime) = Self::new(config)?;
        runtime.install_services(services);
        Ok((handle, runtime))
    }

    /// Installs a planner without changing the bounded event queue or state.
    #[must_use]
    pub fn with_planner(mut self, planner: Planner) -> Self {
        self.planner = Some(planner);
        self
    }

    /// Installs Core services and uses their model backend for planning.
    pub fn install_services(&mut self, services: CoreServices) {
        let planner =
            Planner::new(services.model.clone()).with_max_trace_depth(self.max_trace_depth);
        self.planner = Some(planner);
        self.services = Some(Arc::new(services));
    }

    #[must_use]
    pub fn planner(&self) -> Option<&Planner> {
        self.planner.as_ref()
    }

    #[must_use]
    pub fn services(&self) -> Option<&CoreServices> {
        self.services.as_deref()
    }

    pub async fn process_next(&mut self) -> Option<ProcessingOutcome> {
        let event = self.receiver.recv().await?;
        Some(self.process_event(event))
    }

    pub fn process_event(&mut self, event: WorldEvent) -> ProcessingOutcome {
        if let Err(error) = event.validate(self.max_trace_depth) {
            return ProcessingOutcome::RejectedEvent { event, error };
        }

        let attention = self.attention.evaluate(&event);
        let state = match self.state.observe(&event, attention) {
            Ok(state) => state,
            Err(error) => {
                return ProcessingOutcome::RejectedState { event, error };
            }
        };
        ProcessingOutcome::Observed(RuntimeObservation {
            event_id: event.id(),
            event_type: event.kind().event_type(),
            scope: event.scope(),
            priority: event.priority(),
            attention,
            state,
        })
    }

    /// Processes one event and invokes the installed planner after the event
    /// has been admitted to attention and working state.
    pub async fn process_event_with_planner(
        &mut self,
        event: WorldEvent,
    ) -> Result<PlannedProcessingOutcome, PlannerError> {
        let planner_event = event.clone();
        let observed = self.process_event(event);
        let ProcessingOutcome::Observed(observation) = observed else {
            return Ok(match observed {
                ProcessingOutcome::RejectedEvent { event, error } => {
                    PlannedProcessingOutcome::RejectedEvent { event, error }
                }
                ProcessingOutcome::RejectedState { event, error } => {
                    PlannedProcessingOutcome::RejectedState { event, error }
                }
                ProcessingOutcome::Observed(_) => unreachable!("observed outcome matched above"),
            });
        };
        let planner = self
            .planner
            .as_ref()
            .ok_or(PlannerError::Model(crate::ModelBackendError::Unavailable))?;
        let input = self
            .planner_input(planner_event)
            .with_capabilities(Vec::new());
        let plan = planner.plan(&input).await?;
        Ok(PlannedProcessingOutcome::Planned {
            observation,
            plan,
            actions: Vec::new(),
            feedback: Vec::new(),
        })
    }

    /// Runs one complete Core decision turn. Every intent is converted to a
    /// proposed action, admitted by the arbiter, executed by the host port,
    /// and represented as a derived WorldEvent that is observed by this
    /// runtime. The method deliberately does not call the planner again for
    /// feedback events, preventing action-result loops.
    pub async fn process_event_with_planner_and_actions(
        &mut self,
        event: WorldEvent,
        arbiter: &ActionArbiter,
        port: &dyn ActionPort,
    ) -> Result<PlannedProcessingOutcome, PlannerError> {
        let planner_event = event.clone();
        let observed = self.process_event(event);
        let observation = match observed {
            ProcessingOutcome::Observed(observation) => observation,
            ProcessingOutcome::RejectedEvent { event, error } => {
                return Ok(PlannedProcessingOutcome::RejectedEvent { event, error });
            }
            ProcessingOutcome::RejectedState { event, error } => {
                return Ok(PlannedProcessingOutcome::RejectedState { event, error });
            }
        };
        let planner = self
            .planner
            .as_ref()
            .ok_or(PlannerError::Model(crate::ModelBackendError::Unavailable))?;
        let capabilities = arbiter.config().capabilities.actions().to_vec();
        let input = self
            .planner_input(planner_event.clone())
            .with_capabilities(capabilities);
        let plan = planner.plan(&input).await?;
        let mut actions = Vec::with_capacity(plan.intents.len());
        let mut feedback = Vec::new();
        for intent in &plan.intents {
            let proposed = intent.propose_action().map_err(|error| {
                PlannerError::InvalidOutput(PlannerOutputValidationError::InvalidIntent(error))
            })?;
            let result = arbiter.dispatch(proposed.clone(), port).await;
            if let Some(feedback_event) =
                action_result_event(&planner_event, &proposed, &result, self.max_trace_depth)
            {
                if let ProcessingOutcome::Observed(feedback_observation) =
                    self.process_event(feedback_event)
                {
                    feedback.push(feedback_observation);
                }
            }
            actions.push(result);
        }
        Ok(PlannedProcessingOutcome::Planned {
            observation,
            plan,
            actions,
            feedback,
        })
    }

    /// Queue variant of [`Self::process_event_with_planner`].
    pub async fn process_next_with_planner(
        &mut self,
    ) -> Option<Result<PlannedProcessingOutcome, PlannerError>> {
        let event = self.receiver.recv().await?;
        Some(self.process_event_with_planner(event).await)
    }

    /// Builds a planner context from the current runtime state.  Hosts that
    /// have retrieved durable memories/open loops can extend the returned
    /// input before calling [`Planner::plan`].
    pub fn planner_input(&self, event: WorldEvent) -> PlannerInput {
        let conversation = event
            .scope()
            .conversation_id()
            .and_then(|conversation_id| self.state.conversation(conversation_id));
        PlannerInput::new(
            event,
            PlannerStateSnapshot::new(self.state.global_version(), conversation),
        )
    }

    #[must_use]
    pub const fn state(&self) -> &WorkingState {
        &self.state
    }
}

fn action_result_event(
    parent: &WorldEvent,
    action: &crate::ProposedAction,
    result: &ActionResult,
    max_trace_depth: u8,
) -> Option<WorldEvent> {
    let idempotency_key = action.idempotency_key()?.to_owned();
    let scope = match action.scope() {
        crate::ActionScope::Conversation(conversation_id) => {
            EventScope::Conversation { conversation_id }
        }
        crate::ActionScope::Person(person_id) => EventScope::Person { person_id },
        crate::ActionScope::Global => EventScope::Global,
    };
    let kind = match result {
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::Delivered { .. },
            ..
        } => {
            crate::WorldEventKind::ActionSucceeded(crate::ActionSucceededEvent { idempotency_key })
        }
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::Deferred { reason },
            ..
        } => crate::WorldEventKind::ActionFailed(crate::ActionFailedEvent {
            idempotency_key,
            error_category: format!("deferred:{reason}"),
        }),
        ActionResult::Failed { error, .. } => {
            crate::WorldEventKind::ActionFailed(crate::ActionFailedEvent {
                idempotency_key,
                error_category: error.category.clone(),
            })
        }
        ActionResult::Rejected(rejection) => {
            crate::WorldEventKind::ActionRejected(crate::ActionRejectedEvent {
                idempotency_key,
                reason: rejection.to_string(),
            })
        }
        ActionResult::Noop => return None,
    };
    WorldEvent::derived_from(
        parent,
        Utc::now(),
        scope,
        EventPriority::High,
        kind,
        max_trace_depth,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Admission, CognitiveRuntime, PlannedProcessingOutcome, ProcessingOutcome, RuntimeConfig,
        RuntimeConfigError, SubmitError,
    };
    use crate::arbiter::{
        ActionArbiter, ActionArbiterConfig, ActionPort, ActionPortFuture, ActionPortOutcome,
        EnvironmentCapabilities,
    };
    use crate::event::{
        EventPriority, EventScope, EventValidationError, MessageContent, MessageReceivedEvent,
        WorldEvent, WorldEventKind,
    };
    use crate::identity::{ConversationId, ConversationKind, MessageId, PersonId};
    use crate::planner::{
        DecisionDisposition, DecisionPlan, ModelBackend, ModelBackendFuture, PlannerInput,
    };
    use crate::ports::CoreServices;
    use chrono::Utc;

    fn event(priority: EventPriority) -> WorldEvent {
        WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            priority,
            WorldEventKind::IdleTick,
        )
    }

    struct FakeModel;

    impl ModelBackend for FakeModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            Box::pin(async {
                Ok(DecisionPlan {
                    disposition: DecisionDisposition::Silent,
                    intents: vec![crate::CognitiveIntent::noop()],
                    state_updates: Vec::new(),
                })
            })
        }
    }

    struct ActionModel {
        conversation_id: ConversationId,
    }

    impl ModelBackend for ActionModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            Box::pin(async move {
                Ok(DecisionPlan {
                    disposition: DecisionDisposition::Reply,
                    intents: vec![crate::CognitiveIntent::send_message(
                        conversation_id,
                        MessageContent::text("planned reply"),
                    )],
                    state_updates: Vec::new(),
                })
            })
        }
    }

    struct FakeActionPort;

    impl ActionPort for FakeActionPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            Box::pin(async {
                Ok(ActionPortOutcome::Delivered {
                    external_reference: Some("fake".to_owned()),
                })
            })
        }
    }

    #[tokio::test]
    async fn runtime_can_run_a_fake_model_after_observing_an_event() {
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(FakeModel),
        )
        .expect("valid runtime");
        let output = runtime
            .process_event_with_planner(event(EventPriority::Normal))
            .await
            .expect("fake model should plan");
        assert!(matches!(
            output,
            PlannedProcessingOutcome::Planned {
                plan: DecisionPlan {
                    disposition: DecisionDisposition::Silent,
                    ..
                },
                ..
            }
        ));
        assert_eq!(runtime.state().global_version(), 1);
    }

    #[tokio::test]
    async fn runtime_dispatches_planned_actions_and_observes_feedback_events() {
        let conversation_id = ConversationId::new();
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(ActionModel { conversation_id }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let event = WorldEvent::message_received(
            EventPriority::High,
            MessageReceivedEvent {
                message_id: MessageId::new(),
                conversation_id,
                sender: PersonId::new(),
                content: MessageContent::text("hello"),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind: ConversationKind::Direct,
                addressed_to_agent: true,
                replies_to_agent: false,
                stop_requested: false,
                explicit_request: true,
            },
        );
        let output = runtime
            .process_event_with_planner_and_actions(event, &arbiter, &FakeActionPort)
            .await
            .expect("planner and action dispatch should succeed");
        assert!(matches!(
            output,
            PlannedProcessingOutcome::Planned {
                actions,
                feedback,
                ..
            } if actions.len() == 1 && feedback.len() == 1
        ));
        assert_eq!(runtime.state().global_version(), 2);
    }

    #[tokio::test]
    async fn low_priority_events_drop_when_the_bounded_queue_is_full() {
        let (handle, mut runtime) = CognitiveRuntime::new(RuntimeConfig {
            event_queue_capacity: 1,
            ..RuntimeConfig::default()
        })
        .expect("valid runtime");

        assert_eq!(
            handle.submit(event(EventPriority::Low)).await,
            Ok(Admission::Accepted)
        );
        assert_eq!(
            handle.submit(event(EventPriority::Low)).await,
            Ok(Admission::DroppedAtCapacity)
        );
        assert!(matches!(
            runtime.process_next().await,
            Some(ProcessingOutcome::Observed(_))
        ));
    }

    #[tokio::test]
    async fn critical_events_wait_for_capacity_instead_of_dropping() {
        let (handle, mut runtime) = CognitiveRuntime::new(RuntimeConfig {
            event_queue_capacity: 1,
            ..RuntimeConfig::default()
        })
        .expect("valid runtime");
        handle
            .submit(event(EventPriority::Low))
            .await
            .expect("first event should fit");

        let submitter = tokio::spawn({
            let handle = handle.clone();
            async move { handle.submit(event(EventPriority::Critical)).await }
        });
        tokio::task::yield_now().await;
        assert!(!submitter.is_finished());

        runtime.process_next().await.expect("first event");
        assert_eq!(
            submitter.await.expect("submission task should join"),
            Ok(Admission::Accepted)
        );
        runtime.process_next().await.expect("critical event");
        assert_eq!(runtime.state().global_version(), 2);
    }

    #[tokio::test]
    async fn admission_rejects_events_beyond_the_runtime_trace_limit() {
        let (handle, runtime) = CognitiveRuntime::new(RuntimeConfig {
            max_trace_depth: 1,
            ..RuntimeConfig::default()
        })
        .expect("valid runtime");
        let root = event(EventPriority::Normal);
        let child = WorldEvent::derived_from(
            &root,
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::IdleTick,
            2,
        )
        .expect("first child");
        let grandchild = WorldEvent::derived_from(
            &child,
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::IdleTick,
            2,
        )
        .expect("second child");
        let event_id = grandchild.id();

        assert!(matches!(
            handle.submit(grandchild).await,
            Err(SubmitError::InvalidEvent {
                event,
                error: EventValidationError::TraceDepthExceeded {
                    depth: 2,
                    maximum: 1,
                },
            }) if event.id() == event_id
        ));
        assert_eq!(runtime.state().global_version(), 0);
    }

    #[tokio::test]
    async fn closed_runtime_returns_the_reliable_event_to_the_host() {
        let (handle, runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        drop(runtime);
        let reliable = event(EventPriority::Critical);
        let event_id = reliable.id();

        assert!(matches!(
            handle.submit(reliable).await,
            Err(SubmitError::RuntimeClosed(event)) if event.id() == event_id
        ));
    }

    #[tokio::test]
    async fn accepted_reliable_event_is_returned_when_state_rejects_it() {
        let (handle, mut runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        let conversation_id = ConversationId::new();
        let message = |conversation_kind| {
            WorldEvent::message_received(
                EventPriority::Critical,
                MessageReceivedEvent {
                    message_id: MessageId::new(),
                    conversation_id,
                    sender: PersonId::new(),
                    content: MessageContent::text("hello"),
                    reply_to: None,
                    timestamp: Utc::now(),
                    conversation_kind,
                    addressed_to_agent: false,
                    replies_to_agent: false,
                    stop_requested: false,
                    explicit_request: false,
                },
            )
        };
        handle
            .submit(message(ConversationKind::Group))
            .await
            .expect("first event should be accepted");
        assert!(matches!(
            runtime.process_next().await,
            Some(ProcessingOutcome::Observed(_))
        ));
        let conflicting = message(ConversationKind::Direct);
        let event_id = conflicting.id();
        handle
            .submit(conflicting)
            .await
            .expect("validated event should be admitted");

        assert!(matches!(
            runtime.process_next().await,
            Some(ProcessingOutcome::RejectedState {
                event,
                error: crate::working_state::WorkingStateError::ConversationKindMismatch,
            }) if event.id() == event_id
        ));
    }

    #[test]
    fn runtime_rejects_zero_and_excessive_queue_capacities_without_panicking() {
        assert_eq!(
            CognitiveRuntime::new(RuntimeConfig {
                event_queue_capacity: 0,
                ..RuntimeConfig::default()
            })
            .expect_err("zero capacity must fail"),
            RuntimeConfigError::ZeroQueueCapacity
        );
        assert_eq!(
            CognitiveRuntime::new(RuntimeConfig {
                event_queue_capacity: 4_097,
                ..RuntimeConfig::default()
            })
            .expect_err("excessive capacity must fail"),
            RuntimeConfigError::QueueCapacityTooLarge { value: 4_097 }
        );
    }
}
