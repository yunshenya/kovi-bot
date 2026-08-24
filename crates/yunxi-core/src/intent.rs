//! Platform-neutral cognitive intents.

use crate::action::{ActionValidationError, ProposedAction, SendMessageAction};
use crate::proactive::{ProactiveValidationError, ReachOutIntent};
use crate::{ConversationId, MessageContent, MessageId};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

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
            Self::Noop => Ok(ProposedAction::Noop),
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
    use crate::{PersonId, ProactiveMotive, ProactiveOpportunity};

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
}
