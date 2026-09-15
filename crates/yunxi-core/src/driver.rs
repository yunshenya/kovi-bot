//! Host-agnostic cognitive loop driver.
//!
//! A runtime on its own only knows how to advance one step: take an event,
//! plan, arbitrate, dispatch, and turn action results into follow-up events.
//! Something still has to *drive* that step in a loop, and before this module
//! existed every host wrote that loop itself. A copy in the Kovi adapter and a
//! copy in `yunxi-cli` meant two different notions of when a turn is over, what
//! a failed turn should retry, and which event consumes the loop next.
//!
//! The driver owns the topology of the cycle and the disposition rules that
//! belong to Core semantics:
//!
//! - tool and action results re-enter the loop as follow-up events, so a
//!   multi-step task runs to completion inside the driver;
//! - the host-declared cancellation check (`should_process`) is consulted for
//!   every event, so a superseded lease never completes a turn;
//! - an autonomous turn's retry/finish decision is computed here from the
//!   action results and the plan's directive, not by the adapter.
//!
//! What stays on the host side is bookkeeping Core cannot perform: delivering
//! messages, writeback, lease release, and persistence. The driver reaches the
//! host only through [`CognitiveTurnObserver`].

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::arbiter::{ActionArbiter, ActionPort, ActionRejection, ActionResult};
use crate::event::{EventType, WorldEvent};
use crate::planner::{ConversationTurnDirective, DecisionPlan, PlannerError, StateUpdateProposal};
use crate::runtime::{
    CognitiveRuntime, PlannedProcessingOutcome, ProcessingOutcome, RuntimeObservation,
};

/// Upper bound for one `run` call without an explicit `max_turns` limit.
const MAX_TURNS_WITHOUT_LIMIT: usize = 1_000_000;

/// How one driven turn ended.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// The event was planned and dispatched. The plan may be a silent one.
    Planned {
        plan: DecisionPlan,
        actions: Vec<ActionResult>,
    },
    /// The host's cancellation guard discarded the turn before planning, so
    /// the host must release whatever lease the event carries.
    Cancelled,
    /// Core rejected the event envelope itself.
    InvalidEvent,
    /// The event could not be applied to working state.
    InvalidState,
}

/// Core's verdict for one driven turn.
#[derive(Debug, Clone)]
pub struct TurnReport {
    /// What happened to this turn.
    pub outcome: TurnOutcome,
    /// The observation Core recorded for the event, absent when the event was
    /// rejected before it could be observed. Compatibility hosts that run
    /// without a planner report this instead of a plan.
    pub observed: Option<RuntimeObservation>,
    /// Messages that actually crossed the delivery boundary this turn, in plan
    /// order. The host records these; Core does not persist.
    pub delivered_replies: Vec<String>,
    /// The directive the planner asked for, if any.
    pub expected_directive: Option<ConversationTurnDirective>,
    /// True when the processed event was an autonomous conversation tick.
    pub autonomous_tick: bool,
    /// The autonomous disposition, present only for autonomous ticks.
    pub autonomous: Option<AutonomousTurnDisposition>,
}

impl TurnReport {
    /// The plan this turn produced, if it produced one.
    #[must_use]
    pub fn plan(&self) -> Option<&DecisionPlan> {
        match &self.outcome {
            TurnOutcome::Planned { plan, .. } => Some(plan),
            _ => None,
        }
    }

    /// The action results of this turn, if it reached dispatch.
    #[must_use]
    pub fn actions(&self) -> Option<&[ActionResult]> {
        match &self.outcome {
            TurnOutcome::Planned { actions, .. } => Some(actions),
            _ => None,
        }
    }

    /// A turn the host refused. Any action that already crossed its delivery
    /// boundary is preserved so the host can still record what went out.
    fn cancelled_turn(
        observation: Option<RuntimeObservation>,
        plan: Option<&DecisionPlan>,
        actions: &[ActionResult],
    ) -> Self {
        let delivered_replies =
            plan.map_or_else(Vec::new, |plan| Self::collect_delivered(plan, actions));
        Self {
            outcome: TurnOutcome::Cancelled,
            observed: observation,
            delivered_replies,
            expected_directive: None,
            autonomous_tick: observation.is_some_and(|observation| {
                observation.event_type == EventType::AutonomousConversationTick
            }),
            autonomous: None,
        }
    }

    fn invalid(outcome: TurnOutcome) -> Self {
        Self {
            outcome,
            observed: None,
            delivered_replies: Vec::new(),
            expected_directive: None,
            autonomous_tick: false,
            autonomous: None,
        }
    }

    /// Collects the visible text of every message this turn actually delivered.
    fn collect_delivered(plan: &DecisionPlan, actions: &[ActionResult]) -> Vec<String> {
        let mut delivered = Vec::new();
        for (intent, action) in plan.intents.iter().zip(actions.iter()) {
            let crate::intent::CognitiveIntent::SendMessage { content, .. } = intent else {
                continue;
            };
            let crossed_boundary = matches!(
                action,
                ActionResult::Executed {
                    outcome: crate::arbiter::ActionPortOutcome::Delivered { .. },
                    ..
                }
            );
            if !crossed_boundary {
                continue;
            }
            let sent = content.history_text();
            let sent = sent.trim();
            if !sent.is_empty() {
                delivered.push(sent.to_owned());
            }
        }
        delivered
    }
}

/// What the host must do with an autonomous conversation turn now that Core
/// has seen its action results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomousTurnDisposition {
    /// The turn never crossed a delivery boundary and may be attempted again
    /// inside the host's bounded retry budget.
    Retry,
    /// The turn consumed its claim and its continuation decision is final.
    Finish {
        delivered: bool,
        directive: ConversationTurnDirective,
    },
}

/// Host callbacks and declaration points for one driven loop.
///
/// Every method has a default so a host implements only what it genuinely
/// needs; a minimal host can drive Core with an empty implementation.
///
/// `Sync` is required because [`should_process`](Self::should_process) is a
/// query, not a mutation: the runtime hands it to the step guard, which must
/// be a shared `Fn`. A host that wants to record which events were checked
/// uses interior mutability rather than a `&mut` borrow.
pub trait CognitiveTurnObserver: Send + Sync {
    /// Host-declared cancellation check. Returning `false` means this event's
    /// work was superseded (typically because an external lease was replaced)
    /// and the turn must be released rather than completed.
    ///
    /// Core asks this once per event, immediately after the event is taken
    /// from the queue. A `false` answer discards whatever plan the turn
    /// produced, so the host must release the lease the event carries.
    fn should_process(&self, _event: &WorldEvent) -> bool {
        true
    }

    /// Called immediately before each event is taken from the queue so the
    /// host can refresh anything the next turn reads (dynamic capabilities,
    /// configuration, clocks).
    fn before_turn(&mut self) -> TurnHook<'_> {
        Box::pin(async {})
    }

    /// Called once per turn whose event reached a decision.
    ///
    /// Asynchronous because the work Core cannot do — writeback, reply
    /// linkage, lease bookkeeping — is asynchronous in every real host.
    fn on_turn<'a>(&'a mut self, event: &'a WorldEvent, report: &'a TurnReport) -> TurnHook<'a>;

    /// Called once when planning itself failed. The runtime consumed the event,
    /// so the host must release any lease or reservation carried by it.
    fn on_planner_error<'a>(
        &'a mut self,
        event: &'a WorldEvent,
        error: &'a PlannerError,
    ) -> TurnHook<'a>;

    /// Called after every turn, including rejected ones.
    fn on_turn_end(&mut self) -> TurnHook<'_> {
        Box::pin(async {})
    }
}

/// A host hook in progress.
///
/// The trait is boxed rather than using `async fn` in traits so that drivers
/// can accept `&mut dyn CognitiveTurnObserver`; a generic parameter would push
/// the whole loop into every call site instead.
pub type TurnHook<'a> = std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Drives a runtime as a resident loop until the queue closes.
///
/// This is the production shape: the driver blocks while the queue is idle so
/// the host can stay resident, and returns when every producer has dropped
/// their handle. Tool results and action outcomes re-enter the runtime as
/// follow-up events during the same call, so a multi-step task runs to its
/// terminal response without the host re-entering the loop.
///
/// Returns the number of turns consumed. Tests and bounded scaffolding that
/// must return once the queue is quiet use [`drain`] instead.
pub async fn run(
    runtime: &mut CognitiveRuntime,
    arbiter: &ActionArbiter,
    port: &dyn ActionPort,
    observer: &mut dyn CognitiveTurnObserver,
) -> usize {
    drive(runtime, arbiter, port, observer, None).await
}

/// [`run`] with a bound on how many turns one call may consume.
///
/// The bound is a safety valve for hosts that must yield the loop; it is not
/// the loop's stopping condition. A turn limit that silently truncates a real
/// task is indistinguishable from a task that finished, so hosts should pass
/// `None` unless they have a concrete reason to yield.
pub async fn run_bounded(
    runtime: &mut CognitiveRuntime,
    arbiter: &ActionArbiter,
    port: &dyn ActionPort,
    observer: &mut dyn CognitiveTurnObserver,
    max_turns: Option<usize>,
) -> usize {
    drive(runtime, arbiter, port, observer, max_turns).await
}

/// Drives the runtime until it is quiescent, never waiting for new work.
///
/// A bounded driver returns as soon as no event is ready, which is what unit
/// tests and scaffolding need. It is not the production loop: quiescence is
/// indistinguishable from "the next message has not arrived yet", so a host
/// that must stay resident uses [`run`].
pub async fn drain(
    runtime: &mut CognitiveRuntime,
    arbiter: &ActionArbiter,
    port: &dyn ActionPort,
    observer: &mut dyn CognitiveTurnObserver,
) -> usize {
    let mut driven = 0;
    while runtime.has_pending_event() {
        observer.before_turn().await;
        let Some((event, result, dismissed)) = step(runtime, arbiter, port, observer).await else {
            return driven;
        };
        record(result, dismissed, &event, observer).await;
        observer.on_turn_end().await;
        driven += 1;
    }
    driven
}

async fn drive(
    runtime: &mut CognitiveRuntime,
    arbiter: &ActionArbiter,
    port: &dyn ActionPort,
    observer: &mut dyn CognitiveTurnObserver,
    max_turns: Option<usize>,
) -> usize {
    let limit = match max_turns {
        Some(0) => return 0,
        Some(limit) => limit,
        None => MAX_TURNS_WITHOUT_LIMIT,
    };
    let mut driven = 0;
    while driven < limit {
        observer.before_turn().await;
        let Some((event, result, dismissed)) = step(runtime, arbiter, port, observer).await else {
            return driven;
        };
        record(result, dismissed, &event, observer).await;
        observer.on_turn_end().await;
        driven += 1;
    }
    driven
}

/// Advances the runtime by exactly one event.
///
/// Returns `None` when the queue closed. The third value reports whether the
/// host's cancellation guard dismissed the turn: Core answers a refusal by
/// discarding the plan, which is indistinguishable from a model that chose
/// silence unless the guard call is observed here.
async fn step(
    runtime: &mut CognitiveRuntime,
    arbiter: &ActionArbiter,
    port: &dyn ActionPort,
    observer: &mut dyn CognitiveTurnObserver,
) -> Option<(
    WorldEvent,
    Result<PlannedProcessingOutcome, PlannerError>,
    bool,
)> {
    // An atomic flag keeps the guard a shared, cross-thread `Fn` while still
    // recording its answer. Core may consult the guard before planning, after
    // planning, and before each dispatched intent, so what matters is whether
    // it ever said no. The first consultation happens before the first await,
    // so a refusal can never be published late.
    let dismissed = AtomicBool::new(false);
    let next_step = {
        let observer = &*observer;
        let dismissed = &dismissed;
        let guard = move |event: &WorldEvent| {
            if observer.should_process(event) {
                true
            } else {
                dismissed.store(true, Ordering::Relaxed);
                false
            }
        };
        runtime
            .process_next_with_planner_and_actions_with_event_and_guard(arbiter, port, &guard)
            .await
    };
    let (event, result) = next_step?;
    Some((event, result, dismissed.load(Ordering::Relaxed)))
}

/// Drives a runtime that has no planner installed, until the queue closes.
///
/// Compatibility mode consumes events and observes them into working state.
/// Without a planner Core cannot produce a continuation, so every event is
/// terminal here — including autonomous ticks.
pub async fn run_observed(
    runtime: &mut CognitiveRuntime,
    observer: &mut dyn CognitiveTurnObserver,
) -> usize {
    observe(runtime, observer, None, true).await
}

/// [`drain`] for a runtime that has no planner installed.
pub async fn drain_observed(
    runtime: &mut CognitiveRuntime,
    observer: &mut dyn CognitiveTurnObserver,
) -> usize {
    observe(runtime, observer, None, false).await
}

/// [`run_observed`] with a bound on how many turns one call may consume.
pub async fn run_observed_bounded(
    runtime: &mut CognitiveRuntime,
    observer: &mut dyn CognitiveTurnObserver,
    max_turns: Option<usize>,
) -> usize {
    observe(runtime, observer, max_turns, true).await
}

async fn observe(
    runtime: &mut CognitiveRuntime,
    observer: &mut dyn CognitiveTurnObserver,
    max_turns: Option<usize>,
    resident: bool,
) -> usize {
    let limit = match max_turns {
        Some(0) => return 0,
        Some(limit) => limit,
        None => MAX_TURNS_WITHOUT_LIMIT,
    };
    let mut driven = 0;
    while driven < limit {
        if !resident && !runtime.has_pending_event() {
            return driven;
        }
        observer.before_turn().await;
        let Some((event, outcome)) = runtime.process_next_with_event().await else {
            return driven;
        };
        // A compatibility runtime has no planner, so it never produces a
        // continuation and there is no superseded work to skip: the host's
        // cancellation query is not consulted here. Rejected events are still
        // reported so the host can release the lease they carry.
        let report = match outcome {
            ProcessingOutcome::Observed(observation) => TurnReport {
                outcome: TurnOutcome::Planned {
                    plan: DecisionPlan::silent(),
                    actions: Vec::new(),
                },
                observed: Some(observation),
                delivered_replies: Vec::new(),
                expected_directive: None,
                autonomous_tick: observation.event_type == EventType::AutonomousConversationTick,
                autonomous: None,
            },
            ProcessingOutcome::RejectedEvent { .. } => {
                TurnReport::invalid(TurnOutcome::InvalidEvent)
            }
            ProcessingOutcome::RejectedState { .. } => {
                TurnReport::invalid(TurnOutcome::InvalidState)
            }
        };
        observer.on_turn(&event, &report).await;
        observer.on_turn_end().await;
        driven += 1;
    }
    driven
}

async fn record(
    result: Result<PlannedProcessingOutcome, PlannerError>,
    dismissed: bool,
    event: &WorldEvent,
    observer: &mut dyn CognitiveTurnObserver,
) {
    match result {
        // The host refused the event, so Core discarded the turn. A refusal
        // still reaches here as a `Planned` outcome — sometimes with the
        // already-executed prefix of a multi-intent plan — which is why
        // `dismissed` has to travel alongside the result.
        Ok(PlannedProcessingOutcome::Planned {
            observation,
            plan,
            actions,
            ..
        }) if dismissed => {
            let report = TurnReport::cancelled_turn(Some(observation), Some(&plan), &actions);
            observer.on_turn(event, &report).await;
        }
        Ok(PlannedProcessingOutcome::Planned {
            observation,
            plan,
            actions,
            ..
        }) => {
            let report = planned_report(observation, &plan, actions);
            observer.on_turn(event, &report).await;
        }
        Ok(PlannedProcessingOutcome::RejectedEvent { .. }) => {
            observer
                .on_turn(event, &TurnReport::invalid(TurnOutcome::InvalidEvent))
                .await;
        }
        Ok(PlannedProcessingOutcome::RejectedState { .. }) => {
            observer
                .on_turn(event, &TurnReport::invalid(TurnOutcome::InvalidState))
                .await;
        }
        Err(error) => observer.on_planner_error(event, &error).await,
    }
}

fn planned_report(
    observation: RuntimeObservation,
    plan: &DecisionPlan,
    actions: Vec<ActionResult>,
) -> TurnReport {
    let autonomous_tick = observation.event_type == EventType::AutonomousConversationTick;
    let conversation_id = observation.scope.conversation_id();
    let expected_directive = conversation_id.and_then(|conversation_id| {
        plan.state_updates.iter().find_map(|update| match update {
            StateUpdateProposal::ConversationDirective {
                conversation_id: updated,
                directive,
            } if *updated == conversation_id => Some(*directive),
            _ => None,
        })
    });
    let delivered_replies = TurnReport::collect_delivered(plan, &actions);
    let autonomous = autonomous_tick.then(|| {
        let delivered = !delivered_replies.is_empty();
        if autonomous_turn_should_retry(&actions, delivered, expected_directive) {
            AutonomousTurnDisposition::Retry
        } else {
            AutonomousTurnDisposition::Finish {
                delivered,
                directive: resolved_directive(delivered, expected_directive),
            }
        }
    });
    TurnReport {
        outcome: TurnOutcome::Planned {
            plan: plan.clone(),
            actions,
        },
        observed: Some(observation),
        delivered_replies,
        expected_directive,
        autonomous_tick,
        autonomous,
    }
}

/// Whether an autonomous turn that produced no visible message deserves
/// another bounded attempt.
///
/// Action results are stronger evidence than the model's directive: a
/// deferred or retryable result means the message never crossed the
/// side-effect boundary. A `Continue` directive with no action at all means a
/// further thought was requested but never materialized. A missing, `Wait`, or
/// `End` directive is a legitimate silent turn and must not become a hot loop.
#[must_use]
fn autonomous_turn_should_retry(
    actions: &[ActionResult],
    delivered: bool,
    expected_directive: Option<ConversationTurnDirective>,
) -> bool {
    if delivered {
        return false;
    }
    if !actions.is_empty() {
        return autonomous_action_needs_retry(actions);
    }
    expected_directive == Some(ConversationTurnDirective::Continue)
}

/// Whether a failed action never crossed an irreversible delivery boundary.
///
/// Indeterminate delivery is deliberately terminal: replaying it could
/// duplicate a message whose platform outcome is unknown.
#[must_use]
fn autonomous_action_needs_retry(actions: &[ActionResult]) -> bool {
    actions.iter().any(|action| match action {
        ActionResult::Executed {
            outcome: crate::arbiter::ActionPortOutcome::Deferred { .. },
            ..
        } => true,
        ActionResult::Failed { error, .. } => error.retryable,
        ActionResult::Rejected(rejection) => matches!(
            rejection,
            ActionRejection::CapabilityUnavailable { .. }
                | ActionRejection::CooldownActive { .. }
                | ActionRejection::RateLimitExceeded { .. }
                | ActionRejection::IdempotencyStateFull { .. }
                | ActionRejection::CooldownStateFull { .. }
                | ActionRejection::Stale { .. }
                | ActionRejection::TargetUnavailable { .. }
                | ActionRejection::DeliveryResolutionFailed { .. }
        ),
        ActionResult::Noop
        | ActionResult::Executed {
            outcome:
                crate::arbiter::ActionPortOutcome::Delivered { .. }
                | crate::arbiter::ActionPortOutcome::DeliveryIndeterminate { .. }
                | crate::arbiter::ActionPortOutcome::ToolCompleted { .. }
                | crate::arbiter::ActionPortOutcome::ToolFailed { .. },
            ..
        } => false,
    })
}

/// The continuation decision an autonomous turn settles on.
///
/// A turn that delivered obeys the planner's directive; an explicit `End`
/// survives even when nothing was delivered (she decided to stop talking); and
/// anything else waits for the host scheduler rather than promising a
/// continuation that was never produced.
#[must_use]
fn resolved_directive(
    delivered: bool,
    expected_directive: Option<ConversationTurnDirective>,
) -> ConversationTurnDirective {
    match (delivered, expected_directive) {
        (true, Some(directive)) => directive,
        (false, Some(ConversationTurnDirective::End)) => ConversationTurnDirective::End,
        _ => ConversationTurnDirective::Wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{ActionScope, ProposedAction};
    use crate::arbiter::{
        ActionArbiterConfig, ActionPortError, ActionPortFuture, ActionPortOutcome, ActionResult,
        EnvironmentCapabilities,
    };
    use crate::event::{EventPriority, MessageContent, MessageReceivedEvent, WorldEventKind};
    use crate::identity::{ConversationId, ConversationKind, MessageId, PersonId};
    use crate::intent::{CognitiveIntent, ToolNotificationPolicy};
    use crate::planner::{
        DecisionDisposition, ModelBackend, ModelBackendError, ModelBackendFuture, PlannerInput,
    };
    use crate::ports::CoreServices;
    use crate::runtime::RuntimeConfig;
    use chrono::Utc;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};
    use std::thread;

    #[test]
    fn autonomous_retry_follows_evidence_not_optimism() {
        // Nothing delivered and no action: only an explicit Continue retries.
        assert!(!autonomous_turn_should_retry(&[], false, None));
        assert!(!autonomous_turn_should_retry(
            &[],
            false,
            Some(ConversationTurnDirective::Wait)
        ));
        assert!(!autonomous_turn_should_retry(
            &[],
            false,
            Some(ConversationTurnDirective::End)
        ));
        assert!(autonomous_turn_should_retry(
            &[],
            false,
            Some(ConversationTurnDirective::Continue)
        ));
        // A delivered turn never retries, whatever the directive said.
        assert!(!autonomous_turn_should_retry(
            &[],
            true,
            Some(ConversationTurnDirective::Continue)
        ));
    }

    #[test]
    fn autonomous_delivery_boundary_decides_retry() {
        let receipt = crate::arbiter::ActionReceipt {
            action_id: None,
            idempotency_key: None,
            admitted_at: Utc::now(),
        };
        let deferred = ActionResult::Executed {
            receipt: receipt.clone(),
            outcome: ActionPortOutcome::Deferred {
                reason: "state_busy".to_owned(),
            },
        };
        let indeterminate = ActionResult::Executed {
            receipt,
            outcome: ActionPortOutcome::DeliveryIndeterminate {
                reason: "timeout".to_owned(),
                conversation_id: None,
            },
        };
        // Deferred never crossed the boundary: retry.
        assert!(autonomous_action_needs_retry(&[deferred]));
        // Indeterminate may already have reached the user: terminal.
        assert!(!autonomous_action_needs_retry(&[indeterminate]));
        assert!(!autonomous_action_needs_retry(&[ActionResult::Noop]));
    }

    #[test]
    fn resolved_directive_keeps_only_what_the_turn_earned() {
        assert_eq!(
            resolved_directive(true, Some(ConversationTurnDirective::Continue)),
            ConversationTurnDirective::Continue
        );
        assert_eq!(
            resolved_directive(false, Some(ConversationTurnDirective::Continue)),
            ConversationTurnDirective::Wait
        );
        assert_eq!(
            resolved_directive(false, Some(ConversationTurnDirective::End)),
            ConversationTurnDirective::End
        );
        assert_eq!(
            resolved_directive(true, None),
            ConversationTurnDirective::Wait
        );
    }

    #[derive(Debug, Clone, Copy)]
    struct ThreeStepModel;

    impl ModelBackend for ThreeStepModel {
        fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            Box::pin(async move {
                let conversation_id = input
                    .event
                    .scope()
                    .conversation_id()
                    .ok_or(ModelBackendError::Unavailable)?;
                match input.event.kind() {
                    WorldEventKind::MessageReceived(_) => Ok(DecisionPlan {
                        disposition: DecisionDisposition::Reply,
                        intents: vec![CognitiveIntent::UseTool {
                            tool_name: "step.one".to_owned(),
                            input: "{}".to_owned(),
                            scope: ActionScope::Conversation(conversation_id),
                            notification_policy: ToolNotificationPolicy::Final,
                        }],
                        state_updates: Vec::new(),
                    }),
                    WorldEventKind::ToolCompleted(tool) if tool.requires_follow_up => {
                        let next = if tool.operation == "step.one" {
                            CognitiveIntent::UseTool {
                                tool_name: "step.two".to_owned(),
                                input: "{}".to_owned(),
                                scope: ActionScope::Conversation(conversation_id),
                                notification_policy: ToolNotificationPolicy::Final,
                            }
                        } else {
                            CognitiveIntent::send_message(
                                conversation_id,
                                MessageContent::text("三步完成"),
                            )
                        };
                        Ok(DecisionPlan {
                            disposition: DecisionDisposition::Reply,
                            intents: vec![next],
                            state_updates: Vec::new(),
                        })
                    }
                    _ => Ok(DecisionPlan::silent()),
                }
            })
        }
    }

    /// An action port that completes tools and delivers messages immediately.
    #[derive(Debug, Default)]
    struct ImmediatePort;

    impl ActionPort for ImmediatePort {
        fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a> {
            Box::pin(async move {
                match action {
                    ProposedAction::UseTool(tool) => Ok(ActionPortOutcome::ToolCompleted {
                        operation: tool.tool_name.clone(),
                        output: "ok".to_owned(),
                    }),
                    ProposedAction::SendMessage(_) => Ok(ActionPortOutcome::Delivered {
                        external_reference: Some("cli-message:1".to_owned()),
                        message_id: None,
                        conversation_id: None,
                    }),
                    other => Err(ActionPortError::new(
                        format!("unsupported: {other:?}"),
                        false,
                    )),
                }
            })
        }
    }

    #[derive(Debug, Default)]
    struct Recorder {
        turns: Vec<TurnOutcome>,
        delivered: Vec<Vec<String>>,
        planner_errors: usize,
        turn_ends: usize,
        /// `should_process` is a shared query, so recording uses interior
        /// mutability exactly as a real host would.
        authorized: Mutex<Vec<bool>>,
        replying_allowed: bool,
    }

    impl CognitiveTurnObserver for Recorder {
        fn should_process(&self, event: &WorldEvent) -> bool {
            let authorized = match event.kind() {
                WorldEventKind::ToolCompleted(_) => self.replying_allowed,
                _ => true,
            };
            self.authorized
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(authorized);
            authorized
        }

        fn on_turn<'a>(
            &'a mut self,
            _event: &'a WorldEvent,
            report: &'a TurnReport,
        ) -> TurnHook<'a> {
            self.turns.push(report.outcome.clone());
            self.delivered.push(report.delivered_replies.clone());
            Box::pin(async {})
        }

        fn on_planner_error<'a>(
            &'a mut self,
            _event: &'a WorldEvent,
            _error: &'a PlannerError,
        ) -> TurnHook<'a> {
            self.planner_errors += 1;
            Box::pin(async {})
        }

        fn on_turn_end(&mut self) -> TurnHook<'_> {
            self.turn_ends += 1;
            Box::pin(async {})
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
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

    fn message_event(conversation_id: ConversationId) -> WorldEvent {
        let timestamp = Utc::now();
        WorldEvent::message_received(
            EventPriority::High,
            MessageReceivedEvent {
                message_id: MessageId::new(),
                conversation_id,
                sender: PersonId::new(),
                content: MessageContent::text("做三步"),
                reply_to: None,
                timestamp,
                conversation_kind: ConversationKind::Direct,
                addressed_to_agent: true,
                replies_to_agent: false,
                continuation_to_agent: false,
                stop_requested: false,
                explicit_request: false,
                visible_reply_allowed: true,
            },
        )
    }

    struct Fixture {
        runtime: CognitiveRuntime,
        arbiter: ActionArbiter,
        port: ImmediatePort,
        observer: Recorder,
    }

    fn fixture(replying_allowed: bool) -> Fixture {
        let conversation_id = ConversationId::new();
        let (handle, mut runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("runtime");
        // Installing services also installs the planner that shares their
        // model, so the fixture exercises the same wiring a host uses.
        runtime.install_services(CoreServices::with_model(ThreeStepModel));
        block_on(handle.submit(message_event(conversation_id))).expect("submit");
        Fixture {
            runtime,
            // The driver can only reach tools when the host declares the
            // capability; a default arbiter rejects every action.
            arbiter: ActionArbiter::new(ActionArbiterConfig {
                capabilities: EnvironmentCapabilities::all(),
                ..ActionArbiterConfig::default()
            }),
            port: ImmediatePort,
            observer: Recorder {
                replying_allowed,
                ..Recorder::default()
            },
        }
    }

    #[test]
    fn driver_carries_a_multi_step_tool_task_to_its_final_reply() {
        // The model only advances after each tool result reaches it, so this
        // asserts the loop really drove planning -> tool -> follow-up -> tool
        // -> follow-up -> delivery inside one `run` call.
        let mut fixture = fixture(true);
        let driven = block_on(drain(
            &mut fixture.runtime,
            &fixture.arbiter,
            &fixture.port,
            &mut fixture.observer,
        ));
        assert_eq!(driven, 3, "start, second tool, final reply");
        assert_eq!(fixture.observer.delivered.len(), 3);
        assert_eq!(
            fixture.observer.delivered.last().expect("final turn"),
            &vec!["三步完成".to_owned()]
        );
        assert_eq!(fixture.observer.planner_errors, 0);
        assert_eq!(fixture.observer.turn_ends, 3);
    }

    #[test]
    fn cancelled_turns_are_released_instead_of_planned() {
        // A superseded lease must not complete a turn: Core reports the
        // cancellation so the host releases the claim it carries. The first
        // turn is the message that was still authorized; the follow-up it
        // produced is the superseded one.
        let mut fixture = fixture(false);
        let driven = block_on(drain(
            &mut fixture.runtime,
            &fixture.arbiter,
            &fixture.port,
            &mut fixture.observer,
        ));
        assert_eq!(driven, 2);
        assert!(
            matches!(
                fixture.observer.turns.as_slice(),
                [TurnOutcome::Planned { .. }, TurnOutcome::Cancelled]
            ),
            "turns={:?}",
            fixture.observer.turns
        );
        assert_eq!(fixture.observer.delivered[0], Vec::<String>::new());
        // Core consults the guard before planning, after planning, and before
        // each dispatched intent; the assertions only care that the follow-up
        // was refused.
        let authorized = fixture
            .observer
            .authorized
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(authorized.first(), Some(&true));
        assert_eq!(authorized.last(), Some(&false));
    }

    #[test]
    fn bounded_run_yields_after_the_requested_turns() {
        let mut fixture = fixture(true);
        let driven = block_on(run_bounded(
            &mut fixture.runtime,
            &fixture.arbiter,
            &fixture.port,
            &mut fixture.observer,
            Some(1),
        ));
        assert_eq!(driven, 1);
        // The remaining follow-ups are still queued, so the host can resume.
        let resumed = block_on(drain(
            &mut fixture.runtime,
            &fixture.arbiter,
            &fixture.port,
            &mut fixture.observer,
        ));
        assert_eq!(resumed, 2);
        assert_eq!(fixture.observer.turn_ends, 3);
    }

    #[test]
    fn observed_mode_consumes_events_without_a_planner() {
        let conversation_id = ConversationId::new();
        let (handle, mut runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("runtime");
        block_on(handle.submit(message_event(conversation_id))).expect("submit");
        let mut observer = Recorder::default();
        let driven = block_on(drain_observed(&mut runtime, &mut observer));
        assert_eq!(driven, 1);
        assert!(matches!(
            observer.turns.as_slice(),
            [TurnOutcome::Planned { .. }]
        ));
        assert_eq!(observer.turn_ends, 1);
    }
}
