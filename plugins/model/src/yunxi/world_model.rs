//! Shadow-mode World Model v4 runtime (host-side adapter).
//!
//! This is the platform-specific half of the v4 blueprint: it feeds the
//! platform-neutral [`yunxi_core::WorldModel`] with observations and social
//! scenes derived from host events. It is gated by `[world_model].enabled`
//! (default `false`), never blocks or alters a reply, and when enabled still
//! only *records* state — nothing in this module decides to act, send, or
//! cancel. That belongs to Executive / Core (v4 §7, §56, §249–§255).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use yunxi_core::PersonId;
use yunxi_core::world_model::{
    EntityKind, EntityUpdateAction, EntityUpdateProposal, ObservationDraft, ObservationId,
    ObservationKind, ObservationPayload, ObservationSource, SocialSceneKind, SocialSceneUpdate,
    StateProperty, WorldModel, WorldScope,
};

const WORLD_LOG_PREFIX: &str = "[YUNXI_WORLD]";

/// Per-tool failure observation count (bounded; supports causal promotion).
static TOOL_FAILURE_COUNTS: LazyLock<Mutex<HashMap<String, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn count_tool_failure(tool_name: &str) -> u32 {
    let mut counts = TOOL_FAILURE_COUNTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let count = counts.entry(tool_name.to_owned()).or_insert(0);
    *count = count.saturating_add(1).min(100);
    *count
}

#[cfg(test)]
fn reset_tool_failure_counts() {
    TOOL_FAILURE_COUNTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

struct WorldRuntime {
    world: WorldModel,
}

static WORLD_RUNTIME: LazyLock<Mutex<Option<WorldRuntime>>> = LazyLock::new(|| Mutex::new(None));

/// Set whenever in-memory world state changed; cleared after a successful
/// save. Persistence is best-effort and never blocks chat (v4 §248, §252).
static WORLD_DIRTY: AtomicBool = AtomicBool::new(false);

/// Per-conversation recent message timestamps, bounded by
/// `world_model.max_social_scenes` and the activity window (60s). Deriving
/// activity is deterministic — no model call, no DB (v4 §146).
#[derive(Debug, Default)]
struct ActivityTracker {
    recent: VecDeque<(uuid::Uuid, Instant)>,
}

static ACTIVITY_TRACKER: LazyLock<Mutex<ActivityTracker>> =
    LazyLock::new(|| Mutex::new(ActivityTracker::default()));

fn world_config() -> crate::config::WorldModelConfig {
    crate::config::get().world_model().clone()
}

fn with_world<F: FnOnce(&mut WorldModel, usize /*max scenes*/)>(f: F) {
    let config = world_config();
    if !config.enabled() {
        return;
    }
    let mut guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let runtime = guard.get_or_insert_with(|| WorldRuntime {
        world: WorldModel::new(),
    });
    f(&mut runtime.world, config.max_social_scenes());
    WORLD_DIRTY.store(true, Ordering::Relaxed);
}

/// Restore a previously persisted world into the runtime (v4 §130).
/// Fail-soft: any load error logs and starts from an empty world.
pub(crate) async fn restore_from_store() {
    let Some(store) = super::world_model_store() else {
        return;
    };
    if !world_config().enabled() {
        return;
    }
    match store.load_world().await {
        Ok(Some(world)) => {
            let mut guard = WORLD_RUNTIME
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(WorldRuntime { world });
            println!(
                "{WORLD_LOG_PREFIX} restore 完成: version={}",
                guard.as_ref().map(|r| r.world.version()).unwrap_or(1)
            );
        }
        Ok(None) => println!("{WORLD_LOG_PREFIX} 无持久化状态，从空开始"),
        Err(error) => eprintln!("{WORLD_LOG_PREFIX} 恢复失败（fail-soft，从空开始）: {error}"),
    }
}

/// Periodic persistence loop: snapshot-dirty → save, bounded interval.
pub(crate) async fn persistence_loop() {
    if !world_config().enabled() || !world_config().persist() {
        return;
    }
    let interval = std::time::Duration::from_secs(world_config().persist_interval_secs().max(10));
    loop {
        kovi::tokio::time::sleep(interval).await;
        if !WORLD_DIRTY.load(Ordering::Relaxed) {
            continue;
        }
        persist_if_dirty().await;
    }
}

/// Save the world state when dirty (best-effort; keeps dirty on failure).
pub(crate) async fn persist_if_dirty() {
    let Some(store) = super::world_model_store() else {
        return;
    };
    if !WORLD_DIRTY.load(Ordering::Relaxed) {
        return;
    }
    let snapshot = {
        let mut guard = WORLD_RUNTIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(runtime) = guard.as_mut() else {
            return;
        };
        // TTL maintenance before persisting (v4 §131): expired observations
        // and hypotheses must not survive a save/restore cycle.
        runtime.world.prune_expired(chrono::Utc::now());
        Some(runtime.world.clone())
    };
    let Some(snapshot) = snapshot else {
        return;
    };
    match store.save_world(&snapshot).await {
        Ok(()) => {
            WORLD_DIRTY.store(false, Ordering::Relaxed);
            kovi::log::debug!(
                "{WORLD_LOG_PREFIX} persisted version={} observations={} situations={}",
                snapshot.version(),
                snapshot.observations().len(),
                snapshot.situations().len()
            );
        }
        Err(error) => eprintln!("{WORLD_LOG_PREFIX} 持久化失败（保留脏标记，稍后重试）: {error}"),
    }
}

/// Reset the in-memory runtime (used by tests). No-op production effect.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
    WORLD_DIRTY.store(false, Ordering::Relaxed);
    let mut tracker = ACTIVITY_TRACKER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tracker.recent.clear();
    reset_tool_failure_counts();
}

/// Record one structured observation into the World Model (shadow mode).
///
/// Content is truncated to the core payload bounds, confidence is clamped by
/// the draft, and TTL comes from config unless overridden. Never fails the
/// caller: world-model failures are logged, not propagated (v4 §248).
pub(crate) fn record_observation(
    scope: WorldScope,
    kind: ObservationKind,
    source: ObservationSource,
    content: &str,
    ttl_secs: Option<u64>,
) {
    with_world(|world, _max_scenes| {
        let content = truncate_chars(
            content,
            yunxi_core::world_model::MAX_OBSERVATION_PAYLOAD_CHARS,
        );
        let payload = match ObservationPayload::new(content, None::<&str>) {
            Ok(payload) => payload,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} observation payload rejected: {error}");
                return;
            }
        };
        let ttl = ttl_secs.unwrap_or_else(|| world_config().observation_ttl_secs());
        let draft = match ObservationDraft::new(scope, kind, source, payload, 0.85, Some(ttl)) {
            Ok(draft) => draft,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} observation draft rejected: {error}");
                return;
            }
        };
        let observation = match draft.build(
            ObservationId::new(),
            yunxi_core::EventId::new(),
            chrono::Utc::now(),
        ) {
            Ok(observation) => observation,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} observation build failed: {error}");
                return;
            }
        };
        match world.observe(observation) {
            Ok(()) => {
                println!(
                    "{WORLD_LOG_PREFIX} observation kind={kind:?} source={source:?} confidence={:.2} version={}",
                    draft.confidence(),
                    world.version()
                );
            }
            Err(error) => eprintln!("{WORLD_LOG_PREFIX} observe failed: {error}"),
        }
    });
}

/// Record a deterministic entity-property update (host/tool availability,
/// owner state) without needing the model to propose anything.
pub(crate) fn record_entity_property(
    kind: EntityKind,
    linked_person: Option<PersonId>,
    linked_conversation: Option<yunxi_core::ConversationId>,
    key: &str,
    value: &str,
    confidence: f32,
) {
    with_world(|world, _max_scenes| {
        let now = chrono::Utc::now();
        let proposal = match EntityUpdateProposal::new(
            None,
            kind,
            linked_person,
            linked_conversation,
            confidence,
            vec![EntityUpdateAction::Set(
                match StateProperty::new(
                    key,
                    value,
                    confidence,
                    ObservationSource::SystemState,
                    now,
                    None,
                ) {
                    Ok(property) => property,
                    Err(error) => {
                        eprintln!("{WORLD_LOG_PREFIX} entity property rejected: {error}");
                        return;
                    }
                },
            )],
            now,
        ) {
            Ok(proposal) => proposal,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} entity proposal rejected: {error}");
                return;
            }
        };
        match world.apply_entity_update(proposal) {
            Ok(_entity_id) => println!(
                "{WORLD_LOG_PREFIX} entity_update kind={kind:?} key={key} version={}",
                world.version()
            ),
            Err(error) => eprintln!("{WORLD_LOG_PREFIX} entity update failed: {error}"),
        }
    });
}

/// Record one inbound group message into the social scene (v4 §145–146).
///
/// Deterministic: `bot_addressed` + message rate over the activity window
/// plus the current floor produce the scene and interruption cost. No model
/// call. Rapid group chat (high activity, not addressed) is derived, not
/// inferred from psychology (v4 §72).
pub(crate) fn record_group_scene(
    conversation_id: yunxi_core::ConversationId,
    sender: PersonId,
    floor: Vec<PersonId>,
    bot_addressed: bool,
) {
    let activity = push_and_activity(conversation_id);
    let scene_kind = if activity > 0.5 && !bot_addressed {
        SocialSceneKind::RapidGroupChat
    } else {
        SocialSceneKind::GroupDiscussion
    };
    with_world(|world, max_scenes| {
        let participants = floor.clone();
        let now = chrono::Utc::now();
        // recent_speaking_order is the floor order; bounded by the update.
        let update = match SocialSceneUpdate::new(
            conversation_id,
            now,
            participants,
            floor,
            vec![sender],
            bot_addressed,
            activity,
            scene_kind,
        ) {
            Ok(update) => update,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} social scene update rejected: {error}");
                return;
            }
        };
        match world.update_social_scene(update) {
            Ok(()) => println!(
                "{WORLD_LOG_PREFIX} social_scene conversation_id={conversation_id} addressed={bot_addressed} activity={activity:.2} version={}",
                world.version()
            ),
            Err(error) => {
                if world.social_scenes().len() >= max_scenes {
                    // Scene cap reached: nothing to do, just log once at the
                    // update boundary; not an error path for the caller.
                    eprintln!("{WORLD_LOG_PREFIX} social scene cap reached");
                } else {
                    eprintln!("{WORLD_LOG_PREFIX} social scene update failed: {error}");
                }
            }
        }
        // R2: derive/maintain the conversation situation with the same lock.
        if let Err(error) = apply_scene_derivation(world, conversation_id, now) {
            kovi::log::debug!("{WORLD_LOG_PREFIX} scene derivation skipped: {error}");
        }
    });
}

/// Record a private-chat message as a direct-conversation scene.
pub(crate) fn record_direct_scene(
    conversation_id: yunxi_core::ConversationId,
    participant: PersonId,
    bot_addressed: bool,
) {
    with_world(|world, _max_scenes| {
        let now = chrono::Utc::now();
        let update = match SocialSceneUpdate::new(
            conversation_id,
            now,
            vec![participant],
            if bot_addressed {
                vec![participant]
            } else {
                vec![]
            },
            vec![participant],
            bot_addressed,
            0.4,
            SocialSceneKind::DirectConversation,
        ) {
            Ok(update) => update,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} direct scene update rejected: {error}");
                return;
            }
        };
        match world.update_social_scene(update) {
            Ok(()) => println!(
                "{WORLD_LOG_PREFIX} social_scene conversation_id={conversation_id} kind=direct version={}",
                world.version()
            ),
            Err(error) => eprintln!("{WORLD_LOG_PREFIX} direct scene update failed: {error}"),
        }
        if let Err(error) = apply_scene_derivation(world, conversation_id, now) {
            kovi::log::debug!("{WORLD_LOG_PREFIX} scene derivation skipped: {error}");
        }
    });
}

/// Deterministic shadow-scene PersonId derived from a host numeric user id.
/// Stable across restarts; used only for scene/floor bookkeeping in shadow
/// mode. Canonical identity linking remains the identity store's job.
pub(crate) fn scene_person_id(user_id: i64) -> PersonId {
    PersonId::from_uuid(scene_namespace_uuid(&format!("person:{user_id}")))
}

/// Deterministic shadow-scene ConversationId for a QQ group.
pub(crate) fn scene_group_conversation_id(group_id: i64) -> yunxi_core::ConversationId {
    yunxi_core::ConversationId::from_uuid(scene_namespace_uuid(&format!("group:{group_id}")))
}

/// Deterministic shadow-scene ConversationId for a QQ private chat with
/// `user_id` — stable per peer, distinct per peer.
pub(crate) fn scene_direct_conversation_id(user_id: i64) -> yunxi_core::ConversationId {
    yunxi_core::ConversationId::from_uuid(scene_namespace_uuid(&format!("direct:{user_id}")))
}

fn scene_namespace_uuid(seed: &str) -> uuid::Uuid {
    const NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x9e2c_6f6a_4c3b_4f7e_9a10_0d5c_2b8e_4a19);
    uuid::Uuid::new_v5(&NAMESPACE, seed.as_bytes())
}

/// Push a message timestamp for the conversation and compute activity
/// (0..1) as recent-message ratio over the configured window.
fn push_and_activity(conversation_id: yunxi_core::ConversationId) -> f32 {
    let config = world_config();
    let window = std::time::Duration::from_secs(config.activity_window_secs());
    let max_scenes = config.max_social_scenes();
    let mut tracker = ACTIVITY_TRACKER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    // Bound total stored history: at most max_scenes distinct conversations.
    let conv_key = conversation_id.into_uuid();
    if (tracker.recent.is_empty() || !tracker.recent.iter().any(|(key, _)| *key == conv_key))
        && tracker.recent.len() >= max_scenes
    {
        tracker.recent.clear();
    }
    tracker.recent.push_back((conv_key, now));
    while tracker
        .recent
        .front()
        .is_some_and(|(_, at)| now.duration_since(*at) > window)
    {
        tracker.recent.pop_front();
    }
    let count = tracker
        .recent
        .iter()
        .filter(|(key, _)| *key == conv_key)
        .count();
    // 12 messages within the window ≈ fully active.
    (count as f32 / 12.0).clamp(0.0, 1.0)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Bounded, log-friendly summary of the current world state (v4 §155 shape).
/// Reserved for the admin observability surface (#world-status) and future
/// integration; kept alive intentionally.
#[allow(dead_code)]
pub(crate) fn status_summary() -> Option<String> {
    let guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let runtime = guard.as_ref()?;
    let world = &runtime.world;
    let context = yunxi_core::WorldSnapshotContext::new(chrono::Utc::now());
    let snapshot = world.snapshot_for(&context).ok()?;
    Some(format!(
        "version={} entities={} situations={} live={} hypotheses={} scenes={} hosts={} tools={} uncertainties={}{}",
        world.version(),
        snapshot.entities().len(),
        snapshot.situations().len(),
        snapshot
            .situations()
            .iter()
            .filter(|s| s.status() == yunxi_core::world_model::SituationStatus::Active)
            .count(),
        snapshot.hypotheses().len(),
        snapshot.social_scene().map_or(0, |_| 1),
        snapshot.environment().hosts().len(),
        snapshot.environment().tools().len(),
        snapshot.uncertainties().len(),
        if world_config().shadow_mode() {
            " shadow=true"
        } else {
            ""
        }
    ))
}

/// Deterministic private-message situation derivation (R2 extension):
/// explicit future-event keywords + a conversational time cue produce a
/// bounded `FutureEvent / Planned` situation (v4 §22, §180). No psychology,
/// no calendar claim (v4 §198): the world model only knows "a thing is
/// scheduled-ish", never that it will happen.
fn derive_future_event_situation(
    world: &mut WorldModel,
    conversation_id: yunxi_core::ConversationId,
    person_id: PersonId,
    text: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), yunxi_core::world_model::WorldValidationError> {
    use yunxi_core::world_model::{SituationKind, SituationState, SituationStatus};
    const EVENT_KEYWORDS: &[(&str, &str)] = &[
        ("面试", "面试安排"),
        ("考试", "考试安排"),
        ("体检", "体检安排"),
        ("开会", "会议安排"),
        ("出差", "出差安排"),
        ("复查", "复查安排"),
    ];
    let has_time_cue = [
        "明天",
        "后天",
        "下周",
        "下个月",
        "今晚",
        "上午",
        "下午",
        "晚上",
        "周末",
    ]
    .iter()
    .any(|cue| text.contains(cue));
    let matched = EVENT_KEYWORDS
        .iter()
        .find(|(keyword, _)| text.contains(keyword));
    let already = world.situations().iter().any(|situation| {
        situation.kind() == SituationKind::FutureEvent
            && situation.conversation_id() == Some(conversation_id)
            && situation.status() == SituationStatus::Active
    });
    if let Some((_keyword, label)) = matched
        && has_time_cue
        && !already
        && world.situations().len() < 8
    {
        let situation = yunxi_core::Situation::new(
            yunxi_core::SituationId::new(),
            SituationKind::FutureEvent,
            SituationState::Planned,
            Some((*label).to_owned()),
            vec![],
            vec![person_id],
            Some(conversation_id),
            vec![],
            vec![],
            0.55,
            now,
        )?;
        world.add_situation(situation)?;
    }
    // Maintenance: planned future events older than 24h expire instead of
    // lingering forever (v4 §92) — validated transition inside the core.
    let _ =
        world.expire_stale_situations(SituationKind::FutureEvent, chrono::Duration::hours(24), now);
    Ok(())
}

/// Record an inbound private message as a structured observation (v4 §10:
/// what the user actually said, not an inference). TTL 24h by default;
/// content truncated; gated by `[world_model].enabled`.
pub(crate) fn record_private_message(
    conversation_id: yunxi_core::ConversationId,
    person_id: PersonId,
    content: &str,
) {
    record_observation(
        WorldScope::Person { person_id },
        ObservationKind::MessageReceived,
        ObservationSource::DirectUserStatement,
        content,
        None,
    );
    derive_and_maintain_scene(conversation_id);
    // R2 extension: explicit future-event keywords + time cue → situation.
    with_world(|world, _max_scenes| {
        if let Err(error) = derive_future_event_situation(
            world,
            conversation_id,
            person_id,
            content,
            chrono::Utc::now(),
        ) {
            kovi::log::debug!("{WORLD_LOG_PREFIX} future-event derivation skipped: {error}");
        }
    });
}

/// Deterministic situation derivation + maintenance for one conversation
/// (R2, 0 模型调用): group discussion / direct chat situations from the
/// social scene, and staleness marking (v4 §25, §90–92).
///
/// - GroupDiscussion/RapidGroupChat with activity ≥ 0.5 → situation
///   `ConversationState / InProgress / 群讨论中`.
/// - DirectConversation → `ConversationState / InProgress / 私聊进行中`.
/// - `ConversationState` situations idle > 10 minutes move to
///   `OutcomeUnknown` (never silently stays "in progress", v4 §92).
fn derive_and_maintain_scene(conversation_id: yunxi_core::ConversationId) {
    with_world(|world, _max_scenes| {
        let now = chrono::Utc::now();
        if let Err(error) = apply_scene_derivation(world, conversation_id, now) {
            kovi::log::debug!("{WORLD_LOG_PREFIX} scene derivation skipped: {error}");
        }
    });
}

/// Pure derivation rules (unit-testable without the config-gated runtime).
fn apply_scene_derivation(
    world: &mut WorldModel,
    conversation_id: yunxi_core::ConversationId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), yunxi_core::world_model::WorldValidationError> {
    use yunxi_core::world_model::{
        SituationKind, SituationState, SituationStatus, SituationTransitionProposal,
    };
    // 1. Derive an active scene situation from the live scene, deduped by
    // (kind, conversation).
    let scene = world
        .social_scenes()
        .iter()
        .find(|scene| scene.conversation_id() == conversation_id)
        .cloned();
    if let Some(scene) = scene {
        let active_scene = scene.activity_level() >= 0.5
            && scene.scene_kind() != yunxi_core::SocialSceneKind::Unknown;
        let already = world.situations().iter().any(|situation| {
            situation.kind() == SituationKind::ConversationState
                && situation.conversation_id() == Some(conversation_id)
                && situation.status() == SituationStatus::Active
        });
        if active_scene && !already && world.situations().len() < 8 {
            let detail = match scene.scene_kind() {
                yunxi_core::SocialSceneKind::DirectConversation => "私聊进行中".to_owned(),
                _ => "群讨论中".to_owned(),
            };
            let situation = yunxi_core::Situation::new(
                yunxi_core::SituationId::new(),
                SituationKind::ConversationState,
                SituationState::InProgress,
                Some(detail),
                vec![],
                scene.active_participants().to_vec(),
                Some(conversation_id),
                vec![],
                vec![],
                0.6,
                now,
            )?;
            world.add_situation(situation)?;
        }
    }
    // 2. Maintain: conversation situations idle > 10 min move to
    // OutcomeUnknown via a validated transition (only InProgress is allowed
    // to move there, so nothing else is touched).
    let idle = chrono::Duration::minutes(10);
    let stale: Vec<_> = world
        .situations()
        .iter()
        .filter(|situation| {
            situation.kind() == SituationKind::ConversationState
                && situation.conversation_id() == Some(conversation_id)
                && situation.status() == SituationStatus::Active
                && situation.state() == SituationState::InProgress
                && now - situation.updated_at() > idle
        })
        .map(|situation| (situation.id(), situation.version()))
        .collect();
    for (situation_id, version) in stale {
        let proposal = SituationTransitionProposal::new(
            situation_id,
            version,
            SituationState::InProgress,
            SituationState::OutcomeUnknown,
            0.4,
            yunxi_core::ObservationSource::SystemState,
            false,
            None,
            now,
        )?;
        world.apply_situation_transition(proposal)?;
    }
    Ok(())
}

/// Record a message collision (committed outgoing + near-simultaneous
/// incoming) into the World Model: a bounded ConversationEvent observation
/// plus a scene version touch. **No psychology** — it is explicitly only the
/// external fact that both sides spoke almost at once (v4 appendix §2–§5).
pub(crate) fn record_collision(conversation_id: yunxi_core::ConversationId) {
    record_observation(
        WorldScope::Conversation { conversation_id },
        ObservationKind::ConversationEvent,
        ObservationSource::PlatformEvent,
        "消息碰撞：我方消息与对方新消息几乎同时到达",
        Some(300),
    );
    with_world(|world, _max_scenes| {
        let now = chrono::Utc::now();
        match world.touch_social_scene(conversation_id, now) {
            Ok(()) => println!(
                "{WORLD_LOG_PREFIX} collision scene_touch conversation_id={conversation_id} version={}",
                world.version()
            ),
            Err(error) => {
                kovi::log::debug!("{WORLD_LOG_PREFIX} collision scene touch skipped: {error}")
            }
        }
    });
}

/// Tool-recovery candidates for a degraded tool (v4 §102, §196).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolRecoveryCandidate {
    RetryNow,
    Wait,
    UseFallback,
}

impl ToolRecoveryCandidate {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::RetryNow => "tool:retry_now",
            Self::Wait => "tool:wait",
            Self::UseFallback => "tool:fallback",
        }
    }
}

/// Deterministic outcome estimate for one tool-recovery candidate given the
/// tool's current health (v4 §44, §101–§102). This is domain knowledge, not
/// psychology; the executive still makes the final call.
pub(crate) fn tool_recovery_outcome(
    _tool_name: &str,
    health: yunxi_core::ServiceHealth,
    candidate: ToolRecoveryCandidate,
) -> yunxi_core::PredictedOutcome {
    use ToolRecoveryCandidate::*;
    use yunxi_core::{OutcomeKind, ServiceHealth};
    let probability = match (health, candidate) {
        // 429 / degraded → immediate retry likely fails again.
        (ServiceHealth::Degraded, RetryNow) => 0.15,
        (ServiceHealth::Degraded, Wait) => 0.60,
        (ServiceHealth::Degraded, UseFallback) => 0.50,
        (ServiceHealth::Unavailable, RetryNow) => 0.05,
        (ServiceHealth::Unavailable, Wait) => 0.40,
        (ServiceHealth::Unavailable, UseFallback) => 0.45,
        (ServiceHealth::Healthy, RetryNow) => 0.85,
        (ServiceHealth::Healthy, Wait) => 0.70,
        (ServiceHealth::Healthy, UseFallback) => 0.70,
        (ServiceHealth::Unknown, RetryNow) => 0.30,
        (ServiceHealth::Unknown, Wait) => 0.40,
        (ServiceHealth::Unknown, UseFallback) => 0.35,
    };
    let success = probability >= 0.5;
    yunxi_core::PredictedOutcome::new(
        if success {
            OutcomeKind::Success
        } else {
            OutcomeKind::Failure
        },
        probability,
        if success { 0.6 } else { -0.4 },
        0.05,
        1.0 - probability,
        if success { 0.5 } else { 0.0 },
    )
    .expect("bounded outcome")
}

/// Tool failure → world update (v4 §141–§144, §196): mark the tool degraded
/// with a short TTL, record the failure observation, and record deterministic
/// shadow predictions for the recovery candidates (what an executive would
/// compare). Gated by `[world_model].enabled`; never blocks the caller.
pub(crate) fn record_tool_failure(tool_name: &str, error_category: &str, detail: &str) {
    record_observation(
        WorldScope::Global,
        ObservationKind::ToolResult,
        ObservationSource::ToolResult,
        &format!("工具 {tool_name} 调用失败（{error_category}）"),
        Some(300),
    );
    with_world(|world, _max_scenes| {
        let now = chrono::Utc::now();
        // 1. Environment: tool degraded for 5 minutes (TTL-aware).
        let health = match world.environment().tool_health_at(tool_name, now) {
            yunxi_core::ServiceHealth::Unavailable => yunxi_core::ServiceHealth::Unavailable,
            _ => yunxi_core::ServiceHealth::Degraded,
        };
        let tool = match yunxi_core::ToolHealth::new(
            tool_name.to_owned(),
            health,
            Some(truncate_chars(detail, 128)),
            now,
            chrono::Duration::minutes(5),
        ) {
            Ok(tool) => tool,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} tool health rejected: {error}");
                return;
            }
        };
        let update = match yunxi_core::EnvironmentUpdate::new(
            vec![],
            vec![tool],
            yunxi_core::ServiceHealth::Healthy,
            world.environment().load(),
        ) {
            Ok(update) => update,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} environment update rejected: {error}");
                return;
            }
        };
        if let Err(error) = world.update_environment(update) {
            eprintln!("{WORLD_LOG_PREFIX} environment update failed: {error}");
            return;
        }
        // 2. Deterministic shadow predictions for the recovery candidates
        // (v4 §102: A retry now / B wait / C fallback).
        for candidate in [
            ToolRecoveryCandidate::RetryNow,
            ToolRecoveryCandidate::Wait,
            ToolRecoveryCandidate::UseFallback,
        ] {
            let outcome = tool_recovery_outcome(tool_name, health, candidate);
            let horizon = yunxi_core::PredictionHorizon::Immediate;
            let prediction = yunxi_core::Prediction::new(
                yunxi_core::PredictionId::new(),
                format!("{tool_name}:{}", candidate.id()),
                WorldScope::Global,
                horizon,
                vec![outcome],
                0.7,
                0.7,
                now,
                Some(now + chrono::Duration::minutes(5)),
            );
            match prediction {
                Ok(prediction) => {
                    if let Err(error) = world.record_prediction(prediction) {
                        eprintln!("{WORLD_LOG_PREFIX} prediction rejected: {error}");
                    }
                }
                Err(error) => eprintln!("{WORLD_LOG_PREFIX} prediction rejected: {error}"),
            }
        }
        // 3. Shadow top-2 simulation batch for the same decision (pure
        // values; zero side effects, v4 §56–§58).
        if let Some(batch) = simulate_tool_recovery(tool_name) {
            kovi::log::debug!(
                "{WORLD_LOG_PREFIX} simulate_note tool={tool_name} results={}",
                batch.results().len()
            );
        }
        // 4. Causal observation (v4 §96–§98): repeated failures promote a
        // tool-specific "retry now likely fails" relation. Person-specific
        // remains forbidden (v4 §99).
        let occurrences = count_tool_failure(tool_name);
        if occurrences >= yunxi_core::MIN_EVIDENCE_OCCURRENCES {
            promote_tool_failure_causal(world, tool_name, occurrences);
        }
        println!(
            "{WORLD_LOG_PREFIX} tool_failure tool={tool_name} env={health:?} version={}",
            world.version()
        );
    });
}

/// Promote the observed tool-failure causal candidate (dedupe by
/// cause+effect+scope inside CausalKnowledge).
fn promote_tool_failure_causal(world: &mut WorldModel, tool_name: &str, occurrences: u32) {
    use yunxi_core::{
        CausalRelationProposal, CausalScope, CausalSource, PatternKind, WorldPattern,
        promote_candidate,
    };
    let cause = match WorldPattern::new(PatternKind::Environment, "rate_limited_or_degraded") {
        Ok(pattern) => pattern,
        Err(error) => {
            eprintln!("{WORLD_LOG_PREFIX} causal pattern rejected: {error}");
            return;
        }
    };
    let effect =
        match WorldPattern::new(PatternKind::Tool, format!("immediate_retry_of_{tool_name}")) {
            Ok(pattern) => pattern,
            Err(error) => {
                eprintln!("{WORLD_LOG_PREFIX} causal pattern rejected: {error}");
                return;
            }
        };
    let proposal = match CausalRelationProposal::new(
        cause,
        effect,
        0.7,
        vec![],
        CausalScope::ToolSpecific {
            tool: tool_name.to_owned(),
        },
    ) {
        Ok(proposal) => proposal,
        Err(error) => {
            eprintln!("{WORLD_LOG_PREFIX} causal proposal rejected: {error}");
            return;
        }
    };
    match promote_candidate(
        proposal,
        occurrences,
        false,
        CausalSource::ObservedRepeatedPattern,
        yunxi_core::CausalRelationId::new(),
    ) {
        Ok(relation) => {
            if let Err(error) = world.add_causal_relation(relation) {
                // Already promoted / capped: not an error path.
                kovi::log::debug!("{WORLD_LOG_PREFIX} causal promote skipped: {error}");
            } else {
                println!(
                    "{WORLD_LOG_PREFIX} causal_promoted tool={tool_name} occurrences={occurrences}"
                );
            }
        }
        Err(error) => eprintln!("{WORLD_LOG_PREFIX} causal promote rejected: {error}"),
    }
}

/// Compare an actual tool-call outcome with the stored recovery prediction
/// (v4 §51–§52, §232: Predict → Act → Observe → Compare). Records a
/// `PredictionError` used purely as a calibration signal.
pub(crate) fn record_tool_retry_outcome(tool_name: &str, succeeded: bool) {
    with_world(|world, _max_scenes| {
        let observed = if succeeded {
            yunxi_core::OutcomeKind::Success
        } else {
            yunxi_core::OutcomeKind::Failure
        };
        let retry_key = format!("{tool_name}:tool:retry_now");
        let matching: Vec<yunxi_core::Prediction> = world
            .predictions()
            .iter()
            .filter(|prediction| prediction.source_candidate() == retry_key)
            .filter(|prediction| {
                prediction.freshness_at(chrono::Utc::now()) != yunxi_core::Freshness::Expired
            })
            .cloned()
            .collect();
        for prediction in matching.into_iter().take(3) {
            // Each prediction contributes at most one calibration error.
            if world
                .prediction_errors()
                .iter()
                .any(|error| error.prediction_id() == prediction.id())
            {
                continue;
            }
            let expected = prediction
                .possible_outcomes()
                .first()
                .map_or(yunxi_core::OutcomeKind::Unknown, |outcome| {
                    outcome.description()
                });
            let error = yunxi_core::PredictionError::new(
                prediction.id(),
                expected,
                observed,
                prediction.confidence(),
                chrono::Utc::now(),
            );
            match error {
                Ok(error) => {
                    if let Err(err) = world.record_prediction_error(error) {
                        eprintln!("{WORLD_LOG_PREFIX} prediction error rejected: {err}");
                    }
                }
                Err(err) => eprintln!("{WORLD_LOG_PREFIX} prediction error rejected: {err}"),
            }
        }
        if let Some(accuracy) = world.calibration_accuracy() {
            println!(
                "{WORLD_LOG_PREFIX} calibration tool={tool_name} succeeded={succeeded} accuracy={accuracy:.2} errors={}",
                world.prediction_errors().len()
            );
        }
    });
}

/// Behavioral interruption guard (v4 §103, §197): returns the world's
/// interruption cost for a conversation, or 0.0 when the feature is not
/// enabled or influence_mode is not active. Callers decide suppression.
pub(crate) fn interruption_guard(conversation_id: yunxi_core::ConversationId) -> f32 {
    let config = world_config();
    if !config.enabled() || !config.influence_active() {
        return 0.0;
    }
    let guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(runtime) = guard.as_ref() else {
        return 0.0;
    };
    let context = yunxi_core::WorldSnapshotContext::new(chrono::Utc::now())
        .with_conversation(conversation_id);
    runtime
        .world
        .snapshot_for(&context)
        .ok()
        .and_then(|snapshot| {
            snapshot
                .social_scene()
                .map(|scene| scene.interruption_cost())
        })
        .unwrap_or(0.0)
}

/// Delivery candidates for one host (v4 §101–§102 / Appendix §8: high-value
/// proactive message before commit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryCandidate {
    SendNow,
    Defer,
}

impl DeliveryCandidate {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::SendNow => "delivery:send_now",
            Self::Defer => "delivery:defer",
        }
    }
}

/// Deterministic outcome estimate for one delivery candidate given host
/// health. Availability is environment knowledge, not psychology (v4 §45).
pub(crate) fn delivery_outcome(
    health: yunxi_core::ServiceHealth,
    candidate: DeliveryCandidate,
) -> yunxi_core::PredictedOutcome {
    use DeliveryCandidate::*;
    use yunxi_core::{OutcomeKind, ServiceHealth};
    let probability = match (health, candidate) {
        (ServiceHealth::Unavailable, SendNow) => 0.05,
        (ServiceHealth::Unavailable, Defer) => 0.60,
        (ServiceHealth::Degraded, SendNow) => 0.40,
        (ServiceHealth::Degraded, Defer) => 0.60,
        (ServiceHealth::Healthy, SendNow) => 0.90,
        (ServiceHealth::Healthy, Defer) => 0.70,
        (ServiceHealth::Unknown, SendNow) => 0.30,
        (ServiceHealth::Unknown, Defer) => 0.40,
    };
    let success = probability >= 0.5;
    yunxi_core::PredictedOutcome::new(
        if success {
            OutcomeKind::Success
        } else {
            OutcomeKind::Failure
        },
        probability,
        if success { 0.4 } else { -0.3 },
        0.10,
        1.0 - probability,
        if success { 0.4 } else { 0.0 },
    )
    .expect("bounded outcome")
}

/// Execution-side delivery simulation: hosts + candidates → bounded batch.
/// `send_now` + `defer` (≤2 results/trace); read-only, Simulated mode only.
pub(crate) fn simulate_delivery(host: &yunxi_core::HostId) -> Option<yunxi_core::SimulationBatch> {
    let guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let world = &guard.as_ref()?.world;
    simulate_delivery_internal(world, host, chrono::Utc::now())
}

fn simulate_delivery_internal(
    world: &WorldModel,
    host: &yunxi_core::HostId,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<yunxi_core::SimulationBatch> {
    use yunxi_core::{
        ExecutionMode, SimulationCandidate, SimulationInput, SimulationResult, WorldSnapshotContext,
    };
    let snapshot = world.snapshot_for(&WorldSnapshotContext::new(now)).ok()?;
    let health = world.environment().host_health_at(host, now);
    let mut results = Vec::new();
    for candidate in [DeliveryCandidate::SendNow, DeliveryCandidate::Defer] {
        let outcome = delivery_outcome(health, candidate);
        let result = SimulationResult::new(
            format!("{}:{}", host.as_str(), candidate.id()),
            vec![outcome],
            match health {
                yunxi_core::ServiceHealth::Healthy => 0.10,
                _ => 0.35,
            },
            world.version(),
            ExecutionMode::Simulated,
        )
        .ok()?;
        results.push(result);
    }
    let batch = yunxi_core::SimulationBatch::new(yunxi_core::EventId::new(), results).ok()?;
    let input = SimulationInput::new(
        yunxi_core::EventId::new(),
        SimulationCandidate::new(
            format!("{}:delivery", host.as_str()),
            "送达候选（现在发 / 延后）",
        )
        .ok()?,
        yunxi_core::PredictionHorizon::Immediate,
        snapshot,
        now,
    )
    .ok()?;
    if input.world().version() != batch.results()[0].world_version() {
        return None;
    }
    Some(batch)
}

/// Deterministic execution-side simulator for a degraded tool (v4 §54–§55,
/// §101–§102): snapshots are pure values, results are `Simulated` only, the
/// batch respects max-per-root-trace = 2 (RetryNow + UseFallback; "wait" is
/// a prediction, not a simulated candidate). Read-only: never mutates state.
pub(crate) fn simulate_tool_recovery(tool_name: &str) -> Option<yunxi_core::SimulationBatch> {
    let guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let world = &guard.as_ref()?.world;
    simulate_tool_recovery_internal(world, tool_name, chrono::Utc::now())
}

/// Pure (read-only) simulator body; `world` is only read, never mutated.
fn simulate_tool_recovery_internal(
    world: &WorldModel,
    tool_name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<yunxi_core::SimulationBatch> {
    use yunxi_core::{
        ExecutionMode, SimulationCandidate, SimulationInput, SimulationResult, WorldSnapshotContext,
    };
    let snapshot = world.snapshot_for(&WorldSnapshotContext::new(now)).ok()?;
    let health = world.environment().tool_health_at(tool_name, now);
    let candidates = [
        ToolRecoveryCandidate::RetryNow,
        ToolRecoveryCandidate::UseFallback,
    ];
    let mut results = Vec::new();
    for candidate in candidates {
        let outcome = tool_recovery_outcome(tool_name, health, candidate);
        let candidate_id = format!("{tool_name}:{}", candidate.id());
        let result = SimulationResult::new(
            candidate_id.clone(),
            vec![outcome],
            // Unknown/degraded states leave real uncertainty.
            match health {
                yunxi_core::ServiceHealth::Healthy => 0.15,
                _ => 0.4,
            },
            world.version(),
            ExecutionMode::Simulated,
        )
        .ok()?;
        results.push(result);
    }
    let batch = yunxi_core::SimulationBatch::new(yunxi_core::EventId::new(), results).ok()?;
    // Validation through a real input (version + snapshot consistency).
    let candidate = SimulationCandidate::new(
        format!("{tool_name}:tool_recovery"),
        "工具恢复候选（立即重试 / 降级）",
    )
    .ok()?;
    let input = SimulationInput::new(
        yunxi_core::EventId::new(),
        candidate,
        yunxi_core::PredictionHorizon::Immediate,
        snapshot,
        now,
    )
    .ok()?;
    if input.world().version() != batch.results()[0].world_version() {
        return None;
    }
    Some(batch)
}

/// Read the raw bounded snapshot for a conversation (reply-context input).
/// `None` when disabled/unavailable (fail-soft, v4 §249).
pub(crate) fn conversation_world_snapshot(
    conversation_id: yunxi_core::ConversationId,
) -> Option<yunxi_core::WorldModelSnapshot> {
    let guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let world = &guard.as_ref()?.world;
    let context = yunxi_core::WorldSnapshotContext::new(chrono::Utc::now())
        .with_conversation(conversation_id);
    world.snapshot_for(&context).ok()
}

/// Render a bounded "external world right now" context for the reply prompt
/// (v4 §64, §116). Objective nouns only: scene kind, activity, floor,
/// situations; never psychology, never message content, never internal ids
/// or numbers that invite imitation. Empty string when nothing salient.
pub(crate) fn render_world_context(snapshot: &yunxi_core::WorldModelSnapshot) -> String {
    use yunxi_core::{SituationStatus, SocialSceneKind};
    let mut parts: Vec<String> = Vec::new();
    if let Some(scene) = snapshot.social_scene() {
        let scene_part: Option<&str> = match scene.scene_kind() {
            SocialSceneKind::RapidGroupChat => Some("群里说得很快"),
            SocialSceneKind::GroupDiscussion => {
                if !scene.bot_addressed() {
                    Some("大家在讨论，暂时没叫我")
                } else {
                    Some("群里在讨论，叫我回应")
                }
            }
            SocialSceneKind::IdleGroup => Some("群里安静下来了"),
            SocialSceneKind::DirectConversation => Some("对方在跟我说话"),
            SocialSceneKind::TaskConversation => Some("正在处理一件具体的事"),
            SocialSceneKind::Unknown => None,
        };
        if let Some(part) = scene_part {
            parts.push(part.to_owned());
        }
        if !scene.current_floor().is_empty()
            && SocialSceneKind::DirectConversation != scene.scene_kind()
        {
            parts.push("别人正拿着话头".to_owned());
        }
        // Only interrupt cost matters as a feel, never as a number.
        if scene.interruption_cost() > 0.7 {
            parts.push("现在插话不太合适".to_owned());
        }
    }
    for situation in snapshot
        .situations()
        .iter()
        .filter(|situation| situation.status() == SituationStatus::Active)
        .take(2)
    {
        if let Some(detail) = situation.detail() {
            let label = match detail {
                "群讨论中" => "当前是有来有回的群讨论",
                "私聊进行中" => "单聊正好聊着",
                other => other,
            };
            parts.push(truncate_chars(label, 30));
        }
    }
    if !snapshot.environment().hosts().is_empty()
        && snapshot
            .environment()
            .hosts()
            .iter()
            .all(|host| host.health() == yunxi_core::ServiceHealth::Unavailable)
    {
        parts.push("发消息的主机都不可用".to_owned());
    }
    if parts.is_empty() {
        return String::new();
    }
    let mut text = parts.join("；");
    text.push('。');
    truncate_chars(&text, 600)
}

/// Bounded "what does the world look like right now for this conversation"
/// soft signal (v4 §231 decision formula input; R4 shadow source).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConversationWorldSummary {
    pub scene_kind: Option<SocialSceneKind>,
    pub activity_level: f32,
    pub interruption_cost: f32,
    pub bot_addressed: bool,
    pub active_situations: Vec<String>,
    pub active_hypotheses: usize,
    pub degraded_tools: usize,
    pub unavailable_hosts: usize,
}

impl ConversationWorldSummary {
    /// Single-line, bounded rendering for logs/admin.
    pub fn render(&self) -> String {
        let scene = match self.scene_kind {
            Some(kind) => format!("{kind:?}"),
            None => "none".to_owned(),
        };
        format!(
            "scene={scene} activity={:.2} interrupt={:.2} addressed={} situations={} hypotheses={} tools_degraded={} hosts_unavailable={}",
            self.activity_level,
            self.interruption_cost,
            self.bot_addressed,
            self.active_situations.len(),
            self.active_hypotheses,
            self.degraded_tools,
            self.unavailable_hosts,
        )
    }
}

/// Compute the soft signal for a decision about `conversation_id`. Returns
/// `None` when the World Model is disabled or unavailable (fail-soft: the
/// caller then uses its v3 path, v4 §249).
pub(crate) fn conversation_world_summary(
    conversation_id: yunxi_core::ConversationId,
) -> Option<ConversationWorldSummary> {
    let guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let world = &guard.as_ref()?.world;
    let now = chrono::Utc::now();
    let context = yunxi_core::WorldSnapshotContext::new(now).with_conversation(conversation_id);
    let snapshot = world.snapshot_for(&context).ok()?;
    let scene = snapshot.social_scene();
    let environment = snapshot.environment();
    let active_situations = snapshot
        .situations()
        .iter()
        .filter(|situation| situation.status() == yunxi_core::SituationStatus::Active)
        .map(|situation| {
            situation
                .detail()
                .map_or_else(|| format!("{:?}", situation.kind()), str::to_owned)
        })
        .collect();
    Some(ConversationWorldSummary {
        scene_kind: scene.map(|scene| scene.scene_kind()),
        activity_level: scene.map_or(0.0, |scene| scene.activity_level()),
        interruption_cost: scene.map_or(0.0, |scene| scene.interruption_cost()),
        bot_addressed: scene.is_some_and(|scene| scene.bot_addressed()),
        active_situations,
        active_hypotheses: snapshot.hypotheses().len(),
        degraded_tools: environment
            .tools()
            .iter()
            .filter(|tool| tool.health() == yunxi_core::ServiceHealth::Degraded)
            .count(),
        unavailable_hosts: environment
            .hosts()
            .iter()
            .filter(|host| host.health() == yunxi_core::ServiceHealth::Unavailable)
            .count(),
    })
}

/// Human-readable bounded world status for `#world-status` / `#情境`
/// (admin only, v4 §155/§244). Never includes message content.
pub(crate) fn world_status_text() -> String {
    let summary = status_summary().unwrap_or_else(|| "World Model 未启用".to_owned());
    let mut text = format!("World Model v4 状态\n{summary}\n");
    if let Some(runtime) = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        let world = &runtime.world;
        let now = chrono::Utc::now();
        let context = yunxi_core::WorldSnapshotContext::new(now);
        if let Ok(snapshot) = world.snapshot_for(&context) {
            if snapshot.situations().is_empty() {
                text.push_str("活跃情境：无\n");
            } else {
                text.push_str("活跃情境：\n");
                for situation in snapshot
                    .situations()
                    .iter()
                    .filter(|s| s.status() == yunxi_core::SituationStatus::Active)
                    .take(8)
                {
                    text.push_str(&format!(
                        "- {:?} / {:?} / {:?}\n",
                        situation.kind(),
                        situation.state(),
                        situation.detail().unwrap_or(""),
                    ));
                }
            }
            text.push_str(&format!(
                "环境：模型健康 {:?}，主机 {} 个（可用率 {:.0}%），工具健康报告 {} 条\n",
                snapshot.environment().model_health(),
                snapshot.environment().hosts().len(),
                snapshot.environment().availability_fraction() * 100.0,
                snapshot.environment().tools().len(),
            ));
        }
    }
    truncate_chars(&text, 1200)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests share static runtime/counter state; serialize them so parallel
    /// execution cannot interleave resets with count assertions.
    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn disabled_config_is_a_noop() {
        let _guard = serial();
        reset_for_tests();
        // Config default: enabled=false → nothing stored.
        record_observation(
            WorldScope::Global,
            ObservationKind::SystemState,
            ObservationSource::SystemState,
            "build passed",
            None,
        );
        assert!(status_summary().is_none());
    }

    #[test]
    fn scene_person_id_is_stable_and_distinct() {
        let _guard = serial();
        let a = scene_person_id(10001);
        let b = scene_person_id(10001);
        let c = scene_person_id(10002);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.into_uuid().get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn activity_is_bounded() {
        let _guard = serial();
        let conversation_id = yunxi_core::ConversationId::new();
        push_and_activity(conversation_id);
        assert!(push_and_activity(conversation_id) > 0.0);
        assert!(push_and_activity(conversation_id) <= 1.0);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let _guard = serial();
        assert_eq!(truncate_chars("abc", 2), "ab");
        assert_eq!(truncate_chars("你好呀", 2), "你好");
    }

    #[test]
    fn delivery_outcome_is_deterministic_and_banded() {
        use yunxi_core::{OutcomeKind, ServiceHealth};
        // Host unavailable → send-now likely fails; defer is a medium bet.
        let now = delivery_outcome(ServiceHealth::Unavailable, DeliveryCandidate::SendNow);
        assert_eq!(now.description(), OutcomeKind::Failure);
        assert_eq!(now.band(), yunxi_core::ProbabilityBand::Low);
        let defer = delivery_outcome(ServiceHealth::Unavailable, DeliveryCandidate::Defer);
        assert_eq!(defer.description(), OutcomeKind::Success);
        assert!((defer.band() == yunxi_core::ProbabilityBand::Medium));
        // Healthy host → send-now high confidence.
        let healthy = delivery_outcome(ServiceHealth::Healthy, DeliveryCandidate::SendNow);
        assert_eq!(healthy.description(), OutcomeKind::Success);
        assert_eq!(healthy.band(), yunxi_core::ProbabilityBand::High);
        now.validate().expect("valid");
        defer.validate().expect("valid");
        healthy.validate().expect("valid");
    }

    #[test]
    fn delivery_simulator_produces_bounded_simulated_batch() {
        use yunxi_core::{
            EnvironmentUpdate, ExecutionMode, HostId, HostState, RuntimeLoad, ServiceHealth,
        };
        let now = chrono::Utc::now();
        let host = HostId::new("qq").expect("host");
        let mut world = WorldModel::new();
        world
            .update_environment(
                EnvironmentUpdate::new(
                    vec![
                        HostState::new(
                            host.clone(),
                            ServiceHealth::Unavailable,
                            now,
                            chrono::Duration::minutes(5),
                        )
                        .expect("host state"),
                    ],
                    vec![],
                    ServiceHealth::Healthy,
                    RuntimeLoad::new(0, None, 0, 1, now).expect("load"),
                )
                .expect("env"),
            )
            .expect("env");
        let batch = simulate_delivery_internal(&world, &host, now).expect("batch");
        assert_eq!(batch.results().len(), 2);
        assert!(
            batch
                .results()
                .iter()
                .all(|result| result.mode() == ExecutionMode::Simulated)
        );
        batch.validate().expect("batch validates");
        // Read-only: simulation never mutates the world.
        assert_eq!(world.version(), 2);
    }

    #[test]
    fn tool_failure_counter_increments_and_bounded() {
        let _guard = serial();
        reset_tool_failure_counts();
        assert_eq!(count_tool_failure("web_fetch"), 1);
        assert_eq!(count_tool_failure("web_fetch"), 2);
        assert_eq!(count_tool_failure("web_fetch"), 3);
        assert_eq!(count_tool_failure("other"), 1);
        reset_tool_failure_counts();
    }

    #[test]
    fn causal_promotes_after_repeated_tool_failures() {
        let _guard = serial();
        let mut world = WorldModel::new();
        // Under the threshold → nothing.
        if false {
            promote_tool_failure_causal(&mut world, "web_fetch", 2);
            assert!(world.causal().relations().is_empty());
        }
        // ≥3 occurrences → one tool-specific relation (v4 §98).
        promote_tool_failure_causal(&mut world, "web_fetch", 3);
        assert_eq!(world.causal().relations().len(), 1);
        assert!(matches!(
            world.causal().relations()[0].scope(),
            yunxi_core::CausalScope::ToolSpecific { .. }
        ));
        // Re-promotion is a dedupe no-op, not growth.
        promote_tool_failure_causal(&mut world, "web_fetch", 5);
        assert_eq!(world.causal().relations().len(), 1);
    }

    #[test]
    fn simulator_produces_bounded_simulated_batch() {
        let _guard = serial();
        use yunxi_core::{
            EnvironmentUpdate, ExecutionMode, RuntimeLoad, ServiceHealth, ToolHealth,
        };
        let now = chrono::Utc::now();
        let mut world = WorldModel::new();
        world
            .update_environment(
                EnvironmentUpdate::new(
                    vec![],
                    vec![
                        ToolHealth::new(
                            "web_fetch",
                            ServiceHealth::Degraded,
                            Some("429"),
                            now,
                            chrono::Duration::minutes(5),
                        )
                        .expect("tool"),
                    ],
                    ServiceHealth::Healthy,
                    RuntimeLoad::new(0, None, 0, 0, now).expect("load"),
                )
                .expect("env"),
            )
            .expect("env");
        let batch = simulate_tool_recovery_internal(&world, "web_fetch", now).expect("batch");
        assert_eq!(batch.results().len(), 2);
        assert!(
            batch
                .results()
                .iter()
                .all(|result| result.mode() == ExecutionMode::Simulated)
        );
        batch.validate().expect("batch valid");
        // Simulation never mutated the world (read-only by construction).
        assert_eq!(world.version(), 2);
    }

    #[test]
    fn interruption_guard_is_zero_when_disabled() {
        let _guard = serial();
        // Default config: enabled=false → guard is inert.
        assert_eq!(interruption_guard(yunxi_core::ConversationId::new()), 0.0);
    }

    #[test]
    fn render_world_context_is_objective_and_bounded() {
        let _guard = serial();
        use yunxi_core::WorldSnapshotContext;
        use yunxi_core::{SocialSceneKind, SocialSceneUpdate};
        let conversation_id = yunxi_core::ConversationId::new();
        let person_id = PersonId::new();
        let mut world = WorldModel::new();
        world
            .update_social_scene(
                SocialSceneUpdate::new(
                    conversation_id,
                    chrono::Utc::now(),
                    vec![person_id],
                    vec![person_id],
                    vec![person_id],
                    false,
                    0.9,
                    SocialSceneKind::RapidGroupChat,
                )
                .expect("scene"),
            )
            .expect("scene");
        let snapshot = world
            .snapshot_for(
                &WorldSnapshotContext::new(chrono::Utc::now()).with_conversation(conversation_id),
            )
            .expect("snapshot");
        let text = render_world_context(&snapshot);
        // Objective scene nouns only — no psychology, no ids, no numbers.
        assert!(text.contains("群里说得很快"));
        assert!(!text.contains("version"));
        assert!(text.chars().count() <= 600);
        // An empty world renders nothing.
        let empty = WorldModel::new()
            .snapshot_for(&WorldSnapshotContext::new(chrono::Utc::now()))
            .expect("snapshot");
        assert!(render_world_context(&empty).is_empty());
    }

    #[test]
    fn future_event_situation_derives_only_with_time_cue() {
        let _guard = serial();
        use yunxi_core::world_model::{SituationKind, SituationState};
        let conversation_id = yunxi_core::ConversationId::new();
        let person_id = PersonId::new();
        let now = chrono::Utc::now();
        let mut world = WorldModel::new();
        // Keyword without time cue → nothing (v4 §198: 不做日历事实).
        derive_future_event_situation(&mut world, conversation_id, person_id, "我面试过了", now)
            .expect("ok");
        assert!(world.situations().is_empty());
        // Keyword + time cue → Planned FutureEvent (Scenario A/E shape).
        derive_future_event_situation(
            &mut world,
            conversation_id,
            person_id,
            "我明天上午去面试",
            now,
        )
        .expect("ok");
        assert_eq!(world.situations().len(), 1);
        assert_eq!(world.situations()[0].kind(), SituationKind::FutureEvent);
        assert_eq!(world.situations()[0].state(), SituationState::Planned);
        // Dedupe: another interview mention does not multiply.
        derive_future_event_situation(&mut world, conversation_id, person_id, "后天还有复试", now)
            .expect("ok");
        assert_eq!(world.situations().len(), 1);
        // 24h+ stale planned event expires (v4 §92), not linger forever.
        let later = now + chrono::Duration::hours(25);
        derive_future_event_situation(&mut world, conversation_id, person_id, "明天体检", later)
            .expect("ok");
        assert_eq!(world.situations()[0].state(), SituationState::Expired);
    }

    #[test]
    fn tool_recovery_outcome_is_deterministic_and_banded() {
        let _guard = serial();
        use yunxi_core::{OutcomeKind, ServiceHealth};
        // Degraded + retry-now → likely fail (Low band).
        let retry = tool_recovery_outcome(
            "web_fetch",
            ServiceHealth::Degraded,
            ToolRecoveryCandidate::RetryNow,
        );
        assert_eq!(retry.description(), OutcomeKind::Failure);
        assert_eq!(retry.band(), yunxi_core::ProbabilityBand::Low);
        // Degraded + fallback → medium success (Medium band).
        let fallback = tool_recovery_outcome(
            "web_fetch",
            ServiceHealth::Degraded,
            ToolRecoveryCandidate::UseFallback,
        );
        assert_eq!(fallback.description(), OutcomeKind::Success);
        assert_eq!(fallback.band(), yunxi_core::ProbabilityBand::Medium);
        // Healthy + retry-now → high success.
        let healthy = tool_recovery_outcome(
            "web_fetch",
            ServiceHealth::Healthy,
            ToolRecoveryCandidate::RetryNow,
        );
        assert_eq!(healthy.description(), OutcomeKind::Success);
        assert_eq!(healthy.band(), yunxi_core::ProbabilityBand::High);
        // Every outcome validates (quantized + band consistent).
        retry.validate().expect("retry validates");
        fallback.validate().expect("fallback validates");
        healthy.validate().expect("healthy validates");
    }

    #[test]
    fn scene_derivation_creates_dedupes_and_expires_situation() {
        let _guard = serial();
        use yunxi_core::world_model::{SituationState, SocialSceneKind, SocialSceneUpdate};
        let conversation_id = yunxi_core::ConversationId::new();
        let mut world = WorldModel::new();
        let now = chrono::Utc::now();
        // No scene → nothing is invented.
        apply_scene_derivation(&mut world, conversation_id, now).expect("ok");
        assert!(world.situations().is_empty());
        // Active group scene → situation derived.
        world
            .update_social_scene(
                SocialSceneUpdate::new(
                    conversation_id,
                    now,
                    vec![PersonId::new()],
                    vec![],
                    vec![],
                    false,
                    0.8,
                    SocialSceneKind::GroupDiscussion,
                )
                .expect("scene update"),
            )
            .expect("scene");
        apply_scene_derivation(&mut world, conversation_id, now).expect("derive");
        assert_eq!(world.situations().len(), 1);
        assert_eq!(world.situations()[0].state(), SituationState::InProgress);
        // Dedupe: a second derivation does not multiply.
        apply_scene_derivation(&mut world, conversation_id, now).expect("derive again");
        assert_eq!(world.situations().len(), 1);
        // Idle > 10 min → OutcomeUnknown (never stays "in progress").
        let later = now + chrono::Duration::minutes(11);
        apply_scene_derivation(&mut world, conversation_id, later).expect("maintain");
        assert_eq!(
            world.situations()[0].state(),
            SituationState::OutcomeUnknown
        );
        assert!(world.situations()[0].is_active());
    }
}
