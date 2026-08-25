//! Best-effort projection of legacy Kovi activity into Yunxi Core events.

use crate::model::MessageDestination;
use chrono::Utc;
use kovi::tokio::time::timeout;
use std::time::Duration;
use yunxi_core::{
    Admission, EventPriority, EventScope, GoalCompletedEvent, GoalState, GoalUpdatedEvent,
    IdentityStore, InteractionCues, InteractionCuesObservedEvent, WorldEvent, WorldEventKind,
};

const RELIABLE_EVENT_TIMEOUT: Duration = Duration::from_millis(250);
const NORMAL_EVENT_TIMEOUT: Duration = Duration::from_millis(250);
const AGENT_GOAL_TIMEOUT: Duration = Duration::from_secs(1);
const AGENT_TASK_SOURCE_KIND: &str = "kovi_agent_task";

/// Project an event associated with an existing QQ destination. Identity and
/// admission failures are observations only: legacy execution remains the
/// source of truth throughout this migration phase.
pub(crate) async fn project_destination(
    destination: MessageDestination,
    priority: EventPriority,
    kind: WorldEventKind,
) {
    let wait = projection_timeout(priority);
    let Some(bridge) = super::SHADOW_BRIDGE.get() else {
        kovi::log::warn!("Yunxi event projection failed: shadow bridge is not installed");
        return;
    };
    match timeout(
        wait,
        bridge.project_destination(destination, priority, kind),
    )
    .await
    {
        Ok(Ok(Admission::Accepted)) => {}
        Ok(Ok(Admission::DroppedAtCapacity)) => {
            kovi::log::warn!("Yunxi event projection dropped at runtime capacity");
        }
        Ok(Err(error)) => kovi::log::warn!("Yunxi event projection failed: {error}"),
        Err(_) => kovi::log::warn!("Yunxi event projection timed out"),
    }
}

/// Project semantic evidence already produced by a legacy handler. The
/// conversion into [`InteractionCues`] happens at the semantic boundary; this
/// function only resolves the canonical Person and admits a bounded event.
pub(crate) fn project_interaction_cues(user_id: i64, cues: InteractionCues) {
    if !has_interaction_evidence(cues) {
        return;
    }
    kovi::tokio::spawn(async move {
        match timeout(
            NORMAL_EVENT_TIMEOUT,
            project_interaction_cues_inner(user_id, cues),
        )
        .await
        {
            Ok(Ok(Admission::Accepted)) => {}
            Ok(Ok(Admission::DroppedAtCapacity)) => {
                kovi::log::warn!("Yunxi interaction-cue projection dropped at runtime capacity");
            }
            Ok(Err(error)) => {
                kovi::log::warn!("Yunxi interaction-cue projection failed: {error}");
            }
            Err(_) => kovi::log::warn!("Yunxi interaction-cue projection timed out"),
        }
    });
}

fn has_interaction_evidence(cues: InteractionCues) -> bool {
    cues != InteractionCues::default()
}

async fn project_interaction_cues_inner(
    user_id: i64,
    cues: InteractionCues,
) -> Result<Admission, String> {
    let identities = super::IDENTITY_STORE
        .get()
        .ok_or_else(|| "identity store is not installed".to_string())?;
    let bridge = super::SHADOW_BRIDGE
        .get()
        .ok_or_else(|| "shadow bridge is not installed".to_string())?;
    let external = super::qq::person(user_id).map_err(|error| error.to_string())?;
    let person_id = identities
        .resolve_external_identity(&external)
        .await
        .map_err(|error| error.to_string())?;
    let observed =
        InteractionCuesObservedEvent::new(person_id, cues).map_err(|error| error.to_string())?;
    bridge
        .submit_event(WorldEvent::new(
            Utc::now(),
            EventScope::Person { person_id },
            EventPriority::Normal,
            WorldEventKind::InteractionCuesObserved(observed),
        ))
        .await
        .map_err(|error| error.to_string())
}

pub(crate) fn project_agent_task(
    task_id: i64,
    actor_user_id: i64,
    question: &str,
    target: GoalState,
) {
    let question = question.to_owned();
    kovi::tokio::spawn(async move {
        match timeout(
            AGENT_GOAL_TIMEOUT,
            project_agent_task_inner(task_id, actor_user_id, &question, target),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => kovi::log::warn!("Yunxi agent-task goal projection failed: {error}"),
            Err(_) => kovi::log::warn!("Yunxi agent-task goal projection timed out"),
        }
    });
}

async fn project_agent_task_inner(
    task_id: i64,
    actor_user_id: i64,
    question: &str,
    target: GoalState,
) -> Result<(), String> {
    if task_id <= 0 {
        return Err("agent task id is invalid".to_string());
    }
    let identities = super::IDENTITY_STORE
        .get()
        .ok_or_else(|| "identity store is not installed".to_string())?;
    let goals = super::GOAL_STORE
        .get()
        .ok_or_else(|| "goal store is not installed".to_string())?;
    let bridge = super::SHADOW_BRIDGE
        .get()
        .ok_or_else(|| "shadow bridge is not installed".to_string())?;
    let external = super::qq::person(actor_user_id).map_err(|error| error.to_string())?;
    let person_id = identities
        .resolve_external_identity(&external)
        .await
        .map_err(|error| error.to_string())?;
    let source_key = task_id.to_string();
    let mut goal = goals
        .get_or_create_external_person_goal(
            AGENT_TASK_SOURCE_KIND,
            &source_key,
            person_id,
            question,
        )
        .await
        .map_err(|error| error.to_string())?;
    if target != GoalState::Active {
        goal = goals
            .transition_external_goal(AGENT_TASK_SOURCE_KIND, &source_key, target)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "agent-task goal link disappeared".to_string())?;
    }
    let kind = if goal.state() == GoalState::Completed {
        WorldEventKind::GoalCompleted(GoalCompletedEvent { goal_id: goal.id() })
    } else {
        WorldEventKind::GoalUpdated(GoalUpdatedEvent { goal_id: goal.id() })
    };
    bridge
        .submit_event(WorldEvent::new(
            Utc::now(),
            EventScope::Goal { goal_id: goal.id() },
            EventPriority::High,
            kind,
        ))
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

const fn projection_timeout(priority: EventPriority) -> Duration {
    if priority.requires_backpressure() {
        RELIABLE_EVENT_TIMEOUT
    } else {
        NORMAL_EVENT_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yunxi_core::{ReminderDueEvent, ToolCompletedEvent};

    #[test]
    fn projected_payloads_remain_bounded_and_valid() {
        let reminder = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::High,
            WorldEventKind::ReminderDue(ReminderDueEvent {
                reference: "reminder:42".to_string(),
            }),
        );
        let tool = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::ToolCompleted(ToolCompletedEvent {
                operation: "weather.current".to_string(),
                output: String::new(),
                requires_follow_up: false,
            }),
        );

        assert!(reminder.validate(8).is_ok());
        assert!(tool.validate(8).is_ok());
    }

    #[test]
    fn confident_neutral_sentiment_is_still_meaningful_evidence() {
        assert!(!has_interaction_evidence(InteractionCues::default()));
        assert!(has_interaction_evidence(InteractionCues {
            sentiment_confidence: 0.9,
            ..InteractionCues::default()
        }));
    }
}
