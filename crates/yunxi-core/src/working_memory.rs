//! Bounded working memory for one in-flight task.
//!
//! A task that needs several tool rounds is driven by Core's loop, but each
//! round re-enters the planner with no record of the rounds before it: the
//! event carries only the newest result, and the conversation history carries
//! only what was said, not what was attempted. A model that already tried
//! three tools therefore cannot know it did, cannot avoid repeating a failed
//! call, and cannot tell a partial attempt from a fresh request.
//!
//! This module is that record. It is keyed by the trace root — one root event
//! is one task — and is bounded on every axis so a hostile or looping model
//! cannot grow it without limit. It is working state, never durable memory:
//! the runtime drops it when the task reaches a terminal state.

use serde::{Deserialize, Serialize};

use crate::arbiter::{ActionPortOutcome, ActionResult};
use crate::executive::{ExpectationStatus, ExpectedEventPattern};
use crate::intent::CognitiveIntent;

/// Maximum number of history entries kept per task — attempts and expectation
/// results together.
///
/// A trace is already capped at [`crate::MAX_TOOL_ACTIONS_PER_TRACE`] tool
/// actions; this is the smaller, model-visible window, keeping the newest
/// entries and dropping the oldest.
pub const MAX_WORKING_ENTRIES: usize = 16;
/// Maximum characters kept from a tool name.
pub const MAX_WORKING_TOOL_NAME_CHARS: usize = 128;
/// Maximum characters kept from the model's tool arguments.
pub const MAX_WORKING_ARGUMENT_CHARS: usize = 2_048;
/// Maximum characters kept from a tool result summary.
pub const MAX_WORKING_RESULT_CHARS: usize = 512;
/// Maximum characters kept from a failure category.
pub const MAX_WORKING_FAILURE_CHARS: usize = 256;
/// Maximum characters kept from a task's stated goal.
pub const MAX_WORKING_GOAL_CHARS: usize = 512;
/// Maximum characters kept from an expectation's description.
pub const MAX_WORKING_EXPECTATION_CHARS: usize = 256;

/// One recorded tool round inside a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingAttempt {
    tool: String,
    arguments: String,
    outcome: WorkingAttemptOutcome,
}

impl WorkingAttempt {
    #[must_use]
    pub fn new(
        tool: impl Into<String>,
        arguments: impl Into<String>,
        outcome: WorkingAttemptOutcome,
    ) -> Self {
        Self {
            tool: bounded(&tool.into(), MAX_WORKING_TOOL_NAME_CHARS),
            arguments: bounded(&arguments.into(), MAX_WORKING_ARGUMENT_CHARS),
            outcome,
        }
    }

    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    #[must_use]
    pub fn arguments(&self) -> &str {
        &self.arguments
    }

    #[must_use]
    pub const fn outcome(&self) -> &WorkingAttemptOutcome {
        &self.outcome
    }
}

/// How one tool attempt ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkingAttemptOutcome {
    /// The tool ran, with the bounded result the model may read.
    Succeeded {
        #[serde(default)]
        summary: String,
    },
    /// The tool ran and failed, with the host's failure category.
    Failed {
        category: String,
        #[serde(default)]
        detail: String,
    },
    /// An action Core observed for a tool intent that produced no tool result:
    /// the arbiter refused it, or the port answered with an unrelated outcome.
    /// Recorded as neither success nor failure, because neither happened.
    Refused { reason: String },
}

impl WorkingAttemptOutcome {
    /// A short, host- and model-readable description of how the attempt ended.
    ///
    /// This is what a follow-up round reads to tell a repeated failure from a
    /// fresh attempt, so it never reports more confidence than the core has.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Succeeded { summary } if summary.is_empty() => "succeeded".to_owned(),
            Self::Succeeded { summary } => format!("succeeded: {summary}"),
            Self::Failed { category, detail } if detail.is_empty() => {
                format!("failed: {category}")
            }
            Self::Failed { category, detail } => format!("failed: {category}: {detail}"),
            Self::Refused { reason } => format!("refused: {reason}"),
        }
    }

    /// Builds an outcome from the action result Core observed for `tool`.
    ///
    /// Every [`ActionResult`] variant maps to a definite answer, so a recorded
    /// attempt never claims an outcome that did not happen.
    #[must_use]
    pub fn from_result(tool: &str, result: &ActionResult) -> Self {
        match result {
            ActionResult::Executed {
                outcome: ActionPortOutcome::ToolCompleted { output, .. },
                ..
            } => Self::Succeeded {
                summary: bounded(output, MAX_WORKING_RESULT_CHARS),
            },
            ActionResult::Executed {
                outcome:
                    ActionPortOutcome::ToolFailed {
                        error_category,
                        detail,
                        ..
                    },
                ..
            } => Self::Failed {
                category: bounded(error_category, MAX_WORKING_FAILURE_CHARS),
                detail: bounded(detail, MAX_WORKING_RESULT_CHARS),
            },
            ActionResult::Failed { error, .. } => Self::Failed {
                category: bounded(&error.category, MAX_WORKING_FAILURE_CHARS),
                detail: if error.retryable {
                    "the host reported this failure as retryable".to_owned()
                } else {
                    String::new()
                },
            },
            ActionResult::Rejected(rejection) => Self::Refused {
                reason: bounded(&rejection.to_string(), MAX_WORKING_FAILURE_CHARS),
            },
            // A tool intent answered with something other than a tool outcome
            // is a host contract violation; recording it as success or failure
            // would invent a result the tool never produced.
            ActionResult::Executed { .. } => Self::Refused {
                reason: bounded(
                    &format!("tool `{tool}` returned a non-tool outcome"),
                    MAX_WORKING_FAILURE_CHARS,
                ),
            },
            ActionResult::Noop => Self::Refused {
                reason: "the action produced no result".to_owned(),
            },
        }
    }
}

/// One thing that happened to a task, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingEntry {
    /// Monotonic position within the task.
    ///
    /// Attempts and expectation results are recorded at different moments — an
    /// expectation resolves while the event that resolved it is being observed,
    /// which is *after* the round that produced it was dispatched — so the
    /// order of insertion is not the order of history. Sorting by this field
    /// restores it.
    pub sequence: u64,
    pub payload: WorkingEntryPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkingEntryPayload {
    /// A tool the task called, with its arguments and result.
    Attempt(WorkingAttempt),
    /// What an action's expectation turned out to be. This is the half of the
    /// loop that raw events cannot express: an event says what happened, an
    /// expectation result says whether what was supposed to happen did.
    Observation(WorkingObservation),
}

/// How one expectation ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingObservation {
    /// What the task expected to happen, as a short readable phrase.
    expected: String,
    outcome: WorkingObservationOutcome,
}

impl WorkingObservation {
    #[must_use]
    pub fn new(expected: impl Into<String>, outcome: WorkingObservationOutcome) -> Self {
        Self {
            expected: bounded(&expected.into(), MAX_WORKING_EXPECTATION_CHARS),
            outcome,
        }
    }

    #[must_use]
    pub fn expected(&self) -> &str {
        &self.expected
    }

    #[must_use]
    pub const fn outcome(&self) -> &WorkingObservationOutcome {
        &self.outcome
    }

    /// A short description of how the expectation ended.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.outcome {
            WorkingObservationOutcome::Satisfied => {
                format!("{}：如期发生", self.expected)
            }
            WorkingObservationOutcome::Expired => {
                format!("{}：**没有发生**", self.expected)
            }
            WorkingObservationOutcome::Violated => {
                format!("{}：被判定为没发生", self.expected)
            }
            WorkingObservationOutcome::Cancelled => {
                format!("{}：已作废", self.expected)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingObservationOutcome {
    Satisfied,
    Expired,
    Violated,
    Cancelled,
}

/// What one task has tried, and what came of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerWorkingMemory {
    /// What this task is for, as stated by the round that formed it.
    ///
    /// Held here rather than in the step log because it is not a step: it is the
    /// thing every step is measured against. The first statement wins, so a
    /// task's purpose cannot quietly change halfway through — a later round that
    /// wants a different outcome is a different task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    goal: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    entries: Vec<WorkingEntry>,
    #[serde(default, skip_serializing_if = "is_zero")]
    next_sequence: u64,
}

/// How many consecutive failures on the same call before the task is told it is
/// going in circles.
///
/// Three, not two: a retry after a transient failure is reasonable, and telling
/// her she is stuck after one retry would be crying wolf. Four or more means the
/// approach itself is not working.
pub const STUCK_FAILURE_RUN: usize = 3;

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl PlannerWorkingMemory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The task's history in order: attempts interleaved with what their
    /// expectations turned out to be.
    #[must_use]
    pub fn entries(&self) -> &[WorkingEntry] {
        &self.entries
    }

    /// The tool calls this task made, in order.
    pub fn attempts(&self) -> impl Iterator<Item = &WorkingAttempt> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                WorkingEntryPayload::Attempt(attempt) => Some(attempt),
                WorkingEntryPayload::Observation(_) => None,
            })
    }

    /// The expectation results this task learned, in order.
    pub fn observations(&self) -> impl Iterator<Item = &WorkingObservation> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                WorkingEntryPayload::Observation(observation) => Some(observation),
                WorkingEntryPayload::Attempt(_) => None,
            })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.goal.is_none()
    }

    /// Records what this task is for, once.
    ///
    /// Returns whether this call set it. A later statement does not overwrite
    /// the first, because a task whose purpose can be silently replaced cannot
    /// be measured against anything.
    pub fn set_goal_once(&mut self, goal: &str) -> bool {
        if self.goal.is_some() {
            return false;
        }
        let goal = bounded(goal, MAX_WORKING_GOAL_CHARS);
        let goal = goal.trim();
        if goal.is_empty() {
            return false;
        }
        self.goal = Some(goal.to_owned());
        true
    }

    /// What this task is for, when the forming round said.
    #[must_use]
    pub fn goal(&self) -> Option<&str> {
        self.goal.as_deref()
    }

    /// The trailing run of failures on one call, when it is long enough to
    /// mean the approach is not working.
    ///
    /// Counts attempts that ended in `Failed` or `Refused`: a refusal is the
    /// host saying no, which is just as much "this is not the way" as an error.
    /// A success anywhere in the run ends it — progress resets the count.
    #[must_use]
    pub fn stuck_on(&self) -> Option<(&str, usize)> {
        let mut run = 0_usize;
        let mut tool: Option<&str> = None;
        for entry in self.entries.iter().rev() {
            let WorkingEntryPayload::Attempt(attempt) = &entry.payload else {
                continue;
            };
            let failed = matches!(
                attempt.outcome(),
                WorkingAttemptOutcome::Failed { .. } | WorkingAttemptOutcome::Refused { .. }
            );
            if !failed {
                break;
            }
            match tool {
                Some(name) if name != attempt.tool() => break,
                None => tool = Some(attempt.tool()),
                Some(_) => {}
            }
            run += 1;
        }
        (run >= STUCK_FAILURE_RUN).then(|| (tool.unwrap_or_default(), run))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Records one round's tool attempts.
    pub fn record_round(&mut self, attempts: &[WorkingAttempt]) {
        for attempt in attempts {
            self.push(WorkingEntryPayload::Attempt(attempt.clone()));
        }
    }

    /// Records one resolved expectation.
    pub fn record_observation(&mut self, observation: WorkingObservation) {
        self.push(WorkingEntryPayload::Observation(observation));
    }

    /// Appends an entry, dropping the oldest when full.
    ///
    /// The newest entries matter most: the model is deciding what to do *next*,
    /// and a window that kept stale rounds while dropping the round it just ran
    /// would hide the evidence it needs.
    fn push(&mut self, payload: WorkingEntryPayload) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.entries.push(WorkingEntry { sequence, payload });
        self.entries.sort_by_key(|entry| entry.sequence);
        while self.entries.len() > MAX_WORKING_ENTRIES {
            self.entries.remove(0);
        }
    }
}

/// Describes an expectation in the words a follow-up round needs.
///
/// The model has to decide what to do next from this line alone, so it says
/// what was supposed to happen rather than naming an internal pattern.
#[must_use]
pub fn describe_expectation(pattern: &ExpectedEventPattern) -> String {
    match pattern {
        ExpectedEventPattern::EventType(event_type) => {
            format!("接下来应当出现 {event_type:?} 事件")
        }
        ExpectedEventPattern::MessageContains(needle) => {
            format!("接下来的消息里应当出现「{}」", bounded(needle, 120))
        }
        ExpectedEventPattern::ToolCompleted { operation } => {
            format!("工具 `{operation}` 应当执行成功")
        }
        ExpectedEventPattern::ToolFailed { operation } => {
            format!("工具 `{operation}` 应当失败")
        }
        ExpectedEventPattern::ActionSucceeded { idempotency_key } => {
            format!("动作 `{}` 应当成功", bounded(idempotency_key, 120))
        }
        ExpectedEventPattern::Custom(value) => {
            format!("应当发生「{}」", bounded(value, 120))
        }
    }
}

/// Maps a terminal expectation status onto a recorded observation.
#[must_use]
pub fn observation_outcome(status: ExpectationStatus) -> Option<WorkingObservationOutcome> {
    match status {
        ExpectationStatus::Satisfied => Some(WorkingObservationOutcome::Satisfied),
        ExpectationStatus::Expired => Some(WorkingObservationOutcome::Expired),
        ExpectationStatus::Violated => Some(WorkingObservationOutcome::Violated),
        ExpectationStatus::Cancelled => Some(WorkingObservationOutcome::Cancelled),
        ExpectationStatus::Pending => None,
    }
}

/// Maps one dispatched intent and its result into a recorded attempt.
///
/// Returns `None` for intents that are not tool calls: working memory tracks
/// what a task *tried with tools*, so recording messages, reminders, or goals
/// would mix transport bookkeeping into a reasoning trace.
#[must_use]
pub fn attempt_from_intent(
    intent: &CognitiveIntent,
    result: &ActionResult,
) -> Option<WorkingAttempt> {
    let CognitiveIntent::UseTool {
        tool_name, input, ..
    } = intent
    else {
        return None;
    };
    Some(WorkingAttempt::new(
        tool_name.clone(),
        input.clone(),
        WorkingAttemptOutcome::from_result(tool_name, result),
    ))
}

fn bounded(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbiter::{ActionPortError, ActionReceipt, ActionRejection};
    use chrono::Utc;

    fn receipt() -> ActionReceipt {
        ActionReceipt {
            action_id: None,
            idempotency_key: None,
            admitted_at: Utc::now(),
        }
    }

    #[test]
    fn every_action_result_maps_to_a_definite_outcome() {
        let completed = ActionResult::Executed {
            receipt: receipt(),
            outcome: ActionPortOutcome::ToolCompleted {
                operation: "web.search".to_owned(),
                output: "found 3 results".to_owned(),
            },
        };
        assert_eq!(
            WorkingAttemptOutcome::from_result("web.search", &completed),
            WorkingAttemptOutcome::Succeeded {
                summary: "found 3 results".to_owned()
            }
        );

        let failed = ActionResult::Failed {
            receipt: receipt(),
            error: ActionPortError::new("network", true),
        };
        assert_eq!(
            WorkingAttemptOutcome::from_result("web.search", &failed),
            WorkingAttemptOutcome::Failed {
                category: "network".to_owned(),
                detail: "the host reported this failure as retryable".to_owned(),
            }
        );

        let rejected = ActionResult::Rejected(ActionRejection::CapabilityUnavailable {
            action_id: None,
            capability: crate::ActionCapability::UseTool,
        });
        assert!(matches!(
            WorkingAttemptOutcome::from_result("web.search", &rejected),
            WorkingAttemptOutcome::Refused { .. }
        ));

        // A tool intent answered with a non-tool outcome is neither success
        // nor failure.
        let unrelated = ActionResult::Executed {
            receipt: receipt(),
            outcome: ActionPortOutcome::Delivered {
                external_reference: None,
                message_id: None,
                conversation_id: None,
            },
        };
        assert!(matches!(
            WorkingAttemptOutcome::from_result("web.search", &unrelated),
            WorkingAttemptOutcome::Refused { .. }
        ));
    }

    #[test]
    fn a_task_goal_is_set_once_and_never_silently_replaced() {
        let mut memory = PlannerWorkingMemory::new();
        assert!(memory.set_goal_once("查一下明天的天气"));
        assert_eq!(memory.goal(), Some("查一下明天的天气"));
        // A later round wanting a different outcome is a different task.
        assert!(!memory.set_goal_once("顺便订张票"));
        assert_eq!(memory.goal(), Some("查一下明天的天气"));
        // An empty statement is not a goal.
        let mut blank = PlannerWorkingMemory::new();
        assert!(!blank.set_goal_once("   "));
        assert_eq!(blank.goal(), None);
        assert!(blank.is_empty());
    }

    #[test]
    fn a_goal_alone_is_enough_to_have_something_to_show() {
        let mut memory = PlannerWorkingMemory::new();
        memory.set_goal_once("把这件事问清楚");
        assert!(!memory.is_empty(), "a stated goal is history worth showing");
        assert_eq!(memory.len(), 0, "but it is not a step");
    }

    #[test]
    fn going_in_circles_is_noticed_only_after_a_real_run() {
        let failed = |tool: &str| {
            WorkingAttempt::new(
                tool,
                "{}",
                WorkingAttemptOutcome::Failed {
                    category: "network".to_owned(),
                    detail: String::new(),
                },
            )
        };
        let ok = |tool: &str| {
            WorkingAttempt::new(
                tool,
                "{}",
                WorkingAttemptOutcome::Succeeded {
                    summary: "ok".to_owned(),
                },
            )
        };

        // One retry after a failure is reasonable, not "stuck".
        let mut memory = PlannerWorkingMemory::new();
        memory.record_round(&[failed("web.search"), failed("web.search")]);
        assert_eq!(memory.stuck_on(), None, "two failures is still a retry");

        memory.record_round(&[failed("web.search")]);
        assert_eq!(memory.stuck_on(), Some(("web.search", 3)));

        // Progress resets it.
        memory.record_round(&[ok("web.search")]);
        assert_eq!(memory.stuck_on(), None);

        // A different tool is a different approach, not a repeat.
        let mut switching = PlannerWorkingMemory::new();
        switching.record_round(&[failed("web.search"), failed("weather.current")]);
        switching.record_round(&[failed("web.search")]);
        assert_eq!(
            switching.stuck_on(),
            None,
            "alternating tools is not repeating one call"
        );

        // A refusal is the host saying no, which is just as much "not this way".
        let refused = |tool: &str| {
            WorkingAttempt::new(
                tool,
                "{}",
                WorkingAttemptOutcome::Refused {
                    reason: "capability unavailable".to_owned(),
                },
            )
        };
        let mut refused_run = PlannerWorkingMemory::new();
        refused_run.record_round(&[refused("group.send")]);
        refused_run.record_round(&[refused("group.send")]);
        refused_run.record_round(&[refused("group.send")]);
        assert_eq!(refused_run.stuck_on(), Some(("group.send", 3)));
    }

    #[test]
    fn recording_keeps_the_newest_attempts() {
        let mut memory = PlannerWorkingMemory::new();
        for index in 0..(MAX_WORKING_ENTRIES + 3) {
            memory.record_round(&[WorkingAttempt::new(
                format!("tool.{index}"),
                "{}",
                WorkingAttemptOutcome::Succeeded {
                    summary: "ok".to_owned(),
                },
            )]);
        }
        assert_eq!(memory.len(), MAX_WORKING_ENTRIES);
        assert_eq!(
            memory.attempts().next().expect("oldest kept").tool(),
            "tool.3"
        );
        assert_eq!(
            memory.attempts().last().expect("newest").tool(),
            format!("tool.{}", MAX_WORKING_ENTRIES + 2)
        );
    }

    #[test]
    fn observation_results_land_after_the_round_they_belong_to() {
        // The attempt is recorded while the round dispatches; the expectation
        // resolves later, when the event that settles it is observed. History
        // must still read in the order things happened.
        let mut memory = PlannerWorkingMemory::new();
        memory.record_round(&[WorkingAttempt::new(
            "web.search",
            "{}",
            WorkingAttemptOutcome::Succeeded {
                summary: "ok".to_owned(),
            },
        )]);
        memory.record_observation(WorkingObservation::new(
            "工具 `web.search` 应当执行成功",
            WorkingObservationOutcome::Expired,
        ));
        let described: Vec<String> = memory
            .entries()
            .iter()
            .map(|entry| match &entry.payload {
                WorkingEntryPayload::Attempt(attempt) => attempt.tool().to_owned(),
                WorkingEntryPayload::Observation(observation) => observation.describe(),
            })
            .collect();
        assert_eq!(described.len(), 2);
        assert_eq!(described[0], "web.search");
        assert!(described[1].contains("没有发生"));
    }

    #[test]
    fn an_empty_round_never_grows_the_memory() {
        let mut memory = PlannerWorkingMemory::new();
        memory.record_round(&[]);
        assert!(memory.is_empty());
    }

    #[test]
    fn bounded_fields_never_split_a_character() {
        let attempt = WorkingAttempt::new(
            "工具".repeat(200),
            "参数".repeat(2_000),
            WorkingAttemptOutcome::Succeeded {
                summary: "结果".repeat(500),
            },
        );
        assert!(attempt.tool().chars().count() <= MAX_WORKING_TOOL_NAME_CHARS);
        assert!(attempt.arguments().chars().count() <= MAX_WORKING_ARGUMENT_CHARS);
        // The value must stay valid UTF-8 after truncation.
        assert!(
            attempt
                .tool()
                .chars()
                .all(|character| character == '工' || character == '具')
        );
    }

    #[test]
    fn only_tool_intents_become_attempts() {
        let result = ActionResult::Noop;
        let send = CognitiveIntent::send_message(
            crate::ConversationId::new(),
            crate::event::MessageContent::text("hi"),
        );
        assert!(attempt_from_intent(&send, &result).is_none());

        let use_tool = CognitiveIntent::UseTool {
            tool_name: "time.now".to_owned(),
            input: "{}".to_owned(),
            scope: crate::action::ActionScope::Person(crate::PersonId::new()),
            notification_policy: crate::intent::ToolNotificationPolicy::Final,
        };
        let attempt = attempt_from_intent(&use_tool, &result).expect("attempt");
        assert_eq!(attempt.tool(), "time.now");
    }
}
