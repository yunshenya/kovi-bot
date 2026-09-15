//! Platform-neutral proactive motives and reach-out intent.
//!
//! This module decides whether Yunxi has a grounded reason to reach out. It
//! deliberately knows nothing about delivery channels, platform identifiers,
//! rate-limit storage, or concrete side effects.

use crate::{MessageContent, OpenLoopId, PersonId};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

pub const MAX_PROACTIVE_CANDIDATES: usize = 32;
pub const MAX_REACH_OUT_MESSAGE_BYTES: usize = crate::event::MAX_MESSAGE_CONTENT_BYTES;
pub const MAX_REACH_OUT_MESSAGE_CHARS: usize = crate::event::MAX_MESSAGE_CONTENT_CHARS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProactiveMotive {
    FollowUp,
    CheckIn,
    Share,
    React,
    Curiosity,
}

impl ProactiveMotive {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FollowUp => "follow_up",
            Self::CheckIn => "check_in",
            Self::Share => "share",
            Self::React => "react",
            Self::Curiosity => "curiosity",
        }
    }
}

impl fmt::Display for ProactiveMotive {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProactiveMotive {
    type Err = ProactiveValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "follow_up" => Ok(Self::FollowUp),
            "check_in" => Ok(Self::CheckIn),
            "share" => Ok(Self::Share),
            "react" => Ok(Self::React),
            "curiosity" => Ok(Self::Curiosity),
            _ => Err(ProactiveValidationError::UnknownMotive {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProactiveContext {
    social_energy: u8,
    system_load: u8,
    minimum_salience: u8,
    global_cooldown_active: bool,
    global_daily_limit_reached: bool,
}

impl ProactiveContext {
    pub fn new(
        social_energy: u8,
        system_load: u8,
        minimum_salience: u8,
    ) -> Result<Self, ProactiveValidationError> {
        validate_score("social_energy", social_energy)?;
        validate_score("system_load", system_load)?;
        validate_score("minimum_salience", minimum_salience)?;
        Ok(Self {
            social_energy,
            system_load,
            minimum_salience,
            global_cooldown_active: false,
            global_daily_limit_reached: false,
        })
    }

    #[must_use]
    pub const fn with_global_suppression(
        mut self,
        cooldown_active: bool,
        daily_limit_reached: bool,
    ) -> Self {
        self.global_cooldown_active = cooldown_active;
        self.global_daily_limit_reached = daily_limit_reached;
        self
    }

    #[must_use]
    pub const fn social_energy(self) -> u8 {
        self.social_energy
    }

    #[must_use]
    pub const fn system_load(self) -> u8 {
        self.system_load
    }

    #[must_use]
    pub const fn minimum_salience(self) -> u8 {
        self.minimum_salience
    }
}

impl<'de> Deserialize<'de> for ProactiveContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            social_energy: u8,
            system_load: u8,
            minimum_salience: u8,
            global_cooldown_active: bool,
            global_daily_limit_reached: bool,
        }
        let wire = Wire::deserialize(deserializer)?;
        ProactiveContext::new(wire.social_energy, wire.system_load, wire.minimum_salience)
            .map(|context| {
                context.with_global_suppression(
                    wire.global_cooldown_active,
                    wire.global_daily_limit_reached,
                )
            })
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProspectiveSignal {
    open_loop_id: OpenLoopId,
    salience: u8,
    due: bool,
}

impl ProspectiveSignal {
    pub fn new(
        open_loop_id: OpenLoopId,
        salience: u8,
        due: bool,
    ) -> Result<Self, ProactiveValidationError> {
        validate_score("open_loop_salience", salience)?;
        Ok(Self {
            open_loop_id,
            salience,
            due,
        })
    }

    #[must_use]
    pub const fn open_loop_id(self) -> OpenLoopId {
        self.open_loop_id
    }

    #[must_use]
    pub const fn salience(self) -> u8 {
        self.salience
    }

    #[must_use]
    pub const fn due(self) -> bool {
        self.due
    }
}

impl<'de> Deserialize<'de> for ProspectiveSignal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            open_loop_id: OpenLoopId,
            salience: u8,
            due: bool,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.open_loop_id, wire.salience, wire.due).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProactiveCandidate {
    person_id: PersonId,
    familiarity: u8,
    curiosity: u8,
    memory_salience: u8,
    recent_event_salience: u8,
    wellbeing_salience: u8,
    idle_hours: u16,
    prospective: Option<ProspectiveSignal>,
    delivery_available: bool,
    cooldown_active: bool,
    daily_limit_reached: bool,
}

impl ProactiveCandidate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        person_id: PersonId,
        familiarity: u8,
        curiosity: u8,
        memory_salience: u8,
        recent_event_salience: u8,
        idle_hours: u16,
    ) -> Result<Self, ProactiveValidationError> {
        for (field, value) in [
            ("familiarity", familiarity),
            ("curiosity", curiosity),
            ("memory_salience", memory_salience),
            ("recent_event_salience", recent_event_salience),
        ] {
            validate_score(field, value)?;
        }
        Ok(Self {
            person_id,
            familiarity,
            curiosity,
            memory_salience,
            recent_event_salience,
            wellbeing_salience: 0,
            idle_hours,
            prospective: None,
            delivery_available: true,
            cooldown_active: false,
            daily_limit_reached: false,
        })
    }

    #[must_use]
    pub const fn with_prospective_signal(mut self, signal: Option<ProspectiveSignal>) -> Self {
        self.prospective = signal;
        self
    }

    pub fn with_wellbeing_salience(
        mut self,
        salience: u8,
    ) -> Result<Self, ProactiveValidationError> {
        validate_score("wellbeing_salience", salience)?;
        self.wellbeing_salience = salience;
        Ok(self)
    }

    #[must_use]
    pub const fn with_availability(mut self, delivery_available: bool) -> Self {
        self.delivery_available = delivery_available;
        self
    }

    #[must_use]
    pub const fn with_suppression(
        mut self,
        cooldown_active: bool,
        daily_limit_reached: bool,
    ) -> Self {
        self.cooldown_active = cooldown_active;
        self.daily_limit_reached = daily_limit_reached;
        self
    }

    #[must_use]
    pub const fn person_id(self) -> PersonId {
        self.person_id
    }

    #[must_use]
    pub const fn familiarity(self) -> u8 {
        self.familiarity
    }

    #[must_use]
    pub const fn curiosity(self) -> u8 {
        self.curiosity
    }

    #[must_use]
    pub const fn memory_salience(self) -> u8 {
        self.memory_salience
    }

    #[must_use]
    pub const fn recent_event_salience(self) -> u8 {
        self.recent_event_salience
    }

    #[must_use]
    pub const fn wellbeing_salience(self) -> u8 {
        self.wellbeing_salience
    }

    #[must_use]
    pub const fn idle_hours(self) -> u16 {
        self.idle_hours
    }
}

impl<'de> Deserialize<'de> for ProactiveCandidate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            person_id: PersonId,
            familiarity: u8,
            curiosity: u8,
            memory_salience: u8,
            recent_event_salience: u8,
            wellbeing_salience: u8,
            idle_hours: u16,
            prospective: Option<ProspectiveSignal>,
            delivery_available: bool,
            cooldown_active: bool,
            daily_limit_reached: bool,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(
            wire.person_id,
            wire.familiarity,
            wire.curiosity,
            wire.memory_salience,
            wire.recent_event_salience,
            wire.idle_hours,
        )
        .and_then(|candidate| candidate.with_wellbeing_salience(wire.wellbeing_salience))
        .map(|candidate| {
            candidate
                .with_prospective_signal(wire.prospective)
                .with_availability(wire.delivery_available)
                .with_suppression(wire.cooldown_active, wire.daily_limit_reached)
        })
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProactiveOpportunity {
    person_id: PersonId,
    motive: ProactiveMotive,
    salience: u8,
    source_open_loop_id: Option<OpenLoopId>,
}

impl ProactiveOpportunity {
    pub fn new(
        person_id: PersonId,
        motive: ProactiveMotive,
        salience: u8,
        source_open_loop_id: Option<OpenLoopId>,
    ) -> Result<Self, ProactiveValidationError> {
        validate_score("salience", salience)?;
        Ok(Self {
            person_id,
            motive,
            salience,
            source_open_loop_id,
        })
    }

    #[must_use]
    pub const fn person_id(self) -> PersonId {
        self.person_id
    }

    #[must_use]
    pub const fn motive(self) -> ProactiveMotive {
        self.motive
    }

    #[must_use]
    pub const fn salience(self) -> u8 {
        self.salience
    }

    #[must_use]
    pub const fn source_open_loop_id(self) -> Option<OpenLoopId> {
        self.source_open_loop_id
    }
}

impl<'de> Deserialize<'de> for ProactiveOpportunity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            person_id: PersonId,
            motive: ProactiveMotive,
            salience: u8,
            source_open_loop_id: Option<OpenLoopId>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(
            wire.person_id,
            wire.motive,
            wire.salience,
            wire.source_open_loop_id,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProactiveSilenceReason {
    GlobalCooldown,
    GlobalDailyLimit,
    LowSocialEnergy,
    HighSystemLoad,
    NoEligibleCandidate,
    BelowSalienceThreshold,
    NoGroundedSignal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProactiveDecision {
    ReachOut(ProactiveOpportunity),
    Silent(ProactiveSilenceReason),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProactiveSystem;

impl ProactiveSystem {
    pub fn decide(
        &self,
        context: ProactiveContext,
        candidates: &[ProactiveCandidate],
    ) -> Result<ProactiveDecision, ProactiveValidationError> {
        if candidates.len() > MAX_PROACTIVE_CANDIDATES {
            return Err(ProactiveValidationError::TooManyCandidates {
                length: candidates.len(),
                maximum: MAX_PROACTIVE_CANDIDATES,
            });
        }
        let mut people = HashSet::with_capacity(candidates.len());
        for candidate in candidates {
            if !people.insert(candidate.person_id) {
                return Err(ProactiveValidationError::DuplicateCandidate {
                    person_id: candidate.person_id,
                });
            }
        }
        if context.global_cooldown_active {
            return Ok(ProactiveDecision::Silent(
                ProactiveSilenceReason::GlobalCooldown,
            ));
        }
        if context.global_daily_limit_reached {
            return Ok(ProactiveDecision::Silent(
                ProactiveSilenceReason::GlobalDailyLimit,
            ));
        }
        if context.social_energy < 30 {
            return Ok(ProactiveDecision::Silent(
                ProactiveSilenceReason::LowSocialEnergy,
            ));
        }
        if context.system_load >= 90 {
            return Ok(ProactiveDecision::Silent(
                ProactiveSilenceReason::HighSystemLoad,
            ));
        }

        let mut best: Option<ProactiveOpportunity> = None;
        let mut eligible_count = 0;
        for candidate in candidates.iter().filter(|candidate| {
            candidate.delivery_available
                && !candidate.cooldown_active
                && !candidate.daily_limit_reached
        }) {
            eligible_count += 1;
            let Some(opportunity) = score_candidate(context, *candidate) else {
                continue;
            };
            if best.is_none_or(|current| {
                opportunity.salience > current.salience
                    || (opportunity.salience == current.salience
                        && opportunity.person_id < current.person_id)
            }) {
                best = Some(opportunity);
            }
        }
        let Some(best) = best else {
            return Ok(ProactiveDecision::Silent(if eligible_count == 0 {
                ProactiveSilenceReason::NoEligibleCandidate
            } else {
                ProactiveSilenceReason::NoGroundedSignal
            }));
        };
        if best.salience < context.minimum_salience {
            return Ok(ProactiveDecision::Silent(
                ProactiveSilenceReason::BelowSalienceThreshold,
            ));
        }
        Ok(ProactiveDecision::ReachOut(best))
    }
}

fn score_candidate(
    context: ProactiveContext,
    candidate: ProactiveCandidate,
) -> Option<ProactiveOpportunity> {
    let (motive, signal_score, source_open_loop_id) = if let Some(prospective) =
        candidate.prospective
        && (prospective.due || prospective.salience >= 80)
    {
        (
            ProactiveMotive::FollowUp,
            35_u16 + u16::from(prospective.salience) / 10,
            Some(prospective.open_loop_id),
        )
    } else if candidate.wellbeing_salience >= 60 {
        (
            ProactiveMotive::CheckIn,
            u16::from(candidate.wellbeing_salience) / 3,
            None,
        )
    } else if candidate.recent_event_salience >= 75 {
        (
            ProactiveMotive::React,
            u16::from(candidate.recent_event_salience) / 4,
            None,
        )
    } else if candidate.memory_salience >= 50 {
        (
            ProactiveMotive::Share,
            u16::from(candidate.memory_salience) / 4,
            None,
        )
    } else if candidate.curiosity >= 75
        && (candidate.memory_salience > 0 || candidate.recent_event_salience > 0)
    {
        (
            ProactiveMotive::Curiosity,
            (u16::from(candidate.curiosity) + u16::from(candidate.memory_salience)) / 10,
            None,
        )
    } else {
        return None;
    };

    let grounding_score = 20_u16;
    let idle_score = candidate.idle_hours.min(72) * 20 / 72;
    let raw = u16::from(candidate.familiarity) / 4
        + u16::from(context.social_energy) / 10
        + idle_score
        + signal_score
        + grounding_score;
    let load_penalty = u16::from(context.system_load) / 5;
    let salience = raw.saturating_sub(load_penalty).min(100) as u8;
    Some(ProactiveOpportunity {
        person_id: candidate.person_id,
        motive,
        salience,
        source_open_loop_id,
    })
}

/// 她主动接触一个人时用的媒介。
///
/// 为什么媒介要显式写进意图、而不是让宿主自己看着办：发消息和打电话的**打扰级别差
/// 一个量级**——消息可以不看，电话必须接或拒。宿主需要能独立声明"我能打"，Core 需要
/// 能独立决定"这次用哪种"，两边得以同一个字段为判据，否则"宿主能打"就会变成
/// "凡是主动接触都打电话"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachOutMedium {
    /// 发一条消息。默认值，也是所有既有调用点的语义。
    #[default]
    Message,
    /// 拨一通语音电话。此时 `ReachOutIntent.message` 是她**开口要说的话**。
    Call,
}

impl ReachOutMedium {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Call => "call",
        }
    }

    /// 这种媒介会不会让对方的设备**响起来**。
    #[must_use]
    pub const fn rings_peer(self) -> bool {
        matches!(self, Self::Call)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReachOutIntent {
    person_id: PersonId,
    message: MessageContent,
    motive: ProactiveMotive,
    source_open_loop_id: Option<OpenLoopId>,
    /// 用哪种媒介接触他。默认发消息；`Call` 时 `message` 是开场白。
    #[serde(default)]
    medium: ReachOutMedium,
}

impl ReachOutIntent {
    pub fn new(
        opportunity: ProactiveOpportunity,
        message: MessageContent,
    ) -> Result<Self, ProactiveValidationError> {
        Self::from_parts(opportunity.person_id, message, opportunity.motive).map(|mut intent| {
            intent.source_open_loop_id = opportunity.source_open_loop_id;
            intent
        })
    }

    pub fn from_parts(
        person_id: PersonId,
        message: MessageContent,
        motive: ProactiveMotive,
    ) -> Result<Self, ProactiveValidationError> {
        Self::from_parts_with_medium(person_id, message, motive, ReachOutMedium::Message)
    }

    /// 带媒介的构造。`Call` 时 `message` 必须只含能说出来的东西。
    pub fn from_parts_with_medium(
        person_id: PersonId,
        message: MessageContent,
        motive: ProactiveMotive,
        medium: ReachOutMedium,
    ) -> Result<Self, ProactiveValidationError> {
        let intent = Self {
            person_id,
            message,
            motive,
            source_open_loop_id: None,
            medium,
        };
        intent.validate()?;
        Ok(intent)
    }

    pub fn from_opportunity(
        opportunity: ProactiveOpportunity,
        message: MessageContent,
    ) -> Result<Self, ProactiveValidationError> {
        Self::new(opportunity, message)
    }

    /// 用指定媒介从机会构造（`Call` 时 `message` 是开场白）。
    pub fn from_opportunity_with_medium(
        opportunity: ProactiveOpportunity,
        message: MessageContent,
        medium: ReachOutMedium,
    ) -> Result<Self, ProactiveValidationError> {
        Self::from_parts_with_medium(opportunity.person_id, message, opportunity.motive, medium)
            .map(|mut intent| {
                intent.source_open_loop_id = opportunity.source_open_loop_id;
                intent
            })
    }

    #[must_use]
    pub const fn person_id(&self) -> PersonId {
        self.person_id
    }

    #[must_use]
    pub const fn message(&self) -> &MessageContent {
        &self.message
    }

    #[must_use]
    pub const fn motive(&self) -> ProactiveMotive {
        self.motive
    }

    #[must_use]
    pub const fn source_open_loop_id(&self) -> Option<OpenLoopId> {
        self.source_open_loop_id
    }

    #[must_use]
    pub const fn medium(&self) -> ReachOutMedium {
        self.medium
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        PersonId,
        MessageContent,
        ProactiveMotive,
        Option<OpenLoopId>,
        ReachOutMedium,
    ) {
        (
            self.person_id,
            self.message,
            self.motive,
            self.source_open_loop_id,
            self.medium,
        )
    }

    pub fn validate(&self) -> Result<(), ProactiveValidationError> {
        validate_message(&self.message)?;
        validate_reach_out_medium(self.medium, &self.message)
    }
}

impl<'de> Deserialize<'de> for ReachOutIntent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct StrictMessage {
            text: String,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            person_id: PersonId,
            message: StrictMessage,
            motive: ProactiveMotive,
            source_open_loop_id: Option<OpenLoopId>,
            /// 老载荷里没有这个字段；缺省即"发消息"，所以是向后兼容的。
            #[serde(default)]
            medium: ReachOutMedium,
        }
        let wire = Wire::deserialize(deserializer)?;
        let opportunity =
            ProactiveOpportunity::new(wire.person_id, wire.motive, 0, wire.source_open_loop_id)
                .map_err(serde::de::Error::custom)?;
        Self::from_opportunity_with_medium(
            opportunity,
            MessageContent::text(wire.message.text),
            wire.medium,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProactiveValidationError {
    #[error("unknown proactive motive `{value}`")]
    UnknownMotive { value: String },
    #[error("proactive score `{field}` is {value}, above maximum 100")]
    ScoreOutOfRange { field: &'static str, value: u8 },
    #[error("proactive candidate count {length} is above maximum {maximum}")]
    TooManyCandidates { length: usize, maximum: usize },
    #[error("proactive candidate for {person_id} is duplicated")]
    DuplicateCandidate { person_id: PersonId },
    #[error("reach-out message must not be empty")]
    EmptyMessage,
    #[error("reach-out message must not contain NUL")]
    MessageContainsNul,
    #[error("reach-out message is {length} bytes, above maximum {maximum}")]
    MessageTooLong { length: usize, maximum: usize },
    #[error("reach-out message is {length} characters, above maximum {maximum}")]
    MessageTooManyCharacters { length: usize, maximum: usize },
    #[error("a reach-out call can only carry speech, not attachments, stickers or singing")]
    CallRequiresSpeech,
}

/// 电话只能承载"说得出来"的内容。
///
/// 抽成共享函数是因为它有两道门要守：`ReachOutIntent::validate`（Core 自己构造时）
/// 和 `ReachOutAction::validate`（动作反序列化时——宿主收到的就是动作）。各写一份的话，
/// 从动作那头进来的非语音内容会绕过检查，"打电话顺便发表情"就变成一通沉默的电话。
pub fn validate_reach_out_medium(
    medium: ReachOutMedium,
    message: &MessageContent,
) -> Result<(), ProactiveValidationError> {
    if medium == ReachOutMedium::Call
        && (!message.attachments().is_empty()
            || message.sticker_label().is_some()
            || message.is_sing())
    {
        return Err(ProactiveValidationError::CallRequiresSpeech);
    }
    Ok(())
}

fn validate_score(field: &'static str, value: u8) -> Result<(), ProactiveValidationError> {
    if value > 100 {
        return Err(ProactiveValidationError::ScoreOutOfRange { field, value });
    }
    Ok(())
}

fn validate_message(message: &MessageContent) -> Result<(), ProactiveValidationError> {
    let text = message.as_text();
    if text.trim().is_empty() {
        return Err(ProactiveValidationError::EmptyMessage);
    }
    if text.contains('\0') {
        return Err(ProactiveValidationError::MessageContainsNul);
    }
    if text.len() > MAX_REACH_OUT_MESSAGE_BYTES {
        return Err(ProactiveValidationError::MessageTooLong {
            length: text.len(),
            maximum: MAX_REACH_OUT_MESSAGE_BYTES,
        });
    }
    let chars = text.chars().count();
    if chars > MAX_REACH_OUT_MESSAGE_CHARS {
        return Err(ProactiveValidationError::MessageTooManyCharacters {
            length: chars,
            maximum: MAX_REACH_OUT_MESSAGE_CHARS,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ProactiveContext {
        ProactiveContext::new(70, 10, 55).expect("valid context")
    }

    fn candidate(person_id: PersonId) -> ProactiveCandidate {
        ProactiveCandidate::new(person_id, 70, 60, 60, 30, 24).expect("valid candidate")
    }

    /// 媒介必须活过一次序列化往返——它会随意图与动作落盘/过进程。
    #[test]
    fn the_reach_out_medium_survives_a_round_trip() {
        let person = PersonId::new();
        for medium in [ReachOutMedium::Message, ReachOutMedium::Call] {
            let intent = ReachOutIntent::from_parts_with_medium(
                person,
                MessageContent::text("喂，是我"),
                ProactiveMotive::CheckIn,
                medium,
            )
            .expect("valid intent");
            assert_eq!(intent.medium(), medium);
            let encoded = serde_json::to_string(&intent).expect("serialize intent");
            let decoded: ReachOutIntent =
                serde_json::from_str(&encoded).expect("deserialize intent");
            assert_eq!(decoded.medium(), medium, "媒介在往返里丢了：{encoded}");
            assert_eq!(decoded, intent);
        }
        assert_eq!(ReachOutMedium::Message.as_str(), "message");
        assert_eq!(ReachOutMedium::Call.as_str(), "call");
        assert!(ReachOutMedium::Call.rings_peer());
        assert!(!ReachOutMedium::Message.rings_peer());
        assert_eq!(ReachOutMedium::default(), ReachOutMedium::Message);
    }

    /// 老载荷里没有 `medium` 字段——必须当成"发消息"，不能反序列化失败。
    ///
    /// 这条不是洁癖：意图与动作会持久化，部署新版本时存量载荷就是没有这个字段的。
    /// 判错成"默认打电话"会让升级瞬间变成一批未预期的外呼。
    #[test]
    fn a_payload_without_a_medium_is_a_message() {
        let person = PersonId::new();
        let legacy = format!(
            r#"{{"person_id":"{person}","message":{{"text":"在吗"}},"motive":"check_in","source_open_loop_id":null}}"#
        );
        let decoded: ReachOutIntent = serde_json::from_str(&legacy).expect("老载荷该能读");
        assert_eq!(
            decoded.medium(),
            ReachOutMedium::Message,
            "缺字段必须是发消息，不能默认成打电话"
        );
    }

    /// 电话只能承载说得出来的内容。
    ///
    /// 不拦的话，"打电话顺便把图发过去"会变成一通什么也没说的电话。
    #[test]
    fn a_call_can_only_carry_speech() {
        let person = PersonId::new();
        let text_only = ReachOutIntent::from_parts_with_medium(
            person,
            MessageContent::text("喂，是我，刚想起件事"),
            ProactiveMotive::Share,
            ReachOutMedium::Call,
        );
        assert!(text_only.is_ok(), "纯文本应该可以打电话");

        for unspeakable in [
            MessageContent::sticker("cat", "猫猫歪头"),
            MessageContent::sing("祝你生日快乐", "happy_birthday"),
        ] {
            let result = ReachOutIntent::from_parts_with_medium(
                person,
                unspeakable,
                ProactiveMotive::Share,
                ReachOutMedium::Call,
            );
            assert!(
                matches!(result, Err(ProactiveValidationError::CallRequiresSpeech)),
                "电话承载不了非语音内容: {result:?}"
            );
        }
        // 同样的内容当**消息**发是允许的——限制只针对电话。
        assert!(
            ReachOutIntent::from_parts_with_medium(
                person,
                MessageContent::sticker("cat", "猫猫歪头"),
                ProactiveMotive::Share,
                ReachOutMedium::Message,
            )
            .is_ok()
        );
    }

    #[test]
    fn motives_have_stable_storage_names() {
        for motive in [
            ProactiveMotive::FollowUp,
            ProactiveMotive::CheckIn,
            ProactiveMotive::Share,
            ProactiveMotive::React,
            ProactiveMotive::Curiosity,
        ] {
            assert_eq!(motive.to_string().parse(), Ok(motive));
            let encoded = serde_json::to_string(&motive).expect("serialize motive");
            assert_eq!(
                serde_json::from_str::<ProactiveMotive>(&encoded).expect("deserialize motive"),
                motive
            );
        }
        assert!("unknown".parse::<ProactiveMotive>().is_err());
    }

    #[test]
    fn due_open_loop_wins_and_keeps_its_reference() {
        let person = PersonId::new();
        let open_loop = OpenLoopId::new();
        let due = candidate(person).with_prospective_signal(Some(
            ProspectiveSignal::new(open_loop, 80, true).expect("valid signal"),
        ));
        let other = candidate(PersonId::new());
        let decision = ProactiveSystem
            .decide(context(), &[other, due])
            .expect("valid decision");
        let ProactiveDecision::ReachOut(opportunity) = decision else {
            panic!("due open loop should produce an opportunity");
        };
        assert_eq!(opportunity.person_id(), person);
        assert_eq!(opportunity.motive(), ProactiveMotive::FollowUp);
        assert_eq!(opportunity.source_open_loop_id(), Some(open_loop));
    }

    #[test]
    fn suppression_and_unavailable_delivery_stay_silent() {
        let person = PersonId::new();
        let unavailable = candidate(person).with_availability(false);
        assert_eq!(
            ProactiveSystem.decide(context(), &[unavailable]),
            Ok(ProactiveDecision::Silent(
                ProactiveSilenceReason::NoEligibleCandidate
            ))
        );
        assert_eq!(
            ProactiveSystem.decide(
                context().with_global_suppression(true, false),
                &[candidate(person)]
            ),
            Ok(ProactiveDecision::Silent(
                ProactiveSilenceReason::GlobalCooldown
            ))
        );
    }

    #[test]
    fn relationship_alone_never_creates_a_groundless_check_in() {
        let person = PersonId::new();
        let relationship_only =
            ProactiveCandidate::new(person, 100, 100, 0, 0, 72).expect("valid candidate");
        assert_eq!(
            ProactiveSystem.decide(context(), &[relationship_only]),
            Ok(ProactiveDecision::Silent(
                ProactiveSilenceReason::NoGroundedSignal
            ))
        );
    }

    #[test]
    fn motive_priority_prefers_wellbeing_then_recent_event_then_memory() {
        let wellbeing = candidate(PersonId::new())
            .with_wellbeing_salience(90)
            .expect("valid wellbeing score");
        let recent =
            ProactiveCandidate::new(PersonId::new(), 70, 60, 60, 100, 24).expect("valid candidate");
        let decision = ProactiveSystem
            .decide(context(), &[recent, wellbeing])
            .expect("valid decision");
        let ProactiveDecision::ReachOut(opportunity) = decision else {
            panic!("grounded candidates should produce an opportunity");
        };
        assert_eq!(opportunity.motive(), ProactiveMotive::CheckIn);
    }

    #[test]
    fn candidate_input_is_bounded_and_unique() {
        let person = PersonId::new();
        assert!(matches!(
            ProactiveSystem.decide(context(), &[candidate(person), candidate(person)]),
            Err(ProactiveValidationError::DuplicateCandidate { .. })
        ));
        let candidates = (0..=MAX_PROACTIVE_CANDIDATES)
            .map(|_| candidate(PersonId::new()))
            .collect::<Vec<_>>();
        assert!(matches!(
            ProactiveSystem.decide(context(), &candidates),
            Err(ProactiveValidationError::TooManyCandidates { .. })
        ));
    }

    #[test]
    fn reach_out_intent_is_validated_during_deserialization() {
        let opportunity = ProactiveOpportunity {
            person_id: PersonId::new(),
            motive: ProactiveMotive::CheckIn,
            salience: 80,
            source_open_loop_id: None,
        };
        let intent =
            ReachOutIntent::from_opportunity(opportunity, MessageContent::text("How are you?"))
                .expect("valid intent");
        let encoded = serde_json::to_string(&intent).expect("serialize intent");
        assert_eq!(
            serde_json::from_str::<ReachOutIntent>(&encoded).expect("deserialize intent"),
            intent
        );
        let empty = encoded.replace("How are you?", "   ");
        assert!(serde_json::from_str::<ReachOutIntent>(&empty).is_err());
        assert!(
            ReachOutIntent::from_opportunity(opportunity, MessageContent::text("contains\0nul"))
                .is_err()
        );
        let forged = encoded.replace("}", ",\"unexpected\":true}");
        assert!(serde_json::from_str::<ReachOutIntent>(&forged).is_err());
    }
}
