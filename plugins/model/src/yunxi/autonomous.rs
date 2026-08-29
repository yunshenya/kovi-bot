//! Bounded autonomous conversation loop state.
//!
//! The Core runtime owns the actual planner turn. This module only tracks
//! which conversations recently had a meaningful inbound turn and arbitrates
//! when one may receive an autonomous continuation. Keeping this state in the
//! host makes the admission policy explicit and prevents an idle heartbeat
//! from turning into an unbounded self-chat loop.

use crate::config::ProactiveConfig;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use yunxi_core::{ConversationId, ConversationKind, ConversationTurnDirective};

const MAX_TRACKED_CONVERSATIONS: usize = 512;
const IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(180);
const EXPLICIT_CONTINUATION_RETRY_SLACK: u8 = 2;

#[derive(Debug, Clone, Copy)]
struct ConversationActivity {
    kind: ConversationKind,
    last_inbound_at: DateTime<Utc>,
    last_bot_at: Option<DateTime<Utc>>,
    last_autonomous_at: Option<DateTime<Utc>>,
    autonomous_messages: u8,
    autonomous_turns: u8,
    directive: ConversationTurnDirective,
    continuation_decided: bool,
    explicit_continuation_requested: bool,
    explicit_min_autonomous_messages: u8,
    explicit_max_autonomous_turns: u8,
    next_wake_at: Option<DateTime<Utc>>,
    in_flight_since: Option<Instant>,
}

#[derive(Debug, Default)]
struct Registry {
    entries: HashMap<ConversationId, ConversationActivity>,
    order: VecDeque<ConversationId>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

/// Record a high-value inbound turn. Ordinary ambient group observations are
/// deliberately excluded by the caller so they cannot activate autonomous
/// speaking for every busy group.
pub(crate) fn observe_inbound(
    conversation_id: ConversationId,
    kind: ConversationKind,
    occurred_at: DateTime<Utc>,
    eligible: bool,
) {
    if !eligible {
        return;
    }
    let Ok(mut registry) = REGISTRY.lock() else {
        return;
    };
    touch(&mut registry.order, conversation_id);
    if !registry.entries.contains_key(&conversation_id)
        && registry.entries.len() >= MAX_TRACKED_CONVERSATIONS
        && let Some(evicted) = registry.order.pop_front()
    {
        registry.entries.remove(&evicted);
    }
    let entry = registry
        .entries
        .entry(conversation_id)
        .or_insert(ConversationActivity {
            kind,
            last_inbound_at: occurred_at,
            last_bot_at: None,
            last_autonomous_at: None,
            autonomous_messages: 0,
            autonomous_turns: 0,
            directive: ConversationTurnDirective::Wait,
            continuation_decided: false,
            explicit_continuation_requested: false,
            explicit_min_autonomous_messages: 0,
            explicit_max_autonomous_turns: 0,
            next_wake_at: None,
            in_flight_since: None,
        });
    entry.kind = kind;
    entry.last_inbound_at = entry.last_inbound_at.max(occurred_at);
    // A new user turn reopens the autonomous budget and invalidates any stale
    // heartbeat claim that was waiting behind the incoming message.
    entry.autonomous_messages = 0;
    entry.autonomous_turns = 0;
    entry.last_autonomous_at = None;
    entry.directive = ConversationTurnDirective::Wait;
    entry.continuation_decided = false;
    entry.explicit_continuation_requested = false;
    entry.explicit_min_autonomous_messages = 0;
    entry.explicit_max_autonomous_turns = 0;
    entry.next_wake_at = None;
    entry.in_flight_since = None;
}

/// Record ambient group activity without activating autonomous conversation.
/// A new group message means the old continuation may no longer be relevant,
/// so any pending claim is cancelled before the next scheduler pass.
pub(crate) fn observe_group_activity(conversation_id: ConversationId, occurred_at: DateTime<Utc>) {
    let Ok(mut registry) = REGISTRY.lock() else {
        return;
    };
    let Some(entry) = registry.entries.get_mut(&conversation_id) else {
        return;
    };
    if entry.kind != ConversationKind::Group || occurred_at <= entry.last_inbound_at {
        return;
    }
    entry.last_inbound_at = occurred_at;
    entry.autonomous_messages = 0;
    entry.autonomous_turns = 0;
    entry.last_autonomous_at = None;
    entry.directive = ConversationTurnDirective::Wait;
    entry.continuation_decided = false;
    entry.explicit_continuation_requested = false;
    entry.explicit_min_autonomous_messages = 0;
    entry.explicit_max_autonomous_turns = 0;
    entry.next_wake_at = None;
    entry.in_flight_since = None;
}

/// Record a successfully delivered Core message for a tracked conversation.
/// This is separate from `observe_inbound` so a normal reply can establish the
/// "agent answered, now wait before continuing" boundary.
pub(crate) fn record_outbound(conversation_id: ConversationId, occurred_at: DateTime<Utc>) {
    let _ = record_outbound_with_directive(conversation_id, occurred_at, None, None);
}

/// Record a normal reply that explicitly opted into a later, distinct
/// continuation. The model must opt in; the registry only schedules the next
/// turn and never infers continuation from message punctuation or length.
pub(crate) fn record_outbound_with_directive(
    conversation_id: ConversationId,
    occurred_at: DateTime<Utc>,
    directive: Option<ConversationTurnDirective>,
    config: Option<&ProactiveConfig>,
) -> Option<ConversationTurnDirective> {
    let Ok(mut registry) = REGISTRY.lock() else {
        return None;
    };
    let entry = registry.entries.get_mut(&conversation_id)?;
    entry.last_bot_at = Some(
        entry
            .last_bot_at
            .map_or(occurred_at, |current| current.max(occurred_at)),
    );
    let directive = if entry.explicit_continuation_requested {
        Some(ConversationTurnDirective::Continue)
    } else {
        directive
    };
    if let (Some(directive), Some(config)) = (directive, config) {
        let continue_delay_secs = match entry.kind {
            ConversationKind::Direct => config.autonomous_conversation_cooldown_secs(),
            ConversationKind::Group => config.autonomous_conversation_group_cooldown_secs(),
            ConversationKind::System => 0,
        };
        entry.directive = directive;
        entry.continuation_decided = true;
        entry.next_wake_at = match directive {
            ConversationTurnDirective::Continue => {
                Some(occurred_at + chrono::Duration::seconds(continue_delay_secs.max(1) as i64))
            }
            ConversationTurnDirective::Wait | ConversationTurnDirective::End => None,
        };
    }
    touch(&mut registry.order, conversation_id);
    directive
}

/// Mark a direct-message request for multiple independent follow-up turns.
/// The first two follow-ups establish a real three-message sequence; any
/// remaining budget is optional and still receives a fresh model decision.
pub(crate) fn request_continuation(conversation_id: ConversationId, max_autonomous_turns: u8) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
        && entry.kind == ConversationKind::Direct
    {
        let max_autonomous_turns = max_autonomous_turns.max(1);
        entry.explicit_continuation_requested = true;
        entry.explicit_min_autonomous_messages = max_autonomous_turns.min(2);
        entry.explicit_max_autonomous_turns = max_autonomous_turns;
    }
}

pub(crate) fn explicit_continuation_request(text: &str) -> Option<u8> {
    let normalized = text.split_whitespace().collect::<String>();
    if normalized.is_empty() || !normalized.contains("连续") {
        return None;
    }
    let mentions_messages = ["条", "消息", "气泡", "回合", "轮", "句"]
        .iter()
        .any(|marker| normalized.contains(marker));
    let bounded = normalized.contains("最多")
        || normalized.contains("三四")
        || normalized.contains("几条")
        || normalized.contains("多条")
        || normalized.contains("多轮")
        || normalized.contains("一条一条")
        || normalized.contains("一个一个");
    if !mentions_messages || !bounded {
        return None;
    }
    let arabic_max = normalized
        .chars()
        .filter_map(|character| character.to_digit(10))
        .filter_map(|value| u8::try_from(value).ok())
        .filter(|value| (2..=8).contains(value))
        .max();
    let chinese_max = [
        ('二', 2),
        ('两', 2),
        ('三', 3),
        ('四', 4),
        ('五', 5),
        ('六', 6),
        ('七', 7),
        ('八', 8),
    ]
    .into_iter()
    .filter_map(|(character, value)| normalized.contains(character).then_some(value))
    .max();
    let max_total_messages = arabic_max.or(chinese_max).unwrap_or(4);
    Some(max_total_messages.saturating_sub(1))
}

/// Claim at most one eligible autonomous turn per call. The scheduler calls
/// this frequently, but the registry itself remains the single concurrency
/// boundary when multiple background tasks wake together.
pub(crate) fn claim_due(config: &ProactiveConfig, now: DateTime<Utc>) -> Option<ConversationId> {
    if !config.autonomous_conversation_enabled() {
        return None;
    }
    let Ok(mut registry) = REGISTRY.lock() else {
        return None;
    };
    let candidate = registry.order.iter().copied().find(|conversation_id| {
        let Some(entry) = registry.entries.get(conversation_id) else {
            return false;
        };
        let (idle_secs, max_turns) = match entry.kind {
            ConversationKind::Direct => (
                config.autonomous_conversation_idle_secs(),
                if entry.explicit_continuation_requested {
                    config
                        .autonomous_conversation_max_turns()
                        .min(entry.explicit_max_autonomous_turns)
                } else {
                    config.autonomous_conversation_max_turns()
                },
            ),
            ConversationKind::Group => (
                config.autonomous_conversation_group_idle_secs(),
                config.autonomous_conversation_group_max_turns(),
            ),
            ConversationKind::System => return false,
        };
        let idle = chrono::Duration::seconds(idle_secs as i64);
        if entry
            .in_flight_since
            .is_some_and(|started| started.elapsed() < IN_FLIGHT_TIMEOUT)
        {
            return false;
        }
        let Some(last_bot_at) = entry.last_bot_at else {
            return false;
        };
        let exhausted = if entry.explicit_continuation_requested {
            entry.autonomous_messages >= max_turns
                || entry.autonomous_turns
                    >= max_turns.saturating_add(EXPLICIT_CONTINUATION_RETRY_SLACK)
        } else {
            entry.autonomous_turns >= max_turns
        };
        if exhausted || last_bot_at < entry.last_inbound_at {
            return false;
        }
        if entry.last_autonomous_at.is_none() {
            if entry.continuation_decided {
                return entry.directive == ConversationTurnDirective::Continue
                    && entry.next_wake_at.is_some_and(|wake| now >= wake);
            }
            return now - entry.last_inbound_at >= idle;
        }
        entry.directive == ConversationTurnDirective::Continue
            && entry.next_wake_at.is_some_and(|wake| now >= wake)
    })?;
    if let Some(entry) = registry.entries.get_mut(&candidate) {
        entry.in_flight_since = Some(Instant::now());
    }
    Some(candidate)
}

/// Release a claim when Core could not admit the event. A short retry is safe
/// because the next scheduler pass still applies the same idle/cooldown gates.
pub(crate) fn release_claim(conversation_id: ConversationId) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
    {
        entry.in_flight_since = None;
    }
}

pub(crate) fn explicit_continuation_claimed(conversation_id: ConversationId) -> bool {
    REGISTRY.lock().is_ok_and(|registry| {
        registry.entries.get(&conversation_id).is_some_and(|entry| {
            entry.in_flight_since.is_some() && entry.explicit_continuation_requested
        })
    })
}

pub(crate) fn explicit_continuation_message_required_claimed(
    conversation_id: ConversationId,
) -> bool {
    REGISTRY.lock().is_ok_and(|registry| {
        registry.entries.get(&conversation_id).is_some_and(|entry| {
            entry.in_flight_since.is_some()
                && entry.explicit_continuation_requested
                && entry.autonomous_messages < entry.explicit_min_autonomous_messages
        })
    })
}

/// Finish a claimed autonomous turn. Attempts remain bounded separately from
/// delivered messages so a transient silence cannot satisfy an explicit
/// request without producing a real QQ bubble.
pub(crate) fn finish_claim(
    conversation_id: ConversationId,
    occurred_at: DateTime<Utc>,
    directive: ConversationTurnDirective,
    delivered: bool,
    config: &ProactiveConfig,
) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
        && entry.in_flight_since.take().is_some()
    {
        let continue_delay_secs = match entry.kind {
            ConversationKind::Direct => config.autonomous_conversation_cooldown_secs(),
            ConversationKind::Group => config.autonomous_conversation_group_cooldown_secs(),
            ConversationKind::System => 0,
        };
        entry.last_autonomous_at = Some(occurred_at);
        entry.autonomous_turns = entry.autonomous_turns.saturating_add(1);
        if delivered {
            entry.autonomous_messages = entry.autonomous_messages.saturating_add(1);
        }
        let minimum_messages_pending = entry.explicit_continuation_requested
            && entry.autonomous_messages < entry.explicit_min_autonomous_messages;
        let directive = if minimum_messages_pending {
            ConversationTurnDirective::Continue
        } else {
            directive
        };
        entry.directive = directive;
        entry.continuation_decided = true;
        entry.next_wake_at = match directive {
            ConversationTurnDirective::Continue => {
                Some(occurred_at + chrono::Duration::seconds(continue_delay_secs.max(1) as i64))
            }
            ConversationTurnDirective::Wait | ConversationTurnDirective::End => None,
        };
    }
}

pub(crate) fn forget(conversation_ids: &[ConversationId]) {
    if let Ok(mut registry) = REGISTRY.lock() {
        for conversation_id in conversation_ids {
            registry.entries.remove(conversation_id);
        }
        registry
            .order
            .retain(|conversation_id| !conversation_ids.contains(conversation_id));
    }
}

fn touch(order: &mut VecDeque<ConversationId>, conversation_id: ConversationId) {
    order.retain(|candidate| *candidate != conversation_id);
    order.push_back(conversation_id);
}

#[cfg(test)]
mod tests {
    use super::{
        REGISTRY, claim_due, explicit_continuation_message_required_claimed,
        explicit_continuation_request, finish_claim, observe_group_activity, observe_inbound,
        record_outbound, record_outbound_with_directive, request_continuation,
    };
    use crate::config::ProactiveConfig;
    use chrono::{Duration, Utc};
    use std::sync::{LazyLock, Mutex};
    use yunxi_core::{ConversationId, ConversationKind, ConversationTurnDirective};

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn clear() {
        let mut registry = REGISTRY.lock().expect("registry lock");
        registry.entries.clear();
        registry.order.clear();
    }

    #[test]
    fn autonomous_claim_requires_a_completed_inbound_reply() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        observe_inbound(
            id,
            ConversationKind::Direct,
            now - Duration::minutes(5),
            true,
        );
        let config = ProactiveConfig::default();
        assert!(claim_due(&config, now).is_none());
        record_outbound(id, now - Duration::minutes(4));
        assert_eq!(claim_due(&config, now), Some(id));
        finish_claim(id, now, ConversationTurnDirective::Wait, false, &config);
        assert!(claim_due(&config, now + Duration::seconds(30)).is_none());
        clear();
    }

    #[test]
    fn new_inbound_turn_reopens_budget_and_releases_claim() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(
            id,
            ConversationKind::Group,
            now - Duration::minutes(4),
            true,
        );
        record_outbound(id, now - Duration::minutes(4));
        assert_eq!(claim_due(&config, now), Some(id));
        observe_inbound(id, ConversationKind::Group, now, true);
        finish_claim(id, now, ConversationTurnDirective::Continue, false, &config);
        record_outbound(id, now);
        assert_eq!(claim_due(&config, now + Duration::minutes(4)), Some(id));
        clear();
    }

    #[test]
    fn end_directive_waits_for_a_new_inbound_turn() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(
            id,
            ConversationKind::Direct,
            now - Duration::minutes(2),
            true,
        );
        record_outbound(id, now - Duration::minutes(2));
        assert_eq!(claim_due(&config, now), Some(id));
        finish_claim(id, now, ConversationTurnDirective::End, false, &config);
        assert!(claim_due(&config, now + Duration::hours(2)).is_none());
        observe_inbound(id, ConversationKind::Direct, now + Duration::hours(2), true);
        record_outbound(id, now + Duration::hours(2));
        assert_eq!(
            claim_due(&config, now + Duration::hours(2) + Duration::minutes(2)),
            Some(id)
        );
        clear();
    }

    #[test]
    fn group_budget_is_more_conservative_than_direct_budget() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(
            id,
            ConversationKind::Group,
            now - Duration::minutes(4),
            true,
        );
        record_outbound(id, now - Duration::minutes(4));
        assert_eq!(claim_due(&config, now), Some(id));
        finish_claim(id, now, ConversationTurnDirective::Continue, true, &config);
        assert!(claim_due(&config, now + Duration::minutes(5)).is_none());
        clear();
    }

    #[test]
    fn ambient_group_activity_cancels_a_pending_continuation() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(
            id,
            ConversationKind::Group,
            now - Duration::minutes(5),
            true,
        );
        record_outbound(id, now - Duration::minutes(4));
        assert_eq!(claim_due(&config, now), Some(id));
        observe_group_activity(id, now);
        finish_claim(id, now, ConversationTurnDirective::Continue, false, &config);
        assert!(claim_due(&config, now + Duration::minutes(5)).is_none());
        clear();
    }

    #[test]
    fn explicit_continuation_schedules_after_normal_reply_cooldown() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(id, ConversationKind::Direct, now, true);
        record_outbound_with_directive(
            id,
            now,
            Some(ConversationTurnDirective::Continue),
            Some(&config),
        );
        assert!(claim_due(&config, now + Duration::seconds(14)).is_none());
        assert_eq!(claim_due(&config, now + Duration::seconds(15)), Some(id));
        clear();
    }

    #[test]
    fn explicit_continuation_request_is_narrowly_detected() {
        assert_eq!(
            explicit_continuation_request("请给我三四条连续消息，每条都重新判断有没有必要说"),
            Some(3)
        );
        assert_eq!(
            explicit_continuation_request("最多连续 4 轮，每轮一个独立想法"),
            Some(3)
        );
        assert_eq!(explicit_continuation_request("我们连续聊了很久"), None);
        assert_eq!(explicit_continuation_request("请把这句话说完整"), None);
    }

    #[test]
    fn explicit_request_falls_back_to_continue_when_model_omits_a_directive() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(id, ConversationKind::Direct, now, true);
        request_continuation(id, 3);
        record_outbound_with_directive(id, now, None, Some(&config));
        assert_eq!(claim_due(&config, now + Duration::seconds(15)), Some(id));
        clear();
    }

    #[test]
    fn explicit_request_produces_two_fresh_follow_up_messages_before_waiting() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(id, ConversationKind::Direct, now, true);
        request_continuation(id, 3);
        record_outbound_with_directive(id, now, None, Some(&config));

        let first_follow_up = now + Duration::seconds(15);
        assert_eq!(claim_due(&config, first_follow_up), Some(id));
        assert!(explicit_continuation_message_required_claimed(id));
        finish_claim(
            id,
            first_follow_up,
            ConversationTurnDirective::Wait,
            true,
            &config,
        );

        let second_follow_up = first_follow_up + Duration::seconds(15);
        assert_eq!(claim_due(&config, second_follow_up), Some(id));
        assert!(explicit_continuation_message_required_claimed(id));
        finish_claim(
            id,
            second_follow_up,
            ConversationTurnDirective::Wait,
            true,
            &config,
        );

        assert!(claim_due(&config, second_follow_up + Duration::hours(1)).is_none());
        clear();
    }

    #[test]
    fn explicit_silence_is_retried_without_counting_as_a_message() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(id, ConversationKind::Direct, now, true);
        request_continuation(id, 3);
        record_outbound_with_directive(id, now, None, Some(&config));

        let first_attempt = now + Duration::seconds(15);
        assert_eq!(claim_due(&config, first_attempt), Some(id));
        finish_claim(
            id,
            first_attempt,
            ConversationTurnDirective::Wait,
            false,
            &config,
        );

        let retry = first_attempt + Duration::seconds(15);
        assert_eq!(claim_due(&config, retry), Some(id));
        assert!(explicit_continuation_message_required_claimed(id));
        clear();
    }

    #[test]
    fn explicit_wait_suppresses_legacy_idle_fallback() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(id, ConversationKind::Direct, now - Duration::hours(2), true);
        record_outbound_with_directive(
            id,
            now,
            Some(ConversationTurnDirective::Wait),
            Some(&config),
        );
        assert!(claim_due(&config, now + Duration::hours(2)).is_none());
        clear();
    }
}
