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
            WorldEventKind::InteractionCuesObserved(_) => AttentionResult {
                disposition: AttentionDisposition::Attend,
                reason: AttentionReason::RelevantEvent,
                salience: 60,
            },
            WorldEventKind::ToolCompleted(_)
            | WorldEventKind::ToolFailed(_)
            | WorldEventKind::ReminderDue(_)
            | WorldEventKind::GoalCompleted(_) => must_handle(AttentionReason::ReliableTask),
            WorldEventKind::ProspectiveMemoryDue(_) => AttentionResult {
                disposition: AttentionDisposition::Attend,
                reason: AttentionReason::ProspectiveMemory,
                salience: 80,
            },
            // 通话结束。只有一种情况需要她**做点什么**：她自己拨出去、对方没接——
            // 那时正确的收尾往往是改发一条消息。别人打来的电话结束了、或者话已经
            // 通过，记下来就够了，不该为它花一次规划（她刚说完话，没什么可补的）。
            // 这里必须是 `ObserveOnly` 而不是 `Attend`：`should_invoke_planner` 对
            // `Attend` 也返回真，用它会让她在**每一通电话之后**都多跑一轮模型。
            // `ObserveOnly` 仍然把事件记进有界工作状态（"刚打过"她记得住），
            // 只是不为它花钱规划。
            WorldEventKind::CallEnded(call) => {
                if call.initiated_by_self && call.outcome == crate::CallOutcome::Unanswered {
                    must_handle(AttentionReason::ReliableTask)
                } else {
                    AttentionResult {
                        disposition: AttentionDisposition::ObserveOnly,
                        reason: AttentionReason::RelevantEvent,
                        salience: 40,
                    }
                }
            }
            WorldEventKind::AutonomousConversationTick(_) => AttentionResult {
                disposition: AttentionDisposition::Attend,
                reason: AttentionReason::RelevantEvent,
                salience: 55,
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
        AutonomousConversationTickEvent, CallEndedEvent, CallOutcome, EventPriority, EventScope,
        InteractionCuesObservedEvent, MessageContent, MessageReceivedEvent, ProspectiveMemoryEvent,
        ReminderDueEvent, ToolCompletedEvent, ToolFailedEvent, WorldEvent, WorldEventKind,
    };
    use crate::identity::{ConversationId, ConversationKind, MessageId, PersonId};
    use crate::planner::InteractionCues;
    use chrono::Utc;

    fn call_ended(initiated_by_self: bool, outcome: CallOutcome) -> WorldEvent {
        let peer = PersonId::new();
        WorldEvent::new(
            Utc::now(),
            EventScope::Person { person_id: peer },
            EventPriority::Normal,
            WorldEventKind::CallEnded(CallEndedEvent {
                peer,
                initiated_by_self,
                outcome,
                duration_secs: 30,
            }),
        )
    }

    /// 只有"自己拨出去、没接通"这一种通话结局值得花一轮规划。
    ///
    /// 判据错了的代价很具体：如果所有通话结束都 MustHandle，她会在**每通电话之后**
    /// 都多跑一次模型（刚说完话，没什么可补的），白花钱且容易生成废话；反过来如果
    /// 一律不处理，外呼没人接就没有任何收尾，那通电话等于白打。
    #[test]
    fn only_an_unanswered_self_initiated_call_requires_action() {
        let system = AttentionSystem;

        let follow_up = system.evaluate(&call_ended(true, CallOutcome::Unanswered));
        assert!(follow_up.should_invoke_planner());
        assert_eq!(follow_up.disposition, AttentionDisposition::MustHandle);

        for quiet in [
            call_ended(true, CallOutcome::Completed),
            call_ended(false, CallOutcome::Completed),
            call_ended(false, CallOutcome::Unanswered),
            call_ended(false, CallOutcome::Refused),
        ] {
            let result = system.evaluate(&quiet);
            assert!(
                !result.should_invoke_planner(),
                "这种通话结局不该占用一次规划：{:?}",
                quiet.kind()
            );
        }
    }

    /// 结局的取值要稳定：它进日志、进事件载荷，改名等于改口径。
    #[test]
    fn call_outcomes_have_stable_names() {
        for (outcome, name) in [
            (CallOutcome::Completed, "completed"),
            (CallOutcome::Unanswered, "unanswered"),
            (CallOutcome::Refused, "refused"),
        ] {
            assert_eq!(outcome.as_str(), name);
            let encoded = serde_json::to_string(&outcome).expect("serialize outcome");
            assert_eq!(encoded, format!("\"{name}\""));
        }
    }

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
                continuation_to_agent: false,
                stop_requested: false,
                explicit_request: false,
                visible_reply_allowed: true,
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
    fn semantic_cue_events_are_attended() {
        let person_id = PersonId::new();
        let observed = InteractionCuesObservedEvent::new(
            person_id,
            InteractionCues {
                sentiment_confidence: 0.8,
                ..InteractionCues::default()
            },
        )
        .expect("bounded cues");
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Person { person_id },
            EventPriority::Normal,
            WorldEventKind::InteractionCuesObserved(observed),
        );

        let result = AttentionSystem.evaluate(&event);
        assert_eq!(result.disposition, AttentionDisposition::Attend);
        assert_eq!(result.reason, AttentionReason::RelevantEvent);
        assert!(result.should_invoke_planner());
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
                output: String::new(),
                requires_follow_up: false,
            }),
        );
        let failed = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::ToolFailed(ToolFailedEvent {
                operation: "weather.lookup".to_owned(),
                error_category: "upstream_timeout".to_owned(),
                detail: String::new(),
                requires_follow_up: false,
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

    #[test]
    fn autonomous_conversation_ticks_invoke_the_planner() {
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Conversation {
                conversation_id: ConversationId::new(),
            },
            EventPriority::Low,
            WorldEventKind::AutonomousConversationTick(AutonomousConversationTickEvent::default()),
        );

        let result = AttentionSystem.evaluate(&event);
        assert_eq!(result.disposition, AttentionDisposition::Attend);
        assert_eq!(result.reason, AttentionReason::RelevantEvent);
        assert!(result.should_invoke_planner());
    }
}
