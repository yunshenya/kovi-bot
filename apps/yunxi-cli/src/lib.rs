//! A small platform-neutral host for exercising `yunxi-core`.
//!
//! The CLI intentionally owns no persistence or platform adapter code.  Its
//! fake model turns text into a Core intent and its fake environment records
//! the admitted Core action.  That keeps the executable useful as a smoke test
//! for the Core boundary while remaining independent from Kovi and QQ.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};
use std::thread;

use yunxi_core::{
    ActionArbiter, ActionArbiterConfig, ActionCapability, ActionDescriptor, ActionPort,
    ActionPortError, ActionPortFuture, ActionPortOutcome, ActionRejection, ActionResult,
    CognitiveIntent, ConversationId, EnvironmentCapabilities, IntentValidationError,
    MessageContent, ProposedAction,
};

/// A model backend only decides what Core intent should be attempted.
pub trait ModelBackend: Send + Sync {
    fn decide(
        &self,
        conversation_id: ConversationId,
        input: &str,
    ) -> Result<CognitiveIntent, String>;
}

/// Deterministic model used by the standalone demo and acceptance tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeModel;

impl ModelBackend for FakeModel {
    fn decide(
        &self,
        conversation_id: ConversationId,
        input: &str,
    ) -> Result<CognitiveIntent, String> {
        if input.eq_ignore_ascii_case("/noop") {
            return Ok(CognitiveIntent::noop());
        }

        Ok(CognitiveIntent::send_message(
            conversation_id,
            MessageContent::text(format!("Yunxi heard: {input}")),
        ))
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
    Model(String),
    Intent(IntentValidationError),
    Rejected(ActionRejection),
    Port(ActionPortError),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Model(error) => write!(formatter, "model error: {error}"),
            Self::Intent(error) => write!(formatter, "invalid intent: {error}"),
            Self::Rejected(error) => write!(formatter, "action rejected: {error}"),
            Self::Port(error) => write!(formatter, "action port failed: {error}"),
        }
    }
}

impl std::error::Error for CliError {}

/// A minimal host that connects a model decision to Core's action boundary.
pub struct CliHost<M, E> {
    model: M,
    environment: E,
    arbiter: ActionArbiter,
    conversation_id: ConversationId,
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
    M: ModelBackend,
    E: ActionPort,
{
    #[must_use]
    pub fn new(model: M, environment: E, conversation_id: ConversationId) -> Self {
        let capabilities =
            EnvironmentCapabilities::new([ActionDescriptor::new(ActionCapability::SendMessage)]);
        let arbiter =
            ActionArbiter::new(ActionArbiterConfig::default().with_capabilities(capabilities));
        Self {
            model,
            environment,
            arbiter,
            conversation_id,
        }
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub const fn model(&self) -> &M {
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

        let intent = self
            .model
            .decide(self.conversation_id, input)
            .map_err(CliError::Model)?;
        let action = intent.propose_action().map_err(CliError::Intent)?;
        let message = action_message(&action);
        let result = block_on(self.arbiter.dispatch(action, &self.environment));
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
