//! Platform-neutral cognitive intents.

use crate::action::{
    ActionScope, ActionValidationError, CancelGoalAction, CreateOpenLoopAction, ProposedAction,
    ResolveOpenLoopAction, SendMessageAction, StartGoalAction, ToolAction,
};
use crate::goal::{GoalDraft, GoalOwner};
use crate::open_loop::{OpenLoopDraft, OpenLoopOwner};
use crate::proactive::{ProactiveValidationError, ReachOutIntent};
use crate::{ConversationId, GoalId, MessageContent, MessageId, OpenLoopId};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolNotificationPolicy {
    #[default]
    Final,
    Each,
    EachAndFinal,
}

/// A high-level thing Yunxi wants to accomplish.  Intents do not imply that a
/// delivery channel exists; conversion into a proposed action is still
/// subject to host capabilities and arbiter policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum CognitiveIntent {
    SendMessage {
        conversation_id: ConversationId,
        content: MessageContent,
        reply_to: Option<MessageId>,
    },
    ReachOut(ReachOutIntent),
    UseTool {
        tool_name: String,
        input: String,
        scope: ActionScope,
        #[serde(default)]
        notification_policy: ToolNotificationPolicy,
    },
    CreateOpenLoop(OpenLoopDraft),
    ResolveOpenLoop {
        open_loop_id: OpenLoopId,
        owner: OpenLoopOwner,
    },
    StartGoal(GoalDraft),
    CancelGoal {
        goal_id: GoalId,
        owner: GoalOwner,
    },
    Noop,
}

impl<'de> Deserialize<'de> for CognitiveIntent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "type", content = "payload", rename_all = "snake_case")]
        #[serde(deny_unknown_fields)]
        enum Wire {
            SendMessage {
                conversation_id: ConversationId,
                content: MessageContent,
                reply_to: Option<MessageId>,
            },
            ReachOut(ReachOutIntent),
            UseTool {
                tool_name: String,
                input: String,
                scope: ActionScope,
                #[serde(default)]
                notification_policy: ToolNotificationPolicy,
            },
            CreateOpenLoop(OpenLoopDraft),
            ResolveOpenLoop {
                open_loop_id: OpenLoopId,
                owner: OpenLoopOwner,
            },
            StartGoal(GoalDraft),
            CancelGoal {
                goal_id: GoalId,
                owner: GoalOwner,
            },
            Noop,
        }

        let intent = match Wire::deserialize(deserializer)? {
            Wire::SendMessage {
                conversation_id,
                content,
                reply_to,
            } => Self::SendMessage {
                conversation_id,
                content,
                reply_to,
            },
            Wire::ReachOut(intent) => Self::ReachOut(intent),
            Wire::UseTool {
                tool_name,
                input,
                scope,
                notification_policy,
            } => Self::UseTool {
                tool_name,
                input,
                scope,
                notification_policy,
            },
            Wire::CreateOpenLoop(draft) => Self::CreateOpenLoop(draft),
            Wire::ResolveOpenLoop {
                open_loop_id,
                owner,
            } => Self::ResolveOpenLoop {
                open_loop_id,
                owner,
            },
            Wire::StartGoal(draft) => Self::StartGoal(draft),
            Wire::CancelGoal { goal_id, owner } => Self::CancelGoal { goal_id, owner },
            Wire::Noop => Self::Noop,
        };
        intent.validate().map_err(serde::de::Error::custom)?;
        Ok(intent)
    }
}

impl CognitiveIntent {
    pub fn send_message(conversation_id: ConversationId, content: MessageContent) -> Self {
        Self::SendMessage {
            conversation_id,
            content,
            reply_to: None,
        }
    }

    #[must_use]
    pub fn respond_to(
        conversation_id: ConversationId,
        content: MessageContent,
        reply_to: Option<MessageId>,
    ) -> Self {
        Self::SendMessage {
            conversation_id,
            content,
            reply_to,
        }
    }

    #[must_use]
    pub fn reach_out(intent: ReachOutIntent) -> Self {
        Self::ReachOut(intent)
    }

    #[must_use]
    pub fn use_tool(
        tool_name: impl Into<String>,
        input: impl Into<String>,
        scope: ActionScope,
    ) -> Self {
        Self::use_tool_with_notification_policy(
            tool_name,
            input,
            scope,
            ToolNotificationPolicy::Final,
        )
    }

    #[must_use]
    pub fn use_tool_with_notification_policy(
        tool_name: impl Into<String>,
        input: impl Into<String>,
        scope: ActionScope,
        notification_policy: ToolNotificationPolicy,
    ) -> Self {
        Self::UseTool {
            tool_name: tool_name.into(),
            input: input.into(),
            scope,
            notification_policy,
        }
    }

    #[must_use]
    pub fn create_open_loop(draft: OpenLoopDraft) -> Self {
        Self::CreateOpenLoop(draft)
    }

    #[must_use]
    pub const fn resolve_open_loop(open_loop_id: OpenLoopId, owner: OpenLoopOwner) -> Self {
        Self::ResolveOpenLoop {
            open_loop_id,
            owner,
        }
    }

    #[must_use]
    pub fn start_goal(draft: GoalDraft) -> Self {
        Self::StartGoal(draft)
    }

    #[must_use]
    pub const fn cancel_goal(goal_id: GoalId, owner: GoalOwner) -> Self {
        Self::CancelGoal { goal_id, owner }
    }

    #[must_use]
    pub const fn noop() -> Self {
        Self::Noop
    }

    pub fn validate(&self) -> Result<(), IntentValidationError> {
        match self {
            Self::SendMessage {
                conversation_id,
                content,
                reply_to,
            } => SendMessageAction::new(*conversation_id, content.clone())
                .map(|action| action.with_reply_to(*reply_to))
                .map(|_| ())
                .map_err(IntentValidationError::Action),
            Self::ReachOut(intent) => intent.validate().map_err(IntentValidationError::Proactive),
            Self::UseTool {
                tool_name,
                input,
                scope,
                ..
            } => ToolAction::new(tool_name.clone(), input.clone(), *scope)
                .map(|_| ())
                .map_err(IntentValidationError::Action),
            Self::CreateOpenLoop(draft) => CreateOpenLoopAction::new(draft.clone())
                .map(|_| ())
                .map_err(IntentValidationError::Action),
            Self::ResolveOpenLoop {
                open_loop_id,
                owner,
            } => ResolveOpenLoopAction::new(*open_loop_id, *owner)
                .map(|_| ())
                .map_err(IntentValidationError::Action),
            Self::StartGoal(draft) => StartGoalAction::new(draft.clone())
                .map(|_| ())
                .map_err(IntentValidationError::Action),
            Self::CancelGoal { goal_id, owner } => CancelGoalAction::new(*goal_id, *owner)
                .map(|_| ())
                .map_err(IntentValidationError::Action),
            Self::Noop => Ok(()),
        }
    }

    pub fn propose_action(&self) -> Result<ProposedAction, IntentValidationError> {
        self.validate()?;
        match self {
            Self::SendMessage {
                conversation_id,
                content,
                reply_to,
            } => SendMessageAction::new(*conversation_id, content.clone())
                .map(|action| ProposedAction::SendMessage(action.with_reply_to(*reply_to)))
                .map_err(IntentValidationError::Action),
            Self::ReachOut(intent) => crate::ReachOutAction::from_intent(intent.clone())
                .map(ProposedAction::ReachOut)
                .map_err(IntentValidationError::Action),
            Self::UseTool {
                tool_name,
                input,
                scope,
                ..
            } => ToolAction::new(tool_name.clone(), input.clone(), *scope)
                .map(ProposedAction::UseTool)
                .map_err(IntentValidationError::Action),
            Self::CreateOpenLoop(draft) => CreateOpenLoopAction::new(draft.clone())
                .map(ProposedAction::CreateOpenLoop)
                .map_err(IntentValidationError::Action),
            Self::ResolveOpenLoop {
                open_loop_id,
                owner,
            } => ResolveOpenLoopAction::new(*open_loop_id, *owner)
                .map(ProposedAction::ResolveOpenLoop)
                .map_err(IntentValidationError::Action),
            Self::StartGoal(draft) => StartGoalAction::new(draft.clone())
                .map(ProposedAction::StartGoal)
                .map_err(IntentValidationError::Action),
            Self::CancelGoal { goal_id, owner } => CancelGoalAction::new(*goal_id, *owner)
                .map(ProposedAction::CancelGoal)
                .map_err(IntentValidationError::Action),
            Self::Noop => Ok(ProposedAction::Noop),
        }
    }

    #[must_use]
    pub const fn action_scope(&self) -> ActionScope {
        match self {
            Self::SendMessage {
                conversation_id, ..
            } => ActionScope::Conversation(*conversation_id),
            Self::ReachOut(intent) => ActionScope::Person(intent.person_id()),
            Self::UseTool { scope, .. } => *scope,
            Self::CreateOpenLoop(draft) => ActionScope::for_open_loop_owner(draft.owner()),
            Self::ResolveOpenLoop { owner, .. } => ActionScope::for_open_loop_owner(*owner),
            Self::StartGoal(draft) => ActionScope::for_goal_owner(draft.owner()),
            Self::CancelGoal { owner, .. } => ActionScope::for_goal_owner(*owner),
            Self::Noop => ActionScope::Global,
        }
    }

    #[must_use]
    pub const fn tool_notification_policy(&self) -> Option<ToolNotificationPolicy> {
        match self {
            Self::UseTool {
                notification_policy,
                ..
            } => Some(*notification_policy),
            _ => None,
        }
    }
}

impl TryFrom<CognitiveIntent> for ProposedAction {
    type Error = IntentValidationError;

    fn try_from(intent: CognitiveIntent) -> Result<Self, Self::Error> {
        intent.propose_action()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IntentValidationError {
    #[error(transparent)]
    Action(#[from] ActionValidationError),
    #[error(transparent)]
    Proactive(#[from] ProactiveValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GoalKind, OpenLoopKind, PersonId, ProactiveMotive, ProactiveOpportunity};

    #[test]
    fn intent_conversion_keeps_platform_neutral_targets() {
        let conversation = ConversationId::new();
        let intent = CognitiveIntent::respond_to(
            conversation,
            MessageContent::text("answer"),
            Some(MessageId::new()),
        );
        let ProposedAction::SendMessage(action) = intent.propose_action().expect("valid intent")
        else {
            panic!("intent should become a send-message action");
        };
        assert_eq!(action.conversation_id, conversation);
        assert!(action.reply_to.is_some());
    }

    #[test]
    fn reach_out_intent_conversion_preserves_motive() {
        let person = PersonId::new();
        let opportunity = ProactiveOpportunity::new(person, ProactiveMotive::Share, 70, None)
            .expect("valid opportunity");
        let reach_out =
            ReachOutIntent::new(opportunity, MessageContent::text("look")).expect("valid intent");
        let ProposedAction::ReachOut(action) = CognitiveIntent::reach_out(reach_out)
            .propose_action()
            .expect("valid intent")
        else {
            panic!("intent should become reach-out action");
        };
        assert_eq!(action.person_id, person);
        assert_eq!(action.motive, ProactiveMotive::Share);
    }

    #[test]
    fn intent_deserialization_cannot_bypass_content_validation() {
        let intent =
            CognitiveIntent::send_message(ConversationId::new(), MessageContent::text("hello"));
        let mut encoded = serde_json::to_value(intent).expect("serialize intent");
        encoded["payload"]["content"]["text"] = serde_json::json!(" ");
        assert!(serde_json::from_value::<CognitiveIntent>(encoded).is_err());
    }

    #[test]
    fn new_intents_validate_and_convert_without_losing_scope() {
        let person_id = PersonId::new();
        let conversation_id = ConversationId::new();
        let intents = vec![
            CognitiveIntent::use_tool("calendar.read", "{}", ActionScope::Person(person_id)),
            CognitiveIntent::create_open_loop(
                OpenLoopDraft::new(
                    OpenLoopOwner::Conversation(conversation_id),
                    OpenLoopKind::FollowUp,
                    "continue later",
                )
                .expect("open loop"),
            ),
            CognitiveIntent::resolve_open_loop(
                OpenLoopId::new(),
                OpenLoopOwner::Conversation(conversation_id),
            ),
            CognitiveIntent::start_goal(
                GoalDraft::new(GoalOwner::Person(person_id), GoalKind::Personal, "practice")
                    .expect("goal"),
            ),
            CognitiveIntent::cancel_goal(GoalId::new(), GoalOwner::Person(person_id)),
        ];

        for intent in intents {
            let scope = intent.action_scope();
            let encoded = serde_json::to_string(&intent).expect("serialize intent");
            let decoded = serde_json::from_str::<CognitiveIntent>(&encoded)
                .expect("deserialize validated intent");
            assert_eq!(decoded.action_scope(), scope);
            assert_eq!(decoded.propose_action().expect("propose").scope(), scope);
        }
    }

    #[test]
    fn legacy_tool_intents_default_to_final_notification() {
        let scope = ActionScope::Global;
        let legacy = serde_json::json!({
            "type": "use_tool",
            "payload": {
                "tool_name": "weather.current",
                "input": "{}",
                "scope": scope
            }
        });
        let decoded: CognitiveIntent =
            serde_json::from_value(legacy).expect("legacy tool intent should deserialize");
        assert_eq!(
            decoded.tool_notification_policy(),
            Some(ToolNotificationPolicy::Final)
        );
    }
}
