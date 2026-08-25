mod coalesce;
mod conversation_coordinator;
mod group;
mod interrupt;
mod memory_query;
mod memory_repository;
mod message_actions;
mod message_transport;
mod model_gateway;
mod private;
mod recall;
mod reply;
mod reply_disposition;
pub(crate) mod semantic;
mod thinking;
pub(crate) mod tool_access;
mod tracked_send;
mod traffic;
pub(crate) mod utils;

#[cfg(feature = "integration-tests")]
pub(crate) use interrupt::is_active;
pub(crate) use interrupt::{
    OutgoingCommitRejection, OutgoingSource, OutgoingToken, ReplyScope, ReplyTicket,
    begin_outgoing_commit, contextual_outgoing_fingerprint, find_prepared_outgoing, finish,
    interrupt, is_current, mark_active, mark_outgoing_failed, outgoing_fingerprint,
    prepare_outgoing_with_semantic_preview,
    prepare_proactive_outgoing_if_idle_with_semantic_preview, restore_message_collisions,
    take_message_collisions,
};
#[cfg(test)]
pub(crate) use interrupt::{
    OutgoingState, commit_outgoing, mark_outgoing_sent, prepare_outgoing, test_outgoing_state,
};
pub(crate) use message_actions::{MessageDestination, ReplyPlan, normalize_legacy_message_text};
pub(crate) use message_transport::MessageTransport;
pub(crate) use model_gateway::ModelGateway;
pub(crate) use recall::record_standalone_bot_message;
pub(crate) use thinking::strip_thinking_notices;
pub(crate) use tool_access::{ToolExecutionContext, tool_registry};
pub(crate) use tracked_send::{
    TrackedSendError, send_tracked_message_with_revalidation,
    send_tracked_message_with_revalidation_guard, send_tracked_unrecorded_plain_text,
};
pub(crate) use utils::{BotMemory, Roles};

#[cfg(test)]
pub(crate) use crate::model::conversation_coordinator::OutgoingExecutiveDecision;
pub(crate) use crate::model::conversation_coordinator::{
    ConversationCoordinator, IncomingAdmission, IncomingTurnImpact, OutgoingExecutiveContext,
};
pub(crate) use crate::model::group::group_message_event_after_ingress;

pub(crate) use crate::model::private::private_message_event_after_ingress;
pub(crate) use crate::model::recall::{
    is_recent_bot_message, recall_notice_event, send_tracked_private_message,
};
