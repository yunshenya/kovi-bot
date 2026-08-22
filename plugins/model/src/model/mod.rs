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

#[cfg(feature = "integration-tests")]
pub(crate) use interrupt::{ReplyScope, finish, interrupt, is_active, mark_active};
pub(crate) use interrupt::{ReplyTicket, is_current};
pub(crate) use message_actions::normalize_legacy_message_text;

pub use crate::model::group::group_message_event;

pub use crate::model::private::private_message_event;
pub(crate) use crate::model::recall::{
    recall_notice_event, send_tracked_group_message, send_tracked_private_message,
};
