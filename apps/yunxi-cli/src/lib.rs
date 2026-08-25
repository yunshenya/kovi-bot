//! A small platform-neutral host for exercising `yunxi-core`.
//!
//! The CLI intentionally owns no persistence or platform adapter code.  Its
//! fake model turns text into a Core intent and its fake environment records
//! the admitted Core action.  That keeps the executable useful as a smoke test
//! for the Core boundary while remaining independent from Kovi and QQ.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;

use yunxi_core::{
    ActionArbiter, ActionArbiterConfig, ActionCapability, ActionDescriptor, ActionPort,
    ActionPortError, ActionPortFuture, ActionPortOutcome, ActionRejection, ActionResult,
    CognitiveRuntime, ConversationId, CoreServices, DecisionDisposition, DecisionPlan,
    EnvironmentCapabilities, MessageContent, ModelBackend as CoreModelBackend, PersonId,
    PlannedProcessingOutcome, PlannerError, PlannerInput, ProposedAction, RuntimeConfig,
    WorldEvent, WorldEventKind,
};

/// Deterministic model used by the standalone demo and acceptance tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeModel;

impl CoreModelBackend for FakeModel {
    fn plan<'a>(&'a self, input: &'a PlannerInput) -> yunxi_core::ModelBackendFuture<'a> {
        Box::pin(async move {
            let Some(WorldEventKind::MessageReceived(message)) = Some(input.event.kind()) else {
                return Ok(DecisionPlan::silent());
            };
            if message.content.as_text().eq_ignore_ascii_case("/noop") {
                return Ok(DecisionPlan::silent());
            }
            Ok(DecisionPlan {
                disposition: DecisionDisposition::Reply,
                intents: vec![yunxi_core::CognitiveIntent::send_message(
                    message.conversation_id,
                    MessageContent::text(format!("Yunxi heard: {}", message.content.as_text())),
                )],
                state_updates: Vec::new(),
            })
        })
    }
}

/// An in-memory environment that records every action delivered by Core.
#[derive(Debug, Default)]
pub struct FakeEnvironment {
    deliveries: Mutex<Vec<ProposedAction>>,
}

impl FakeEnvironment {
    #[must_use]
    pub fn deliveries(&self) -> Vec<ProposedAction> {
        self.deliveries
            .lock()
            .expect("fake environment lock poisoned")
            .clone()
    }
}

impl ActionPort for FakeEnvironment {
    fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a> {
        let action = action.clone();
        Box::pin(async move {
            let mut deliveries = self
                .deliveries
                .lock()
                .expect("fake environment lock poisoned");
            let sequence = deliveries.len() + 1;
            deliveries.push(action);
            Ok(ActionPortOutcome::Delivered {
                external_reference: Some(format!("fake-delivery-{sequence}")),
            })
        })
    }
}

/// The outcome shown by the CLI host after Core arbitration and delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostResponse {
    Empty,
    Noop,
    Delivered {
        message: String,
        external_reference: Option<String>,
    },
    Deferred {
        message: String,
        reason: String,
    },
}

#[derive(Debug)]
pub enum CliError {
    Planner(PlannerError),
    Rejected(ActionRejection),
    Port(ActionPortError),
    Runtime(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planner(error) => write!(formatter, "planner error: {error}"),
            Self::Rejected(error) => write!(formatter, "action rejected: {error}"),
            Self::Port(error) => write!(formatter, "action port failed: {error}"),
            Self::Runtime(error) => write!(formatter, "runtime error: {error}"),
        }
    }
}

impl std::error::Error for CliError {}

/// A minimal host that connects a model decision to Core's action boundary.
pub struct CliHost<M, E> {
    model: Arc<M>,
    environment: E,
    arbiter: ActionArbiter,
    conversation_id: ConversationId,
    runtime: Mutex<CognitiveRuntime>,
}

impl<M, E> fmt::Debug for CliHost<M, E>
where
    M: fmt::Debug,
    E: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliHost")
            .field("model", &self.model)
            .field("environment", &self.environment)
            .field("conversation_id", &self.conversation_id)
            .finish_non_exhaustive()
    }
}

impl<M, E> CliHost<M, E>
where
    M: CoreModelBackend + 'static,
    E: ActionPort,
{
    #[must_use]
    pub fn new(model: M, environment: E, conversation_id: ConversationId) -> Self {
        let capabilities =
            EnvironmentCapabilities::new([ActionDescriptor::new(ActionCapability::SendMessage)]);
        let model = Arc::new(model);
        let (_, runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::new(Arc::clone(&model) as Arc<dyn CoreModelBackend>),
        )
        .expect("default CLI runtime configuration must be valid");
        let arbiter =
            ActionArbiter::new(ActionArbiterConfig::default().with_capabilities(capabilities));
        Self {
            model,
            environment,
            arbiter,
            conversation_id,
            runtime: Mutex::new(runtime),
        }
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub fn model(&self) -> &M {
        &self.model
    }

    #[must_use]
    pub const fn environment(&self) -> &E {
        &self.environment
    }

    /// Processes one line of user input through model, intent, arbiter, and
    /// environment boundaries.
    pub fn process_line(&self, input: &str) -> Result<HostResponse, CliError> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(HostResponse::Empty);
        }

        let event = WorldEvent::message_received(
            yunxi_core::EventPriority::High,
            yunxi_core::MessageReceivedEvent {
                message_id: yunxi_core::MessageId::new(),
                conversation_id: self.conversation_id,
                sender: PersonId::new(),
                content: MessageContent::text(input),
                reply_to: None,
                timestamp: chrono::Utc::now(),
                conversation_kind: yunxi_core::ConversationKind::Direct,
                addressed_to_agent: true,
                replies_to_agent: false,
                stop_requested: false,
                explicit_request: true,
            },
        );
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| CliError::Runtime("runtime lock poisoned".to_owned()))?;
        let outcome = block_on(runtime.process_event_with_planner_and_actions(
            event,
            &self.arbiter,
            &self.environment,
        ))
        .map_err(CliError::Planner)?;
        let PlannedProcessingOutcome::Planned { plan, actions, .. } = outcome else {
            return Err(CliError::Runtime(
                "runtime rejected the CLI event".to_owned(),
            ));
        };
        let message = plan.intents.first().and_then(intent_message);
        let result = actions.into_iter().next().unwrap_or(ActionResult::Noop);
        match result {
            ActionResult::Noop => Ok(HostResponse::Noop),
            ActionResult::Executed { outcome, .. } => match outcome {
                ActionPortOutcome::Delivered { external_reference } => {
                    Ok(HostResponse::Delivered {
                        message: message.unwrap_or_default(),
                        external_reference,
                    })
                }
                ActionPortOutcome::Deferred { reason } => Ok(HostResponse::Deferred {
                    message: message.unwrap_or_default(),
                    reason,
                }),
            },
            ActionResult::Rejected(error) => Err(CliError::Rejected(error)),
            ActionResult::Failed { error, .. } => Err(CliError::Port(error)),
        }
    }
}

fn intent_message(intent: &yunxi_core::CognitiveIntent) -> Option<String> {
    let action = intent.propose_action().ok()?;
    action_message(&action)
}

fn action_message(action: &ProposedAction) -> Option<String> {
    match action {
        ProposedAction::SendMessage(action) => Some(action.content.as_text().to_owned()),
        ProposedAction::ReachOut(action) => Some(action.message.as_text().to_owned()),
        ProposedAction::Noop => None,
    }
}

// `yunxi-core` deliberately exposes async ports while this tiny host keeps
// its dependency surface to only `yunxi-core`.  The fake port is immediately
// ready, so a compact single-threaded executor is sufficient here.
fn block_on<F>(future: F) -> F::Output
where
    F: Future,
{
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match Pin::new(&mut future).poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::yield_now(),
        }
    }
}
