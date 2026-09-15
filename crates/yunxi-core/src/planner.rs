//! Platform-neutral planning boundary.
//!
//! A planner turns a bounded snapshot of the Core world into a validated
//! high-level decision.  It does not execute actions and it does not know
//! which host will eventually deliver an intent.

use crate::arbiter::ActionCapability;
use crate::arbiter::ActionDescriptor;
use crate::event::{MessageReceivedEvent, WorldEvent};
use crate::executive::ExecutiveSnapshot;
use crate::goal::Goal;
use crate::identity::{ConversationId, ConversationKind, ConversationMember, OpenLoopId, PersonId};
use crate::intent::{CognitiveIntent, IntentValidationError};
use crate::memory::Memory;
use crate::mind::{MindSnapshot, MindValidationError};
use crate::open_loop::OpenLoop;
use crate::working_state::ConversationSnapshot;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

pub const MAX_PLANNER_INTENTS: usize = 32;
pub const MAX_PLANNER_STATE_UPDATES: usize = 32;
pub const MAX_PLANNER_MEMORIES: usize = 128;
pub const MAX_PLANNER_OPEN_LOOPS: usize = 128;
pub const MAX_PLANNER_GOALS: usize = 128;
pub const MAX_PLANNER_PARTICIPANTS: usize = 256;
pub const MAX_PLANNER_TOPIC_CHARS: usize = 512;
pub const MAX_PLANNER_TOPIC_BYTES: usize = 2 * 1_024;

/// Slow, platform-neutral affect values made available to a planner.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AffectState {
    pub valence: f32,
    pub arousal: f32,
    pub social_energy: f32,
    pub curiosity: f32,
}

impl Default for AffectState {
    fn default() -> Self {
        Self {
            valence: 0.0,
            arousal: 0.0,
            social_energy: 1.0,
            curiosity: 0.5,
        }
    }
}

impl AffectState {
    pub fn validate(self) -> Result<(), PlannerInputValidationError> {
        if !self.valence.is_finite()
            || !self.arousal.is_finite()
            || !self.social_energy.is_finite()
            || !self.curiosity.is_finite()
        {
            return Err(PlannerInputValidationError::NonFiniteAffect);
        }
        if !(-1.0..=1.0).contains(&self.valence)
            || !(-1.0..=1.0).contains(&self.arousal)
            || !(0.0..=1.0).contains(&self.social_energy)
            || !(0.0..=1.0).contains(&self.curiosity)
        {
            return Err(PlannerInputValidationError::AffectOutOfRange);
        }
        Ok(())
    }
}

/// Relation context keyed by the Core person identity, never by a host ID.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RelationState {
    pub person_id: PersonId,
    pub familiarity: f32,
    pub affinity: f32,
    pub trust: f32,
    pub comfort: f32,
    pub tension: f32,
}

impl RelationState {
    pub fn new(person_id: PersonId) -> Self {
        Self {
            person_id,
            familiarity: 0.0,
            affinity: 0.0,
            trust: 0.0,
            comfort: 0.0,
            tension: 0.0,
        }
    }

    pub fn validate(self) -> Result<(), PlannerInputValidationError> {
        let values = [
            self.familiarity,
            self.affinity,
            self.trust,
            self.comfort,
            self.tension,
        ];
        if values.iter().any(|value| !value.is_finite()) {
            return Err(PlannerInputValidationError::NonFiniteRelation);
        }
        if values.iter().any(|value| !(-1.0..=1.0).contains(value)) {
            return Err(PlannerInputValidationError::RelationOutOfRange);
        }
        Ok(())
    }
}

/// Optional semantic evidence supplied by a host that already performed
/// message understanding. The values are fixed-size and platform-neutral;
/// Core never calls another model to populate them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InteractionCues {
    /// User sentiment valence in `[-1, 1]`.
    pub sentiment_valence: f32,
    /// User sentiment activation in `[-1, 1]`.
    pub sentiment_arousal: f32,
    /// Confidence for the sentiment fields in `[0, 1]`.
    pub sentiment_confidence: f32,
    /// Strength of explicit gratitude directed at the agent in `[0, 1]`.
    pub gratitude_strength: f32,
}

impl Default for InteractionCues {
    fn default() -> Self {
        Self {
            sentiment_valence: 0.0,
            sentiment_arousal: 0.0,
            sentiment_confidence: 0.0,
            gratitude_strength: 0.0,
        }
    }
}

impl InteractionCues {
    pub fn validate(self) -> Result<(), InteractionCueValidationError> {
        for (field, value) in [
            ("sentiment_valence", self.sentiment_valence),
            ("sentiment_arousal", self.sentiment_arousal),
            ("sentiment_confidence", self.sentiment_confidence),
            ("gratitude_strength", self.gratitude_strength),
        ] {
            if !value.is_finite() {
                return Err(InteractionCueValidationError::NonFinite { field });
            }
        }
        for (field, value) in [
            ("sentiment_valence", self.sentiment_valence),
            ("sentiment_arousal", self.sentiment_arousal),
        ] {
            if !(-1.0..=1.0).contains(&value) {
                return Err(InteractionCueValidationError::OutOfRange { field });
            }
        }
        for (field, value) in [
            ("sentiment_confidence", self.sentiment_confidence),
            ("gratitude_strength", self.gratitude_strength),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(InteractionCueValidationError::OutOfRange { field });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InteractionCueValidationError {
    #[error("interaction cue `{field}` must be finite")]
    NonFinite { field: &'static str },
    #[error("interaction cue `{field}` is outside its supported range")]
    OutOfRange { field: &'static str },
}

/// A deterministic, platform-neutral state transition for one attended
/// message. These are deliberately slow contextual signals, not sentiment
/// analysis: richer semantic cues can be added by a host without making Core
/// invoke another model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InteractionStateEvolution {
    pub affect: AffectState,
    pub relation: RelationState,
}

/// Evolve the sender-scoped affect and relation state from the message's own
/// structural signals. The transition scans at most a bounded prefix of the
/// message and never accepts a relation belonging to another person.
///
/// This is the structural half of a turn; the semantic half is
/// [`apply_interaction_cues`]. Hosts apply both once per understood message.
#[must_use]
pub fn evolve_interaction_state(
    message: &MessageReceivedEvent,
    relation: Option<RelationState>,
    affect: AffectState,
) -> InteractionStateEvolution {
    evolve_interaction_state_inner(message, relation, affect)
}

/// Apply semantic evidence after the structural message event has already
/// been observed. This lets a host project the result of an existing
/// understanding pass without counting the same interaction twice.
pub fn apply_interaction_cues(
    person_id: PersonId,
    relation: Option<RelationState>,
    affect: AffectState,
    cues: InteractionCues,
) -> Result<InteractionStateEvolution, InteractionCueValidationError> {
    cues.validate()?;
    let relation = relation
        .filter(|state| state.person_id == person_id && state.validate().is_ok())
        .unwrap_or_else(|| RelationState::new(person_id));
    let mut affect = if affect.validate().is_ok() {
        affect
    } else {
        AffectState::default()
    };

    let gratitude = cues.gratitude_strength;
    if gratitude > 0.0 {
        affect.social_energy = blend_bounded(
            affect.social_energy,
            (affect.social_energy + 0.04 * gratitude).clamp(0.0, 1.0),
            0.04,
            0.0,
            1.0,
        );
    }

    let confidence = cues.sentiment_confidence;
    let valence_rate = 0.075 * confidence + 0.03 * gratitude;
    if valence_rate > 0.0 {
        let target = (cues.sentiment_valence * confidence + 0.25 * gratitude).clamp(-1.0, 1.0);
        affect.valence = blend_bounded(affect.valence, target, valence_rate, -1.0, 1.0);
    }
    if confidence > 0.0 {
        affect.arousal = blend_bounded(
            affect.arousal,
            cues.sentiment_arousal,
            0.12 * confidence,
            -1.0,
            1.0,
        );
    }

    // **这条通道只改情绪，不改关系。** `affect` 是小时级的当下状态，由这一轮
    // 语义理解直接写；而 `relation` 的五个维度各自只有一个写者，由宿主的两条
    // 通道负责（见 `relation_evidence_nudge`）：
    //
    // - `familiarity` / `comfort`：回合收尾的整行回写（结构演化，本文件的
    //   `evolve_interaction_state_inner`）。
    // - `tension` / `affinity` / `trust`：相处证据的 delta 通道。
    //
    // 为什么把关系挡在这条通道之外：回合收尾写回的是**回合开始时的快照**。历史上
    // 它同时携带 tension，于是在回合进行中到账的相处证据会被旧快照覆盖
    // （2026-09-14 实测：证据写 0.0256、回合收尾写回 0）。同一个坑对 affinity /
    // trust 一样成立，所以三者一起交给 delta 通道，整行回写不再写它们。
    //
    // 善意因此仍然会让关系变好：`gratitude_strength` 由宿主折算成一条正向
    // evidence 送进 delta 通道（`relation_evidence_nudge`），字段本身留在
    // `InteractionCues` 里没有变，只是换了落库的那条路。

    Ok(InteractionStateEvolution { affect, relation })
}

/// Structural evolution only: it reads the message's own deterministic shape
/// (conversation kind, whether it answers her, length, punctuation) and never
/// the semantic cues. Cue-driven affect lives in [`apply_interaction_cues`],
/// which the host applies once per understanding pass.
fn evolve_interaction_state_inner(
    message: &MessageReceivedEvent,
    relation: Option<RelationState>,
    affect: AffectState,
) -> InteractionStateEvolution {
    const MAX_SIGNAL_CHARS: usize = 1_024;

    let mut relation = relation
        .filter(|state| state.person_id == message.sender && state.validate().is_ok())
        .unwrap_or_else(|| RelationState::new(message.sender));
    let mut affect = if affect.validate().is_ok() {
        affect
    } else {
        AffectState::default()
    };

    let mut character_count = 0usize;
    let mut question = false;
    let mut emphasis_count = 0usize;
    for character in message.content.as_text().chars().take(MAX_SIGNAL_CHARS + 1) {
        character_count = character_count.saturating_add(1);
        question |= matches!(character, '?' | '？');
        if matches!(character, '!' | '！') {
            emphasis_count = emphasis_count.saturating_add(1).min(3);
        }
    }
    character_count = character_count.min(MAX_SIGNAL_CHARS);

    let base_familiarity = match message.conversation_kind {
        ConversationKind::Direct => 0.012,
        ConversationKind::Group => 0.005,
        ConversationKind::System => 0.0,
    };
    let familiarity_rate = base_familiarity
        + if message.replies_to_agent { 0.004 } else { 0.0 }
        + if message.addressed_to_agent {
            0.002
        } else {
            0.0
        }
        + if message.explicit_request { 0.002 } else { 0.0 };
    relation.familiarity =
        (relation.familiarity + familiarity_rate * (1.0 - relation.familiarity)).clamp(-1.0, 1.0);

    let comfort_target = if message.stop_requested {
        -0.3
    } else if message.replies_to_agent {
        0.22
    } else if message.conversation_kind == ConversationKind::Direct {
        0.12
    } else if message.addressed_to_agent {
        0.05
    } else {
        relation.comfort
    };
    relation.comfort = blend_bounded(
        relation.comfort,
        comfort_target,
        if message.stop_requested { 0.08 } else { 0.025 },
        -1.0,
        1.0,
    );
    // 结构演化只碰 familiarity（这次互动本身让她更熟悉对方）与 comfort（这次互动
    // 的相处口径）。tension / affinity / trust 由证据 delta 通道独占：回合收尾写回的
    // 是回合开始时的快照，它一旦也写这三列，就会覆盖掉回合进行中到账的证据。

    // 没有语义 cues 时，情绪只被结构信号往基线拉。语义那部分由
    // `apply_interaction_cues` 在同一次理解里补上，两边各写一次，不重复计入。
    affect.valence = blend_bounded(affect.valence, 0.0, 0.025, -1.0, 1.0);
    let structural_arousal = if question { 0.18 } else { 0.0 } + emphasis_count as f32 * 0.12;
    let arousal_target = structural_arousal
        .clamp(-1.0, 1.0)
        .max(if message.stop_requested { 0.65 } else { -1.0 });
    affect.arousal = blend_bounded(affect.arousal, arousal_target, 0.12, -1.0, 1.0);

    let length_load = match character_count {
        0..=40 => 0.0,
        41..=160 => 0.06,
        161..=480 => 0.13,
        _ => 0.2,
    };
    let attachment_load = (message.content.attachments().len().min(4) as f32) * 0.05;
    let social_energy_target = (0.88 - length_load - attachment_load
        + if message.replies_to_agent { 0.04 } else { 0.0 }
        - if message.stop_requested { 0.12 } else { 0.0 })
    .clamp(0.25, 0.95);
    affect.social_energy =
        blend_bounded(affect.social_energy, social_energy_target, 0.04, 0.0, 1.0);

    let curiosity_target = (0.5_f32
        + if message.explicit_request { 0.08 } else { 0.0 }
        + if question { 0.24 } else { 0.0 }
        + if message.content.attachments().is_empty() {
            0.0
        } else {
            0.05
        }
        - if message.stop_requested { 0.16 } else { 0.0 })
    .clamp(0.0, 1.0);
    affect.curiosity = blend_bounded(affect.curiosity, curiosity_target, 0.08, 0.0, 1.0);

    InteractionStateEvolution { affect, relation }
}

/// Apply elapsed-time affect drift without consulting a clock. A host chooses
/// the elapsed duration, while Core owns the stable equilibrium and rates.
#[must_use]
pub fn drift_affect_state(state: AffectState, elapsed: Duration) -> AffectState {
    let mut state = if state.validate().is_ok() {
        state
    } else {
        AffectState::default()
    };
    state.valence = decay_toward(state.valence, 0.0, elapsed, 12.0 * 60.0 * 60.0, -1.0, 1.0);
    state.arousal = decay_toward(state.arousal, 0.0, elapsed, 2.0 * 60.0 * 60.0, -1.0, 1.0);
    state.social_energy = decay_toward(
        state.social_energy,
        1.0,
        elapsed,
        6.0 * 60.0 * 60.0,
        0.0,
        1.0,
    );
    state.curiosity = decay_toward(state.curiosity, 0.5, elapsed, 24.0 * 60.0 * 60.0, 0.0, 1.0);
    state
}

/// Half-lives of the five relation dimensions, in seconds.
///
/// These are the single source of truth for decay: [`drift_relation_state`]
/// uses them for reads, and a persistence adapter that materialises drift
/// inside its own UPDATE must bind these same values rather than restating
/// them. A second copy of the numbers is how the two paths would silently
/// disagree.
pub mod relation_half_lives {
    const DAY_SECONDS: f64 = 24.0 * 60.0 * 60.0;

    /// 180 days.
    pub const FAMILIARITY_SECONDS: f64 = 180.0 * DAY_SECONDS;
    /// 365 days.
    pub const AFFINITY_SECONDS: f64 = 365.0 * DAY_SECONDS;
    /// 730 days.
    pub const TRUST_SECONDS: f64 = 730.0 * DAY_SECONDS;
    /// 30 days.
    pub const COMFORT_SECONDS: f64 = 30.0 * DAY_SECONDS;
    /// 3 days.
    pub const TENSION_SECONDS: f64 = 3.0 * DAY_SECONDS;
}

/// Apply slow elapsed-time relation drift while preserving the canonical
/// person identity. Durable dimensions decay much more slowly than transient
/// comfort and tension. Every dimension decays multiplicatively toward zero.
#[must_use]
pub fn drift_relation_state(mut state: RelationState, elapsed: Duration) -> RelationState {
    if state.validate().is_err() {
        return RelationState::new(state.person_id);
    }
    state.familiarity = decay_toward(
        state.familiarity,
        0.0,
        elapsed,
        relation_half_lives::FAMILIARITY_SECONDS,
        -1.0,
        1.0,
    );
    state.affinity = decay_toward(
        state.affinity,
        0.0,
        elapsed,
        relation_half_lives::AFFINITY_SECONDS,
        -1.0,
        1.0,
    );
    state.trust = decay_toward(
        state.trust,
        0.0,
        elapsed,
        relation_half_lives::TRUST_SECONDS,
        -1.0,
        1.0,
    );
    state.comfort = decay_toward(
        state.comfort,
        0.0,
        elapsed,
        relation_half_lives::COMFORT_SECONDS,
        -1.0,
        1.0,
    );
    state.tension = decay_toward(
        state.tension,
        0.0,
        elapsed,
        relation_half_lives::TENSION_SECONDS,
        -1.0,
        1.0,
    );
    state
}

/// One dimension's move toward a target by a bounded mixing rate:
/// `current + (target - current) * rate`.
///
/// This is deliberately the *only* shape a relation update can take. Every
/// writer (a turn's structural evolution, a piece of relationship evidence)
/// expresses itself this way, so two writers touching the same row can both be
/// applied — even when they overlap — instead of the later one overwriting the
/// earlier one with a stale absolute value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RelationPull {
    pub target: f32,
    pub rate: f32,
}

impl RelationPull {
    #[must_use]
    pub const fn new(target: f32, rate: f32) -> Self {
        Self { target, rate }
    }

    #[must_use]
    pub fn validate(self) -> bool {
        self.target.is_finite()
            && self.rate.is_finite()
            && (-1.0..=1.0).contains(&self.target)
            && (0.0..=1.0).contains(&self.rate)
    }

    #[must_use]
    pub fn apply(self, current: f32) -> f32 {
        blend_bounded(current, self.target, self.rate, -1.0, 1.0)
    }
}

/// The relation dimensions one piece of evidence moves.
///
/// `familiarity` and `comfort` are absent by design: they belong to the turn's
/// structural evolution. The three that are here have exactly one writer — the
/// evidence channel — which is what keeps a turn-end writeback from erasing
/// what arrived while the turn was running.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelationNudge {
    pub tension: Option<RelationPull>,
    pub affinity: Option<RelationPull>,
    pub trust: Option<RelationPull>,
}

impl RelationNudge {
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.tension.is_none() && self.affinity.is_none() && self.trust.is_none()
    }

    #[must_use]
    pub fn validate(self) -> bool {
        [self.tension, self.affinity, self.trust]
            .into_iter()
            .flatten()
            .all(RelationPull::validate)
    }
}

/// How far a piece of evidence moves tension, per unit of normalized strength.
///
/// Anchors (kept where they always were, see
/// `docs/memory-driven-silence-2026-09-14.md`): one high-confidence hostility
/// is `0.15`, so it takes about 31 of them to cross the 0.6 silence threshold;
/// one warm message cools by `0.05`.
const HOSTILE_TENSION_TARGET: f32 = 1.0;
const HOSTILE_TENSION_RATE: f32 = 0.2;
const WARM_TENSION_TARGET: f32 = 0.0;
const WARM_TENSION_RATE: f32 = 0.12;

/// How fast sustained evidence moves the two durable warmth dimensions.
///
/// A warm message at full strength moves affinity by 6% of the remaining
/// distance and trust by 3%; sustained hostility moves affinity by 4% and trust
/// by 3% toward the cold end. At one warm message every other day that puts
/// "clearly likes her" (0.5) about 23 messages away, while a single bad night
/// costs a well-liked person roughly 0.04 of affinity — real, durable, and
/// nothing like a verdict. Affinity's 365-day and trust's 730-day half-lives
/// unwind both directions without a second mechanism.
const WARM_AFFINITY_RATE: f32 = 0.06;
const WARM_TRUST_RATE: f32 = 0.03;
const HOSTILE_AFFINITY_RATE: f32 = 0.04;
const HOSTILE_TRUST_RATE: f32 = 0.03;

/// Map one signed piece of relationship evidence into the relation dimensions
/// it moves.
///
/// **Sign convention (kept from the host's `tension_delta`): positive is
/// hostility, negative is warmth.** One number must not mean opposite things
/// on either side of the boundary.
///
/// The judgement of *what a message was* belongs to the host (today: one
/// bounded model call). The anchors, targets and rates that turn it into a
/// relationship change belong here, so a literal hit and a model-classified
/// hit move the relationship along the same scale.
#[must_use]
pub fn relation_evidence_nudge(signed_hostility: f32) -> RelationNudge {
    if !signed_hostility.is_finite() || signed_hostility == 0.0 {
        return RelationNudge::default();
    }
    let strength = signed_hostility.clamp(-1.0, 1.0);
    if strength > 0.0 {
        // 不友好：张力上升，好感与信任被压低。
        RelationNudge {
            tension: Some(RelationPull::new(
                HOSTILE_TENSION_TARGET,
                HOSTILE_TENSION_RATE * strength,
            )),
            affinity: Some(RelationPull::new(-1.0, HOSTILE_AFFINITY_RATE * strength)),
            trust: Some(RelationPull::new(-1.0, HOSTILE_TRUST_RATE * strength)),
        }
    } else {
        // 友好：张力回落，好感与信任被抬高。
        let magnitude = -strength;
        RelationNudge {
            tension: Some(RelationPull::new(
                WARM_TENSION_TARGET,
                WARM_TENSION_RATE * magnitude,
            )),
            affinity: Some(RelationPull::new(1.0, WARM_AFFINITY_RATE * magnitude)),
            trust: Some(RelationPull::new(1.0, WARM_TRUST_RATE * magnitude)),
        }
    }
}

/// Apply a [`RelationNudge`] to an in-memory relation state.
///
/// Hosts that keep relations in memory (the offline CLI harness, tests) use
/// this; the PostgreSQL adapter applies the same pulls inside one UPDATE
/// instead, so that overlapping writers compose.
#[must_use]
pub fn apply_relation_nudge(mut state: RelationState, nudge: RelationNudge) -> RelationState {
    if state.validate().is_err() || !nudge.validate() {
        return RelationState::new(state.person_id);
    }
    if let Some(pull) = nudge.tension {
        state.tension = pull.apply(state.tension);
    }
    if let Some(pull) = nudge.affinity {
        state.affinity = pull.apply(state.affinity);
    }
    if let Some(pull) = nudge.trust {
        state.trust = pull.apply(state.trust);
    }
    state
}

/// Adjust only the tension dimension of a relation by an explicit signed
/// strength. Kept as the narrow entry point for hosts that hold a literal
/// piece of evidence; it is exactly the tension half of
/// [`relation_evidence_nudge`], so both callers move tension along one scale.
#[must_use]
pub fn adjust_relation_tension(state: RelationState, signed_strength: f32) -> RelationState {
    let nudge = relation_evidence_nudge(signed_strength);
    apply_relation_nudge(
        state,
        RelationNudge {
            tension: nudge.tension,
            ..RelationNudge::default()
        },
    )
}

fn blend_bounded(current: f32, target: f32, rate: f32, minimum: f32, maximum: f32) -> f32 {
    (current + (target - current) * rate).clamp(minimum, maximum)
}

fn decay_toward(
    current: f32,
    target: f32,
    elapsed: Duration,
    half_life_seconds: f64,
    minimum: f32,
    maximum: f32,
) -> f32 {
    let retention = (-std::f64::consts::LN_2 * elapsed.as_secs_f64() / half_life_seconds).exp();
    (f64::from(target) + (f64::from(current) - f64::from(target)) * retention)
        .clamp(f64::from(minimum), f64::from(maximum)) as f32
}

/// The bounded working-state view exposed to a model.  Keeping the complete
/// mutable [`WorkingState`](crate::WorkingState) out of this type prevents a
/// planner from retaining a lock or mutating runtime state after a call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerStateSnapshot {
    pub global_version: u64,
    pub conversation: Option<ConversationSnapshot>,
}

impl PlannerStateSnapshot {
    #[must_use]
    pub const fn new(global_version: u64, conversation: Option<ConversationSnapshot>) -> Self {
        Self {
            global_version,
            conversation,
        }
    }

    #[must_use]
    pub const fn empty() -> Self {
        Self {
            global_version: 0,
            conversation: None,
        }
    }

    #[must_use]
    pub fn conversation_id(&self) -> Option<ConversationId> {
        self.conversation
            .as_ref()
            .map(ConversationSnapshot::conversation_id)
    }
}

impl Default for PlannerStateSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

/// All context needed for one planning turn.  The event is included in full,
/// while durable context is bounded by the stores and the caller's retrieval
/// policy before it reaches a model backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerInput {
    pub event: WorldEvent,
    pub state: PlannerStateSnapshot,
    pub memories: Vec<Memory>,
    pub open_loops: Vec<OpenLoop>,
    #[serde(default)]
    pub goals: Vec<Goal>,
    /// Membership is a Core identity projection, never a platform-specific
    /// nickname or external user id. It lets group-capable model backends
    /// reason about roles and stable speakers without coupling to QQ.
    #[serde(default)]
    pub participants: Vec<ConversationMember>,
    pub relation: Option<RelationState>,
    pub affect: AffectState,
    pub capabilities: Vec<ActionDescriptor>,
    /// Bounded, replayable Mind v2 context. Older payloads and V1 hosts
    /// deserialize to an empty snapshot and preserve their original behavior.
    #[serde(default)]
    pub mind: MindSnapshot,
    /// Bounded Executive context. This is additive so V1/V2 hosts can keep
    /// deserializing old planner inputs without manufacturing executive data.
    #[serde(default)]
    pub executive: ExecutiveSnapshot,
}

impl PlannerInput {
    #[must_use]
    pub fn new(event: WorldEvent, state: PlannerStateSnapshot) -> Self {
        Self {
            event,
            state,
            memories: Vec::new(),
            open_loops: Vec::new(),
            goals: Vec::new(),
            participants: Vec::new(),
            relation: None,
            affect: AffectState::default(),
            capabilities: Vec::new(),
            mind: MindSnapshot::empty(),
            executive: ExecutiveSnapshot::default(),
        }
    }

    #[must_use]
    pub fn with_memories(mut self, memories: Vec<Memory>) -> Self {
        self.memories = memories;
        self
    }

    #[must_use]
    pub fn with_open_loops(mut self, open_loops: Vec<OpenLoop>) -> Self {
        self.open_loops = open_loops;
        self
    }

    #[must_use]
    pub fn with_goals(mut self, goals: Vec<Goal>) -> Self {
        self.goals = goals;
        self
    }

    #[must_use]
    pub fn with_participants(mut self, participants: Vec<ConversationMember>) -> Self {
        self.participants = participants;
        self
    }

    #[must_use]
    pub fn with_relation(mut self, relation: Option<RelationState>) -> Self {
        self.relation = relation;
        self
    }

    #[must_use]
    pub fn with_affect(mut self, affect: AffectState) -> Self {
        self.affect = affect;
        self
    }

    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<ActionDescriptor>) -> Self {
        self.capabilities = capabilities;
        self
    }

    #[must_use]
    pub fn with_mind(mut self, mind: MindSnapshot) -> Self {
        self.mind = mind;
        self
    }

    #[must_use]
    pub fn with_executive(mut self, executive: ExecutiveSnapshot) -> Self {
        self.executive = executive;
        self
    }

    pub fn validate(&self, max_trace_depth: u8) -> Result<(), PlannerInputValidationError> {
        self.event
            .validate(max_trace_depth)
            .map_err(PlannerInputValidationError::InvalidEvent)?;
        if self.memories.len() > MAX_PLANNER_MEMORIES {
            return Err(PlannerInputValidationError::TooManyMemories {
                length: self.memories.len(),
                maximum: MAX_PLANNER_MEMORIES,
            });
        }
        if self.open_loops.len() > MAX_PLANNER_OPEN_LOOPS {
            return Err(PlannerInputValidationError::TooManyOpenLoops {
                length: self.open_loops.len(),
                maximum: MAX_PLANNER_OPEN_LOOPS,
            });
        }
        if self.goals.len() > MAX_PLANNER_GOALS {
            return Err(PlannerInputValidationError::TooManyGoals {
                length: self.goals.len(),
                maximum: MAX_PLANNER_GOALS,
            });
        }
        if self.participants.len() > MAX_PLANNER_PARTICIPANTS {
            return Err(PlannerInputValidationError::TooManyParticipants {
                length: self.participants.len(),
                maximum: MAX_PLANNER_PARTICIPANTS,
            });
        }
        if let Some(conversation_id) = self.state.conversation_id()
            && self
                .participants
                .iter()
                .any(|member| member.conversation_id() != conversation_id)
        {
            return Err(PlannerInputValidationError::ParticipantOutsideConversation);
        }
        self.affect.validate()?;
        if let Some(relation) = self.relation {
            relation.validate()?;
        }
        self.mind.validate()?;
        self.executive
            .validate()
            .map_err(|reason| PlannerInputValidationError::InvalidExecutive(reason.to_owned()))?;
        Ok(())
    }

    #[must_use]
    pub fn supports(&self, capability: ActionCapability) -> bool {
        let scope = self
            .state
            .conversation_id()
            .map(crate::ActionScope::Conversation)
            .unwrap_or(crate::ActionScope::Global);
        self.capabilities
            .iter()
            .any(|descriptor| descriptor.capability == capability && descriptor.allows(scope))
    }
}

/// The planner's disposition is intentionally less specific than an action.
/// A host only sees an action after an intent has passed the arbiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DecisionDisposition {
    Reply,
    #[default]
    Silent,
    Defer,
    ReactOnly,
    AskQuestion,
    ChangeTopic,
    ResumeAgenda,
    SpecialAction,
}

/// Model-selected lifecycle for an autonomous conversation turn.
///
/// Disposition describes the visible turn; this directive tells the host
/// whether the session should wake again without a new inbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationTurnDirective {
    Continue,
    Wait,
    End,
}

impl DecisionDisposition {
    /// Compatibility spelling for callers that use "respond" in their
    /// product language while the wire representation remains `reply`.
    #[allow(non_upper_case_globals)]
    pub const Respond: Self = Self::Reply;
    #[allow(non_upper_case_globals)]
    pub const Ignore: Self = Self::Silent;
}

/// A bounded, declarative update requested alongside a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum StateUpdateProposal {
    Affect(AffectState),
    Relation(RelationState),
    SetTopic {
        conversation_id: ConversationId,
        topic: String,
    },
    ConversationDirective {
        conversation_id: ConversationId,
        directive: ConversationTurnDirective,
    },
    ResolveOpenLoop {
        open_loop_id: OpenLoopId,
    },
    DeferOpenLoop {
        open_loop_id: OpenLoopId,
        due_at: Option<DateTime<Utc>>,
    },
}

impl StateUpdateProposal {
    pub fn validate(&self) -> Result<(), PlannerOutputValidationError> {
        match self {
            Self::Affect(affect) => affect
                .validate()
                .map_err(PlannerOutputValidationError::InvalidAffect),
            Self::Relation(relation) => relation
                .validate()
                .map_err(PlannerOutputValidationError::InvalidRelation),
            Self::SetTopic { topic, .. } => validate_topic(topic),
            Self::ConversationDirective { .. } => Ok(()),
            Self::ResolveOpenLoop { .. } | Self::DeferOpenLoop { .. } => Ok(()),
        }
    }
}

/// High-level planner output.  It contains no adapter call and no side
/// effect; conversion to [`ProposedAction`](crate::ProposedAction) is a later
/// boundary that remains under arbiter policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionPlan {
    pub disposition: DecisionDisposition,
    pub intents: Vec<CognitiveIntent>,
    pub state_updates: Vec<StateUpdateProposal>,
}

/// Alternate name used by integrations that speak in terms of input/output.
pub type PlannerOutput = DecisionPlan;

impl DecisionPlan {
    #[must_use]
    pub const fn new(disposition: DecisionDisposition) -> Self {
        Self {
            disposition,
            intents: Vec::new(),
            state_updates: Vec::new(),
        }
    }

    #[must_use]
    pub const fn silent() -> Self {
        Self {
            disposition: DecisionDisposition::Silent,
            intents: Vec::new(),
            state_updates: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), PlannerOutputValidationError> {
        if self.intents.len() > MAX_PLANNER_INTENTS {
            return Err(PlannerOutputValidationError::TooManyIntents {
                length: self.intents.len(),
                maximum: MAX_PLANNER_INTENTS,
            });
        }
        if self.state_updates.len() > MAX_PLANNER_STATE_UPDATES {
            return Err(PlannerOutputValidationError::TooManyStateUpdates {
                length: self.state_updates.len(),
                maximum: MAX_PLANNER_STATE_UPDATES,
            });
        }
        for intent in &self.intents {
            intent
                .validate()
                .map_err(PlannerOutputValidationError::InvalidIntent)?;
        }
        for update in &self.state_updates {
            update.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn with_intent(mut self, intent: CognitiveIntent) -> Self {
        self.intents.push(intent);
        self
    }

    #[must_use]
    pub fn with_state_update(mut self, update: StateUpdateProposal) -> Self {
        self.state_updates.push(update);
        self
    }
}

pub type ModelBackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PlannerOutput, ModelBackendError>> + Send + 'a>>;

/// Platform-neutral model boundary.  An implementation may use a local
/// model, a remote service, or a deterministic policy engine.  It receives
/// typed Core data and returns a declarative plan only.
pub trait ModelBackend: Send + Sync {
    /// Produces one declarative plan.  Backends that use a different
    /// vocabulary can implement [`Self::complete`] instead; the default
    /// methods keep both names available without coupling Core to a vendor.
    fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
        Box::pin(async { Err(ModelBackendError::Unavailable) })
    }

    fn complete<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
        self.plan(input)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelBackendError {
    #[error("model backend is unavailable")]
    Unavailable,
    #[error("model backend returned an invalid plan: {reason}")]
    InvalidPlan { reason: String },
    #[error("model backend failed: {message}")]
    Failed { message: String, retryable: bool },
}

impl ModelBackendError {
    #[must_use]
    pub fn failed(message: impl Into<String>, retryable: bool) -> Self {
        Self::Failed {
            message: message.into(),
            retryable,
        }
    }
}

/// Planner orchestration and output validation.  The planner owns no runtime
/// state, so the same instance can safely be shared by multiple runtimes.
#[derive(Clone)]
pub struct Planner {
    model: Arc<dyn ModelBackend>,
    max_trace_depth: u8,
}

impl std::fmt::Debug for Planner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Planner")
            .field("max_trace_depth", &self.max_trace_depth)
            .finish_non_exhaustive()
    }
}

impl Planner {
    #[must_use]
    pub fn new(model: Arc<dyn ModelBackend>) -> Self {
        Self {
            model,
            max_trace_depth: u8::MAX,
        }
    }

    #[must_use]
    pub fn from_model<M>(model: M) -> Self
    where
        M: ModelBackend + 'static,
    {
        Self::new(Arc::new(model))
    }

    #[must_use]
    pub fn with_max_trace_depth(mut self, max_trace_depth: u8) -> Self {
        self.max_trace_depth = max_trace_depth;
        self
    }

    #[must_use]
    pub fn model(&self) -> &Arc<dyn ModelBackend> {
        &self.model
    }

    pub async fn plan(&self, input: &PlannerInput) -> Result<PlannerOutput, PlannerError> {
        input
            .validate(self.max_trace_depth)
            .map_err(PlannerError::InvalidInput)?;
        let output = self
            .model
            .complete(input)
            .await
            .map_err(PlannerError::Model)?;
        output.validate().map_err(PlannerError::InvalidOutput)?;
        Ok(output)
    }
}

#[derive(Debug, Error)]
pub enum PlannerError {
    #[error("planner input is invalid: {0}")]
    InvalidInput(PlannerInputValidationError),
    #[error(transparent)]
    Model(#[from] ModelBackendError),
    #[error("planner output is invalid: {0}")]
    InvalidOutput(PlannerOutputValidationError),
    #[error(
        "planner state update `{kind}` failed after {applied_before_failure} earlier update(s) were applied: {message}"
    )]
    StateUpdate {
        kind: &'static str,
        message: String,
        /// State-update ports are independent and cannot share a transaction.
        /// This count makes a fail-fast partial commit explicit to callers.
        applied_before_failure: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlannerInputValidationError {
    #[error("world event is invalid: {0}")]
    InvalidEvent(crate::EventValidationError),
    #[error("planner received too many memories: {length}, maximum {maximum}")]
    TooManyMemories { length: usize, maximum: usize },
    #[error("planner received too many open loops: {length}, maximum {maximum}")]
    TooManyOpenLoops { length: usize, maximum: usize },
    #[error("planner received too many goals: {length}, maximum {maximum}")]
    TooManyGoals { length: usize, maximum: usize },
    #[error("planner received too many conversation participants: {length}, maximum {maximum}")]
    TooManyParticipants { length: usize, maximum: usize },
    #[error("planner received a participant outside the current conversation")]
    ParticipantOutsideConversation,
    #[error("affect contains a non-finite value")]
    NonFiniteAffect,
    #[error("affect value is outside its supported range")]
    AffectOutOfRange,
    #[error("relation contains a non-finite value")]
    NonFiniteRelation,
    #[error("relation value is outside its supported range")]
    RelationOutOfRange,
    #[error("mind snapshot is invalid: {0}")]
    InvalidMind(#[from] MindValidationError),
    #[error("executive snapshot is invalid: {0}")]
    InvalidExecutive(String),
}

#[derive(Debug, Error)]
pub enum PlannerOutputValidationError {
    #[error("planner returned too many intents: {length}, maximum {maximum}")]
    TooManyIntents { length: usize, maximum: usize },
    #[error(
        "planner requested too many tool actions for this causal trace: used {used}, requested {requested}, maximum {maximum}"
    )]
    ToolActionBudgetExceeded {
        used: usize,
        requested: usize,
        maximum: usize,
    },
    #[error("planner returned too many state updates: {length}, maximum {maximum}")]
    TooManyStateUpdates { length: usize, maximum: usize },
    #[error("planner returned an invalid intent: {0}")]
    InvalidIntent(#[from] IntentValidationError),
    #[error("planner returned an intent outside the current event scope: {reason}")]
    IntentOutsideEventScope { reason: String },
    #[error("planner returned an invalid affect update: {0}")]
    InvalidAffect(PlannerInputValidationError),
    #[error("planner returned an invalid relation update: {0}")]
    InvalidRelation(PlannerInputValidationError),
    #[error("planner returned an invalid topic: {0}")]
    InvalidTopic(String),
}

fn validate_topic(topic: &str) -> Result<(), PlannerOutputValidationError> {
    if topic.trim().is_empty() {
        return Err(PlannerOutputValidationError::InvalidTopic(
            "topic must not be empty".to_owned(),
        ));
    }
    if topic.as_bytes().contains(&0) {
        return Err(PlannerOutputValidationError::InvalidTopic(
            "topic must not contain NUL".to_owned(),
        ));
    }
    if topic.len() > MAX_PLANNER_TOPIC_BYTES {
        return Err(PlannerOutputValidationError::InvalidTopic(format!(
            "topic is {} bytes, maximum {}",
            topic.len(),
            MAX_PLANNER_TOPIC_BYTES
        )));
    }
    if topic.chars().count() > MAX_PLANNER_TOPIC_CHARS {
        return Err(PlannerOutputValidationError::InvalidTopic(format!(
            "topic is too long, maximum {} characters",
            MAX_PLANNER_TOPIC_CHARS
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventPriority, EventScope, WorldEventKind};
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn input() -> PlannerInput {
        PlannerInput::new(
            WorldEvent::new(
                Utc::now(),
                EventScope::Global,
                EventPriority::High,
                WorldEventKind::IdleTick,
            ),
            PlannerStateSnapshot::empty(),
        )
    }

    fn interaction_message(
        sender: PersonId,
        conversation_kind: ConversationKind,
        text: &str,
    ) -> MessageReceivedEvent {
        MessageReceivedEvent {
            message_id: crate::MessageId::new(),
            conversation_id: ConversationId::new(),
            sender,
            content: crate::MessageContent::text(text),
            reply_to: None,
            timestamp: Utc::now(),
            conversation_kind,
            addressed_to_agent: false,
            replies_to_agent: false,
            continuation_to_agent: false,
            stop_requested: false,
            explicit_request: false,
            visible_reply_allowed: true,
        }
    }

    struct FakeModel {
        calls: AtomicUsize,
        output: DecisionPlan,
    }

    impl ModelBackend for FakeModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let output = self.output.clone();
            Box::pin(async move { Ok(output) })
        }
    }

    #[tokio::test]
    async fn planner_calls_fake_model_and_validates_declarative_output() {
        let model = Arc::new(FakeModel {
            calls: AtomicUsize::new(0),
            output: DecisionPlan {
                disposition: DecisionDisposition::Silent,
                intents: vec![CognitiveIntent::noop()],
                state_updates: Vec::new(),
            },
        });
        let planner = Planner::new(model.clone());
        let output = planner.plan(&input()).await.expect("valid plan");
        assert_eq!(output.disposition, DecisionDisposition::Silent);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn planner_rejects_invalid_model_intent_before_host_execution() {
        let model = Arc::new(FakeModel {
            calls: AtomicUsize::new(0),
            output: DecisionPlan {
                disposition: DecisionDisposition::Reply,
                intents: vec![CognitiveIntent::send_message(
                    crate::ConversationId::new(),
                    crate::MessageContent::text(""),
                )],
                state_updates: Vec::new(),
            },
        });
        let error = Planner::new(model)
            .plan(&input())
            .await
            .expect_err("invalid intent must be rejected");
        assert!(matches!(error, PlannerError::InvalidOutput(_)));
    }

    #[test]
    fn affect_and_relation_ranges_are_checked() {
        assert!(
            AffectState {
                valence: 2.0,
                ..AffectState::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            RelationState {
                person_id: PersonId::new(),
                tension: f32::NAN,
                ..RelationState::new(PersonId::new())
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn interaction_evolution_uses_normalized_engagement_signals() {
        let person_id = PersonId::new();
        let initial_affect = AffectState {
            valence: 0.4,
            arousal: 0.0,
            social_energy: 0.5,
            curiosity: 0.5,
        };
        let initial_relation = RelationState {
            person_id,
            familiarity: 0.2,
            affinity: 0.35,
            trust: 0.1,
            comfort: 0.0,
            tension: 0.2,
        };
        let mut engaged =
            interaction_message(person_id, ConversationKind::Direct, "Could you help?!!");
        engaged.addressed_to_agent = true;
        engaged.replies_to_agent = true;
        engaged.explicit_request = true;
        let ambient = interaction_message(person_id, ConversationKind::Group, "status update");

        let engaged = evolve_interaction_state(&engaged, Some(initial_relation), initial_affect);
        let ambient = evolve_interaction_state(&ambient, Some(initial_relation), initial_affect);

        assert!(engaged.relation.familiarity > ambient.relation.familiarity);
        assert!(engaged.relation.comfort > ambient.relation.comfort);
        assert!(engaged.affect.arousal > ambient.affect.arousal);
        assert!(engaged.affect.curiosity > ambient.affect.curiosity);
        assert!(engaged.affect.social_energy > ambient.affect.social_energy);
        // 结构演化只写 familiarity 与 comfort：tension / affinity / trust 由证据
        // delta 通道独占，回合收尾的整行回写不许碰它们。
        assert_eq!(engaged.relation.affinity, initial_relation.affinity);
        assert_eq!(engaged.relation.trust, initial_relation.trust);
        assert_eq!(engaged.relation.tension, initial_relation.tension);
        engaged.affect.validate().expect("affect stays bounded");
        engaged.relation.validate().expect("relation stays bounded");
    }

    #[test]
    fn interaction_cues_are_validated_at_the_cue_boundary() {
        assert_eq!(
            InteractionCues {
                sentiment_confidence: f32::NAN,
                ..InteractionCues::default()
            }
            .validate(),
            Err(InteractionCueValidationError::NonFinite {
                field: "sentiment_confidence"
            })
        );
        assert_eq!(
            InteractionCues {
                gratitude_strength: 1.1,
                ..InteractionCues::default()
            }
            .validate(),
            Err(InteractionCueValidationError::OutOfRange {
                field: "gratitude_strength"
            })
        );
        // 越界的 cues 整条拒绝，绝不截断成"近似有效"。
        assert!(
            apply_interaction_cues(
                PersonId::new(),
                None,
                AffectState::default(),
                InteractionCues {
                    sentiment_valence: -1.5,
                    ..InteractionCues::default()
                },
            )
            .is_err()
        );
    }

    #[test]
    fn sentiment_moves_affect_only_and_never_the_relation() {
        let person_id = PersonId::new();
        let relation = RelationState {
            person_id,
            familiarity: 0.2,
            affinity: 0.3,
            trust: 0.1,
            comfort: 0.0,
            tension: 0.5,
        };
        let affect = AffectState {
            valence: 0.0,
            arousal: 0.0,
            social_energy: 0.5,
            curiosity: 0.5,
        };
        let sad = apply_interaction_cues(
            person_id,
            Some(relation),
            affect,
            InteractionCues {
                sentiment_valence: -0.8,
                sentiment_arousal: 0.4,
                sentiment_confidence: 1.0,
                gratitude_strength: 0.0,
            },
        )
        .expect("bounded sentiment cues");
        let grateful = apply_interaction_cues(
            person_id,
            Some(relation),
            affect,
            InteractionCues {
                sentiment_valence: 0.7,
                sentiment_arousal: 0.3,
                sentiment_confidence: 0.9,
                gratitude_strength: 1.0,
            },
        )
        .expect("bounded gratitude cues");

        assert!(sad.affect.valence < affect.valence);
        assert!(grateful.affect.valence > affect.valence);
        assert!(grateful.affect.social_energy > affect.social_energy);
        // 关系一列都不动：这条通道看到的是"说话人此刻的情绪"，判不出"这句话是不是
        // 冲着她"，更不该在回合收尾用旧快照把它写回去。
        for evolved in [sad.relation, grateful.relation] {
            assert_eq!(evolved, relation, "语义 cues 不得改关系");
        }
        sad.affect.validate().expect("affect stays bounded");
        grateful
            .relation
            .validate()
            .expect("relation stays bounded");
    }

    #[test]
    fn only_the_delta_channel_moves_tension_affinity_and_trust() {
        let person_id = PersonId::new();
        let message = interaction_message(person_id, ConversationKind::Direct, "hello");
        let affect = AffectState::default();
        // 强烈敌意的语义 cues 连续三十条：关系一列都不许动——累积只发生在 delta
        // 通道里，这样回合收尾的整行回写不可能把它抹掉。
        let hostile = InteractionCues {
            sentiment_valence: -0.9,
            sentiment_arousal: 0.6,
            sentiment_confidence: 0.9,
            gratitude_strength: 0.0,
        };
        let mut relation = RelationState::new(person_id);
        for _ in 0..30 {
            relation = apply_interaction_cues(person_id, Some(relation), affect, hostile)
                .expect("bounded cues")
                .relation;
        }
        assert_eq!(
            relation,
            RelationState::new(person_id),
            "语义 cues 连强烈敌意也不得累积成关系变化"
        );
        // 结构演化同样不碰这三列。
        let structural = evolve_interaction_state(&message, Some(relation), affect);
        assert_eq!(structural.relation.tension, relation.tension);
        assert_eq!(structural.relation.affinity, relation.affinity);
        assert_eq!(structural.relation.trust, relation.trust);

        // delta 通道：单条远不到静默阈值，持续才累积，善意能拉回来，且始终有界。
        let once =
            apply_relation_nudge(RelationState::new(person_id), relation_evidence_nudge(0.15));
        assert!(
            once.tension < 0.3,
            "单条证据不该把关系推到静默边缘：{}",
            once.tension
        );
        let mut accumulated = RelationState::new(person_id);
        for _ in 0..30 {
            accumulated = apply_relation_nudge(accumulated, relation_evidence_nudge(0.15));
        }
        assert!(
            accumulated.tension > once.tension,
            "持续证据必须累积：单次 {} vs 三十次 {}",
            once.tension,
            accumulated.tension
        );
        accumulated.validate().expect("relation stays bounded");
        let cooled = apply_relation_nudge(accumulated, relation_evidence_nudge(-0.05));
        assert!(
            cooled.tension < accumulated.tension,
            "善意必须能降温：{} -> {}",
            accumulated.tension,
            cooled.tension
        );
        // 同一条证据同时移动好感与信任：持续善意把它推向 1，一条高置信敌意真的扣分。
        // 符号沿用 tension_delta 的约定：正 = 不友好，负 = 友好。
        let mut warmed = RelationState::new(person_id);
        for _ in 0..23 {
            warmed = apply_relation_nudge(warmed, relation_evidence_nudge(-0.5));
        }
        assert!(
            warmed.affinity > 0.5,
            "23 条中等强度的善意应当把好感推过 0.5：{}",
            warmed.affinity
        );
        assert!(warmed.trust > 0.0);
        let chilled = apply_relation_nudge(
            RelationState {
                affinity: 0.9,
                trust: 0.9,
                ..RelationState::new(person_id)
            },
            relation_evidence_nudge(1.0),
        );
        assert!(
            chilled.affinity > 0.5 && chilled.affinity < 0.9,
            "一条高置信敌意要真的扣好感，但不能一句话毁掉长期关系：{}",
            chilled.affinity
        );
        assert!(chilled.trust < 0.9);
        chilled.validate().expect("relation stays bounded");
        // 无效证据不产生任何更新。
        assert!(relation_evidence_nudge(f32::NAN).is_empty());
        assert!(relation_evidence_nudge(0.0).is_empty());
    }

    #[test]
    fn semantic_sidecar_updates_do_not_count_the_interaction_twice() {
        let person_id = PersonId::new();
        let relation = RelationState {
            person_id,
            familiarity: 0.4,
            affinity: 0.2,
            trust: 0.1,
            comfort: 0.3,
            tension: 0.5,
        };
        let affect = AffectState {
            valence: 0.0,
            arousal: 0.0,
            social_energy: 0.5,
            curiosity: 0.5,
        };
        let evolved = apply_interaction_cues(
            person_id,
            Some(relation),
            affect,
            InteractionCues {
                sentiment_valence: 0.8,
                sentiment_arousal: 0.4,
                sentiment_confidence: 0.9,
                gratitude_strength: 0.75,
            },
        )
        .expect("bounded semantic cues");

        // 语义通道只写情绪：关系那一份由结构演化（familiarity/comfort）与证据
        // delta 通道（tension/affinity/trust）分别负责，同一条互动不会被记两次。
        assert_eq!(evolved.relation, relation);
        assert!(evolved.affect.valence > affect.valence);
        assert!(evolved.affect.arousal > affect.arousal);
        assert!(evolved.affect.social_energy > affect.social_energy);
    }

    #[test]
    fn elapsed_drift_is_deterministic_bounded_and_moves_toward_baselines() {
        let person_id = PersonId::new();
        let affect = AffectState {
            valence: -0.8,
            arousal: 0.9,
            social_energy: 0.2,
            curiosity: 1.0,
        };
        let relation = RelationState {
            person_id,
            familiarity: 0.9,
            affinity: -0.8,
            trust: 0.7,
            comfort: -0.6,
            tension: 1.0,
        };

        assert_eq!(drift_affect_state(affect, Duration::ZERO), affect);
        assert_eq!(drift_relation_state(relation, Duration::ZERO), relation);

        let affect_elapsed = Duration::from_secs(24 * 60 * 60);
        let drifted_affect = drift_affect_state(affect, affect_elapsed);
        assert_eq!(drifted_affect, drift_affect_state(affect, affect_elapsed));
        assert!(drifted_affect.valence.abs() < affect.valence.abs());
        assert!(drifted_affect.arousal.abs() < affect.arousal.abs());
        assert!(drifted_affect.social_energy > affect.social_energy);
        assert!((drifted_affect.curiosity - 0.5).abs() < (affect.curiosity - 0.5).abs());
        drifted_affect.validate().expect("drifted affect is valid");

        let relation_elapsed = Duration::from_secs(365 * 24 * 60 * 60);
        let drifted_relation = drift_relation_state(relation, relation_elapsed);
        assert_eq!(drifted_relation.person_id, person_id);
        assert!(drifted_relation.familiarity.abs() < relation.familiarity.abs());
        assert!(drifted_relation.affinity.abs() < relation.affinity.abs());
        assert!(drifted_relation.trust.abs() < relation.trust.abs());
        assert!(drifted_relation.comfort.abs() < relation.comfort.abs());
        assert!(drifted_relation.tension.abs() < relation.tension.abs());
        drifted_relation
            .validate()
            .expect("drifted relation is valid");
    }

    #[test]
    fn interaction_evolution_never_reuses_another_persons_relation() {
        let sender = PersonId::new();
        let foreign_person = PersonId::new();
        let message = interaction_message(sender, ConversationKind::Direct, "hello");
        let foreign_relation = RelationState {
            person_id: foreign_person,
            familiarity: 0.8,
            affinity: 0.9,
            trust: 0.7,
            comfort: 0.6,
            tension: -0.5,
        };

        let evolved =
            evolve_interaction_state(&message, Some(foreign_relation), AffectState::default());

        assert_eq!(evolved.relation.person_id, sender);
        assert_eq!(evolved.relation.affinity, 0.0);
        assert_eq!(evolved.relation.trust, 0.0);
    }

    #[test]
    fn repeated_high_intensity_interactions_remain_bounded() {
        let person_id = PersonId::new();
        let mut message = interaction_message(
            person_id,
            ConversationKind::Direct,
            &format!("{}?!", "long message ".repeat(100)),
        );
        message.addressed_to_agent = true;
        message.replies_to_agent = true;
        message.stop_requested = true;
        message.explicit_request = true;
        let mut affect = AffectState {
            valence: -1.0,
            arousal: 1.0,
            social_energy: 0.0,
            curiosity: 1.0,
        };
        let mut relation = RelationState {
            person_id,
            familiarity: 1.0,
            affinity: -1.0,
            trust: 1.0,
            comfort: -1.0,
            tension: 1.0,
        };

        for _ in 0..10_000 {
            let evolved = evolve_interaction_state(&message, Some(relation), affect);
            affect = evolved.affect;
            relation = evolved.relation;
        }

        affect.validate().expect("affect remains valid");
        relation.validate().expect("relation remains valid");
        assert_eq!(relation.person_id, person_id);
    }

    #[test]
    fn planner_input_serialization_keeps_host_capabilities() {
        let input =
            input().with_capabilities(vec![ActionDescriptor::new(ActionCapability::SendMessage)]);
        let encoded = serde_json::to_value(&input).expect("planner input should serialize");
        assert_eq!(
            encoded["capabilities"][0]["capability"],
            serde_json::json!("send_message")
        );
        let decoded: PlannerInput = serde_json::from_value(encoded).expect("round trip");
        assert_eq!(decoded.capabilities, input.capabilities);
    }

    #[test]
    fn planner_input_carries_platform_neutral_group_membership() {
        let conversation_id = ConversationId::new();
        let member = ConversationMember::new(conversation_id, PersonId::new())
            .with_role(Some("moderator".to_owned()))
            .expect("valid role");
        let event = WorldEvent::message_received(
            EventPriority::Normal,
            MessageReceivedEvent {
                message_id: crate::MessageId::new(),
                conversation_id,
                sender: member.person_id(),
                content: crate::MessageContent::text("hello"),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind: ConversationKind::Group,
                addressed_to_agent: true,
                replies_to_agent: false,
                continuation_to_agent: false,
                stop_requested: false,
                explicit_request: false,
                visible_reply_allowed: true,
            },
        );
        let mut working_state =
            crate::WorkingState::new(crate::WorkingStateConfig::default()).expect("working state");
        let attention = crate::AttentionSystem;
        working_state
            .observe(&event, attention.evaluate(&event))
            .expect("event should be observed");
        let state = PlannerStateSnapshot::new(
            working_state.global_version(),
            working_state.conversation(conversation_id),
        );
        let input = PlannerInput::new(event, state).with_participants(vec![member.clone()]);
        let encoded = serde_json::to_value(&input).expect("participant context serializes");
        let decoded: PlannerInput = serde_json::from_value(encoded).expect("round trip");
        assert_eq!(decoded.participants, vec![member]);
        assert!(decoded.validate(8).is_ok());
    }

    #[test]
    fn planner_rejects_membership_from_another_conversation() {
        let conversation_id = ConversationId::new();
        let foreign = ConversationMember::new(ConversationId::new(), PersonId::new());
        let event = WorldEvent::message_received(
            EventPriority::Normal,
            MessageReceivedEvent {
                message_id: crate::MessageId::new(),
                conversation_id,
                sender: foreign.person_id(),
                content: crate::MessageContent::text("hello"),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind: ConversationKind::Group,
                addressed_to_agent: true,
                replies_to_agent: false,
                continuation_to_agent: false,
                stop_requested: false,
                explicit_request: false,
                visible_reply_allowed: true,
            },
        );
        let mut working_state =
            crate::WorkingState::new(crate::WorkingStateConfig::default()).expect("working state");
        let attention = crate::AttentionSystem;
        working_state
            .observe(&event, attention.evaluate(&event))
            .expect("event should be observed");
        let state = PlannerStateSnapshot::new(
            working_state.global_version(),
            working_state.conversation(conversation_id),
        );
        assert_eq!(
            PlannerInput::new(event, state)
                .with_participants(vec![foreign])
                .validate(8),
            Err(PlannerInputValidationError::ParticipantOutsideConversation)
        );
    }

    #[test]
    fn planner_input_goal_context_is_bounded() {
        let draft = crate::GoalDraft::new(
            crate::GoalOwner::Global,
            crate::GoalKind::Project,
            "bounded planner goal",
        )
        .expect("valid goal draft");
        let goals = (0..=MAX_PLANNER_GOALS)
            .map(|_| {
                crate::Goal::from_draft(crate::GoalId::new(), &draft, Utc::now())
                    .expect("valid goal")
            })
            .collect();

        assert_eq!(
            input().with_goals(goals).validate(8),
            Err(PlannerInputValidationError::TooManyGoals {
                length: MAX_PLANNER_GOALS + 1,
                maximum: MAX_PLANNER_GOALS,
            })
        );
    }

    #[test]
    fn planner_input_goal_context_round_trips_and_defaults_for_older_payloads() {
        let draft = crate::GoalDraft::new(
            crate::GoalOwner::Global,
            crate::GoalKind::Project,
            "serialize planner goal",
        )
        .expect("valid goal draft");
        let goal =
            crate::Goal::from_draft(crate::GoalId::new(), &draft, Utc::now()).expect("valid goal");
        let input = input().with_goals(vec![goal.clone()]);
        let mut encoded = serde_json::to_value(&input).expect("planner input should serialize");

        let decoded: PlannerInput =
            serde_json::from_value(encoded.clone()).expect("goal context should round trip");
        assert_eq!(decoded.goals, vec![goal]);

        encoded
            .as_object_mut()
            .expect("planner input serializes as an object")
            .remove("goals");
        encoded
            .as_object_mut()
            .expect("planner input serializes as an object")
            .remove("mind");
        let legacy: PlannerInput =
            serde_json::from_value(encoded).expect("older planner input should remain readable");
        assert!(legacy.goals.is_empty());
        assert!(legacy.mind.is_empty());
        assert_eq!(
            legacy.mind.influence_mode(),
            crate::MindInfluenceMode::Disabled
        );
    }
}
