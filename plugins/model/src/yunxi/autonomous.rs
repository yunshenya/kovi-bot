//! Autonomous conversation loop state.
//!
//! The Core runtime owns the actual planner turn. This module only tracks
//! which conversations recently had a meaningful inbound turn and arbitrates
//! when one may receive an autonomous continuation. Every fresh Core turn
//! decides whether there is another meaningful thing to say.

use crate::config::{ProactiveConfig, ServerConfig, TrafficConfig};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use yunxi_core::{
    AutonomyPolicy, ConversationId, ConversationKind, ConversationLifecycle,
    ConversationTurnDirective, PersonId,
};

const MAX_TRACKED_CONVERSATIONS: usize = 512;
/// Minimum lease retained for compatibility with deployments that disable the
/// external model. With Strong enabled the effective lease is calculated from
/// the actual queue/request/retry budgets below.
const MIN_IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(180);
/// A Strong autonomous turn may make one side-effect-free intent call and one
/// final generation call. Keep both inside one host claim so a slow upstream
/// cannot create a second concurrent claim for the same conversation.
const AUTONOMOUS_MODEL_PHASES: u32 = 2;
const IN_FLIGHT_LEASE_MARGIN: Duration = Duration::from_secs(30);
const TRANSIENT_FAILURE_RETRY_BACKOFF: Duration = Duration::from_secs(5);
const MAX_TRANSIENT_FAILURE_RETRIES: u8 = 6;

/// Return the sum of the retry sleeps used by `model/utils.rs` for a single
/// gateway call. The gateway uses 350ms * 2^min(attempt, 4) after every
/// retryable attempt, so this intentionally mirrors that bounded schedule.
fn model_retry_backoff(retries: u8) -> Duration {
    let mut millis = 0_u64;
    for attempt in 0..u32::from(retries) {
        let exponent = attempt.min(4);
        let delay = 350_u64.saturating_mul(1_u64 << exponent);
        millis = millis.saturating_add(delay);
    }
    Duration::from_millis(millis)
}

/// Upper bound for one external model gateway invocation, excluding planner
/// bookkeeping. Queue acquisition happens once per invocation, while the
/// request timeout and retry sleeps apply to each configured attempt.
fn autonomous_model_phase_budget(
    server_config: &ServerConfig,
    traffic_config: &TrafficConfig,
) -> Duration {
    let attempts = u32::from(server_config.max_retries()).saturating_add(1);
    Duration::from_secs(traffic_config.model_queue_timeout_secs())
        .saturating_add(
            Duration::from_secs(server_config.request_timeout_secs()).saturating_mul(attempts),
        )
        .saturating_add(model_retry_backoff(server_config.max_retries()))
}

/// Compute the host-side lease from the same limits used by the model gateway.
/// A floor keeps crash recovery bounded in local/Intrinsic-only mode; the
/// margin covers planner, persistence, and action-arbiter bookkeeping after
/// the final HTTP response arrives.
fn autonomous_in_flight_timeout_for(
    server_config: &ServerConfig,
    traffic_config: &TrafficConfig,
) -> Duration {
    let phase = autonomous_model_phase_budget(server_config, traffic_config);
    let configured = phase
        .saturating_mul(AUTONOMOUS_MODEL_PHASES)
        .saturating_add(IN_FLIGHT_LEASE_MARGIN);
    configured.max(MIN_IN_FLIGHT_TIMEOUT)
}

fn autonomous_in_flight_timeout() -> Duration {
    let config = crate::config::get();
    autonomous_in_flight_timeout_for(config.server_config(), config.traffic())
}

#[derive(Debug, Clone)]
struct ConversationActivity {
    lifecycle: ConversationLifecycle,
    in_flight_since: Option<Instant>,
    in_flight_token: Option<u64>,
    retry_after: Option<Instant>,
    retry_attempts: u8,
    suspended: bool,
}

/// Immutable routing context captured at the same time as an autonomous
/// lifecycle claim. The context travels with the tick so Core can still
/// resolve a direct recipient after its bounded working-state snapshot has
/// evicted the original inbound event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutonomousConversationClaim {
    pub(crate) conversation_id: ConversationId,
    pub(crate) conversation_kind: ConversationKind,
    pub(crate) person_id: Option<PersonId>,
    pub(crate) token: u64,
}

#[derive(Debug, Default)]
struct Registry {
    entries: HashMap<ConversationId, ConversationActivity>,
    order: VecDeque<ConversationId>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
static NEXT_CLAIM_TOKEN: AtomicU64 = AtomicU64::new(1);

fn next_claim_token() -> u64 {
    loop {
        let current = NEXT_CLAIM_TOKEN.load(Ordering::Relaxed);
        let next = current.checked_add(1).unwrap_or(1);
        if NEXT_CLAIM_TOKEN
            .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current.max(1);
        }
    }
}

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
            in_flight_token: None,
            retry_after: None,
            retry_attempts: 0,
            suspended: false,
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
    entry.in_flight_token = None;
    entry.retry_after = None;
    entry.retry_attempts = 0;
    entry.suspended = false;
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
    entry.in_flight_token = None;
    entry.retry_after = None;
    entry.retry_attempts = 0;
    entry.suspended = false;
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
    entry.retry_after = None;
    entry.retry_attempts = 0;
    entry.suspended = false;
    touch(&mut registry.order, conversation_id);
    effective_directive
}

/// Claim at most one eligible autonomous turn per call and return the stable
/// routing context captured with that claim. The scheduler calls this
/// frequently, but the registry itself remains the single concurrency
/// boundary when multiple background tasks wake together.
pub(crate) fn claim_due_with_context(
    config: &ProactiveConfig,
    now: DateTime<Utc>,
) -> Option<AutonomousConversationClaim> {
    if !config.autonomous_conversation_enabled() {
        return None;
    }
    // Compute this before taking the registry lock. Configuration access and
    // registry mutation are independent locks; keeping their order stable
    // avoids a lock inversion with config reload paths.
    let in_flight_timeout = autonomous_in_flight_timeout();
    let Ok(mut registry) = REGISTRY.lock() else {
        return None;
    };
    let policy = autonomy_policy(config);
    let stale = registry
        .order
        .iter()
        .copied()
        .filter(|conversation_id| {
            registry
                .entries
                .get(conversation_id)
                .and_then(|entry| entry.in_flight_since)
                .is_some_and(|started| started.elapsed() >= in_flight_timeout)
        })
        .collect::<Vec<_>>();
    for conversation_id in stale {
        let Some(entry) = registry.entries.get_mut(&conversation_id) else {
            continue;
        };
        // A stale lease represents a failed host/model attempt just like an
        // explicit retry result. Invalidate its token and consume the same
        // bounded retry budget before another scheduler pass may reclaim it.
        entry.in_flight_since = None;
        entry.in_flight_token = None;
        let _ = entry.lifecycle.release_autonomous_claim();
        schedule_retry(entry);
    }
    let candidate = registry.order.iter().copied().find(|conversation_id| {
        let Some(entry) = registry.entries.get(conversation_id) else {
            return false;
        };
        if entry
            .in_flight_since
            .is_some_and(|started| started.elapsed() < in_flight_timeout)
        {
            return false;
        }
        if entry
            .retry_after
            .is_some_and(|retry_after| Instant::now() < retry_after)
        {
            return false;
        }
        if entry.suspended {
            return false;
        }
        entry
            .lifecycle
            .autonomous_due(now, policy)
            .is_ok_and(|due| due)
    })?;
    let (conversation_kind, person_id, token) = {
        let entry = registry.entries.get_mut(&candidate)?;
        if entry.lifecycle.claim_autonomous(now, policy).ok() != Some(true) {
            return None;
        }
        entry.in_flight_since = Some(Instant::now());
        let token = next_claim_token();
        entry.in_flight_token = Some(token);
        entry.retry_after = None;
        let conversation_kind = entry.lifecycle.kind();
        let person_id = (conversation_kind == ConversationKind::Direct)
            .then(|| entry.lifecycle.active_people().last())
            .flatten();
        (conversation_kind, person_id, token)
    };
    // Move the claimed conversation to the back before returning. A session
    // that keeps selecting Continue must not monopolize the scheduler and
    // starve other due conversations.
    touch(&mut registry.order, candidate);
    Some(AutonomousConversationClaim {
        conversation_id: candidate,
        conversation_kind,
        person_id,
        token,
    })
}

/// Compatibility wrapper for callers that only need the conversation ID.
#[allow(dead_code)]
pub(crate) fn claim_due(config: &ProactiveConfig, now: DateTime<Utc>) -> Option<ConversationId> {
    claim_due_with_context(config, now).map(|claim| claim.conversation_id)
}

/// Release a claim when Core could not admit the event. A short retry is safe
/// because the next scheduler pass still applies the same idle/cooldown gates.
pub(crate) fn release_claim(conversation_id: ConversationId) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
    {
        entry.in_flight_since = None;
        entry.in_flight_token = None;
        let _ = entry.lifecycle.release_autonomous_claim();
    }
}

/// Release a claim only when the caller still owns its token. A late result
/// from an older tick must never release a newer claim created after an
/// intervening inbound message.
pub(crate) fn release_claim_token(conversation_id: ConversationId, token: u64) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
        && entry.in_flight_token == Some(token)
    {
        entry.in_flight_since = None;
        entry.in_flight_token = None;
        let _ = entry.lifecycle.release_autonomous_claim();
    }
}

/// Returns whether a queued/active tick still owns the current host claim.
/// This is used as a cancellation guard while Core is waiting on a model so a
/// newer inbound turn can invalidate the old tick before it reaches delivery.
pub(crate) fn claim_is_current(conversation_id: ConversationId, token: u64) -> bool {
    REGISTRY
        .lock()
        .ok()
        .and_then(|registry| {
            registry
                .entries
                .get(&conversation_id)
                .map(|entry| entry.in_flight_token == Some(token))
        })
        .unwrap_or(false)
}

/// Release a claim after a transient Core/model/transport failure. Unlike a
/// plain release, this adds a bounded retry backoff so a failing upstream
/// cannot turn the scheduler's frequent heartbeat into a hot loop.
pub(crate) fn retry_claim(conversation_id: ConversationId) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
        && entry.in_flight_since.take().is_some()
    {
        entry.in_flight_token = None;
        let _ = entry.lifecycle.release_autonomous_claim();
        schedule_retry(entry);
    }
}

/// Token-checked variant used by the production runtime.
pub(crate) fn retry_claim_token(conversation_id: ConversationId, token: u64) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
        && entry.in_flight_token == Some(token)
    {
        entry.in_flight_since = None;
        entry.in_flight_token = None;
        let _ = entry.lifecycle.release_autonomous_claim();
        schedule_retry(entry);
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
        entry.in_flight_token = None;
        entry.retry_after = None;
        entry.retry_attempts = 0;
        entry.suspended = false;
        let _ = entry.lifecycle.finish_autonomous_claim(
            occurred_at,
            delivered,
            directive,
            autonomy_policy(config),
        );
    }
}

/// Token-checked variant used by the production runtime.
pub(crate) fn finish_claim_token(
    conversation_id: ConversationId,
    token: u64,
    occurred_at: DateTime<Utc>,
    delivered: bool,
    directive: ConversationTurnDirective,
    config: &ProactiveConfig,
) {
    if let Ok(mut registry) = REGISTRY.lock()
        && let Some(entry) = registry.entries.get_mut(&conversation_id)
        && entry.in_flight_token == Some(token)
    {
        entry.in_flight_since = None;
        entry.in_flight_token = None;
        entry.retry_after = None;
        entry.retry_attempts = 0;
        entry.suspended = false;
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

fn schedule_retry(entry: &mut ConversationActivity) {
    entry.retry_attempts = entry.retry_attempts.saturating_add(1);
    if entry.retry_attempts >= MAX_TRANSIENT_FAILURE_RETRIES {
        // A broken upstream must not keep consuming the runtime forever. A
        // fresh inbound turn clears this suspension and starts a new session.
        entry.suspended = true;
        entry.retry_after = None;
        return;
    }
    let exponent = u32::from(entry.retry_attempts.saturating_sub(1)).min(6);
    let multiplier = 1_u64 << exponent;
    let seconds = TRANSIENT_FAILURE_RETRY_BACKOFF
        .as_secs()
        .saturating_mul(multiplier)
        .min(300);
    entry.retry_after = Some(Instant::now() + Duration::from_secs(seconds));
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
        MAX_TRANSIENT_FAILURE_RETRIES, REGISTRY, autonomous_in_flight_timeout,
        autonomous_in_flight_timeout_for, autonomous_model_phase_budget, claim_due,
        claim_due_with_context, claim_is_current, finish_claim, finish_claim_token,
        model_retry_backoff, observe_group_activity, observe_inbound, observe_inbound_from_person,
        record_outbound, record_outbound_with_directive, release_claim, release_claim_token,
        retry_claim, retry_claim_token,
    };
    use crate::config::ProactiveConfig;
    use chrono::{Duration, Utc};
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration as StdDuration, Instant};
    use yunxi_core::{ConversationId, ConversationKind, ConversationTurnDirective, PersonId};

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn lease_budget_covers_two_sequential_gateway_phases() {
        let server = crate::config::ServerConfig::default();
        let traffic = crate::config::TrafficConfig::default();
        let phase = autonomous_model_phase_budget(&server, &traffic);
        let lease = autonomous_in_flight_timeout_for(&server, &traffic);
        assert!(lease >= phase.saturating_mul(2));
        assert!(lease >= StdDuration::from_secs(180));
    }

    #[test]
    fn retry_backoff_matches_gateway_schedule() {
        assert_eq!(model_retry_backoff(0), StdDuration::ZERO);
        assert_eq!(model_retry_backoff(1), StdDuration::from_millis(350));
        assert_eq!(model_retry_backoff(2), StdDuration::from_millis(1_050));
        assert_eq!(model_retry_backoff(5), StdDuration::from_millis(10_850));
    }

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

    #[test]
    fn claim_carries_direct_routing_context() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let person = PersonId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound_from_person(
            id,
            ConversationKind::Direct,
            now - Duration::minutes(5),
            true,
            person,
        );
        record_outbound(id, now - Duration::minutes(4));
        let claim = claim_due_with_context(&config, now).expect("conversation should be due");
        assert_eq!(claim.conversation_id, id);
        assert_eq!(claim.conversation_kind, ConversationKind::Direct);
        assert_eq!(claim.person_id, Some(person));
        release_claim(id);
        clear();
    }

    #[test]
    fn scheduler_round_robins_continuing_conversations() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let first = ConversationId::new();
        let second = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        for id in [first, second] {
            observe_inbound(
                id,
                ConversationKind::Direct,
                now - Duration::minutes(5),
                true,
            );
            record_outbound_with_directive(
                id,
                now - Duration::minutes(4),
                Some(ConversationTurnDirective::Continue),
                Some(&config),
            );
        }

        let first_claim = claim_due_with_context(&config, now).expect("first claim");
        finish_claim_token(
            first_claim.conversation_id,
            first_claim.token,
            now,
            true,
            ConversationTurnDirective::Continue,
            &config,
        );
        let next_wake =
            now + Duration::seconds(config.autonomous_conversation_cooldown_secs() as i64);
        let second_claim =
            claim_due_with_context(&config, next_wake).expect("second conversation should claim");
        assert_eq!(second_claim.conversation_id, second);
        release_claim_token(second_claim.conversation_id, second_claim.token);
        clear();
    }

    #[test]
    fn transient_claim_failure_is_backed_off_without_finishing_session() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(
            id,
            ConversationKind::Direct,
            now - Duration::minutes(5),
            true,
        );
        record_outbound(id, now - Duration::minutes(4));
        assert_eq!(claim_due(&config, now), Some(id));
        retry_claim(id);
        assert!(claim_due(&config, now).is_none());
        let registry = REGISTRY.lock().expect("registry lock");
        assert!(registry
            .entries
            .get(&id)
            .is_some_and(|entry| entry.retry_after.is_some() && entry.in_flight_since.is_none()));
        drop(registry);
        clear();
    }

    #[test]
    fn stale_claim_recovery_consumes_the_bounded_retry_budget() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(
            id,
            ConversationKind::Direct,
            now - Duration::minutes(5),
            true,
        );
        record_outbound(id, now - Duration::minutes(4));
        assert_eq!(claim_due(&config, now), Some(id));

        for attempt in 1..=MAX_TRANSIENT_FAILURE_RETRIES {
            {
                let mut registry = REGISTRY.lock().expect("registry lock");
                let entry = registry.entries.get_mut(&id).expect("tracked conversation");
                entry.in_flight_since = Some(
                    Instant::now()
                        .checked_sub(autonomous_in_flight_timeout() + StdDuration::from_secs(1))
                        .expect("lease duration should fit in Instant"),
                );
            }

            assert!(
                claim_due(&config, now).is_none(),
                "a stale lease must back off before it can be reclaimed"
            );

            let mut registry = REGISTRY.lock().expect("registry lock");
            let entry = registry.entries.get_mut(&id).expect("tracked conversation");
            assert_eq!(entry.retry_attempts, attempt);
            assert_eq!(entry.suspended, attempt == MAX_TRANSIENT_FAILURE_RETRIES);
            assert!(entry.in_flight_since.is_none());
            if attempt < MAX_TRANSIENT_FAILURE_RETRIES {
                entry.retry_after = Some(
                    Instant::now()
                        .checked_sub(StdDuration::from_secs(1))
                        .expect("one second should fit in Instant"),
                );
            }
            drop(registry);

            if attempt < MAX_TRANSIENT_FAILURE_RETRIES {
                assert_eq!(claim_due(&config, now), Some(id));
            }
        }

        assert!(claim_due(&config, now + Duration::hours(1)).is_none());
        clear();
    }

    #[test]
    fn stale_claim_token_cannot_finish_a_newer_claim() {
        let _guard = TEST_LOCK.lock().expect("test lock");
        clear();
        let id = ConversationId::new();
        let now = Utc::now();
        let config = ProactiveConfig::default();
        observe_inbound(
            id,
            ConversationKind::Direct,
            now - Duration::minutes(5),
            true,
        );
        record_outbound(id, now - Duration::minutes(4));
        let first = claim_due_with_context(&config, now).expect("first claim");
        // A new inbound invalidates the first queued tick and opens a fresh
        // lifecycle; after the reply, the scheduler may claim it again.
        observe_inbound(id, ConversationKind::Direct, now, true);
        record_outbound(id, now);
        let second =
            claim_due_with_context(&config, now + Duration::minutes(2)).expect("second claim");
        assert_ne!(first.token, second.token);
        assert!(claim_is_current(id, second.token));
        finish_claim_token(
            id,
            first.token,
            now + Duration::minutes(2),
            true,
            ConversationTurnDirective::End,
            &config,
        );
        assert!(claim_is_current(id, second.token));
        retry_claim_token(id, second.token);
        clear();
    }
}
