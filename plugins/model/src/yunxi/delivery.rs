//! QQ delivery adapter for platform-neutral proactive reach-outs.
//!
//! Core supplies a `PersonId` and message content. This module is the only
//! place where that intent is translated back to a concrete QQ user ID.

use super::identity_store::PostgresIdentityStore;
use crate::model::send_tracked_private_message;
use kovi::RuntimeBot;
use std::sync::Arc;
use yunxi_core::{MessageContent, ReachOutIntent};

/// Parse a delivery lookup result conservatively. A person must have exactly
/// one positive numeric QQ identity; zero, malformed, and ambiguous mappings
/// are all unavailable until a delivery policy exists.
#[must_use]
pub(crate) fn single_positive_qq_id(external_ids: &[String]) -> Option<i64> {
    let [external_id] = external_ids else {
        return None;
    };
    let user_id = external_id.parse::<i64>().ok()?;
    (user_id > 0).then_some(user_id)
}

pub(crate) async fn send_reach_out(
    bot: &Arc<RuntimeBot>,
    identity_store: &PostgresIdentityStore,
    intent: &ReachOutIntent,
    expected_user_id: i64,
) -> bool {
    let Ok(Some(external_id)) = identity_store
        .qq_external_identity_for_delivery(intent.person_id())
        .await
    else {
        return false;
    };
    let Some(user_id) = single_positive_qq_id(&[external_id]) else {
        return false;
    };
    if user_id != expected_user_id {
        return false;
    }
    let content: &MessageContent = intent.message();
    send_tracked_private_message(bot, user_id, content.as_text().to_string()).await
}

#[cfg(test)]
mod tests {
    use super::single_positive_qq_id;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn delivery_requires_one_positive_numeric_identity() {
        assert_eq!(single_positive_qq_id(&ids(&["123456"])), Some(123456));
        assert_eq!(single_positive_qq_id(&[]), None);
        assert_eq!(single_positive_qq_id(&ids(&["0"])), None);
        assert_eq!(single_positive_qq_id(&ids(&["-1"])), None);
        assert_eq!(single_positive_qq_id(&ids(&["not-a-qq"])), None);
        assert_eq!(single_positive_qq_id(&ids(&["123", "456"])), None);
    }
}
