use crate::event::{EventPriority, WorldEvent, WorldEventKind};
use crate::identity::ConversationKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionDisposition {
    Ignore,
    ObserveOnly,
    Attend,
    MustHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    DirectConversation,
    AddressedToAgent,
    ReplyToAgent,
    StopRequested,
    ExplicitRequest,
    ReliableTask,
    ProspectiveMemory,
    CriticalEvent,
    RelevantEvent,
    BackgroundObservation,
    Maintenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionResult {
    pub disposition: AttentionDisposition,
    pub reason: AttentionReason,
    pub salience: u8,
}

impl AttentionResult {
    /// Whether this observation is important enough to spend a planner turn.
    /// Ignore and observe-only events still update bounded working state, but
    /// are intentionally handled entirely by the Rust runtime.
    #[must_use]
    pub const fn should_invoke_planner(self) -> bool {
        matches!(
            self.disposition,
            AttentionDisposition::Attend | AttentionDisposition::MustHandle
        )
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AttentionSystem;

impl AttentionSystem {
    #[must_use]
    pub fn evaluate(&self, event: &WorldEvent) -> AttentionResult {
        if event.priority() == EventPriority::Critical {
            return must_handle(AttentionReason::CriticalEvent);
        }

        match event.kind() {
            WorldEventKind::MessageReceived(message) => {
                if message.conversation_kind == ConversationKind::Direct {
                    must_handle(AttentionReason::DirectConversation)
                } else if message.stop_requested {
                    must_handle(AttentionReason::StopRequested)
                } else if message.replies_to_agent {
                    must_handle(AttentionReason::ReplyToAgent)
                } else if message.addressed_to_agent {
                    must_handle(AttentionReason::AddressedToAgent)
                } else if message.explicit_request {
                    must_handle(AttentionReason::ExplicitRequest)
                } else if event.priority() == EventPriority::High {
                    AttentionResult {
                        disposition: AttentionDisposition::Attend,
                        reason: AttentionReason::RelevantEvent,
                        salience: 70,
                    }
                } else {
                    AttentionResult {
                        disposition: AttentionDisposition::ObserveOnly,
                        reason: AttentionReason::BackgroundObservation,
                        salience: 20,
                    }
                }
            }
            WorldEventKind::ToolCompleted(_)
            | WorldEventKind::ToolFailed(_)
            | WorldEventKind::ReminderDue(_)
            | WorldEventKind::GoalCompleted(_) => must_handle(AttentionReason::ReliableTask),
            WorldEventKind::ProspectiveMemoryDue(_) => AttentionResult {
                disposition: AttentionDisposition::Attend,
                reason: AttentionReason::ProspectiveMemory,
                salience: 80,
            },
            WorldEventKind::MaintenanceTick => AttentionResult {
                disposition: AttentionDisposition::Ignore,
                reason: AttentionReason::Maintenance,
                salience: 0,
            },
            WorldEventKind::IdleTick => AttentionResult {
                disposition: AttentionDisposition::ObserveOnly,
                reason: AttentionReason::BackgroundObservation,
                salience: 10,
            },
            _ if event.priority() == EventPriority::High => AttentionResult {
                disposition: AttentionDisposition::Attend,
                reason: AttentionReason::RelevantEvent,
                salience: 70,
            },
            _ => AttentionResult {
                disposition: AttentionDisposition::ObserveOnly,
                reason: AttentionReason::BackgroundObservation,
                salience: 30,
            },
        }
    }
}

const fn must_handle(reason: AttentionReason) -> AttentionResult {
    AttentionResult {
        disposition: AttentionDisposition::MustHandle,
        reason,
        salience: 100,
    }
}

#[cfg(test)]
mod tests {
    use super::{AttentionDisposition, AttentionReason, AttentionSystem};
    use crate::event::{
        EventPriority, EventScope, MessageContent, MessageReceivedEvent, ProspectiveMemoryEvent,
        ReminderDueEvent, ToolCompletedEvent, ToolFailedEvent, WorldEvent, WorldEventKind,
    };
    use crate::identity::{ConversationId, ConversationKind, MessageId, PersonId};
    use chrono::Utc;

    fn message(kind: ConversationKind, addressed: bool, replied: bool) -> WorldEvent {
        WorldEvent::message_received(
            EventPriority::Normal,
            MessageReceivedEvent {
                message_id: MessageId::new(),
                conversation_id: ConversationId::new(),
                sender: PersonId::new(),
                content: MessageContent::text("hello"),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind: kind,
                addressed_to_agent: addressed,
                replies_to_agent: replied,
                stop_requested: false,
                explicit_request: false,
            },
        )
    }

    #[test]
    fn direct_and_addressed_messages_must_be_handled() {
        let attention = AttentionSystem;
        let direct = attention.evaluate(&message(ConversationKind::Direct, false, false));
        let addressed = attention.evaluate(&message(ConversationKind::Group, true, false));
        let replied = attention.evaluate(&message(ConversationKind::Group, false, true));

        assert_eq!(direct.disposition, AttentionDisposition::MustHandle);
        assert_eq!(direct.reason, AttentionReason::DirectConversation);
        assert_eq!(addressed.disposition, AttentionDisposition::MustHandle);
        assert_eq!(replied.disposition, AttentionDisposition::MustHandle);
    }

    #[test]
    fn ordinary_group_message_is_observation_only() {
        let result = AttentionSystem.evaluate(&message(ConversationKind::Group, false, false));

        assert_eq!(result.disposition, AttentionDisposition::ObserveOnly);
        assert_eq!(result.reason, AttentionReason::BackgroundObservation);
        assert!(!result.should_invoke_planner());
    }

    #[test]
    fn high_priority_group_message_is_attended_without_becoming_mandatory() {
        let message = match message(ConversationKind::Group, false, false)
            .kind()
            .clone()
        {
            WorldEventKind::MessageReceived(message) => message,
            _ => unreachable!("helper always creates a received message"),
        };
        let result =
            AttentionSystem.evaluate(&WorldEvent::message_received(EventPriority::High, message));

        assert_eq!(result.disposition, AttentionDisposition::Attend);
        assert_eq!(result.reason, AttentionReason::RelevantEvent);
        assert!(result.should_invoke_planner());
    }

    #[test]
    fn stop_requests_and_reliable_tasks_must_be_handled() {
        let stop = match message(ConversationKind::Group, false, false)
            .kind()
            .clone()
        {
            WorldEventKind::MessageReceived(mut message) => {
                message.stop_requested = true;
                WorldEvent::message_received(EventPriority::Normal, message)
            }
            _ => unreachable!("helper always creates a received message"),
        };
        let reminder = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::ReminderDue(ReminderDueEvent {
                reference: "reminder".to_string(),
            }),
        );

        assert_eq!(
            AttentionSystem.evaluate(&stop).disposition,
            AttentionDisposition::MustHandle
        );
        assert_eq!(
            AttentionSystem.evaluate(&reminder).disposition,
            AttentionDisposition::MustHandle
        );
    }

    #[test]
    fn normal_priority_tool_results_are_must_handle() {
        let completed = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::ToolCompleted(ToolCompletedEvent {
                operation: "weather.lookup".to_owned(),
            }),
        );
        let failed = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::ToolFailed(ToolFailedEvent {
                operation: "weather.lookup".to_owned(),
                error_category: "upstream_timeout".to_owned(),
            }),
        );

        for event in [&completed, &failed] {
            let result = AttentionSystem.evaluate(event);
            assert_eq!(result.disposition, AttentionDisposition::MustHandle);
            assert_eq!(result.reason, AttentionReason::ReliableTask);
            assert!(result.should_invoke_planner());
        }
    }

    #[test]
    fn prospective_memory_is_attended_without_implying_delivery() {
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::High,
            WorldEventKind::ProspectiveMemoryDue(ProspectiveMemoryEvent {
                open_loop_id: crate::OpenLoopId::new(),
            }),
        );
        let result = AttentionSystem.evaluate(&event);
        assert_eq!(result.disposition, AttentionDisposition::Attend);
        assert_eq!(result.reason, AttentionReason::ProspectiveMemory);
    }
}
