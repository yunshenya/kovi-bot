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
mod traffic;
pub(crate) mod utils;

pub(crate) use interrupt::{ReplyScope, ReplyTicket, is_current};
#[cfg(feature = "integration-tests")]
pub(crate) use interrupt::{finish, interrupt, is_active, mark_active};
pub(crate) use message_actions::{MessageDestination, normalize_legacy_message_text};
pub(crate) use message_transport::MessageTransport;
pub(crate) use recall::record_standalone_bot_message;

pub use crate::model::group::group_message_event;

pub use crate::model::private::private_message_event;
pub(crate) use crate::model::recall::{
    recall_notice_event, send_tracked_group_message, send_tracked_private_message,
};
