//! Adapter from the existing Kovi model gateway to the Core planner port.
//!
//! The gateway remains the owner of provider configuration and tool policy;
//! this module only translates a bounded Core input into a legacy request and
//! turns the visible reply back into a declarative Core plan.

use crate::model::interrupt;
use crate::model::{BotMemory, ModelGateway, ReplyPlan, ReplyScope, Roles};
use crate::yunxi::identity_store::PostgresIdentityStore;
use kovi::RuntimeBot;
use kovi::tokio::sync::Mutex;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Arc;
use yunxi_core::{
    CognitiveIntent, ConversationId, ConversationKind, DecisionDisposition, DecisionPlan,
    IdentityStoreError, MessageContent, MessageId, ModelBackend, ModelBackendFuture, PersonId,
    PlannerInput, ProactiveMotive, ReachOutIntent, StateUpdateProposal, WorldEventKind,
};

const FALLBACK_ROUTE_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy)]
pub(crate) enum LegacyConversation {
    Group { group_id: i64 },
    Private { user_id: i64 },
}

impl LegacyConversation {
    fn scope(self) -> ReplyScope {
        match self {
            Self::Group { group_id } => ReplyScope::Group(group_id),
            Self::Private { user_id } => ReplyScope::Private(user_id),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RouteContext {
    conversation: LegacyConversation,
}

#[derive(Debug, Clone, Copy)]
enum VisibleReplyTarget {
    Response {
        conversation_id: ConversationId,
        message_id: MessageId,
    },
    Send {
        conversation_id: ConversationId,
    },
    ReachOut {
        person_id: PersonId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistentRouteLookup<T> {
    Found(T),
    AuthoritativeMiss,
    StorageUnavailable,
}

fn route_from_lookup<T>(lookup: PersistentRouteLookup<T>, cached: Option<T>) -> Option<T> {
    match lookup {
        PersistentRouteLookup::Found(context) => Some(context),
        PersistentRouteLookup::StorageUnavailable => cached,
        PersistentRouteLookup::AuthoritativeMiss => None,
    }
}

/// A small insertion-ordered cache used only when persistent route recovery is
/// temporarily unavailable. It is deliberately not the source of truth.
#[derive(Debug)]
struct BoundedRouteCache<K> {
    entries: HashMap<K, RouteContext>,
    insertion_order: VecDeque<K>,
    capacity: usize,
}

impl<K> BoundedRouteCache<K>
where
    K: Copy + Eq + Hash,
{
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            insertion_order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn insert(&mut self, key: K, value: RouteContext) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.remove(&key).is_some() {
            self.insertion_order.retain(|candidate| *candidate != key);
        }
        while self.entries.len() >= self.capacity {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, value);
        self.insertion_order.push_back(key);
    }

    fn get(&self, key: &K) -> Option<RouteContext> {
        self.entries.get(key).copied()
    }

    fn remove(&mut self, key: &K) -> bool {
        let removed = self.entries.remove(key).is_some();
        if removed {
            self.insertion_order.retain(|candidate| candidate != key);
        }
        removed
    }
}

#[derive(Clone)]
pub(crate) struct KoviModelBackend {
    identities: Arc<PostgresIdentityStore>,
    conversations: Arc<Mutex<BoundedRouteCache<ConversationId>>>,
    people: Arc<Mutex<BoundedRouteCache<PersonId>>>,
}

impl std::fmt::Debug for KoviModelBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KoviModelBackend")
            .field("contexts", &"bounded host mappings")
            .finish_non_exhaustive()
    }
}

impl KoviModelBackend {
    pub(crate) fn new(_bot: Arc<RuntimeBot>, identities: Arc<PostgresIdentityStore>) -> Arc<Self> {
        Arc::new(Self {
            identities,
            conversations: Arc::new(Mutex::new(BoundedRouteCache::new(FALLBACK_ROUTE_CAPACITY))),
            people: Arc::new(Mutex::new(BoundedRouteCache::new(FALLBACK_ROUTE_CAPACITY))),
        })
    }

    pub(crate) async fn register(
        &self,
        conversation_id: ConversationId,
        person_id: PersonId,
        conversation: LegacyConversation,
        _sender_user_id: i64,
    ) {
        let context = RouteContext { conversation };
        self.conversations
            .lock()
            .await
            .insert(conversation_id, context);
        self.people.lock().await.insert(person_id, context);
    }

    pub(crate) async fn purge_routes(
        &self,
        canonical_person_id: Option<PersonId>,
        direct_conversation_ids: &[ConversationId],
    ) -> (usize, usize) {
        let person_routes = if let Some(person_id) = canonical_person_id {
            usize::from(self.people.lock().await.remove(&person_id))
        } else {
            0
        };
        let mut conversations = self.conversations.lock().await;
        let conversation_routes = direct_conversation_ids
            .iter()
            .filter(|conversation_id| conversations.remove(conversation_id))
            .count();
        (person_routes, conversation_routes)
    }

    async fn context(&self, input: &PlannerInput) -> Option<RouteContext> {
        match input.event.kind() {
            WorldEventKind::MessageReceived(message) => {
                let persistent = self
                    .persistent_conversation_context(message.conversation_id)
                    .await;
                route_from_lookup(
                    persistent,
                    self.conversations
                        .lock()
                        .await
                        .get(&message.conversation_id),
                )
            }
            WorldEventKind::ProspectiveMemoryDue(_) => match input.event.scope() {
                yunxi_core::EventScope::Person { person_id } => {
                    let persistent = self.persistent_person_context(person_id).await;
                    route_from_lookup(persistent, self.people.lock().await.get(&person_id))
                }
                yunxi_core::EventScope::Conversation { conversation_id } => {
                    let persistent = self.persistent_conversation_context(conversation_id).await;
                    route_from_lookup(
                        persistent,
                        self.conversations.lock().await.get(&conversation_id),
                    )
                }
                yunxi_core::EventScope::Global | yunxi_core::EventScope::Goal { .. } => None,
            },
            _ => None,
        }
    }

    async fn persistent_conversation_context(
        &self,
        conversation_id: ConversationId,
    ) -> PersistentRouteLookup<RouteContext> {
        let mappings = match self
            .identities
            .qq_external_conversations_for_id(conversation_id)
            .await
        {
            Ok(mappings) => mappings,
            Err(IdentityStoreError::Storage { .. }) => {
                return PersistentRouteLookup::StorageUnavailable;
            }
            Err(IdentityStoreError::ConversationKindMismatch { .. }) => {
                return PersistentRouteLookup::AuthoritativeMiss;
            }
        };
        let [(external_id, kind)] = mappings.as_slice() else {
            return PersistentRouteLookup::AuthoritativeMiss;
        };
        let Some(conversation) = parse_qq_conversation(external_id, *kind) else {
            return PersistentRouteLookup::AuthoritativeMiss;
        };
        PersistentRouteLookup::Found(RouteContext { conversation })
    }

    async fn persistent_person_context(
        &self,
        person_id: PersonId,
    ) -> PersistentRouteLookup<RouteContext> {
        let user_id = match classify_persistent_person_identity(
            self.identities
                .qq_external_identity_for_delivery(person_id)
                .await,
        ) {
            PersistentRouteLookup::Found(user_id) => user_id,
            PersistentRouteLookup::AuthoritativeMiss => {
                return PersistentRouteLookup::AuthoritativeMiss;
            }
            PersistentRouteLookup::StorageUnavailable => {
                return PersistentRouteLookup::StorageUnavailable;
            }
        };
        PersistentRouteLookup::Found(RouteContext {
            conversation: LegacyConversation::Private { user_id },
        })
    }
}

fn classify_persistent_person_identity(
    result: Result<Option<String>, IdentityStoreError>,
) -> PersistentRouteLookup<i64> {
    match result {
        Ok(Some(external_id)) => parse_positive_i64(&external_id).map_or(
            PersistentRouteLookup::AuthoritativeMiss,
            PersistentRouteLookup::Found,
        ),
        Ok(None) | Err(IdentityStoreError::ConversationKindMismatch { .. }) => {
            PersistentRouteLookup::AuthoritativeMiss
        }
        Err(IdentityStoreError::Storage { .. }) => PersistentRouteLookup::StorageUnavailable,
    }
}

fn visible_reply_intent(target: VisibleReplyTarget, content: String) -> Option<CognitiveIntent> {
    let content = MessageContent::text(content);
    match target {
        VisibleReplyTarget::Response {
            conversation_id,
            message_id,
        } => Some(CognitiveIntent::respond_to(
            conversation_id,
            content,
            Some(message_id),
        )),
        VisibleReplyTarget::Send { conversation_id } => {
            Some(CognitiveIntent::send_message(conversation_id, content))
        }
        VisibleReplyTarget::ReachOut { person_id } => {
            ReachOutIntent::from_parts(person_id, content, ProactiveMotive::FollowUp)
                .ok()
                .map(CognitiveIntent::reach_out)
        }
    }
}

fn due_reply_target(scope: yunxi_core::EventScope) -> Option<VisibleReplyTarget> {
    match scope {
        yunxi_core::EventScope::Person { person_id } => {
            Some(VisibleReplyTarget::ReachOut { person_id })
        }
        yunxi_core::EventScope::Conversation { conversation_id } => {
            Some(VisibleReplyTarget::Send { conversation_id })
        }
        yunxi_core::EventScope::Global | yunxi_core::EventScope::Goal { .. } => None,
    }
}

fn parse_qq_conversation(external_id: &str, kind: ConversationKind) -> Option<LegacyConversation> {
    match kind {
        ConversationKind::Group => external_id
            .strip_prefix("group:")
            .and_then(parse_positive_i64)
            .map(|group_id| LegacyConversation::Group { group_id }),
        ConversationKind::Direct => {
            let mut parts = external_id.split(':');
            if parts.next() != Some("direct") {
                return None;
            }
            parse_positive_i64(parts.next()?)?;
            let user_id = parse_positive_i64(parts.next()?)?;
            if parts.next().is_some() {
                return None;
            }
            Some(LegacyConversation::Private { user_id })
        }
        ConversationKind::System => None,
    }
}

fn parse_positive_i64(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().filter(|value| *value > 0)
}

impl ModelBackend for KoviModelBackend {
    fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
        Box::pin(async move {
            let Some(context) = self.context(input).await else {
                return Ok(DecisionPlan::silent());
            };
            let conversation = context.conversation;
            if matches!(conversation, LegacyConversation::Group { .. })
                && matches!(input.event.kind(), WorldEventKind::MessageReceived(_))
            {
                // Group replies still use the mature coalescing/mention host
                // path until that adapter is migrated. Core remains an input
                // projection for those events without creating duplicate sends.
                return Ok(DecisionPlan::silent());
            }
            if matches!(input.event.kind(), WorldEventKind::MessageReceived(_))
                && !crate::core_private_cutover_enabled()
            {
                // Keep Core as a shadow observer until the host explicitly
                // enables the private-message canary. The legacy handler owns
                // the visible reply in this mode, so planning here would risk
                // sending the same turn twice.
                return Ok(DecisionPlan::silent());
            }
            let (message, reply_target) = match input.event.kind() {
                WorldEventKind::MessageReceived(message) => {
                    if message.stop_requested
                        || message.content.as_text().trim_start().starts_with('#')
                    {
                        return Ok(DecisionPlan::silent());
                    }
                    (
                        Some(message),
                        VisibleReplyTarget::Response {
                            conversation_id: message.conversation_id,
                            message_id: message.message_id,
                        },
                    )
                }
                WorldEventKind::ProspectiveMemoryDue(_) => {
                    let Some(reply_target) = due_reply_target(input.event.scope()) else {
                        return Ok(DecisionPlan::silent());
                    };
                    (None, reply_target)
                }
                _ => return Ok(DecisionPlan::silent()),
            };

            let prompt = if let Some(message) = message {
                message.content.as_text().to_owned()
            } else {
                let WorldEventKind::ProspectiveMemoryDue(due) = input.event.kind() else {
                    return Ok(DecisionPlan::silent());
                };
                let Some(open_loop) = input
                    .open_loops
                    .iter()
                    .find(|item| item.id() == due.open_loop_id)
                else {
                    // Never describe one item and resolve another. A missing
                    // claimed item is safer to retry than to improvise.
                    return Ok(DecisionPlan::silent());
                };
                format!(
                    "请根据这个到期的待办事项自然地联系对方：{}",
                    open_loop.summary()
                )
            };
            let mut messages = vec![BotMemory {
                role: Roles::User,
                content: prompt,
            }];
            if !input.memories.is_empty() {
                let context = input
                    .memories
                    .iter()
                    .take(32)
                    .map(|memory| memory.content().to_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.insert(
                    0,
                    BotMemory {
                        role: Roles::Data,
                        content: format!("Core memory context:\n{context}"),
                    },
                );
            }
            if !input.open_loops.is_empty() {
                let context = input
                    .open_loops
                    .iter()
                    .take(32)
                    .map(|item| item.summary().to_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.insert(
                    0,
                    BotMemory {
                        role: Roles::Data,
                        content: format!("Core open-loop context:\n{context}"),
                    },
                );
            }

            let ticket = interrupt(conversation.scope()).await;
            let response =
                ModelGateway::complete_without_tools(&mut messages, ticket, None, &[], None).await;
            crate::model::finish(ticket).await;
            let Some(response) = response else {
                return Ok(DecisionPlan::silent());
            };
            let plan = ReplyPlan::from_model_output(conversation.scope(), &response.content).await;
            if !plan.has_visible_reply() || plan.content.trim().is_empty() {
                return Ok(DecisionPlan::silent());
            }
            let Some(intent) = visible_reply_intent(reply_target, plan.content) else {
                return Ok(DecisionPlan::silent());
            };
            Ok(DecisionPlan {
                disposition: DecisionDisposition::Reply,
                intents: vec![intent],
                state_updates: visible_reply_state_updates(input.event.kind()),
            })
        })
    }
}

fn visible_reply_state_updates(event: &WorldEventKind) -> Vec<StateUpdateProposal> {
    match event {
        WorldEventKind::ProspectiveMemoryDue(due) => {
            vec![StateUpdateProposal::ResolveOpenLoop {
                open_loop_id: due.open_loop_id,
            }]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedRouteCache, LegacyConversation, PersistentRouteLookup, RouteContext,
        VisibleReplyTarget, classify_persistent_person_identity, due_reply_target,
        parse_qq_conversation, route_from_lookup, visible_reply_intent,
        visible_reply_state_updates,
    };
    use yunxi_core::{
        CognitiveIntent, ConversationId, ConversationKind, EventScope, IdentityStoreError,
        MessageId, OpenLoopId, PersonId, ProactiveMotive, ProspectiveMemoryEvent,
        StateUpdateProposal, WorldEventKind,
    };

    #[test]
    fn persistent_qq_routes_are_parsed_conservatively() {
        assert!(matches!(
            parse_qq_conversation("direct:10:20", ConversationKind::Direct),
            Some(LegacyConversation::Private { user_id: 20 })
        ));
        assert!(matches!(
            parse_qq_conversation("group:30", ConversationKind::Group),
            Some(LegacyConversation::Group { group_id: 30 })
        ));
        assert!(parse_qq_conversation("direct:0:20", ConversationKind::Direct).is_none());
        assert!(parse_qq_conversation("direct:10:20:30", ConversationKind::Direct).is_none());
        assert!(parse_qq_conversation("group:30", ConversationKind::Direct).is_none());
    }

    #[test]
    fn fallback_route_cache_evicts_old_entries_at_capacity() {
        let first_id = ConversationId::new();
        let second_id = ConversationId::new();
        let context = || RouteContext {
            conversation: LegacyConversation::Private { user_id: 20 },
        };
        let mut cache = BoundedRouteCache::new(1);
        cache.insert(first_id, context());
        cache.insert(second_id, context());

        assert!(cache.get(&first_id).is_none());
        assert!(cache.get(&second_id).is_some());
    }

    #[test]
    fn updating_a_fallback_route_does_not_evict_an_unrelated_entry() {
        let first_id = ConversationId::new();
        let second_id = ConversationId::new();
        let context = |user_id| RouteContext {
            conversation: LegacyConversation::Private { user_id },
        };
        let mut cache = BoundedRouteCache::new(2);
        cache.insert(first_id, context(20));
        cache.insert(second_id, context(30));
        cache.insert(first_id, context(40));

        assert!(cache.get(&second_id).is_some());
        assert!(matches!(
            cache.get(&first_id).map(|item| item.conversation),
            Some(LegacyConversation::Private { user_id: 40 })
        ));
    }

    #[test]
    fn removing_a_fallback_route_clears_the_entry_and_order_slot() {
        let first_id = ConversationId::new();
        let second_id = ConversationId::new();
        let context = |user_id| RouteContext {
            conversation: LegacyConversation::Private { user_id },
        };
        let mut cache = BoundedRouteCache::new(1);
        cache.insert(first_id, context(20));
        assert!(cache.remove(&first_id));
        assert!(!cache.remove(&first_id));

        cache.insert(second_id, context(30));
        assert!(cache.get(&first_id).is_none());
        assert!(cache.get(&second_id).is_some());
    }

    #[test]
    fn due_person_reply_is_a_follow_up_reach_out() {
        let person_id = PersonId::new();
        let intent = visible_reply_intent(
            VisibleReplyTarget::ReachOut { person_id },
            "follow up".to_string(),
        )
        .expect("valid reach-out intent");

        let CognitiveIntent::ReachOut(reach_out) = intent else {
            panic!("due person reply must use ReachOut");
        };
        assert_eq!(reach_out.person_id(), person_id);
        assert_eq!(reach_out.motive(), ProactiveMotive::FollowUp);
        assert_eq!(reach_out.message().as_text(), "follow up");
    }

    #[test]
    fn due_conversation_reply_sends_without_inventing_a_reply_target() {
        let conversation_id = ConversationId::new();
        let target = due_reply_target(EventScope::Conversation { conversation_id })
            .expect("conversation due events have a delivery target");
        let intent = visible_reply_intent(target, "follow up".to_string())
            .expect("valid conversation follow-up intent");

        assert!(matches!(
            intent,
            CognitiveIntent::SendMessage {
                conversation_id: actual,
                reply_to: None,
                ..
            } if actual == conversation_id
        ));
        assert!(due_reply_target(EventScope::Global).is_none());
    }

    #[test]
    fn conversation_due_route_uses_cache_only_when_persistent_storage_is_unavailable() {
        let cached = RouteContext {
            conversation: LegacyConversation::Group { group_id: 30 },
        };

        assert!(
            route_from_lookup(PersistentRouteLookup::StorageUnavailable, Some(cached)).is_some()
        );
        assert!(
            route_from_lookup(PersistentRouteLookup::AuthoritativeMiss, Some(cached)).is_none()
        );
    }

    #[test]
    fn due_visible_reply_resolves_the_exact_claimed_open_loop() {
        let open_loop_id = OpenLoopId::new();
        assert_eq!(
            visible_reply_state_updates(&WorldEventKind::ProspectiveMemoryDue(
                ProspectiveMemoryEvent { open_loop_id }
            )),
            vec![StateUpdateProposal::ResolveOpenLoop { open_loop_id }]
        );
        assert!(visible_reply_state_updates(&WorldEventKind::IdleTick).is_empty());
    }

    #[test]
    fn received_message_reply_keeps_its_conversation_and_reply_target() {
        let conversation_id = ConversationId::new();
        let message_id = MessageId::new();
        let intent = visible_reply_intent(
            VisibleReplyTarget::Response {
                conversation_id,
                message_id,
            },
            "reply".to_string(),
        )
        .expect("valid response intent");

        assert!(matches!(
            intent,
            CognitiveIntent::SendMessage {
                conversation_id: actual_conversation_id,
                reply_to: Some(actual_message_id),
                ..
            } if actual_conversation_id == conversation_id && actual_message_id == message_id
        ));
    }

    #[test]
    fn only_storage_failure_permits_a_stale_person_route_fallback() {
        let cached = 20;
        let authoritative_miss = classify_persistent_person_identity(Ok(None));
        assert!(matches!(
            authoritative_miss,
            PersistentRouteLookup::AuthoritativeMiss
        ));
        assert!(route_from_lookup(authoritative_miss, Some(cached)).is_none());

        let storage_failure = classify_persistent_person_identity(Err(
            IdentityStoreError::storage(std::io::Error::other("database unavailable")),
        ));
        assert!(matches!(
            storage_failure,
            PersistentRouteLookup::StorageUnavailable
        ));
        assert!(route_from_lookup(storage_failure, Some(cached)).is_some());

        assert_eq!(
            route_from_lookup(PersistentRouteLookup::Found(30), Some(cached)),
            Some(30)
        );
    }
}
