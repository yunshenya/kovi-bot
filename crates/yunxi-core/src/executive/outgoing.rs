//! Outbound message envelope metadata shared by the host reply ticket.
//!
//! The host owns the concrete reply ticket, delivery, and any revalidation of a
//! prepared outgoing value. This module only describes the bounded, serializable
//! envelope that crosses the reply/coalesce path; it never sends a message or
//! invokes a model.

use crate::{ConversationId, EventId, PersonId};
use serde::{Deserialize, Serialize};

pub const MAX_OUTGOING_REWRITES: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutgoingSource {
    Reply,
    Proactive,
    Reminder,
    TaskDelivery,
    System,
}

impl OutgoingSource {
    #[must_use]
    pub const fn is_must_execute(self) -> bool {
        matches!(self, Self::Reminder | Self::TaskDelivery | Self::System)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOutgoing {
    pub event_id: EventId,
    pub conversation_id: Option<ConversationId>,
    pub recipient_id: Option<PersonId>,
    pub source: OutgoingSource,
    pub generation: u64,
    pub committed: bool,
    #[serde(default)]
    pub rewrite_count: u8,
    #[serde(default)]
    pub merge_count: u8,
    #[serde(default = "default_max_rewrites")]
    pub max_rewrites: u8,
}

impl PendingOutgoing {
    #[must_use]
    pub fn new(event_id: EventId, source: OutgoingSource) -> Self {
        Self {
            event_id,
            conversation_id: None,
            recipient_id: None,
            source,
            generation: 1,
            committed: false,
            rewrite_count: 0,
            merge_count: 0,
            max_rewrites: MAX_OUTGOING_REWRITES,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.generation == 0 {
            return Err("pending outgoing generation must be non-zero");
        }
        if self.rewrite_count > self.max_rewrites || self.max_rewrites > MAX_OUTGOING_REWRITES {
            return Err("pending outgoing rewrite count is out of bounds");
        }
        Ok(())
    }

    #[must_use]
    pub const fn must_execute(&self) -> bool {
        self.source.is_must_execute()
    }
}

fn default_max_rewrites() -> u8 {
    MAX_OUTGOING_REWRITES
}
