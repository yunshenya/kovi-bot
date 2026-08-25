//! QQ delivery adapter for platform-neutral actions.
//!
//! Core only exposes opaque people and conversation IDs. This module is the
//! only place where those IDs are translated to concrete QQ destinations or
//! Kovi API calls. Legacy proactive-chat delivery remains below as a small
//! compatibility helper; new actions use [`QqActionAdapter`].

use super::identity_store::PostgresIdentityStore;
use super::qq;
use crate::model::{
    MessageDestination, MessageTransport, record_standalone_bot_message,
    send_tracked_private_message,
};
use kovi::RuntimeBot;
use kovi::tokio::sync::Mutex;
use std::fmt;
use std::sync::Arc;
use thiserror::Error;
use yunxi_core::{
    ActionPort, ActionPortError, ActionPortFuture, ActionPortOutcome, ConversationId,
    ConversationKind, DeliveryResolutionError, DeliveryResolver, DeliveryResolverFuture,
    DeliveryRoute, IdentityStore, MessageContent, ProposedAction, ReachOutIntent,
};

/// Concrete QQ destination after a canonical Core conversation has been
/// resolved. The enum is intentionally private so platform identifiers do not
/// leak through the Core-facing traits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QqDestination {
    Group(i64),
    Private(i64),
}

impl QqDestination {
    fn message_destination(self) -> MessageDestination {
        match self {
            Self::Group(group_id) => MessageDestination::Group(group_id),
            Self::Private(user_id) => MessageDestination::Private(user_id),
        }
    }

    fn reply_scope(self) -> crate::model::ReplyScope {
        match self {
            Self::Group(group_id) => crate::model::ReplyScope::Group(group_id),
            Self::Private(user_id) => crate::model::ReplyScope::Private(user_id),
        }
    }
}

#[derive(Debug, Error)]
#[error("QQ delivery adapter {operation} failed: {detail}")]
struct QqAdapterFailure {
    operation: &'static str,
    detail: String,
}

impl QqAdapterFailure {
    fn new(operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            operation,
            detail: detail.into(),
        }
    }
}

/// Kovi host implementation of both Core delivery resolution and action
/// execution. A single adapter keeps the login identity and mapping policy
/// consistent between `ReachOut` resolution and actual sends.
#[derive(Clone)]
pub(crate) struct QqActionAdapter {
    bot: Arc<RuntimeBot>,
    identity_store: Arc<PostgresIdentityStore>,
    /// Login info is stable for a running Kovi bot. Cache it after the first
    /// successful lookup, but leave it unset when the API is temporarily down
    /// so a later action can retry.
    self_id: Arc<Mutex<Option<i64>>>,
}

impl fmt::Debug for QqActionAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QqActionAdapter")
            .field(
                "self_id_cached",
                &self.self_id.try_lock().ok().and_then(|id| *id),
            )
            .finish_non_exhaustive()
    }
}

impl QqActionAdapter {
    pub(crate) fn new(
        bot: Arc<RuntimeBot>,
        identity_store: Arc<PostgresIdentityStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            bot,
            identity_store,
            self_id: Arc::new(Mutex::new(None)),
        })
    }

    async fn current_self_id(&self) -> Result<i64, QqAdapterFailure> {
        if let Some(self_id) = *self.self_id.lock().await {
            return Ok(self_id);
        }

        let response =
            self.bot.get_login_info().await.map_err(|error| {
                QqAdapterFailure::new("login lookup", format_api_return(&error))
            })?;
        let self_id = response
            .data
            .get("user_id")
            .and_then(value_as_i64)
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                QqAdapterFailure::new(
                    "login lookup",
                    "Kovi returned no positive user_id in get_login_info",
                )
            })?;
        *self.self_id.lock().await = Some(self_id);
        Ok(self_id)
    }

    async fn resolve_person_destination(
        &self,
        person_id: yunxi_core::PersonId,
    ) -> Result<(DeliveryRoute, QqDestination), DeliveryResolutionError> {
        let external_id = self
            .identity_store
            .qq_external_identity_for_delivery(person_id)
            .await
            .map_err(|error| {
                DeliveryResolutionError::failed(QqAdapterFailure::new(
                    "person identity lookup",
                    error.to_string(),
                ))
            })?;
        let Some(user_id) = external_id
            .as_deref()
            .and_then(|value| single_positive_qq_id(&[value.to_owned()]))
        else {
            return Err(DeliveryResolutionError::Unavailable { person_id });
        };

        let self_id = self
            .current_self_id()
            .await
            .map_err(DeliveryResolutionError::failed)?;
        let external = qq::direct(self_id, user_id).map_err(|error| {
            DeliveryResolutionError::failed(QqAdapterFailure::new(
                "direct conversation reference",
                error.to_string(),
            ))
        })?;
        let conversation_id = self
            .identity_store
            .resolve_external_conversation(&external)
            .await
            .map_err(|error| {
                DeliveryResolutionError::failed(QqAdapterFailure::new(
                    "direct conversation lookup",
                    error.to_string(),
                ))
            })?;
        Ok((
            DeliveryRoute::new(conversation_id, ConversationKind::Direct),
            QqDestination::Private(user_id),
        ))
    }

    async fn resolve_conversation_destination(
        &self,
        conversation_id: ConversationId,
    ) -> Result<QqDestination, ActionPortError> {
        let mappings = self
            .identity_store
            .qq_external_conversations_for_id(conversation_id)
            .await
            .map_err(|error| {
                ActionPortError::new(format!("delivery_lookup_failed:{error}"), true)
            })?;
        let [(external_id, kind)] = mappings.as_slice() else {
            return Err(ActionPortError::new(
                if mappings.is_empty() {
                    "delivery_route_unavailable"
                } else {
                    "delivery_route_ambiguous"
                },
                true,
            ));
        };
        let self_id = if *kind == ConversationKind::Direct {
            Some(
                self.current_self_id()
                    .await
                    .map_err(|error| ActionPortError::new(error.to_string(), true))?,
            )
        } else {
            None
        };
        parse_qq_destination(external_id, *kind, self_id)
            .ok_or_else(|| ActionPortError::new("delivery_route_invalid", false))
    }

    async fn send_to_destination(
        &self,
        destination: QqDestination,
        content: &MessageContent,
    ) -> Result<ActionPortOutcome, ActionPortError> {
        let text = content.as_text();
        let message_id = MessageTransport::new(&self.bot)
            .send(destination.message_destination(), text.to_owned().into())
            .await
            .map_err(|error| {
                ActionPortError::new(
                    format!("qq_send_failed:{}", format_api_return(&error)),
                    true,
                )
            })?;
        record_standalone_bot_message(destination.reply_scope(), message_id, text).await;
        Ok(ActionPortOutcome::Delivered {
            external_reference: Some(format!("qq-message:{message_id}")),
        })
    }
}

impl DeliveryResolver for QqActionAdapter {
    fn resolve<'a>(&'a self, person_id: yunxi_core::PersonId) -> DeliveryResolverFuture<'a> {
        Box::pin(async move {
            self.resolve_person_destination(person_id)
                .await
                .map(|(route, _)| route)
        })
    }
}

impl ActionPort for QqActionAdapter {
    fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a> {
        Box::pin(async move {
            match action {
                ProposedAction::SendMessage(send) => {
                    // Core MessageId values are intentionally opaque. The
                    // current adapter has no shared Core->QQ message index,
                    // so silently dropping `reply_to` would alter intent.
                    if send.reply_to.is_some() {
                        return Err(ActionPortError::new("reply_target_unmapped", false));
                    }
                    let destination = self
                        .resolve_conversation_destination(send.conversation_id)
                        .await?;
                    self.send_to_destination(destination, &send.content).await
                }
                ProposedAction::ReachOut(reach_out) => {
                    let (_, destination) = self
                        .resolve_person_destination(reach_out.person_id)
                        .await
                        .map_err(|error| ActionPortError::new(error.to_string(), true))?;
                    self.send_to_destination(destination, &reach_out.message)
                        .await
                }
                ProposedAction::Noop => Ok(ActionPortOutcome::Delivered {
                    external_reference: None,
                }),
            }
        })
    }
}

fn parse_qq_destination(
    external_id: &str,
    kind: ConversationKind,
    current_self_id: Option<i64>,
) -> Option<QqDestination> {
    match kind {
        ConversationKind::Group => external_id
            .strip_prefix("group:")
            .and_then(parse_positive_i64)
            .map(QqDestination::Group),
        ConversationKind::Direct => {
            let mut parts = external_id.split(':');
            if parts.next() != Some("direct") {
                return None;
            }
            let self_id = parts.next()?;
            let peer_user_id = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let self_id = parse_positive_i64(self_id)?;
            let peer_user_id = parse_positive_i64(peer_user_id)?;
            if current_self_id != Some(self_id) {
                return None;
            }
            Some(QqDestination::Private(peer_user_id))
        }
        ConversationKind::System => None,
    }
}

fn parse_positive_i64(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().filter(|value| *value > 0)
}

fn value_as_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse::<i64>().ok()))
}

fn format_api_return(value: &kovi::ApiReturn) -> String {
    format!(
        "status={} retcode={} data={} echo={}",
        value.status, value.retcode, value.data, value.echo
    )
}

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
    use super::{QqDestination, parse_qq_destination, single_positive_qq_id};
    use yunxi_core::ConversationKind;

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

    #[test]
    fn canonical_group_and_direct_routes_are_strictly_parsed() {
        assert_eq!(
            parse_qq_destination("group:123", ConversationKind::Group, None),
            Some(QqDestination::Group(123))
        );
        assert_eq!(
            parse_qq_destination("direct:456:123", ConversationKind::Direct, Some(456)),
            Some(QqDestination::Private(123))
        );
        assert_eq!(
            parse_qq_destination("direct:456:123", ConversationKind::Direct, Some(789)),
            None
        );
    }

    #[test]
    fn malformed_or_cross_kind_routes_fail_closed() {
        for (external_id, kind, self_id) in [
            ("group:0", ConversationKind::Group, None),
            ("group:123:456", ConversationKind::Group, None),
            ("direct:456", ConversationKind::Direct, Some(456)),
            ("direct:456:0", ConversationKind::Direct, Some(456)),
            ("direct:456:123:789", ConversationKind::Direct, Some(456)),
            ("group:123", ConversationKind::Direct, Some(456)),
            ("direct:456:123", ConversationKind::Group, None),
        ] {
            assert_eq!(
                parse_qq_destination(external_id, kind, self_id),
                None,
                "route should be rejected: {external_id}"
            );
        }
    }
}
