//! Autonomous conversation loop state.
//!
//! The Core runtime owns the actual planner turn. This module only tracks
//! which conversations recently had a meaningful inbound turn and arbitrates
//! when one may receive an autonomous continuation. Every fresh Core turn
//! decides whether there is another meaningful thing to say.

use crate::config::ProactiveConfig;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use yunxi_core::{
    AutonomyPolicy, ConversationId, ConversationKind, ConversationLifecycle,
    ConversationTurnDirective, PersonId,
};

const MAX_TRACKED_CONVERSATIONS: usize = 512;
const IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone)]
struct ConversationActivity {
    lifecycle: ConversationLifecycle,
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
#[allow(dead_code)]
pub(crate) fn observe_inbound(
    conversation_id: ConversationId,
    kind: ConversationKind,
    occurred_at: DateTime<Utc>,
    eligible: bool,
) {
    observe_inbound_with_person(conversation_id, kind, occurred_at, eligible, None);
}

/// Record an inbound turn together with its stable Core person identity. The
/// identity is optional at the generic boundary so hosts that cannot resolve a
/// sender yet can still participate in the same lifecycle scheduler.
pub(crate) fn observe_inbound_from_person(
    conversation_id: ConversationId,
    kind: ConversationKind,
    occurred_at: DateTime<Utc>,
    eligible: bool,
    person_id: PersonId,
) {
    observe_inbound_with_person(
        conversation_id,
        kind,
        occurred_at,
        eligible,
        Some(person_id),
    );
}

fn observe_inbound_with_person(
    conversation_id: ConversationId,
    kind: ConversationKind,
    occurred_at: DateTime<Utc>,
    eligible: bool,
    person_id: Option<PersonId>,
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
        .or_insert_with(|| ConversationActivity {
            lifecycle: ConversationLifecycle::new(conversation_id, kind)
                .expect("default autonomy lifecycle policy must be valid"),
            in_flight_since: None,
        });
    let observed = match person_id {
        Some(person_id) => entry
            .lifecycle
            .observe_inbound(kind, person_id, occurred_at),
        None => entry.lifecycle.observe_inbound_activity(kind, occurred_at),
    };
    if observed.is_err() {
        return;
    }
    // A new user turn resets the autonomous lifecycle and invalidates any
    // stale heartbeat claim that was waiting behind the incoming message.
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
    if entry.lifecycle.kind() != ConversationKind::Group {
        return;
    }
    if entry.lifecycle.observe_ambient_group(occurred_at).is_err() {
        return;
    }
    entry.in_flight_since = None;
}

/// Record a successfully delivered Core message for a tracked conversation.
/// This is separate from `observe_inbound` so a normal reply can establish the
/// "agent answered, now wait before continuing" boundary.
#[cfg(test)]
pub(crate) fn record_outbound(conversation_id: ConversationId, occurred_at: DateTime<Utc>) {
    let _ = record_outbound_with_directive(conversation_id, occurred_at, None, None);
}

/// Record a normal reply and its model-selected conversation directive.
///
/// A delivered direct-chat reply enters a short continuation cooldown even
/// when its normal-turn directive is `wait`, `end`, or missing. Those values
/// describe the current answer, not an explicit request to close the private
/// conversation; the next autonomous turn still asks the model whether there
/// is a real next thought worth sending. This is based on conversation kind,
/// never message text. Group directives retain their terminal semantics.
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
    let policy = config.map(autonomy_policy).unwrap_or_default();
    let direct_conversation = entry.lifecycle.kind() == ConversationKind::Direct;
    let effective_directive = if direct_conversation {
        Some(ConversationTurnDirective::Continue)
    } else {
        directive
    };
    entry
        .lifecycle
        .record_outbound(occurred_at, effective_directive, policy)
        .ok()?;
    touch(&mut registry.order, conversation_id);
    effective_directive
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
    let policy = autonomy_policy(config);
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
        entry
            .lifecycle
            .autonomous_due(now, policy)
            .is_ok_and(|due| due)
    })?;
    if let Some(entry) = registry.entries.get_mut(&candidate) {
        if entry
            .in_flight_since
            .is_some_and(|started| started.elapsed() >= IN_FLIGHT_TIMEOUT)
        {
            // A stale host lease can be recovered after a process/task crash;
            // the serializable Core lifecycle remains the source of truth.
            entry.in_flight_since = None;
            let _ = entry.lifecycle.release_autonomous_claim();
        }
        if entry.lifecycle.claim_autonomous(now, policy).ok() != Some(true) {
            return None;
        }
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
        let _ = entry.lifecycle.release_autonomous_claim();
    }
}

/// Finish a claimed autonomous turn. A Continue decision schedules another
/// independent Core turn; Wait/End leaves the conversation dormant.
pub(crate) fn finish_claim(
    conversation_id: ConversationId,
    occurred_at: DateTime<Utc>,
    delivered: bool,
    directive: ConversationTurnDirective,
    config: &ProactiveConfig,
) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
        && entry.in_flight_since.take().is_some()
    {
        let _ = entry.lifecycle.finish_autonomous_claim(
            occurred_at,
            delivered,
            directive,
            autonomy_policy(config),
        );
    }
}

fn autonomy_policy(config: &ProactiveConfig) -> AutonomyPolicy {
    AutonomyPolicy {
        direct_idle: chrono::Duration::seconds(
            config.autonomous_conversation_idle_secs().max(1) as i64
        ),
        group_idle: chrono::Duration::seconds(
            config.autonomous_conversation_group_idle_secs().max(1) as i64,
        ),
        direct_cooldown: chrono::Duration::seconds(
            config.autonomous_conversation_cooldown_secs().max(1) as i64,
        ),
        group_cooldown: chrono::Duration::seconds(
            config.autonomous_conversation_group_cooldown_secs().max(1) as i64,
        ),
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
        REGISTRY, claim_due, finish_claim, observe_group_activity, observe_inbound,
        observe_inbound_from_person, record_outbound, record_outbound_with_directive,
        release_claim,
    };
    use crate::config::ProactiveConfig;
    use chrono::{Duration, Utc};
    use std::sync::{LazyLock, Mutex};
    use yunxi_core::{ConversationId, ConversationKind, ConversationTurnDirective, PersonId};

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
        finish_claim(id, now, true, ConversationTurnDirective::Wait, &config);
        assert!(claim_due(&config, now + Duration::seconds(30)).is_none());
        clear();
    }

    #[test]
    fn new_inbound_turn_resets_lifecycle_and_releases_claim() {
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
        finish_claim(id, now, true, ConversationTurnDirective::Continue, &config);
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
        finish_claim(id, now, true, ConversationTurnDirective::End, &config);
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
    fn group_continuation_is_semantic_not_fixed_budgeted() {
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
        finish_claim(id, now, true, ConversationTurnDirective::Continue, &config);
        let next = now + Duration::minutes(5);
        assert_eq!(claim_due(&config, next), Some(id));
        finish_claim(id, next, true, ConversationTurnDirective::Continue, &config);
        assert_eq!(claim_due(&config, next + Duration::minutes(5)), Some(id));
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
        finish_claim(id, now, true, ConversationTurnDirective::Continue, &config);
        assert!(claim_due(&config, now + Duration::minutes(5)).is_none());
        clear();
    }

    #[test]
    fn model_continue_schedules_after_normal_reply_cooldown() {
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
        let cooldown = config.autonomous_conversation_cooldown_secs() as i64;
        assert!(claim_due(&config, now + Duration::seconds(cooldown - 1)).is_none());
        assert_eq!(
            claim_due(&config, now + Duration::seconds(cooldown)),
            Some(id)
        );
        clear();
    }

    #[test]
    fn omitted_directive_defaults_to_direct_continuation() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(id, ConversationKind::Direct, now, true);
        record_outbound_with_directive(id, now, None, Some(&config));
        assert_eq!(
            claim_due(
                &config,
                now + Duration::seconds(config.autonomous_conversation_cooldown_secs() as i64)
            ),
            Some(id)
        );
        clear();
    }

    #[test]
    fn direct_end_is_rechecked_before_terminal() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(id, ConversationKind::Direct, now, true);
        record_outbound_with_directive(
            id,
            now,
            Some(ConversationTurnDirective::End),
            Some(&config),
        );
        assert_eq!(
            claim_due(
                &config,
                now + Duration::seconds(config.autonomous_conversation_cooldown_secs() as i64)
            ),
            Some(id)
        );
        clear();
    }

    #[test]
    fn direct_wait_is_rechecked_before_terminal() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(id, ConversationKind::Direct, now, true);
        record_outbound_with_directive(
            id,
            now,
            Some(ConversationTurnDirective::Wait),
            Some(&config),
        );
        assert_eq!(
            claim_due(
                &config,
                now + Duration::seconds(config.autonomous_conversation_cooldown_secs() as i64)
            ),
            Some(id)
        );
        clear();
    }

    #[test]
    fn direct_autonomous_turn_is_open_ended() {
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

        let cooldown = config.autonomous_conversation_cooldown_secs() as i64;
        let mut follow_up = now + Duration::seconds(cooldown);
        for _ in 0..8 {
            assert_eq!(claim_due(&config, follow_up), Some(id));
            finish_claim(
                id,
                follow_up,
                true,
                ConversationTurnDirective::Continue,
                &config,
            );
            follow_up += Duration::seconds(cooldown);
        }
        assert_eq!(claim_due(&config, follow_up), Some(id));
        finish_claim(id, follow_up, true, ConversationTurnDirective::End, &config);
        assert!(claim_due(&config, follow_up + Duration::hours(1)).is_none());
        clear();
    }

    #[test]
    fn direct_wait_is_rechecked_instead_of_suppressing_autonomy() {
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
        assert_eq!(
            claim_due(
                &config,
                now + Duration::seconds(config.autonomous_conversation_cooldown_secs() as i64)
            ),
            Some(id)
        );
        clear();
    }

    #[test]
    fn failed_autonomous_delivery_releases_claim_without_scheduling_follow_up() {
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
        let due = now + Duration::seconds(config.autonomous_conversation_cooldown_secs() as i64);
        assert_eq!(claim_due(&config, due), Some(id));
        finish_claim(id, due, false, ConversationTurnDirective::Continue, &config);
        assert!(claim_due(&config, due + Duration::hours(1)).is_none());
        clear();
    }

    #[test]
    fn rejected_submission_can_release_and_retry_the_same_heartbeat() {
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
        let due = now + Duration::seconds(config.autonomous_conversation_cooldown_secs() as i64);
        assert_eq!(claim_due(&config, due), Some(id));
        assert!(claim_due(&config, due).is_none());
        release_claim(id);
        assert_eq!(claim_due(&config, due), Some(id));
        finish_claim(id, due, true, ConversationTurnDirective::Wait, &config);
        clear();
    }

    #[test]
    fn person_aware_inbound_keeps_group_social_context_in_core_state() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let first = PersonId::new();
        let second = PersonId::new();
        let now = Utc::now();
        observe_inbound_from_person(id, ConversationKind::Group, now, true, first);
        observe_inbound_from_person(
            id,
            ConversationKind::Group,
            now + Duration::seconds(1),
            true,
            second,
        );
        let registry = REGISTRY.lock().expect("registry lock");
        let people = registry
            .entries
            .get(&id)
            .expect("conversation is tracked")
            .lifecycle
            .active_people()
            .collect::<Vec<_>>();
        assert_eq!(people, vec![first, second]);
        drop(registry);
        clear();
    }
}
