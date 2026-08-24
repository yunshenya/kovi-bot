use crate::attention::{AttentionResult, AttentionSystem};
use crate::event::{EventPriority, EventScope, EventType, EventValidationError, WorldEvent};
use crate::identity::EventId;
use crate::working_state::{
    StateUpdate, WorkingState, WorkingStateConfig, WorkingStateConfigError, WorkingStateError,
};
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

#[derive(Debug)]
pub struct CognitiveRuntime {
    receiver: mpsc::Receiver<WorldEvent>,
    state: WorkingState,
    attention: AttentionSystem,
    max_trace_depth: u8,
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
            },
        ))
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

    #[must_use]
    pub const fn state(&self) -> &WorkingState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Admission, CognitiveRuntime, ProcessingOutcome, RuntimeConfig, RuntimeConfigError,
        SubmitError,
    };
    use crate::event::{
        EventPriority, EventScope, EventValidationError, MessageContent, MessageReceivedEvent,
        WorldEvent, WorldEventKind,
    };
    use crate::identity::{ConversationId, ConversationKind, MessageId, PersonId};
    use chrono::Utc;

    fn event(priority: EventPriority) -> WorldEvent {
        WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            priority,
            WorldEventKind::IdleTick,
        )
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
