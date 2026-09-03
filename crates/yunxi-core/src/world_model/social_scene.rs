//! SocialScene: the current conversational situation of one conversation
//! (v4 §68–72, §184). Deterministic — no large model, no psychology.

use super::{
    WorldValidationError,
    common::{clamp_unit, dedupe, validate_unit},
};
use crate::{ConversationId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_SCENE_ACTIVITY_PARTICIPANTS: usize = 64;
pub const MAX_SCENE_CURRENT_FLOOR: usize = 8;
pub const MAX_SCENE_RECENT_SPEAKERS: usize = 16;
pub const MAX_SCENES_PER_WORLD: usize = 256;

/// What kind of conversational scene this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SocialSceneKind {
    DirectConversation,
    GroupDiscussion,
    RapidGroupChat,
    IdleGroup,
    TaskConversation,
    Unknown,
}

/// Deterministic interruption cost estimate (v4 §100, §145–146).
///
/// - addressed → low base;
/// - not addressed + high activity → higher;
/// - someone else holds the floor → additional;
/// - rapid group chat adds the most, idle group the least.
#[must_use]
pub fn floor_interruption_cost(
    bot_addressed: bool,
    activity_level: f32,
    others_holding_floor: bool,
    scene_kind: SocialSceneKind,
) -> f32 {
    let activity = if activity_level.is_finite() {
        activity_level.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut cost = if bot_addressed { 0.10 } else { 0.35 } + activity * 0.30;
    if others_holding_floor {
        cost += 0.20;
    }
    match scene_kind {
        SocialSceneKind::RapidGroupChat => {
            if !bot_addressed {
                cost += 0.15;
            }
        }
        SocialSceneKind::IdleGroup => cost -= 0.10,
        SocialSceneKind::DirectConversation | SocialSceneKind::TaskConversation => {}
        SocialSceneKind::GroupDiscussion | SocialSceneKind::Unknown => {}
    }
    cost.clamp(0.0, 1.0)
}

/// A deterministic scene update derived from host message events (v4 §145).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocialSceneUpdate {
    conversation_id: ConversationId,
    now: DateTime<Utc>,
    active_participants: Vec<PersonId>,
    current_floor: Vec<PersonId>,
    recent_speaking_order: Vec<PersonId>,
    bot_addressed: bool,
    activity_level: f32,
    scene_kind: SocialSceneKind,
}

impl SocialSceneUpdate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conversation_id: ConversationId,
        now: DateTime<Utc>,
        active_participants: Vec<PersonId>,
        current_floor: Vec<PersonId>,
        recent_speaking_order: Vec<PersonId>,
        bot_addressed: bool,
        activity_level: f32,
        scene_kind: SocialSceneKind,
    ) -> Result<Self, WorldValidationError> {
        let update = Self {
            conversation_id,
            now,
            active_participants: dedupe(active_participants, "scene participants", true)?,
            current_floor: dedupe(current_floor, "scene floor", true)?,
            recent_speaking_order: dedupe(recent_speaking_order, "scene speakers", true)?,
            bot_addressed,
            activity_level: clamp_unit(activity_level),
            scene_kind,
        };
        update.validate()?;
        Ok(update)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.active_participants.len() > MAX_SCENE_ACTIVITY_PARTICIPANTS {
            return Err(WorldValidationError::TooManyItems {
                field: "scene participants",
                length: self.active_participants.len(),
                maximum: MAX_SCENE_ACTIVITY_PARTICIPANTS,
            });
        }
        if self.current_floor.len() > MAX_SCENE_CURRENT_FLOOR {
            return Err(WorldValidationError::TooManyItems {
                field: "scene floor",
                length: self.current_floor.len(),
                maximum: MAX_SCENE_CURRENT_FLOOR,
            });
        }
        if self.recent_speaking_order.len() > MAX_SCENE_RECENT_SPEAKERS {
            return Err(WorldValidationError::TooManyItems {
                field: "scene speakers",
                length: self.recent_speaking_order.len(),
                maximum: MAX_SCENE_RECENT_SPEAKERS,
            });
        }
        // The floor must be a subset of active participants.
        for person in &self.current_floor {
            if !self.active_participants.contains(person) {
                return Err(WorldValidationError::InvalidState {
                    reason: "floor includes a non-active participant",
                });
            }
        }
        validate_unit(self.activity_level, "scene activity level")?;
        Ok(())
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub const fn now(&self) -> DateTime<Utc> {
        self.now
    }

    #[must_use]
    pub fn active_participants(&self) -> &[PersonId] {
        &self.active_participants
    }

    #[must_use]
    pub fn current_floor(&self) -> &[PersonId] {
        &self.current_floor
    }

    #[must_use]
    pub fn recent_speaking_order(&self) -> &[PersonId] {
        &self.recent_speaking_order
    }

    #[must_use]
    pub const fn bot_addressed(&self) -> bool {
        self.bot_addressed
    }

    #[must_use]
    pub const fn activity_level(&self) -> f32 {
        self.activity_level
    }

    #[must_use]
    pub const fn scene_kind(&self) -> SocialSceneKind {
        self.scene_kind
    }
}

/// Live social scene for one conversation (v4 §69).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocialSceneState {
    conversation_id: ConversationId,
    active_participants: Vec<PersonId>,
    current_floor: Vec<PersonId>,
    recent_speaking_order: Vec<PersonId>,
    bot_addressed: bool,
    activity_level: f32,
    interruption_cost: f32,
    scene_kind: SocialSceneKind,
    conversation_version: u64,
    updated_at: DateTime<Utc>,
}

impl SocialSceneState {
    pub fn new(
        conversation_id: ConversationId,
        now: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let scene = Self {
            conversation_id,
            active_participants: Vec::new(),
            current_floor: Vec::new(),
            recent_speaking_order: Vec::new(),
            bot_addressed: false,
            activity_level: 0.0,
            interruption_cost: 0.0,
            scene_kind: SocialSceneKind::Unknown,
            conversation_version: 1,
            updated_at: now,
        };
        scene.validate()?;
        Ok(scene)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.active_participants.len() > MAX_SCENE_ACTIVITY_PARTICIPANTS {
            return Err(WorldValidationError::TooManyItems {
                field: "scene participants",
                length: self.active_participants.len(),
                maximum: MAX_SCENE_ACTIVITY_PARTICIPANTS,
            });
        }
        if self.current_floor.len() > MAX_SCENE_CURRENT_FLOOR {
            return Err(WorldValidationError::TooManyItems {
                field: "scene floor",
                length: self.current_floor.len(),
                maximum: MAX_SCENE_CURRENT_FLOOR,
            });
        }
        if self.recent_speaking_order.len() > MAX_SCENE_RECENT_SPEAKERS {
            return Err(WorldValidationError::TooManyItems {
                field: "scene speakers",
                length: self.recent_speaking_order.len(),
                maximum: MAX_SCENE_RECENT_SPEAKERS,
            });
        }
        for person in &self.current_floor {
            if !self.active_participants.contains(person) {
                return Err(WorldValidationError::InvalidState {
                    reason: "floor includes a non-active participant",
                });
            }
        }
        validate_unit(self.activity_level, "scene activity level")?;
        validate_unit(self.interruption_cost, "scene interruption cost")?;
        if self.conversation_version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        Ok(())
    }

    /// Restore a persisted scene (adapter use): re-validates everything.
    #[allow(clippy::too_many_arguments)]
    pub fn restore(
        conversation_id: ConversationId,
        active_participants: Vec<PersonId>,
        current_floor: Vec<PersonId>,
        recent_speaking_order: Vec<PersonId>,
        bot_addressed: bool,
        activity_level: f32,
        interruption_cost: f32,
        scene_kind: SocialSceneKind,
        conversation_version: u64,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let scene = Self {
            conversation_id,
            active_participants: dedupe(active_participants, "scene participants", true)?,
            current_floor: dedupe(current_floor, "scene floor", true)?,
            recent_speaking_order: dedupe(recent_speaking_order, "scene speakers", true)?,
            bot_addressed,
            activity_level: clamp_unit(activity_level),
            interruption_cost: clamp_unit(interruption_cost),
            scene_kind,
            conversation_version,
            updated_at,
        };
        scene.validate()?;
        Ok(scene)
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub fn active_participants(&self) -> &[PersonId] {
        &self.active_participants
    }

    #[must_use]
    pub fn current_floor(&self) -> &[PersonId] {
        &self.current_floor
    }

    #[must_use]
    pub fn recent_speaking_order(&self) -> &[PersonId] {
        &self.recent_speaking_order
    }

    #[must_use]
    pub const fn bot_addressed(&self) -> bool {
        self.bot_addressed
    }

    #[must_use]
    pub const fn activity_level(&self) -> f32 {
        self.activity_level
    }

    #[must_use]
    pub const fn interruption_cost(&self) -> f32 {
        self.interruption_cost
    }

    #[must_use]
    pub const fn scene_kind(&self) -> SocialSceneKind {
        self.scene_kind
    }

    #[must_use]
    pub const fn conversation_version(&self) -> u64 {
        self.conversation_version
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    /// Apply a deterministic scene update (v4 §146, no model needed).
    pub fn apply(&mut self, update: SocialSceneUpdate) -> Result<(), WorldValidationError> {
        update.validate()?;
        if update.conversation_id() != self.conversation_id {
            return Err(WorldValidationError::InvalidState {
                reason: "scene update targets a different conversation",
            });
        }
        if update.now() < self.updated_at {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "scene update predates stored state",
            });
        }
        self.active_participants = update.active_participants().to_vec();
        self.current_floor = update.current_floor().to_vec();
        self.recent_speaking_order = update.recent_speaking_order().to_vec();
        self.bot_addressed = update.bot_addressed();
        self.activity_level = update.activity_level();
        self.scene_kind = update.scene_kind();
        self.interruption_cost = floor_interruption_cost(
            self.bot_addressed,
            self.activity_level,
            !self.current_floor.is_empty(),
            self.scene_kind,
        );
        self.conversation_version = self.conversation_version.saturating_add(1);
        self.updated_at = update.now();
        self.validate()
    }

    /// Does `person` currently hold the floor?
    #[must_use]
    pub fn person_has_floor(&self, person_id: PersonId) -> bool {
        self.current_floor.contains(&person_id)
    }

    /// Mark that a conversation-level event happened (e.g. a message
    /// collision): bump `conversation_version` and `updated_at` only. Single
    /// fact, no psychology (v4 appendix §4–§5).
    pub fn touch(&mut self, now: DateTime<Utc>) -> Result<(), WorldValidationError> {
        if now < self.updated_at {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "scene touch predates stored state",
            });
        }
        self.conversation_version = self.conversation_version.saturating_add(1);
        self.updated_at = now;
        self.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interruption_cost_is_deterministic_and_bounded() {
        // Addressed: low base even in a rapid group chat.
        let addressed = floor_interruption_cost(true, 0.9, false, SocialSceneKind::RapidGroupChat);
        // Not addressed, high activity, floor held: the highest cost.
        let distracted = floor_interruption_cost(false, 0.9, true, SocialSceneKind::RapidGroupChat);
        let idle = floor_interruption_cost(false, 0.1, false, SocialSceneKind::IdleGroup);
        assert!((0.0..=1.0).contains(&addressed));
        assert!((0.0..=1.0).contains(&distracted));
        assert!(addressed < distracted);
        assert!(idle < distracted);
        // Direct conversation is cheaper than rapid group chat when unaddressed…
        let direct =
            floor_interruption_cost(false, 0.5, false, SocialSceneKind::DirectConversation);
        let group = floor_interruption_cost(false, 0.5, false, SocialSceneKind::GroupDiscussion);
        assert!(direct <= group);
    }

    #[test]
    fn scene_updates_keep_invariants() {
        let person_a = PersonId::new();
        let person_b = PersonId::new();
        let now = Utc::now();
        let update = SocialSceneUpdate::new(
            ConversationId::new(),
            now,
            vec![person_a, person_b],
            vec![person_a],
            vec![person_b, person_a],
            false,
            0.8,
            SocialSceneKind::RapidGroupChat,
        )
        .expect("update");
        let mut scene = SocialSceneState::new(update.conversation_id(), now).expect("scene");
        scene.apply(update).expect("apply");
        assert_eq!(scene.conversation_version(), 2);
        assert!(scene.person_has_floor(person_a));
        assert!(!scene.person_has_floor(person_b));
        assert_eq!(scene.activity_level(), 0.8);
        let cost = scene.interruption_cost();
        assert!(cost > 0.5); // unaddressed, active, floor taken by person_a
        scene.validate().expect("valid");
    }

    #[test]
    fn floor_must_be_subset_of_participants() {
        let now = Utc::now();
        let update = SocialSceneUpdate::new(
            ConversationId::new(),
            now,
            vec![PersonId::new()],
            vec![PersonId::new()],
            vec![],
            false,
            0.3,
            SocialSceneKind::GroupDiscussion,
        );
        assert!(update.is_err());
    }

    #[test]
    fn out_of_order_scene_update_is_rejected() {
        let now = Utc::now();
        let update = SocialSceneUpdate::new(
            ConversationId::new(),
            now,
            vec![PersonId::new()],
            vec![],
            vec![],
            true,
            0.1,
            SocialSceneKind::DirectConversation,
        )
        .expect("update");
        let mut scene = SocialSceneState::new(update.conversation_id(), now).expect("scene");
        scene.apply(update.clone()).expect("apply");
        let later = SocialSceneUpdate::new(
            update.conversation_id(),
            now - chrono::Duration::minutes(1),
            vec![],
            vec![],
            vec![],
            true,
            0.1,
            SocialSceneKind::DirectConversation,
        )
        .expect("update");
        // Note: `now` is reused; subtraction puts it before stored time.
        assert!(scene.apply(later).is_err());
    }
}
