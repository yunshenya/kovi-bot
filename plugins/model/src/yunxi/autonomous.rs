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
use yunxi_core::{ConversationId, ConversationKind};

const MAX_TRACKED_CONVERSATIONS: usize = 512;
const IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Copy)]
struct ConversationActivity {
    kind: ConversationKind,
    last_inbound_at: DateTime<Utc>,
    last_bot_at: Option<DateTime<Utc>>,
    last_autonomous_at: Option<DateTime<Utc>>,
    autonomous_turns: u8,
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
            autonomous_turns: 0,
            in_flight_since: None,
        });
    entry.kind = kind;
    entry.last_inbound_at = entry.last_inbound_at.max(occurred_at);
    // A new user turn reopens the autonomous budget and invalidates any stale
    // heartbeat claim that was waiting behind the incoming message.
    entry.autonomous_turns = 0;
    entry.last_autonomous_at = None;
    entry.in_flight_since = None;
}

/// Record a successfully delivered Core message for a tracked conversation.
/// This is separate from `observe_inbound` so a normal reply can establish the
/// "agent answered, now wait before continuing" boundary.
pub(crate) fn record_outbound(conversation_id: ConversationId, occurred_at: DateTime<Utc>) {
    let Ok(mut registry) = REGISTRY.lock() else {
        return;
    };
    let Some(entry) = registry.entries.get_mut(&conversation_id) else {
        return;
    };
    entry.last_bot_at = Some(
        entry
            .last_bot_at
            .map_or(occurred_at, |current| current.max(occurred_at)),
    );
    touch(&mut registry.order, conversation_id);
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
    let idle = chrono::Duration::seconds(config.autonomous_conversation_idle_secs() as i64);
    let cooldown = chrono::Duration::seconds(config.autonomous_conversation_cooldown_secs() as i64);
    let max_turns = config.autonomous_conversation_max_turns();
    let candidate = registry.order.iter().copied().find(|conversation_id| {
        let Some(entry) = registry.entries.get(conversation_id) else {
            return false;
        };
        if entry
            .in_flight_since
            .is_some_and(|started| started.elapsed() < IN_FLIGHT_TIMEOUT)
        {
            return false;
        }
        let Some(last_bot_at) = entry.last_bot_at else {
            return false;
        };
        if entry.autonomous_turns >= max_turns || last_bot_at < entry.last_inbound_at {
            return false;
        }
        if now - entry.last_inbound_at < idle {
            return false;
        }
        entry
            .last_autonomous_at
            .is_none_or(|last| now - last >= cooldown)
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

/// Finish a claimed autonomous turn. Even a model-selected silence consumes a
/// turn budget, preventing a silent model from being polled on every heartbeat.
pub(crate) fn finish_claim(conversation_id: ConversationId, occurred_at: DateTime<Utc>) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
        && entry.in_flight_since.take().is_some()
    {
        entry.last_autonomous_at = Some(occurred_at);
        entry.autonomous_turns = entry.autonomous_turns.saturating_add(1);
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
    use super::{REGISTRY, claim_due, finish_claim, observe_inbound, record_outbound};
    use crate::config::ProactiveConfig;
    use chrono::{Duration, Utc};
    use std::sync::{LazyLock, Mutex};
    use yunxi_core::{ConversationId, ConversationKind};

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
        finish_claim(id, now);
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
            now - Duration::minutes(2),
            true,
        );
        record_outbound(id, now - Duration::minutes(2));
        assert_eq!(claim_due(&config, now), Some(id));
        observe_inbound(id, ConversationKind::Group, now, true);
        finish_claim(id, now);
        record_outbound(id, now);
        assert_eq!(claim_due(&config, now + Duration::minutes(2)), Some(id));
        clear();
    }
}
