use super::common::{
    MAX_MIND_TEXT_BYTES, MAX_MIND_TEXT_CHARS, MindSource, MindValidationError, SCHEMA_VERSION,
    validate_label, validate_mind_text, validate_unit,
};
use crate::GoalId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_SELF_TRAITS: usize = 8;
pub const MAX_SELF_LIMITATIONS: usize = 16;
pub const MAX_SELF_LONG_TERM_GOALS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfIdentity {
    name: String,
    description: String,
    ai_driven: bool,
    claims_human_identity: bool,
    host_independent: bool,
}

impl SelfIdentity {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, MindValidationError> {
        let identity = Self {
            name: validate_label(name, "self identity name")?,
            description: validate_mind_text(description, "self identity description")?,
            ai_driven: true,
            claims_human_identity: false,
            host_independent: true,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn yunxi() -> Self {
        Self::new(
            "芸汐",
            "我是由 AI 驱动、具有跨时间持续状态的虚拟角色。Host 和平台只是我与外部世界互动的环境，不是我的身份。",
        )
        .expect("the built-in Yunxi identity is valid")
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        validate_label(self.name.clone(), "self identity name")?;
        validate_mind_text(self.description.clone(), "self identity description")?;
        if !self.ai_driven || self.claims_human_identity || !self.host_independent {
            return Err(MindValidationError::InvalidProposal {
                reason: "self identity must remain AI-driven, non-human, and host-independent",
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub const fn is_ai_driven(&self) -> bool {
        self.ai_driven
    }

    #[must_use]
    pub const fn is_host_independent(&self) -> bool {
        self.host_independent
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraitName {
    Curiosity,
    Playfulness,
    Independence,
    Empathy,
    Directness,
    Patience,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfTrait {
    name: TraitName,
    strength: f32,
    stability: f32,
}

impl SelfTrait {
    pub fn new(
        name: TraitName,
        strength: f32,
        stability: f32,
    ) -> Result<Self, MindValidationError> {
        Ok(Self {
            name,
            strength: validate_unit(strength, "trait strength")?,
            stability: validate_unit(stability, "trait stability")?,
        })
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        validate_unit(self.strength, "trait strength")?;
        validate_unit(self.stability, "trait stability")?;
        Ok(())
    }

    #[must_use]
    pub const fn name(&self) -> TraitName {
        self.name
    }

    #[must_use]
    pub const fn strength(&self) -> f32 {
        self.strength
    }

    #[must_use]
    pub const fn stability(&self) -> f32 {
        self.stability
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueProfile {
    honesty: f32,
    curiosity: f32,
    kindness: f32,
    independence: f32,
    playfulness: f32,
}

impl ValueProfile {
    pub fn new(
        honesty: f32,
        curiosity: f32,
        kindness: f32,
        independence: f32,
        playfulness: f32,
    ) -> Result<Self, MindValidationError> {
        let profile = Self {
            honesty,
            curiosity,
            kindness,
            independence,
            playfulness,
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        validate_unit(self.honesty, "value honesty")?;
        validate_unit(self.curiosity, "value curiosity")?;
        validate_unit(self.kindness, "value kindness")?;
        validate_unit(self.independence, "value independence")?;
        validate_unit(self.playfulness, "value playfulness")?;
        Ok(())
    }

    #[must_use]
    pub const fn honesty(&self) -> f32 {
        self.honesty
    }

    #[must_use]
    pub const fn curiosity(&self) -> f32 {
        self.curiosity
    }

    #[must_use]
    pub const fn kindness(&self) -> f32 {
        self.kindness
    }

    #[must_use]
    pub const fn independence(&self) -> f32 {
        self.independence
    }

    #[must_use]
    pub const fn playfulness(&self) -> f32 {
        self.playfulness
    }
}

impl Default for ValueProfile {
    fn default() -> Self {
        Self::new(0.9, 0.85, 0.85, 0.75, 0.65).expect("seed values are bounded")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfLimitation {
    description: String,
}

impl SelfLimitation {
    pub fn new(description: impl Into<String>) -> Result<Self, MindValidationError> {
        Ok(Self {
            description: validate_mind_text(description, "self limitation")?,
        })
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfModel {
    identity: SelfIdentity,
    traits: Vec<SelfTrait>,
    values: ValueProfile,
    limitations: Vec<SelfLimitation>,
    long_term_goals: Vec<GoalId>,
    source: MindSource,
    updated_at: DateTime<Utc>,
    version: u64,
    schema_version: u16,
}

impl SelfModel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: SelfIdentity,
        traits: Vec<SelfTrait>,
        values: ValueProfile,
        limitations: Vec<SelfLimitation>,
        long_term_goals: Vec<GoalId>,
        source: MindSource,
        updated_at: DateTime<Utc>,
        version: u64,
    ) -> Result<Self, MindValidationError> {
        let model = Self {
            identity,
            traits,
            values,
            limitations,
            long_term_goals,
            source,
            updated_at,
            version,
            schema_version: SCHEMA_VERSION,
        };
        model.validate()?;
        Ok(model)
    }

    #[must_use]
    pub fn seed_yunxi(now: DateTime<Utc>) -> Self {
        let traits = [
            (TraitName::Curiosity, 0.88),
            (TraitName::Playfulness, 0.68),
            (TraitName::Independence, 0.78),
            (TraitName::Empathy, 0.85),
            (TraitName::Directness, 0.72),
            (TraitName::Patience, 0.8),
        ]
        .into_iter()
        .map(|(name, strength)| {
            SelfTrait::new(name, strength, 0.9).expect("seed traits are bounded")
        })
        .collect();
        let limitations = vec![
            SelfLimitation::new("我可能犯错，需要在新证据下修正判断。")
                .expect("seed limitation is bounded"),
            SelfLimitation::new("我只能使用当前 Host 明确提供且获准的能力。")
                .expect("seed limitation is bounded"),
        ];
        Self::new(
            SelfIdentity::yunxi(),
            traits,
            ValueProfile::default(),
            limitations,
            Vec::new(),
            MindSource::Seed,
            now,
            1,
        )
        .expect("the built-in Yunxi self model is valid")
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        self.identity.validate()?;
        self.values.validate()?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported self-model schema version",
            });
        }
        if self.traits.len() > MAX_SELF_TRAITS {
            return Err(MindValidationError::TooManyItems {
                field: "self traits",
                length: self.traits.len(),
                maximum: MAX_SELF_TRAITS,
            });
        }
        if self.limitations.len() > MAX_SELF_LIMITATIONS {
            return Err(MindValidationError::TooManyItems {
                field: "self limitations",
                length: self.limitations.len(),
                maximum: MAX_SELF_LIMITATIONS,
            });
        }
        if self.long_term_goals.len() > MAX_SELF_LONG_TERM_GOALS {
            return Err(MindValidationError::TooManyItems {
                field: "self long-term goals",
                length: self.long_term_goals.len(),
                maximum: MAX_SELF_LONG_TERM_GOALS,
            });
        }
        let mut trait_names = HashSet::new();
        for personality_trait in &self.traits {
            personality_trait.validate()?;
            if !trait_names.insert(personality_trait.name()) {
                return Err(MindValidationError::Duplicate {
                    field: "self trait",
                });
            }
        }
        let mut goals = HashSet::new();
        if self.long_term_goals.iter().any(|goal| !goals.insert(*goal)) {
            return Err(MindValidationError::Duplicate {
                field: "self long-term goal",
            });
        }
        for limitation in &self.limitations {
            super::common::validate_text(
                limitation.description.clone(),
                "self limitation",
                MAX_MIND_TEXT_BYTES,
                MAX_MIND_TEXT_CHARS,
            )?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn identity(&self) -> &SelfIdentity {
        &self.identity
    }

    #[must_use]
    pub fn traits(&self) -> &[SelfTrait] {
        &self.traits
    }

    #[must_use]
    pub const fn values(&self) -> &ValueProfile {
        &self.values
    }

    #[must_use]
    pub fn limitations(&self) -> &[SelfLimitation] {
        &self.limitations
    }

    #[must_use]
    pub fn long_term_goals(&self) -> &[GoalId] {
        &self.long_term_goals
    }

    #[must_use]
    pub const fn source(&self) -> MindSource {
        self.source
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }
}
