//! A small platform-neutral host for exercising `yunxi-core`.
//!
//! The fake model turns text into Core intents and its fake environment
//! records admitted message actions. Optional host-owned JSON state supplies
//! the durable Core ports without adding filesystem concerns to `yunxi-core`.
//! The executable therefore remains independent from Kovi and QQ.

mod journal;
mod state;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
pub use journal::{CliJournal, JournalError, JournalRecord, MAX_JOURNAL_INPUT_BYTES};
use serde::{Deserialize, Serialize};
pub use state::{
    CliCoreState, CliStateError, CliStateStats, MAX_CLI_MEMORIES, MAX_CLI_MEMORIES_PER_SCOPE,
    MAX_CLI_OPEN_LOOPS, MAX_CLI_OPEN_LOOPS_PER_OWNER, MAX_CLI_PEOPLE, MAX_CLI_STATE_BYTES,
};
use yunxi_core::{
    ActionArbiter, ActionArbiterConfig, ActionCapability, ActionDescriptor, ActionPort,
    ActionPortError, ActionPortFuture, ActionPortOutcome, ActionRejection, ActionResult,
    AutonomousConversationTickEvent, AutonomyPolicy, CognitiveRuntime, ConversationId,
    ConversationKind, ConversationLifecycle, ConversationLifecycleError, ConversationTurnDirective,
    CoreServices, DecisionDisposition, DecisionPlan, EnvironmentCapabilities, EventPriority,
    EventScope, MemoryStore, MessageContent, ModelBackend as CoreModelBackend, OpenLoopDraft,
    OpenLoopKind, OpenLoopOwner, OpenLoopStore, PersonId, PlannedProcessingOutcome, PlannerError,
    PlannerInput, ProposedAction, RuntimeConfig, StateUpdateProposal, WorldEvent, WorldEventKind,
};

/// Input marker used for autonomous turns in the optional CLI journal.
pub const AUTONOMOUS_TICK_INPUT: &str = "[autonomous tick]";
const AUTONOMOUS_RETRY_BACKOFF: ChronoDuration = ChronoDuration::seconds(1);

/// Deterministic model used by the standalone demo and acceptance tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeModel;

impl CoreModelBackend for FakeModel {
    fn plan<'a>(&'a self, input: &'a PlannerInput) -> yunxi_core::ModelBackendFuture<'a> {
        Box::pin(async move {
            if let WorldEventKind::AutonomousConversationTick(_) = input.event.kind() {
                let Some(conversation_id) = input.event.scope().conversation_id() else {
                    return Ok(DecisionPlan::silent());
                };
                let conversation = input.state.conversation.as_ref();
                let tick_count = conversation
                    .map(|conversation| {
                        conversation
                            .recent_events
                            .iter()
                            .filter(|event| {
                                event.event_type
                                    == yunxi_core::EventType::AutonomousConversationTick
                            })
                            .count()
                    })
                    .unwrap_or_default();
                let previous_user_message = conversation.and_then(|conversation| {
                    conversation
                        .recent_events
                        .iter()
                        .rev()
                        .find(|event| event.event_type == yunxi_core::EventType::MessageReceived)
                        .and_then(|event| event.text.clone())
                });
                let (text, directive) = if tick_count <= 1 {
                    let text = previous_user_message.map_or_else(
                        || "I had another thought while we were quiet.".to_owned(),
                        |previous| format!("I kept thinking about that: {previous}"),
                    );
                    (text, ConversationTurnDirective::Continue)
                } else {
                    (
                        "That is enough of a thought for now; I will pause here.".to_owned(),
                        ConversationTurnDirective::End,
                    )
                };
                return Ok(DecisionPlan {
                    disposition: DecisionDisposition::Reply,
                    intents: vec![yunxi_core::CognitiveIntent::send_message(
                        conversation_id,
                        MessageContent::text(text),
                    )],
                    state_updates: vec![StateUpdateProposal::ConversationDirective {
                        conversation_id,
                        directive,
                    }],
                });
            }
            let Some(WorldEventKind::MessageReceived(message)) = Some(input.event.kind()) else {
                return Ok(DecisionPlan::silent());
            };
            if message.content.as_text().eq_ignore_ascii_case("/noop") {
                return Ok(DecisionPlan::silent());
            }

            let prior_memories = input
                .memories
                .iter()
                .filter(|memory| memory.occurred_at() < message.timestamp)
                .count();
            let relation_before = input.relation;
            let mut relation =
                relation_before.unwrap_or_else(|| yunxi_core::RelationState::new(message.sender));
            relation.familiarity = (relation.familiarity + 0.05).clamp(-1.0, 1.0);
            relation.comfort = (relation.comfort + 0.02).clamp(-1.0, 1.0);
            let mut affect = input.affect;
            affect.arousal = (affect.arousal + 0.01).clamp(-1.0, 1.0);
            affect.social_energy = (affect.social_energy - 0.01).clamp(0.0, 1.0);
            affect.curiosity = (affect.curiosity + 0.01).clamp(0.0, 1.0);
            let state_updates = vec![
                StateUpdateProposal::Affect(affect),
                StateUpdateProposal::Relation(relation),
            ];

            let context = if prior_memories == 0
                && input.open_loops.is_empty()
                && relation_before.is_none()
            {
                String::new()
            } else {
                format!(
                    " [context: {prior_memories} memories, {} open loops, familiarity {:.2}, energy {:.2}]",
                    input.open_loops.len(),
                    relation_before.map_or(0.0, |state| state.familiarity),
                    input.affect.social_energy,
                )
            };

            let text = message.content.as_text();
            if let Some(summary) = text.strip_prefix("/todo ").map(str::trim)
                && !summary.is_empty()
            {
                let draft = OpenLoopDraft::new(
                    OpenLoopOwner::Conversation(message.conversation_id),
                    OpenLoopKind::FollowUp,
                    summary,
                )
                .map_err(|error| yunxi_core::ModelBackendError::InvalidPlan {
                    reason: error.to_string(),
                })?;
                return Ok(DecisionPlan {
                    disposition: DecisionDisposition::SpecialAction,
                    intents: vec![
                        yunxi_core::CognitiveIntent::send_message(
                            message.conversation_id,
                            MessageContent::text(format!("Yunxi noted: {summary}{context}")),
                        ),
                        yunxi_core::CognitiveIntent::create_open_loop(draft),
                    ],
                    state_updates,
                });
            }

            if text.eq_ignore_ascii_case("/done") {
                let mut intents = vec![yunxi_core::CognitiveIntent::send_message(
                    message.conversation_id,
                    MessageContent::text(if input.open_loops.is_empty() {
                        format!("Yunxi found no open item.{context}")
                    } else {
                        format!("Yunxi closed the next open item.{context}")
                    }),
                )];
                if let Some(item) = input.open_loops.first() {
                    intents.push(yunxi_core::CognitiveIntent::resolve_open_loop(
                        item.id(),
                        item.owner(),
                    ));
                }
                return Ok(DecisionPlan {
                    disposition: DecisionDisposition::SpecialAction,
                    intents,
                    state_updates,
                });
            }

            Ok(DecisionPlan {
                disposition: DecisionDisposition::Reply,
                intents: vec![yunxi_core::CognitiveIntent::send_message(
                    message.conversation_id,
                    MessageContent::text(format!("Yunxi heard: {text}{context}")),
                )],
                state_updates,
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
            let conversation_id = match &action {
                ProposedAction::SendMessage(message) => Some(message.conversation_id),
                ProposedAction::ReachOut(_)
                | ProposedAction::UseTool(_)
                | ProposedAction::CreateOpenLoop(_)
                | ProposedAction::ResolveOpenLoop(_)
                | ProposedAction::StartGoal(_)
                | ProposedAction::CancelGoal(_)
                | ProposedAction::Noop => None,
            };
            deliveries.push(action);
            Ok(ActionPortOutcome::Delivered {
                external_reference: Some(format!("fake-delivery-{sequence}")),
                message_id: Some(yunxi_core::MessageId::new()),
                conversation_id,
            })
        })
    }
}

/// The outcome shown by the CLI host after Core arbitration and delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    Journal(JournalError),
    State(CliStateError),
    Autonomy(ConversationLifecycleError),
    Runtime(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planner(error) => write!(formatter, "planner error: {error}"),
            Self::Rejected(error) => write!(formatter, "action rejected: {error}"),
            Self::Port(error) => write!(formatter, "action port failed: {error}"),
            Self::Journal(error) => write!(formatter, "journal error: {error}"),
            Self::State(error) => write!(formatter, "state error: {error}"),
            Self::Autonomy(error) => write!(formatter, "autonomy error: {error}"),
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
    person_id: PersonId,
    conversation_id: ConversationId,
    runtime: Mutex<CognitiveRuntime>,
    journal: Option<Arc<CliJournal>>,
    core_state: Arc<CliCoreState>,
    autonomy_policy: AutonomyPolicy,
    lifecycle: Mutex<ConversationLifecycle>,
    autonomy_retry_after: Mutex<Option<DateTime<Utc>>>,
    turn_gate: Mutex<()>,
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
            .field("person_id", &self.person_id)
            .field("conversation_id", &self.conversation_id)
            .field("autonomy_policy", &self.autonomy_policy)
            .field("journal", &self.journal)
            .field("core_state", &self.core_state.path())
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
        let capabilities = EnvironmentCapabilities::new([
            ActionDescriptor::new(ActionCapability::SendMessage),
            ActionDescriptor::new(ActionCapability::CreateOpenLoop),
            ActionDescriptor::new(ActionCapability::ResolveOpenLoop),
        ]);
        let model = Arc::new(model);
        let person_id = PersonId::new();
        let core_state = Arc::new(CliCoreState::in_memory_for(person_id, conversation_id));
        let (_, runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            core_services(&model, &core_state),
        )
        .expect("default CLI runtime configuration must be valid");
        let arbiter =
            ActionArbiter::new(ActionArbiterConfig::default().with_capabilities(capabilities));
        let autonomy_policy = AutonomyPolicy::default();
        let lifecycle = ConversationLifecycle::new(conversation_id, ConversationKind::Direct)
            .expect("default CLI autonomy policy must be valid");
        Self {
            model,
            environment,
            arbiter,
            person_id,
            conversation_id,
            runtime: Mutex::new(runtime),
            journal: None,
            core_state,
            autonomy_policy,
            lifecycle: Mutex::new(lifecycle),
            autonomy_retry_after: Mutex::new(None),
            turn_gate: Mutex::new(()),
        }
    }

    /// Replaces the ephemeral stores with a shared persistent CLI snapshot.
    /// The snapshot's stable person and conversation IDs become this host's
    /// local identity so all scopes continue to hydrate after a restart.
    #[must_use]
    pub fn with_core_state(mut self, core_state: Arc<CliCoreState>) -> Self {
        self.person_id = core_state.person_id();
        self.conversation_id = core_state.conversation_id();
        self.runtime
            .get_mut()
            .expect("owned CLI runtime lock cannot be poisoned")
            .install_services(core_services(&self.model, &core_state));
        let lifecycle = ConversationLifecycle::new(self.conversation_id, ConversationKind::Direct)
            .expect("default CLI autonomy policy must be valid");
        *self
            .lifecycle
            .get_mut()
            .expect("owned CLI lifecycle lock cannot be poisoned") = lifecycle;
        *self
            .autonomy_retry_after
            .get_mut()
            .expect("owned CLI autonomy retry lock cannot be poisoned") = None;
        self.core_state = core_state;
        self
    }

    /// Enables an optional durable write-ahead journal for this host.
    #[must_use]
    pub fn with_journal(mut self, journal: Arc<CliJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Replaces the default idle and cooldown windows used by the standalone
    /// autonomous conversation loop. This is primarily useful for deterministic
    /// host tests; production-like callers should keep the defaults.
    pub fn try_with_autonomy_policy(mut self, policy: AutonomyPolicy) -> Result<Self, CliError> {
        policy.validate().map_err(CliError::Autonomy)?;
        self.autonomy_policy = policy;
        Ok(self)
    }

    /// Returns a copy of the current host autonomy policy.
    #[must_use]
    pub const fn autonomy_policy(&self) -> AutonomyPolicy {
        self.autonomy_policy
    }

    /// Returns the serializable lifecycle snapshot used by the scheduler.
    pub fn lifecycle(&self) -> Result<ConversationLifecycle, CliError> {
        self.lifecycle
            .lock()
            .map_err(|_| CliError::Runtime("autonomy lifecycle lock poisoned".to_owned()))
            .map(|lifecycle| lifecycle.clone())
    }

    /// Returns the configured journal, if this host was built with one.
    #[must_use]
    pub fn journal(&self) -> Option<&CliJournal> {
        self.journal.as_deref()
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub const fn person_id(&self) -> PersonId {
        self.person_id
    }

    #[must_use]
    pub fn core_state(&self) -> &CliCoreState {
        &self.core_state
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
        self.process_line_at(input, Utc::now())
    }

    /// Deterministic-clock variant of [`Self::process_line`]. Hosts and tests
    /// can use this to advance the lifecycle without sleeping.
    pub fn process_line_at(
        &self,
        input: &str,
        occurred_at: DateTime<Utc>,
    ) -> Result<HostResponse, CliError> {
        let _turn_guard = self
            .turn_gate
            .lock()
            .map_err(|_| CliError::Runtime("CLI turn gate poisoned".to_owned()))?;
        let input = input.trim();
        if input.is_empty() {
            return Ok(HostResponse::Empty);
        }

        {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| CliError::Runtime("autonomy lifecycle lock poisoned".to_owned()))?;
            lifecycle
                .observe_inbound(ConversationKind::Direct, self.person_id, occurred_at)
                .map_err(CliError::Autonomy)?;
        }
        *self
            .autonomy_retry_after
            .lock()
            .map_err(|_| CliError::Runtime("CLI autonomy retry lock poisoned".to_owned()))? = None;

        let journal = self.journal.clone();
        let journal_sequence = journal
            .as_deref()
            .map(|journal| {
                journal
                    .start(self.conversation_id, input)
                    .map_err(CliError::Journal)
            })
            .transpose()?;

        let event = WorldEvent::message_received(
            EventPriority::High,
            yunxi_core::MessageReceivedEvent {
                message_id: yunxi_core::MessageId::new(),
                conversation_id: self.conversation_id,
                sender: self.person_id,
                content: MessageContent::text(input),
                reply_to: None,
                timestamp: occurred_at,
                conversation_kind: ConversationKind::Direct,
                addressed_to_agent: true,
                replies_to_agent: false,
                stop_requested: false,
                explicit_request: true,
                visible_reply_allowed: true,
            },
        );
        let result = self
            .core_state
            .remember_message(self.conversation_id, input, occurred_at)
            .map_err(CliError::State)
            .and_then(|_| self.run_event(event))
            .and_then(|(plan, response)| {
                if response_is_delivered(&response) {
                    self.record_reactive_outbound(occurred_at, &plan)?;
                }
                Ok(response)
            });

        finish_journal(journal.as_deref(), journal_sequence, result)
    }

    /// Runs one due autonomous conversation turn, if the lifecycle says the
    /// conversation is idle and its continuation cooldown has elapsed.
    pub fn process_autonomous_tick(&self) -> Result<Option<HostResponse>, CliError> {
        self.process_autonomous_tick_at(Utc::now())
    }

    /// Deterministic-clock variant used by hosts and acceptance tests.
    /// Returning `None` means no turn was due; `Some(Noop)` means a due turn
    /// was claimed but the planner elected not to speak.
    pub fn process_autonomous_tick_at(
        &self,
        occurred_at: DateTime<Utc>,
    ) -> Result<Option<HostResponse>, CliError> {
        let _turn_guard = self
            .turn_gate
            .lock()
            .map_err(|_| CliError::Runtime("CLI turn gate poisoned".to_owned()))?;
        if self
            .autonomy_retry_after
            .lock()
            .map_err(|_| CliError::Runtime("CLI autonomy retry lock poisoned".to_owned()))?
            .is_some_and(|retry_after| occurred_at < retry_after)
        {
            return Ok(None);
        }
        let claimed = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| CliError::Runtime("autonomy lifecycle lock poisoned".to_owned()))?;
            lifecycle
                .claim_autonomous(occurred_at, self.autonomy_policy)
                .map_err(CliError::Autonomy)?
        };
        if !claimed {
            return Ok(None);
        }

        let journal = self.journal.clone();
        let journal_sequence = match journal.as_deref() {
            Some(journal) => match journal.start(self.conversation_id, AUTONOMOUS_TICK_INPUT) {
                Ok(sequence) => Some(sequence),
                Err(error) => {
                    self.release_autonomous_claim_for_retry(occurred_at)?;
                    return Err(CliError::Journal(error));
                }
            },
            None => None,
        };
        let event = WorldEvent::new(
            occurred_at,
            EventScope::Conversation {
                conversation_id: self.conversation_id,
            },
            EventPriority::High,
            WorldEventKind::AutonomousConversationTick(AutonomousConversationTickEvent {
                conversation_kind: Some(ConversationKind::Direct),
                person_id: Some(self.person_id),
                claim_token: None,
            }),
        );
        let result = self.run_event(event);
        let host_result = match result {
            Ok((plan, response)) => {
                let delivered = response_is_delivered(&response);
                let directive = plan_directive(&plan, self.conversation_id)
                    .unwrap_or(ConversationTurnDirective::Continue);
                self.finish_autonomous_claim(occurred_at, delivered, directive)?;
                *self.autonomy_retry_after.lock().map_err(|_| {
                    CliError::Runtime("CLI autonomy retry lock poisoned".to_owned())
                })? = None;
                Ok(response)
            }
            Err(error) => {
                self.release_autonomous_claim_for_retry(occurred_at)?;
                Err(error)
            }
        };

        finish_journal(journal.as_deref(), journal_sequence, host_result).map(Some)
    }

    fn run_event(&self, event: WorldEvent) -> Result<(DecisionPlan, HostResponse), CliError> {
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| CliError::Runtime("runtime lock poisoned".to_owned()))?;
        let action_port = CliActionPort {
            environment: &self.environment,
            core_state: &self.core_state,
        };
        let outcome = block_on(runtime.process_event_with_planner_and_actions(
            event,
            &self.arbiter,
            &action_port,
        ))
        .map_err(CliError::Planner)?;
        let PlannedProcessingOutcome::Planned { plan, actions, .. } = outcome else {
            return Err(CliError::Runtime(
                "runtime rejected the CLI event".to_owned(),
            ));
        };
        let response = response_from_actions(&plan, actions)?;
        Ok((plan, response))
    }

    fn record_reactive_outbound(
        &self,
        occurred_at: DateTime<Utc>,
        plan: &DecisionPlan,
    ) -> Result<(), CliError> {
        let directive = plan_directive(plan, self.conversation_id)
            .unwrap_or(ConversationTurnDirective::Continue);
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| CliError::Runtime("autonomy lifecycle lock poisoned".to_owned()))?;
        lifecycle
            .record_outbound(occurred_at, Some(directive), self.autonomy_policy)
            .map_err(CliError::Autonomy)
    }

    fn finish_autonomous_claim(
        &self,
        occurred_at: DateTime<Utc>,
        delivered: bool,
        directive: ConversationTurnDirective,
    ) -> Result<(), CliError> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| CliError::Runtime("autonomy lifecycle lock poisoned".to_owned()))?;
        lifecycle
            .finish_autonomous_claim(occurred_at, delivered, directive, self.autonomy_policy)
            .map_err(CliError::Autonomy)
    }

    fn release_autonomous_claim_for_retry(
        &self,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), CliError> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| CliError::Runtime("autonomy lifecycle lock poisoned".to_owned()))?;
        lifecycle
            .release_autonomous_claim()
            .map_err(CliError::Autonomy)?;
        drop(lifecycle);
        *self
            .autonomy_retry_after
            .lock()
            .map_err(|_| CliError::Runtime("CLI autonomy retry lock poisoned".to_owned()))? =
            Some(occurred_at + AUTONOMOUS_RETRY_BACKOFF);
        Ok(())
    }
}

fn finish_journal(
    journal: Option<&CliJournal>,
    sequence: Option<u64>,
    result: Result<HostResponse, CliError>,
) -> Result<HostResponse, CliError> {
    if let (Some(journal), Some(sequence)) = (journal, sequence) {
        let journal_result = match &result {
            Ok(response) => journal.complete(sequence, response),
            Err(error) => journal.fail(sequence, error.to_string()),
        };
        // A completed action must not be reported as a planner failure if
        // only the post-action audit record could not be written. For a
        // successful turn, however, surface the journal failure so a host
        // can repair its storage rather than silently lose durability.
        if let Err(error) = journal_result
            && result.is_ok()
        {
            return Err(CliError::Journal(error));
        }
    }
    result
}

fn response_is_delivered(response: &HostResponse) -> bool {
    matches!(response, HostResponse::Delivered { .. })
}

fn plan_directive(
    plan: &DecisionPlan,
    conversation_id: ConversationId,
) -> Option<ConversationTurnDirective> {
    plan.state_updates.iter().find_map(|update| {
        let StateUpdateProposal::ConversationDirective {
            conversation_id: target,
            directive,
        } = update
        else {
            return None;
        };
        (*target == conversation_id).then_some(*directive)
    })
}

fn response_from_actions(
    plan: &DecisionPlan,
    actions: Vec<ActionResult>,
) -> Result<HostResponse, CliError> {
    if actions.len() != plan.intents.len() {
        return Err(CliError::Runtime(format!(
            "runtime returned {} action results for {} intents",
            actions.len(),
            plan.intents.len()
        )));
    }
    let visible = plan
        .intents
        .iter()
        .enumerate()
        .find_map(|(index, intent)| intent_message(intent).map(|message| (index, message)));
    let mut visible_reference = None;
    let mut delivered = false;
    let mut deferred_reason = None;
    for (index, action) in actions.into_iter().enumerate() {
        match action {
            ActionResult::Noop => {}
            ActionResult::Executed {
                outcome:
                    ActionPortOutcome::Delivered {
                        external_reference, ..
                    },
                ..
            } => {
                delivered = true;
                if visible
                    .as_ref()
                    .is_some_and(|(visible_index, _)| *visible_index == index)
                {
                    visible_reference = external_reference;
                }
            }
            ActionResult::Executed {
                outcome: ActionPortOutcome::Deferred { reason },
                ..
            } => {
                deferred_reason.get_or_insert(reason);
            }
            ActionResult::Executed {
                outcome: ActionPortOutcome::DeliveryIndeterminate { reason, .. },
                ..
            } => {
                deferred_reason.get_or_insert_with(|| format!("delivery_indeterminate:{reason}"));
            }
            ActionResult::Executed {
                outcome:
                    ActionPortOutcome::ToolCompleted { .. } | ActionPortOutcome::ToolFailed { .. },
                ..
            } => {
                // Tool results are observations for a later planner turn, not
                // proof that this host delivered a visible message.
            }
            ActionResult::Rejected(error) => return Err(CliError::Rejected(error)),
            ActionResult::Failed { error, .. } => return Err(CliError::Port(error)),
        }
    }
    let message = visible.map_or_else(String::new, |(_, message)| message);
    if let Some(reason) = deferred_reason {
        return Ok(HostResponse::Deferred { message, reason });
    }
    if delivered {
        return Ok(HostResponse::Delivered {
            message,
            external_reference: visible_reference,
        });
    }
    Ok(HostResponse::Noop)
}

fn core_services<M>(model: &Arc<M>, core_state: &Arc<CliCoreState>) -> CoreServices
where
    M: CoreModelBackend + 'static,
{
    CoreServices::new(Arc::clone(model) as Arc<dyn CoreModelBackend>)
        .with_memory(Arc::clone(core_state) as Arc<dyn MemoryStore>)
        .with_open_loops(Arc::clone(core_state) as Arc<dyn OpenLoopStore>)
        .with_relations(Arc::clone(core_state) as Arc<dyn yunxi_core::RelationStore>)
        .with_affect(Arc::clone(core_state) as Arc<dyn yunxi_core::AffectStore>)
}

struct CliActionPort<'a, E> {
    environment: &'a E,
    core_state: &'a CliCoreState,
}

impl<E> ActionPort for CliActionPort<'_, E>
where
    E: ActionPort,
{
    fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a> {
        match action {
            ProposedAction::CreateOpenLoop(action) => Box::pin(async move {
                let item = self
                    .core_state
                    .create(&action.draft)
                    .await
                    .map_err(|error| ActionPortError::new(error.to_string(), true))?;
                Ok(ActionPortOutcome::Delivered {
                    external_reference: Some(format!("cli-open-loop:{}", item.id())),
                    message_id: None,
                    conversation_id: item.owner().conversation_id(),
                })
            }),
            ProposedAction::ResolveOpenLoop(action) => Box::pin(async move {
                let item = self
                    .core_state
                    .get(action.open_loop_id)
                    .await
                    .map_err(|error| ActionPortError::new(error.to_string(), true))?
                    .ok_or_else(|| ActionPortError::new("open_loop_not_found", false))?;
                if item.owner() != action.owner {
                    return Err(ActionPortError::new("open_loop_owner_mismatch", false));
                }
                let resolved = self
                    .core_state
                    .resolve(action.open_loop_id, chrono::Utc::now())
                    .await
                    .map_err(|error| ActionPortError::new(error.to_string(), true))?;
                Ok(ActionPortOutcome::Delivered {
                    external_reference: Some(format!("cli-open-loop-resolved:{}", resolved.id())),
                    message_id: None,
                    conversation_id: resolved.owner().conversation_id(),
                })
            }),
            _ => self.environment.execute(action),
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
        ProposedAction::UseTool(_)
        | ProposedAction::CreateOpenLoop(_)
        | ProposedAction::ResolveOpenLoop(_)
        | ProposedAction::StartGoal(_)
        | ProposedAction::CancelGoal(_) => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use yunxi_core::{ActionReceipt, CognitiveIntent};

    #[test]
    fn secondary_deferred_action_defers_the_host_response() {
        let conversation_id = ConversationId::new();
        let draft = OpenLoopDraft::new(
            OpenLoopOwner::Conversation(conversation_id),
            OpenLoopKind::FollowUp,
            "later",
        )
        .expect("draft");
        let plan = DecisionPlan {
            disposition: DecisionDisposition::SpecialAction,
            intents: vec![
                CognitiveIntent::send_message(conversation_id, MessageContent::text("noted")),
                CognitiveIntent::create_open_loop(draft),
            ],
            state_updates: Vec::new(),
        };
        let receipt = ActionReceipt {
            action_id: None,
            idempotency_key: None,
            admitted_at: chrono::Utc::now(),
        };
        let response = response_from_actions(
            &plan,
            vec![
                ActionResult::Executed {
                    receipt: receipt.clone(),
                    outcome: ActionPortOutcome::Delivered {
                        external_reference: Some("message-1".to_owned()),
                        message_id: None,
                        conversation_id: Some(conversation_id),
                    },
                },
                ActionResult::Executed {
                    receipt,
                    outcome: ActionPortOutcome::Deferred {
                        reason: "state_busy".to_owned(),
                    },
                },
            ],
        )
        .expect("deferred response");
        assert_eq!(
            response,
            HostResponse::Deferred {
                message: "noted".to_owned(),
                reason: "state_busy".to_owned(),
            }
        );
    }

    #[test]
    fn tool_results_are_not_reported_as_visible_delivery() {
        let conversation_id = ConversationId::new();
        let plan = DecisionPlan {
            disposition: DecisionDisposition::SpecialAction,
            intents: vec![CognitiveIntent::use_tool(
                "time.now",
                "{}",
                yunxi_core::ActionScope::Conversation(conversation_id),
            )],
            state_updates: Vec::new(),
        };
        let receipt = ActionReceipt {
            action_id: None,
            idempotency_key: None,
            admitted_at: chrono::Utc::now(),
        };

        for outcome in [
            ActionPortOutcome::ToolCompleted {
                operation: "time.now".to_owned(),
                output: "12:00".to_owned(),
            },
            ActionPortOutcome::ToolFailed {
                operation: "time.now".to_owned(),
                error_category: "unavailable".to_owned(),
                detail: "clock unavailable".to_owned(),
            },
        ] {
            let response = response_from_actions(
                &plan,
                vec![ActionResult::Executed {
                    receipt: receipt.clone(),
                    outcome,
                }],
            )
            .expect("tool outcome should remain a non-visible observation");
            assert_eq!(response, HostResponse::Noop);
        }
    }
}
