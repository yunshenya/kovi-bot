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
use crate::intent::CognitiveIntent;

/// Maximum number of recorded attempts kept per task.
///
/// A trace is already capped at [`crate::MAX_TOOL_ACTIONS_PER_TRACE`] tool
/// actions; this is the smaller, model-visible window, keeping the newest
/// entries and dropping the oldest.
pub const MAX_WORKING_ATTEMPTS: usize = 16;
/// Maximum characters kept from a tool name.
pub const MAX_WORKING_TOOL_NAME_CHARS: usize = 128;
/// Maximum characters kept from the model's tool arguments.
pub const MAX_WORKING_ARGUMENT_CHARS: usize = 2_048;
/// Maximum characters kept from a tool result summary.
pub const MAX_WORKING_RESULT_CHARS: usize = 512;
/// Maximum characters kept from a failure category.
pub const MAX_WORKING_FAILURE_CHARS: usize = 256;

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

/// What one task has tried so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerWorkingMemory {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attempts: Vec<WorkingAttempt>,
}

impl PlannerWorkingMemory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn attempts(&self) -> &[WorkingAttempt] {
        &self.attempts
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attempts.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.attempts.len()
    }

    /// Records one round's attempts, dropping the oldest when full.
    ///
    /// The newest attempts matter most: the model is deciding what to do
    /// *next*, and a window that kept stale rounds while dropping the round it
    /// just ran would hide the evidence it needs.
    pub fn record_round(&mut self, attempts: &[WorkingAttempt]) {
        if attempts.is_empty() {
            return;
        }
        for attempt in attempts {
            if self.attempts.len() >= MAX_WORKING_ATTEMPTS {
                self.attempts.remove(0);
            }
            self.attempts.push(attempt.clone());
        }
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
    fn recording_keeps_the_newest_attempts() {
        let mut memory = PlannerWorkingMemory::new();
        for index in 0..(MAX_WORKING_ATTEMPTS + 3) {
            memory.record_round(&[WorkingAttempt::new(
                format!("tool.{index}"),
                "{}",
                WorkingAttemptOutcome::Succeeded {
                    summary: "ok".to_owned(),
                },
            )]);
        }
        assert_eq!(memory.len(), MAX_WORKING_ATTEMPTS);
        assert_eq!(
            memory.attempts().first().expect("oldest kept").tool(),
            "tool.3"
        );
        assert_eq!(
            memory.attempts().last().expect("newest").tool(),
            format!("tool.{}", MAX_WORKING_ATTEMPTS + 2)
        );
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
