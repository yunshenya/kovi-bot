//! Bounded QQ -> Yunxi Core shadow bridge.
//!
//! The bridge deliberately sits beside the existing Kovi handlers. It copies
//! only the small set of fields needed by Core, then resolves platform
//! identities on a single background worker. The legacy handlers remain the
//! owner of all model calls and QQ side effects.

use super::qq;
use crate::model::{ReplyScope, is_recent_bot_message};
use chrono::{DateTime, TimeZone, Utc};
use kovi::bot::message::Message;
use kovi::event::{GroupMsgEvent, PrivateMsgEvent};
use kovi::tokio::sync::mpsc;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use yunxi_core::{
    Admission, CognitiveRuntime, ConversationId, ConversationKind, EventPriority,
    ExternalConversation, IdentityStore, MessageContent, MessageId, MessageReceivedEvent,
    ProcessingOutcome, RuntimeConfig, RuntimeHandle, WorldEvent,
};

pub(crate) const SHADOW_INGRESS_CAPACITY: usize = 256;
pub(crate) const MESSAGE_REFERENCE_CAPACITY: usize = 4_096;
const MAX_MESSAGE_CHARS: usize = 8_192;
const MAX_MESSAGE_BYTES: usize = 32 * 1_024;

/// The result of the synchronous, non-blocking ingress operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    Accepted,
    DroppedAtCapacity,
    SkippedInvalid,
}

/// A handle held by the host's event closures. It owns no Kovi event and never
/// performs an await on the hot path.
#[derive(Debug, Clone)]
pub(crate) struct ShadowBridge {
    ingress: mpsc::Sender<InboundMessage>,
}

impl ShadowBridge {
    pub(crate) fn start(store: Arc<dyn IdentityStore>) -> Arc<Self> {
        let (ingress, receiver) = mpsc::channel(SHADOW_INGRESS_CAPACITY);
        let (runtime_handle, runtime) = CognitiveRuntime::new(RuntimeConfig::default())
            .expect("default Yunxi runtime configuration must be valid");

        kovi::tokio::spawn(run_ingress(receiver, store, runtime_handle));
        kovi::tokio::spawn(run_runtime(runtime));
        Arc::new(Self { ingress })
    }

    pub(crate) fn enqueue_group(&self, event: &GroupMsgEvent) -> EnqueueOutcome {
        let Some(message) = InboundMessage::from_group(event) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        self.try_enqueue(message)
    }

    pub(crate) fn enqueue_private(&self, event: &PrivateMsgEvent) -> EnqueueOutcome {
        let Some(message) = InboundMessage::from_private(event) else {
            return EnqueueOutcome::SkippedInvalid;
        };
        self.try_enqueue(message)
    }

    fn try_enqueue(&self, message: InboundMessage) -> EnqueueOutcome {
        match self.ingress.try_send(message) {
            Ok(()) => EnqueueOutcome::Accepted,
            Err(mpsc::error::TrySendError::Full(_)) => EnqueueOutcome::DroppedAtCapacity,
            Err(mpsc::error::TrySendError::Closed(_)) => EnqueueOutcome::SkippedInvalid,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ConversationAddress {
    Group { group_id: i64 },
    Direct { self_id: i64, peer_user_id: i64 },
}

impl ConversationAddress {
    fn external(&self) -> Result<ExternalConversation, qq::QqReferenceError> {
        match *self {
            Self::Group { group_id } => qq::group(group_id),
            Self::Direct {
                self_id,
                peer_user_id,
            } => qq::direct(self_id, peer_user_id),
        }
    }

    fn reply_scope(self) -> ReplyScope {
        match self {
            Self::Group { group_id } => ReplyScope::Group(group_id),
            Self::Direct { peer_user_id, .. } => ReplyScope::Private(peer_user_id),
        }
    }

    fn kind(self) -> ConversationKind {
        match self {
            Self::Group { .. } => ConversationKind::Group,
            Self::Direct { .. } => ConversationKind::Direct,
        }
    }
}

/// This is the only data allowed to cross from a Kovi event into the bridge.
/// In particular, it contains no `Arc<Event>`, message segments, JSON, or bot
/// handle.
#[derive(Debug, Clone)]
struct InboundMessage {
    address: ConversationAddress,
    sender_user_id: i64,
    external_message_id: Option<i64>,
    reply_to_external_message_id: Option<i64>,
    text: String,
    timestamp: DateTime<Utc>,
    addressed_to_agent: bool,
    explicit_request: bool,
    stop_requested: bool,
}

impl InboundMessage {
    fn from_group(event: &GroupMsgEvent) -> Option<Self> {
        valid_qq_id(event.self_id)
            .then_some(())
            .and_then(|()| valid_qq_id(event.group_id).then_some(()))
            .and_then(|()| valid_qq_id(event.user_id).then_some(()))?;
        if event.user_id == event.self_id {
            return None;
        }
        let text = bounded_text(event.borrow_text().unwrap_or_default());
        Some(Self {
            address: ConversationAddress::Group {
                group_id: event.group_id,
            },
            sender_user_id: event.user_id,
            external_message_id: positive_message_id(event.message_id),
            reply_to_external_message_id: reply_message_id(&event.message),
            addressed_to_agent: message_at_self(&event.message, event.self_id)
                || text_mentions_agent(&text),
            explicit_request: false,
            stop_requested: looks_like_stop_request(&text),
            text,
            timestamp: event_timestamp(event.time),
        })
    }

    fn from_private(event: &PrivateMsgEvent) -> Option<Self> {
        valid_qq_id(event.self_id).then_some(())?;
        valid_qq_id(event.user_id).then_some(())?;
        if event.user_id == event.self_id {
            return None;
        }
        let text = bounded_text(event.borrow_text().unwrap_or_default());
        Some(Self {
            address: ConversationAddress::Direct {
                self_id: event.self_id,
                peer_user_id: event.user_id,
            },
            sender_user_id: event.user_id,
            external_message_id: positive_message_id(event.message_id),
            reply_to_external_message_id: reply_message_id(&event.message),
            addressed_to_agent: true,
            explicit_request: true,
            stop_requested: looks_like_stop_request(&text),
            text,
            timestamp: event_timestamp(event.time),
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct MessageReference {
    message_id: MessageId,
    from_agent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MessageReferenceKey {
    conversation_id: ConversationId,
    external_message_id: i64,
}

/// A small LRU used only for references that have already crossed the Core
/// boundary. It is intentionally owned by the single ingress worker, so no
/// lock can be held across identity resolution.
#[derive(Debug)]
struct MessageReferenceCache {
    entries: HashMap<MessageReferenceKey, MessageReference>,
    order: VecDeque<MessageReferenceKey>,
    capacity: usize,
}

impl MessageReferenceCache {
    fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "message reference cache must be bounded and non-empty"
        );
        Self {
            entries: HashMap::with_capacity(capacity.min(128)),
            order: VecDeque::with_capacity(capacity.min(128)),
            capacity,
        }
    }

    fn get(&mut self, key: MessageReferenceKey) -> Option<MessageReference> {
        let value = self.entries.get(&key).copied()?;
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: MessageReferenceKey, value: MessageReference) {
        self.entries.insert(key, value);
        self.touch(key);
        while self.entries.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if oldest != key || self.entries.len() > self.capacity {
                self.entries.remove(&oldest);
            }
        }
    }

    fn touch(&mut self, key: MessageReferenceKey) {
        self.order.retain(|candidate| *candidate != key);
        self.order.push_back(key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

async fn run_ingress(
    mut receiver: mpsc::Receiver<InboundMessage>,
    store: Arc<dyn IdentityStore>,
    runtime: RuntimeHandle,
) {
    let mut references = MessageReferenceCache::new(MESSAGE_REFERENCE_CAPACITY);
    while let Some(message) = receiver.recv().await {
        if let Err(error) =
            resolve_and_submit(&message, store.as_ref(), &runtime, &mut references).await
        {
            eprintln!("[WARN] Yunxi shadow message dropped during identity resolution: {error}");
        }
    }
}

async fn run_runtime(mut runtime: CognitiveRuntime) {
    while let Some(outcome) = runtime.process_next().await {
        match outcome {
            ProcessingOutcome::Observed(observation) => {
                kovi::log::debug!(
                    "Yunxi shadow event observed: id={} type={:?} scope={:?} priority={:?} attention={:?} state={:?}",
                    observation.event_id,
                    observation.event_type,
                    observation.scope,
                    observation.priority,
                    observation.attention,
                    observation.state,
                );
            }
            ProcessingOutcome::RejectedEvent { .. } | ProcessingOutcome::RejectedState { .. } => {
                kovi::log::warn!("Yunxi shadow runtime rejected an event");
            }
        }
    }
}

async fn resolve_and_submit(
    message: &InboundMessage,
    store: &dyn IdentityStore,
    runtime: &RuntimeHandle,
    references: &mut MessageReferenceCache,
) -> anyhow::Result<()> {
    let external_identity = qq::person(message.sender_user_id)?;
    let external_conversation = message.address.external()?;
    let person_id = store
        .resolve_external_identity(&external_identity)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    let conversation_id = store
        .resolve_external_conversation(&external_conversation)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;

    let reference_key =
        message
            .external_message_id
            .map(|external_message_id| MessageReferenceKey {
                conversation_id,
                external_message_id,
            });
    if let Some(key) = reference_key
        && references.get(key).is_some()
    {
        return Ok(());
    }
    let reply_reference = message
        .reply_to_external_message_id
        .and_then(|external_message_id| {
            references.get(MessageReferenceKey {
                conversation_id,
                external_message_id,
            })
        });
    let recent_agent_reply = if reply_reference.is_some_and(|reference| reference.from_agent) {
        true
    } else {
        recent_bot_message(message.address, message.reply_to_external_message_id).await
    };

    let message_id = MessageId::new();
    let priority = if message.address.kind() == ConversationKind::Direct
        || message.addressed_to_agent
        || recent_agent_reply
        || message.stop_requested
        || message.explicit_request
    {
        EventPriority::High
    } else {
        EventPriority::Normal
    };
    let event = WorldEvent::message_received(
        priority,
        MessageReceivedEvent {
            message_id,
            conversation_id,
            sender: person_id,
            content: MessageContent::text(message.text.clone()),
            reply_to: reply_reference.map(|reference| reference.message_id),
            timestamp: message.timestamp,
            conversation_kind: message.address.kind(),
            addressed_to_agent: message.addressed_to_agent,
            replies_to_agent: recent_agent_reply,
            stop_requested: message.stop_requested,
            explicit_request: message.explicit_request,
        },
    );
    let admission = runtime
        .submit(event)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    if matches!(admission, Admission::Accepted)
        && let Some(key) = reference_key
    {
        references.insert(
            key,
            MessageReference {
                message_id,
                from_agent: false,
            },
        );
    }
    Ok(())
}

async fn recent_bot_message(
    address: ConversationAddress,
    reply_to_external_message_id: Option<i64>,
) -> bool {
    let Some(message_id) = reply_to_external_message_id.and_then(|value| i32::try_from(value).ok())
    else {
        return false;
    };
    is_recent_bot_message(address.reply_scope(), message_id).await
}

fn valid_qq_id(value: i64) -> bool {
    value > 0
}

fn positive_message_id(value: i32) -> Option<i64> {
    (value > 0).then_some(i64::from(value))
}

fn event_timestamp(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .unwrap_or_else(Utc::now)
}

fn bounded_text(value: &str) -> String {
    let mut bounded = String::with_capacity(value.len().min(MAX_MESSAGE_BYTES));
    for character in value.chars().take(MAX_MESSAGE_CHARS) {
        if bounded.len() + character.len_utf8() > MAX_MESSAGE_BYTES {
            break;
        }
        bounded.push(character);
    }
    bounded
}

fn message_at_self(message: &Message, self_id: i64) -> bool {
    message.iter().any(|segment| {
        segment.type_ == "at"
            && segment
                .data
                .get("qq")
                .and_then(value_as_i64)
                .is_some_and(|value| value == self_id)
    })
}

fn value_as_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse::<i64>().ok()))
}

fn reply_message_id(message: &Message) -> Option<i64> {
    message.iter().find_map(|segment| {
        (segment.type_ == "reply")
            .then(|| segment.data.get("id").and_then(value_as_i64))
            .flatten()
            .filter(|value| *value > 0)
    })
}

fn text_mentions_agent(message: &str) -> bool {
    ["芸汐", "云汐"].iter().any(|name| message.contains(name))
}

fn looks_like_stop_request(message: &str) -> bool {
    let normalized = message
        .trim()
        .trim_matches(|character: char| {
            character.is_ascii_punctuation() || "，。！？…".contains(character)
        })
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "别说了"
            | "不要说了"
            | "别回复了"
            | "不要回复了"
            | "停下"
            | "停止回复"
            | "闭嘴"
            | "stop"
            | "stop replying"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationAddress, EnqueueOutcome, InboundMessage, MessageReference,
        MessageReferenceCache, MessageReferenceKey, ShadowBridge, bounded_text,
        looks_like_stop_request, message_at_self, reply_message_id, resolve_and_submit,
        text_mentions_agent,
    };
    use chrono::Utc;
    use kovi::bot::message::{Message, Segment};
    use kovi::tokio::sync::mpsc;
    use serde_json::json;
    use std::sync::Arc;
    use yunxi_core::{
        AttentionDisposition, ConversationId, ConversationKind, EventPriority, IdentityStore,
        IdentityStoreError, IdentityStoreFuture, MessageId, PersonId, ProcessingOutcome,
        RuntimeConfig,
    };

    struct FakeIdentityStore {
        person_id: PersonId,
        conversation_id: ConversationId,
        stored_kind: ConversationKind,
    }

    struct FailingIdentityStore;

    impl IdentityStore for FailingIdentityStore {
        fn resolve_external_identity<'a>(
            &'a self,
            _external: &'a yunxi_core::ExternalIdentity,
        ) -> IdentityStoreFuture<'a, PersonId> {
            Box::pin(async {
                Err(IdentityStoreError::storage(std::io::Error::other(
                    "identity lookup unavailable",
                )))
            })
        }

        fn resolve_external_conversation<'a>(
            &'a self,
            _external: &'a yunxi_core::ExternalConversation,
        ) -> IdentityStoreFuture<'a, ConversationId> {
            Box::pin(async {
                Err(IdentityStoreError::storage(std::io::Error::other(
                    "conversation lookup unavailable",
                )))
            })
        }
    }

    impl IdentityStore for FakeIdentityStore {
        fn resolve_external_identity<'a>(
            &'a self,
            _external: &'a yunxi_core::ExternalIdentity,
        ) -> IdentityStoreFuture<'a, PersonId> {
            Box::pin(async move { Ok(self.person_id) })
        }

        fn resolve_external_conversation<'a>(
            &'a self,
            external: &'a yunxi_core::ExternalConversation,
        ) -> IdentityStoreFuture<'a, ConversationId> {
            Box::pin(async move {
                if external.kind() != self.stored_kind {
                    return Err(IdentityStoreError::ConversationKindMismatch {
                        requested: external.kind(),
                        stored: self.stored_kind,
                    });
                }
                Ok(self.conversation_id)
            })
        }
    }

    fn inbound(address: ConversationAddress, addressed_to_agent: bool) -> InboundMessage {
        InboundMessage {
            address,
            sender_user_id: 456,
            external_message_id: Some(789),
            reply_to_external_message_id: None,
            text: "hello".to_string(),
            timestamp: Utc::now(),
            addressed_to_agent,
            explicit_request: false,
            stop_requested: false,
        }
    }

    #[test]
    fn text_is_bounded_by_unicode_chars_and_bytes() {
        let bounded = bounded_text(&"界".repeat(20_000));
        assert_eq!(bounded.chars().count(), 8_192);
        assert!(bounded.len() <= 32 * 1_024);
    }

    #[test]
    fn reply_ids_accept_numbers_and_decimal_strings() {
        let message = Message::from(vec![
            Segment::new("reply", json!({"id": "12345"})),
            Segment::new("reply", json!({"id": 67890})),
        ]);
        assert_eq!(reply_message_id(&message), Some(12345));
    }

    #[test]
    fn structured_at_and_stop_detection_are_conservative() {
        let at = Message::from(vec![Segment::new("at", json!({"qq": "123"}))]);
        assert!(message_at_self(&at, 123));
        assert!(!message_at_self(&at, 456));
        assert!(text_mentions_agent("芸汐，看看这个"));
        assert!(looks_like_stop_request("STOP！"));
        assert!(!looks_like_stop_request("他说‘别说了’，然后离开了"));
    }

    #[test]
    fn reference_cache_is_bounded_and_isolated_by_conversation() {
        let first_conversation = ConversationId::new();
        let second_conversation = ConversationId::new();
        let mut cache = MessageReferenceCache::new(2);
        let first_key = MessageReferenceKey {
            conversation_id: first_conversation,
            external_message_id: 1,
        };
        cache.insert(
            first_key,
            MessageReference {
                message_id: MessageId::new(),
                from_agent: false,
            },
        );
        cache.insert(
            MessageReferenceKey {
                conversation_id: second_conversation,
                external_message_id: 1,
            },
            MessageReference {
                message_id: MessageId::new(),
                from_agent: false,
            },
        );
        cache.insert(
            MessageReferenceKey {
                conversation_id: first_conversation,
                external_message_id: 2,
            },
            MessageReference {
                message_id: MessageId::new(),
                from_agent: false,
            },
        );
        assert_eq!(cache.len(), 2);
        assert!(cache.get(first_key).is_none());
        assert!(
            cache
                .get(MessageReferenceKey {
                    conversation_id: first_conversation,
                    external_message_id: 2,
                })
                .is_some()
        );
        assert!(
            cache
                .get(MessageReferenceKey {
                    conversation_id: second_conversation,
                    external_message_id: 1,
                })
                .is_some()
        );
    }

    #[test]
    fn ingress_drops_at_capacity_without_waiting() {
        let (ingress, _receiver) = mpsc::channel(1);
        let bridge = ShadowBridge { ingress };
        let message = inbound(ConversationAddress::Group { group_id: 123 }, false);

        assert_eq!(
            bridge.try_enqueue(message.clone()),
            EnqueueOutcome::Accepted
        );
        assert_eq!(
            bridge.try_enqueue(message),
            EnqueueOutcome::DroppedAtCapacity
        );
    }

    #[test]
    fn direct_and_addressed_messages_use_reliable_priority() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            for (address, addressed, expected_priority) in [
                (
                    ConversationAddress::Direct {
                        self_id: 111,
                        peer_user_id: 456,
                    },
                    false,
                    EventPriority::High,
                ),
                (
                    ConversationAddress::Group { group_id: 123 },
                    true,
                    EventPriority::High,
                ),
                (
                    ConversationAddress::Group { group_id: 123 },
                    false,
                    EventPriority::Normal,
                ),
            ] {
                let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                    person_id: PersonId::new(),
                    conversation_id: ConversationId::new(),
                    stored_kind: address.kind(),
                });
                let (handle, mut runtime) =
                    yunxi_core::CognitiveRuntime::new(RuntimeConfig::default())
                        .expect("valid runtime");
                resolve_and_submit(
                    &inbound(address, addressed),
                    store.as_ref(),
                    &handle,
                    &mut MessageReferenceCache::new(4),
                )
                .await
                .expect("fake mappings should resolve");
                let ProcessingOutcome::Observed(observation) = runtime
                    .process_next()
                    .await
                    .expect("submitted event should be processed")
                else {
                    panic!("event should be observed");
                };
                assert_eq!(observation.priority, expected_priority);
            }
        });
    }

    #[test]
    fn fake_store_preserves_direct_and_group_attention() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            for (address, kind, addressed, expected) in [
                (
                    ConversationAddress::Group { group_id: 123 },
                    ConversationKind::Group,
                    false,
                    AttentionDisposition::ObserveOnly,
                ),
                (
                    ConversationAddress::Group { group_id: 123 },
                    ConversationKind::Group,
                    true,
                    AttentionDisposition::MustHandle,
                ),
                (
                    ConversationAddress::Direct {
                        self_id: 111,
                        peer_user_id: 456,
                    },
                    ConversationKind::Direct,
                    true,
                    AttentionDisposition::MustHandle,
                ),
            ] {
                let store: Arc<dyn IdentityStore> = Arc::new(FakeIdentityStore {
                    person_id: PersonId::new(),
                    conversation_id: ConversationId::new(),
                    stored_kind: kind,
                });
                let (handle, mut runtime) =
                    yunxi_core::CognitiveRuntime::new(RuntimeConfig::default())
                        .expect("valid runtime");
                resolve_and_submit(
                    &inbound(address, addressed),
                    store.as_ref(),
                    &handle,
                    &mut MessageReferenceCache::new(4),
                )
                .await
                .expect("fake mappings should resolve");
                let ProcessingOutcome::Observed(observation) = runtime
                    .process_next()
                    .await
                    .expect("submitted event should be processed")
                else {
                    panic!("event should be observed");
                };
                assert_eq!(observation.attention.disposition, expected);
            }
        });
    }

    #[test]
    fn conversation_kind_mismatch_is_dropped_before_core_submission() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let store = FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Direct,
            };
            let (handle, _runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let error = resolve_and_submit(
                &inbound(ConversationAddress::Group { group_id: 123 }, true),
                &store,
                &handle,
                &mut MessageReferenceCache::new(4),
            )
            .await
            .expect_err("kind mismatch must be rejected");
            assert!(error.to_string().contains("kind mismatch"));
        });
    }

    #[test]
    fn identity_store_failure_does_not_submit_or_cache_an_event() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (handle, _runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let mut references = MessageReferenceCache::new(4);
            let error = resolve_and_submit(
                &inbound(
                    ConversationAddress::Direct {
                        self_id: 111,
                        peer_user_id: 456,
                    },
                    true,
                ),
                &FailingIdentityStore,
                &handle,
                &mut references,
            )
            .await
            .expect_err("storage failure should be returned");
            assert!(
                error
                    .chain()
                    .any(|cause| cause.to_string().contains("identity lookup unavailable"))
            );
            assert_eq!(references.len(), 0);
        });
    }

    #[test]
    fn duplicate_external_message_ids_are_not_submitted_twice() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let store = FakeIdentityStore {
                person_id: PersonId::new(),
                conversation_id: ConversationId::new(),
                stored_kind: ConversationKind::Group,
            };
            let (handle, mut runtime) =
                yunxi_core::CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
            let message = inbound(ConversationAddress::Group { group_id: 123 }, false);
            let mut references = MessageReferenceCache::new(4);
            resolve_and_submit(&message, &store, &handle, &mut references)
                .await
                .expect("first message should resolve");
            resolve_and_submit(&message, &store, &handle, &mut references)
                .await
                .expect("duplicate should be ignored");
            assert_eq!(references.len(), 1);
            assert!(matches!(
                runtime.process_next().await,
                Some(ProcessingOutcome::Observed(_))
            ));
        });
    }
}
