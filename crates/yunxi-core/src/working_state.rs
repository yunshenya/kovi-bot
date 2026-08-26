use crate::attention::{AttentionDisposition, AttentionResult};
use crate::event::{EventPriority, EventType, EventValidationError, WorldEvent, WorldEventKind};
use crate::identity::{ConversationId, MessageId, OpenLoopId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkingStateConfig {
    pub max_global_events: usize,
    pub max_conversations: usize,
    pub max_conversation_events: usize,
    pub max_active_people: usize,
    pub max_open_loop_refs: usize,
    pub max_compact_text_chars: usize,
    pub max_compact_text_bytes: usize,
}

const MAX_GLOBAL_EVENTS: usize = 1_024;
const MAX_CONVERSATIONS: usize = 4_096;
const MAX_CONVERSATION_EVENTS: usize = 256;
const MAX_ACTIVE_PEOPLE: usize = 256;
const MAX_OPEN_LOOP_REFS: usize = 256;
const MAX_COMPACT_TEXT_CHARS: usize = 4_096;
const MAX_COMPACT_TEXT_BYTES: usize = 16 * 1_024;

impl Default for WorkingStateConfig {
    fn default() -> Self {
        Self {
            max_global_events: 64,
            max_conversations: 256,
            max_conversation_events: 32,
            max_active_people: 32,
            max_open_loop_refs: 32,
            max_compact_text_chars: 512,
            max_compact_text_bytes: 2_048,
        }
    }
}

impl WorkingStateConfig {
    fn validate(self) -> Result<Self, WorkingStateConfigError> {
        for (field, value, maximum) in [
            (
                "max_global_events",
                self.max_global_events,
                MAX_GLOBAL_EVENTS,
            ),
            (
                "max_conversations",
                self.max_conversations,
                MAX_CONVERSATIONS,
            ),
            (
                "max_conversation_events",
                self.max_conversation_events,
                MAX_CONVERSATION_EVENTS,
            ),
            (
                "max_active_people",
                self.max_active_people,
                MAX_ACTIVE_PEOPLE,
            ),
            (
                "max_open_loop_refs",
                self.max_open_loop_refs,
                MAX_OPEN_LOOP_REFS,
            ),
            (
                "max_compact_text_chars",
                self.max_compact_text_chars,
                MAX_COMPACT_TEXT_CHARS,
            ),
            (
                "max_compact_text_bytes",
                self.max_compact_text_bytes,
                MAX_COMPACT_TEXT_BYTES,
            ),
        ] {
            if value == 0 {
                return Err(WorkingStateConfigError::ZeroCapacity(field));
            }
            if value > maximum {
                return Err(WorkingStateConfigError::CapacityTooLarge {
                    field,
                    value,
                    maximum,
                });
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WorkingStateConfigError {
    #[error("working-state capacity `{0}` must be greater than zero")]
    ZeroCapacity(&'static str),
    #[error("working-state capacity `{field}` is {value}, above maximum {maximum}")]
    CapacityTooLarge {
        field: &'static str,
        value: usize,
        maximum: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactEvent {
    pub id: crate::identity::EventId,
    pub occurred_at: DateTime<Utc>,
    pub event_type: EventType,
    pub priority: EventPriority,
    pub disposition: AttentionDisposition,
    /// Present for received messages so a bounded group snapshot can keep
    /// speakers distinct without retaining host-specific identifiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<PersonId>,
    pub text: Option<String>,
}

impl CompactEvent {
    fn from_event(
        event: &WorldEvent,
        attention: AttentionResult,
        include_text: bool,
        config: WorkingStateConfig,
    ) -> Self {
        let text = include_text
            .then(|| match event.kind() {
                WorldEventKind::MessageReceived(message) => message.content.as_text(),
                WorldEventKind::MessageSent(message) => message
                    .content
                    .as_ref()
                    .map_or("", crate::MessageContent::as_text),
                WorldEventKind::ToolFailed(tool) => tool.error_category.as_str(),
                WorldEventKind::ActionFailed(action) => action.error_category.as_str(),
                _ => "",
            })
            .filter(|value| !value.is_empty())
            .map(|value| {
                bounded_text(
                    value,
                    config.max_compact_text_chars,
                    config.max_compact_text_bytes,
                )
            });
        Self {
            id: event.id(),
            occurred_at: event.occurred_at(),
            event_type: event.kind().event_type(),
            priority: event.priority(),
            disposition: attention.disposition,
            person_id: match event.kind() {
                WorldEventKind::MessageReceived(message) => Some(message.sender),
                _ => None,
            },
            text,
        }
    }
}

#[derive(Debug, Default)]
struct GlobalWorkingState {
    recent_events: VecDeque<CompactEvent>,
    version: u64,
}

#[derive(Debug, Default)]
struct ConversationWorkingState {
    conversation_kind: Option<crate::identity::ConversationKind>,
    current_topic: Option<String>,
    active_people: VecDeque<PersonId>,
    recent_events: VecDeque<CompactEvent>,
    last_message_at: Option<DateTime<Utc>>,
    last_bot_action_at: Option<DateTime<Utc>>,
    last_message_id: Option<MessageId>,
    open_loops: VecDeque<OpenLoopId>,
    version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    conversation_id: ConversationId,
    pub conversation_kind: Option<crate::identity::ConversationKind>,
    pub current_topic: Option<String>,
    pub active_people: Vec<PersonId>,
    pub recent_events: Vec<CompactEvent>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub last_bot_action_at: Option<DateTime<Utc>>,
    pub last_message_id: Option<MessageId>,
    pub open_loops: Vec<OpenLoopId>,
    version: u64,
}

impl ConversationSnapshot {
    fn from_state(conversation_id: ConversationId, state: &ConversationWorkingState) -> Self {
        Self {
            conversation_id,
            conversation_kind: state.conversation_kind,
            current_topic: state.current_topic.clone(),
            active_people: state.active_people.iter().copied().collect(),
            recent_events: state.recent_events.iter().cloned().collect(),
            last_message_at: state.last_message_at,
            last_bot_action_at: state.last_bot_action_at,
            last_message_id: state.last_message_id,
            open_loops: state.open_loops.iter().copied().collect(),
            version: state.version,
        }
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateUpdate {
    pub global_version: u64,
    pub conversation_id: Option<ConversationId>,
    pub conversation_version: Option<u64>,
    pub evicted_conversation: Option<ConversationId>,
}

#[derive(Debug)]
pub struct WorkingState {
    global: GlobalWorkingState,
    conversations: HashMap<ConversationId, ConversationWorkingState>,
    conversation_order: VecDeque<ConversationId>,
    config: WorkingStateConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WorkingStateError {
    #[error(transparent)]
    InvalidEvent(#[from] EventValidationError),
    #[error("working-state version counter is exhausted")]
    VersionExhausted,
    #[error("conversation kind conflicts with its existing working state")]
    ConversationKindMismatch,
}

impl WorkingState {
    pub fn new(config: WorkingStateConfig) -> Result<Self, WorkingStateConfigError> {
        Ok(Self {
            global: GlobalWorkingState::default(),
            conversations: HashMap::new(),
            conversation_order: VecDeque::new(),
            config: config.validate()?,
        })
    }

    pub fn observe(
        &mut self,
        event: &WorldEvent,
        attention: AttentionResult,
    ) -> Result<StateUpdate, WorkingStateError> {
        event.validate(u8::MAX)?;
        let conversation_id = event.scope().conversation_id();
        if let WorldEventKind::MessageReceived(message) = event.kind()
            && let Some(existing_kind) = self
                .conversations
                .get(&message.conversation_id)
                .and_then(|state| state.conversation_kind)
            && existing_kind != message.conversation_kind
        {
            return Err(WorkingStateError::ConversationKindMismatch);
        }
        let next_global_version = self
            .global
            .version
            .checked_add(1)
            .ok_or(WorkingStateError::VersionExhausted)?;
        let next_conversation_version = conversation_id
            .map(|conversation_id| {
                self.conversations
                    .get(&conversation_id)
                    .map_or(Ok(1), |state| {
                        state
                            .version
                            .checked_add(1)
                            .ok_or(WorkingStateError::VersionExhausted)
                    })
            })
            .transpose()?;

        let global_compact = CompactEvent::from_event(event, attention, false, self.config);
        push_bounded(
            &mut self.global.recent_events,
            global_compact,
            self.config.max_global_events,
        );
        self.global.version = next_global_version;

        let mut evicted_conversation = None;
        let conversation_version = if let (Some(conversation_id), Some(next_version)) =
            (conversation_id, next_conversation_version)
        {
            if !self.conversations.contains_key(&conversation_id)
                && self.conversations.len() >= self.config.max_conversations
                && let Some(evicted) = self.conversation_order.pop_front()
            {
                self.conversations.remove(&evicted);
                evicted_conversation = Some(evicted);
            }

            touch_lru(&mut self.conversation_order, conversation_id);
            let compact = CompactEvent::from_event(event, attention, true, self.config);
            let state = self.conversations.entry(conversation_id).or_default();
            push_bounded(
                &mut state.recent_events,
                compact,
                self.config.max_conversation_events,
            );
            match event.kind() {
                WorldEventKind::MessageReceived(message) => {
                    state.conversation_kind = Some(message.conversation_kind);
                    push_unique_bounded(
                        &mut state.active_people,
                        message.sender,
                        self.config.max_active_people,
                    );
                    state.last_message_at = Some(
                        state
                            .last_message_at
                            .map_or(message.timestamp, |current| current.max(message.timestamp)),
                    );
                    state.last_message_id = Some(message.message_id);
                }
                WorldEventKind::MessageSent(message) => {
                    state.last_bot_action_at = Some(
                        state
                            .last_bot_action_at
                            .map_or(message.timestamp, |current| current.max(message.timestamp)),
                    );
                    state.last_message_id = Some(message.message_id);
                }
                WorldEventKind::ProspectiveMemoryDue(memory) => push_unique_bounded(
                    &mut state.open_loops,
                    memory.open_loop_id,
                    self.config.max_open_loop_refs,
                ),
                _ => {}
            }
            state.version = next_version;
            Some(next_version)
        } else {
            None
        };

        Ok(StateUpdate {
            global_version: self.global.version,
            conversation_id,
            conversation_version,
            evicted_conversation,
        })
    }

    /// Applies a planner-validated topic update and advances snapshot
    /// versions just like any other working-state mutation.
    pub(crate) fn set_current_topic(
        &mut self,
        conversation_id: ConversationId,
        topic: String,
    ) -> Result<StateUpdate, WorkingStateError> {
        let next_global_version = self
            .global
            .version
            .checked_add(1)
            .ok_or(WorkingStateError::VersionExhausted)?;
        let next_conversation_version =
            self.conversations
                .get(&conversation_id)
                .map_or(Ok(1), |state| {
                    state
                        .version
                        .checked_add(1)
                        .ok_or(WorkingStateError::VersionExhausted)
                })?;

        let mut evicted_conversation = None;
        if !self.conversations.contains_key(&conversation_id)
            && self.conversations.len() >= self.config.max_conversations
            && let Some(evicted) = self.conversation_order.pop_front()
        {
            self.conversations.remove(&evicted);
            evicted_conversation = Some(evicted);
        }

        touch_lru(&mut self.conversation_order, conversation_id);
        let state = self.conversations.entry(conversation_id).or_default();
        state.current_topic = Some(topic);
        state.version = next_conversation_version;
        self.global.version = next_global_version;

        Ok(StateUpdate {
            global_version: next_global_version,
            conversation_id: Some(conversation_id),
            conversation_version: Some(next_conversation_version),
            evicted_conversation,
        })
    }

    /// Removes a resolved durable open-loop reference from all bounded
    /// conversation snapshots. The durable store remains the source of truth;
    /// this only prevents stale references from lingering in working state.
    pub(crate) fn resolve_open_loop_reference(
        &mut self,
        open_loop_id: OpenLoopId,
    ) -> Result<bool, WorkingStateError> {
        let affected: Vec<_> = self
            .conversations
            .iter()
            .filter_map(|(conversation_id, state)| {
                state
                    .open_loops
                    .contains(&open_loop_id)
                    .then_some((*conversation_id, state.version))
            })
            .collect();
        if affected.is_empty() {
            return Ok(false);
        }

        let next_global_version = self
            .global
            .version
            .checked_add(1)
            .ok_or(WorkingStateError::VersionExhausted)?;
        let next_versions: Vec<_> = affected
            .iter()
            .map(|(conversation_id, version)| {
                version
                    .checked_add(1)
                    .map(|next| (*conversation_id, next))
                    .ok_or(WorkingStateError::VersionExhausted)
            })
            .collect::<Result<_, _>>()?;

        for (conversation_id, next_version) in next_versions {
            if let Some(state) = self.conversations.get_mut(&conversation_id) {
                state.open_loops.retain(|id| *id != open_loop_id);
                state.version = next_version;
            }
            touch_lru(&mut self.conversation_order, conversation_id);
        }
        self.global.version = next_global_version;
        Ok(true)
    }

    /// Removes direct-conversation snapshots and the person's identifier from
    /// retained shared-conversation snapshots at a runtime control barrier.
    /// Shared conversation text is retained because it is shared history and
    /// compact events do not retain sender identifiers. The global event log
    /// contains neither message text nor person identifiers. A successful
    /// mutation advances all affected versions so callers cannot mistake a
    /// pre-erasure snapshot for current runtime state.
    pub(crate) fn purge_person_domain(
        &mut self,
        person_id: PersonId,
        conversation_ids: &[ConversationId],
    ) -> Result<usize, WorkingStateError> {
        let mut removed_conversations = Vec::new();
        for conversation_id in conversation_ids
            .iter()
            .copied()
            .filter(|conversation_id| self.conversations.contains_key(conversation_id))
        {
            if !removed_conversations.contains(&conversation_id) {
                removed_conversations.push(conversation_id);
            }
        }
        let affected_retained: Vec<_> = self
            .conversations
            .iter()
            .filter_map(|(conversation_id, state)| {
                (!removed_conversations.contains(conversation_id)
                    && state.active_people.contains(&person_id))
                .then_some((*conversation_id, state.version))
            })
            .collect();
        if removed_conversations.is_empty() && affected_retained.is_empty() {
            return Ok(0);
        }
        let next_global_version = self
            .global
            .version
            .checked_add(1)
            .ok_or(WorkingStateError::VersionExhausted)?;
        let next_retained_versions: Vec<_> = affected_retained
            .iter()
            .map(|(conversation_id, version)| {
                version
                    .checked_add(1)
                    .map(|next| (*conversation_id, next))
                    .ok_or(WorkingStateError::VersionExhausted)
            })
            .collect::<Result<_, _>>()?;

        for conversation_id in &removed_conversations {
            self.conversations.remove(conversation_id);
        }
        self.conversation_order
            .retain(|conversation_id| !removed_conversations.contains(conversation_id));
        for (conversation_id, next_version) in next_retained_versions {
            if let Some(state) = self.conversations.get_mut(&conversation_id) {
                state
                    .active_people
                    .retain(|candidate| *candidate != person_id);
                state.version = next_version;
            }
        }
        self.global.version = next_global_version;
        Ok(removed_conversations.len())
    }

    /// Removes all retained runtime context for canonical conversations.
    /// Advancing the global version invalidates snapshots captured before the
    /// FIFO erasure barrier even when no other conversation is affected.
    pub(crate) fn purge_conversation_domains(
        &mut self,
        conversation_ids: &[ConversationId],
    ) -> Result<usize, WorkingStateError> {
        let removed: Vec<_> = conversation_ids
            .iter()
            .copied()
            .filter(|conversation_id| self.conversations.contains_key(conversation_id))
            .collect();
        if removed.is_empty() {
            return Ok(0);
        }
        let next_global_version = self
            .global
            .version
            .checked_add(1)
            .ok_or(WorkingStateError::VersionExhausted)?;
        for conversation_id in &removed {
            self.conversations.remove(conversation_id);
        }
        self.conversation_order
            .retain(|candidate| !removed.contains(candidate));
        self.global.version = next_global_version;
        Ok(removed.len())
    }

    #[must_use]
    pub fn conversation(&self, id: ConversationId) -> Option<ConversationSnapshot> {
        self.conversations
            .get(&id)
            .map(|state| ConversationSnapshot::from_state(id, state))
    }

    #[must_use]
    pub fn is_snapshot_stale(&self, snapshot: &ConversationSnapshot) -> bool {
        self.conversations
            .get(&snapshot.conversation_id)
            .is_none_or(|state| state.version != snapshot.version)
    }

    #[must_use]
    pub const fn global_version(&self) -> u64 {
        self.global.version
    }

    #[must_use]
    pub fn global_event_count(&self) -> usize {
        self.global.recent_events.len()
    }

    #[must_use]
    pub fn conversation_count(&self) -> usize {
        self.conversations.len()
    }
}

fn touch_lru(order: &mut VecDeque<ConversationId>, id: ConversationId) {
    order.retain(|candidate| *candidate != id);
    order.push_back(id);
}

fn push_bounded<T>(values: &mut VecDeque<T>, value: T, maximum: usize) {
    if values.len() == maximum {
        values.pop_front();
    }
    values.push_back(value);
}

fn push_unique_bounded<T: Copy + PartialEq>(values: &mut VecDeque<T>, value: T, maximum: usize) {
    values.retain(|candidate| *candidate != value);
    push_bounded(values, value, maximum);
}

fn bounded_text(value: &str, max_chars: usize, max_bytes: usize) -> String {
    let mut bounded = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars().take(max_chars) {
        if bounded.len() + character.len_utf8() > max_bytes {
            break;
        }
        bounded.push(character);
    }
    bounded
}

#[cfg(test)]
mod tests {
    use super::{WorkingState, WorkingStateConfig, WorkingStateConfigError, WorkingStateError};
    use crate::attention::AttentionSystem;
    use crate::event::{
        EventPriority, EventScope, MessageContent, MessageReceivedEvent, ProspectiveMemoryEvent,
        WorldEvent, WorldEventKind,
    };
    use crate::identity::{ConversationId, ConversationKind, MessageId, OpenLoopId, PersonId};
    use chrono::Utc;

    fn event(conversation_id: ConversationId, sender: PersonId, text: &str) -> WorldEvent {
        WorldEvent::message_received(
            EventPriority::Normal,
            MessageReceivedEvent {
                message_id: MessageId::new(),
                conversation_id,
                sender,
                content: MessageContent::text(text),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind: ConversationKind::Group,
                addressed_to_agent: false,
                replies_to_agent: false,
                stop_requested: false,
                explicit_request: false,
                visible_reply_allowed: true,
            },
        )
    }

    fn limits() -> WorkingStateConfig {
        WorkingStateConfig {
            max_global_events: 2,
            max_conversations: 2,
            max_conversation_events: 2,
            max_active_people: 2,
            max_open_loop_refs: 2,
            max_compact_text_chars: 4,
            max_compact_text_bytes: 8,
        }
    }

    #[test]
    fn state_is_bounded_and_versions_detect_stale_snapshots() {
        let mut state = WorkingState::new(limits()).expect("valid limits");
        let conversation = ConversationId::new();
        let attention = AttentionSystem;
        for index in 0..4 {
            let event = event(conversation, PersonId::new(), &format!("message {index}"));
            state
                .observe(&event, attention.evaluate(&event))
                .expect("valid observation");
        }

        let snapshot = state
            .conversation(conversation)
            .expect("conversation state");
        assert_eq!(state.global_event_count(), 2);
        assert_eq!(snapshot.recent_events.len(), 2);
        assert_eq!(snapshot.active_people.len(), 2);
        assert_eq!(snapshot.version(), 4);
        assert_eq!(
            snapshot
                .recent_events
                .last()
                .and_then(|event| event.text.as_deref()),
            Some("mess")
        );
        assert!(
            state
                .global
                .recent_events
                .iter()
                .all(|event| event.text.is_none())
        );
        assert!(!state.is_snapshot_stale(&snapshot));

        let next = event(conversation, PersonId::new(), "next");
        state
            .observe(&next, attention.evaluate(&next))
            .expect("valid observation");
        assert!(state.is_snapshot_stale(&snapshot));
    }

    #[test]
    fn conversations_are_isolated_and_lru_eviction_is_bounded() {
        let mut state = WorkingState::new(limits()).expect("valid limits");
        let a = ConversationId::new();
        let b = ConversationId::new();
        let c = ConversationId::new();
        let person_a = PersonId::new();
        let person_b = PersonId::new();
        let attention = AttentionSystem;

        for event in [event(a, person_a, "private"), event(b, person_b, "group")] {
            state
                .observe(&event, attention.evaluate(&event))
                .expect("valid observation");
        }
        let a_snapshot = state.conversation(a).expect("first conversation");
        let b_snapshot = state.conversation(b).expect("second conversation");
        assert_eq!(a_snapshot.active_people, vec![person_a]);
        assert_eq!(b_snapshot.active_people, vec![person_b]);

        // Touch A so B is the least recently used state.
        let touch_a = event(a, person_a, "again");
        state
            .observe(&touch_a, attention.evaluate(&touch_a))
            .expect("valid observation");
        let add_c = event(c, PersonId::new(), "third");
        let update = state
            .observe(&add_c, attention.evaluate(&add_c))
            .expect("valid observation");

        assert_eq!(state.conversation_count(), 2);
        assert!(state.conversation(a).is_some());
        assert!(state.conversation(b).is_none());
        assert!(state.conversation(c).is_some());
        assert_eq!(update.evicted_conversation, Some(b));
    }

    #[test]
    fn mismatched_message_scope_never_updates_another_conversation() {
        let mut state = WorkingState::new(limits()).expect("valid limits");
        let message_conversation = ConversationId::new();
        let wrong_scope = ConversationId::new();
        let message = event(message_conversation, PersonId::new(), "secret");
        let mismatched = WorldEvent::new(
            message.occurred_at(),
            crate::event::EventScope::Conversation {
                conversation_id: wrong_scope,
            },
            message.priority(),
            message.kind().clone(),
        );

        let update = state.observe(&mismatched, AttentionSystem.evaluate(&mismatched));
        assert!(matches!(
            update,
            Err(super::WorkingStateError::InvalidEvent(
                crate::event::EventValidationError::ScopeMismatch
            ))
        ));
        assert!(state.conversation(message_conversation).is_none());
        assert!(state.conversation(wrong_scope).is_none());
        assert_eq!(state.global_event_count(), 0);
        assert!(matches!(message.kind(), WorldEventKind::MessageReceived(_)));
    }

    #[test]
    fn open_loop_references_are_unique_and_bounded() {
        let mut state = WorkingState::new(limits()).expect("valid limits");
        let conversation_id = ConversationId::new();
        let first = OpenLoopId::new();
        let second = OpenLoopId::new();
        let third = OpenLoopId::new();
        let attention = AttentionSystem;

        for open_loop_id in [first, second, second, third] {
            let event = WorldEvent::new(
                Utc::now(),
                EventScope::Conversation { conversation_id },
                EventPriority::Normal,
                WorldEventKind::ProspectiveMemoryDue(ProspectiveMemoryEvent { open_loop_id }),
            );
            state
                .observe(&event, attention.evaluate(&event))
                .expect("valid observation");
        }

        let snapshot = state.conversation(conversation_id).expect("conversation");
        assert_eq!(snapshot.open_loops, vec![second, third]);
    }

    #[test]
    fn snapshots_are_bound_to_their_conversation() {
        let mut state = WorkingState::new(limits()).expect("valid limits");
        let a = ConversationId::new();
        let b = ConversationId::new();
        let attention = AttentionSystem;
        for event in [
            event(a, PersonId::new(), "a"),
            event(b, PersonId::new(), "b"),
        ] {
            state
                .observe(&event, attention.evaluate(&event))
                .expect("valid observation");
        }
        let snapshot_a = state.conversation(a).expect("conversation a");
        let snapshot_b = state.conversation(b).expect("conversation b");
        assert_eq!(snapshot_a.version(), snapshot_b.version());

        let update_a = event(a, PersonId::new(), "a2");
        state
            .observe(&update_a, attention.evaluate(&update_a))
            .expect("valid observation");
        assert!(state.is_snapshot_stale(&snapshot_a));
        assert!(!state.is_snapshot_stale(&snapshot_b));
    }

    #[test]
    fn conversation_kind_cannot_change_for_an_existing_id() {
        let mut state = WorkingState::new(limits()).expect("valid limits");
        let conversation_id = ConversationId::new();
        let group = event(conversation_id, PersonId::new(), "group");
        state
            .observe(&group, AttentionSystem.evaluate(&group))
            .expect("first kind establishes the conversation");
        let direct = match event(conversation_id, PersonId::new(), "direct")
            .kind()
            .clone()
        {
            WorldEventKind::MessageReceived(mut message) => {
                message.conversation_kind = ConversationKind::Direct;
                WorldEvent::message_received(EventPriority::Normal, message)
            }
            _ => unreachable!("fixture is a received message"),
        };

        assert_eq!(
            state.observe(&direct, AttentionSystem.evaluate(&direct)),
            Err(WorkingStateError::ConversationKindMismatch)
        );
        assert_eq!(state.global_version(), 1);
    }

    #[test]
    fn excessive_state_capacities_are_rejected() {
        assert_eq!(
            WorkingState::new(WorkingStateConfig {
                max_conversations: 4_097,
                ..WorkingStateConfig::default()
            })
            .expect_err("excessive capacity must fail"),
            WorkingStateConfigError::CapacityTooLarge {
                field: "max_conversations",
                value: 4_097,
                maximum: 4_096,
            }
        );
    }

    #[test]
    fn compact_conversation_text_is_bounded_by_characters_and_bytes() {
        let mut state = WorkingState::new(limits()).expect("valid limits");
        let conversation_id = ConversationId::new();
        let event = event(conversation_id, PersonId::new(), "芸汐abcdefghijk");
        state
            .observe(&event, AttentionSystem.evaluate(&event))
            .expect("valid observation");

        let snapshot = state.conversation(conversation_id).expect("conversation");
        assert_eq!(snapshot.recent_events[0].text.as_deref(), Some("芸汐ab"));
        assert_eq!(snapshot.recent_events[0].text.as_ref().unwrap().len(), 8);
    }
}
