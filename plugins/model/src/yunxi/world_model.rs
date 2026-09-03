//! Shadow-mode World Model v4 runtime (host-side adapter).
//!
//! This is the platform-specific half of the v4 blueprint: it feeds the
//! platform-neutral [`yunxi_core::WorldModel`] with observations and social
//! scenes derived from host events. It is gated by `[world_model].enabled`
//! (default `false`), never blocks or alters a reply, and when enabled still
//! only *records* state — nothing in this module decides to act, send, or
//! cancel. That belongs to Executive / Core (v4 §7, §56, §249–§255).

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use yunxi_core::world_model::{
    EntityKind, EntityUpdateAction, EntityUpdateProposal, ObservationDraft, ObservationId,
    ObservationKind, ObservationPayload, ObservationSource, SocialSceneKind, SocialSceneUpdate,
    StateProperty, WorldModel, WorldScope,
};
use yunxi_core::PersonId;

const WORLD_LOG_PREFIX: &str = "[YUNXI_WORLD]";

struct WorldRuntime {
    world: WorldModel,
}

static WORLD_RUNTIME: LazyLock<Mutex<Option<WorldRuntime>>> =
    LazyLock::new(|| Mutex::new(None));

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
}

/// Reset the in-memory runtime (used by tests). No-op production effect.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut guard = WORLD_RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
    let mut tracker = ACTIVITY_TRACKER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tracker.recent.clear();
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
        let content = truncate_chars(content, yunxi_core::world_model::MAX_OBSERVATION_PAYLOAD_CHARS);
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
        let observation = match draft.build(ObservationId::new(), yunxi_core::EventId::new(), chrono::Utc::now())
        {
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
            if bot_addressed { vec![participant] } else { vec![] },
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
    const NAMESPACE: uuid::Uuid =
        uuid::Uuid::from_u128(0x9e2c_6f6a_4c3b_4f7e_9a10_0d5c_2b8e_4a19);
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
        && tracker.recent.len() >= max_scenes {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_config_is_a_noop() {
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
        let a = scene_person_id(10001);
        let b = scene_person_id(10001);
        let c = scene_person_id(10002);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.into_uuid().get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn activity_is_bounded() {
        let conversation_id = yunxi_core::ConversationId::new();
        push_and_activity(conversation_id);
        assert!(push_and_activity(conversation_id) > 0.0);
        assert!(push_and_activity(conversation_id) <= 1.0);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_chars("abc", 2), "ab");
        assert_eq!(truncate_chars("你好呀", 2), "你好");
    }
}
