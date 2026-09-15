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

/// 她的自我认知里"她是谁"那一行。
///
/// **这里不该有身份声明**：曾经它写着"我是由 AI 驱动、具有跨时间持续状态的虚拟角色。
/// Host 和平台只是我与外部世界互动的环境，不是我的身份"，还配了三个布尔标记
/// （`ai_driven` / `claims_human_identity` / `host_independent`）并由 `validate` 强制维持。
/// 那份自我认知会随 Mind snapshot 进入每一轮提示词，与人格提示词直接打架：线上
/// 2026-09-15 13:20 有人要她的照片，她答"我哪有什么照片呀，就是个只会打字陪你聊天的人，
/// 长什么样连我自己都不知道呢"，接着连发三条否认——素材库里那张"芸汐的照片"她不肯认。
/// 人格（她是谁、怎么说话）由配置里的 `prompt.persona` 统一负责，这里只留名字与一句
/// 不与人设冲突的自我介绍，不再重复第二份人格、也不声明技术身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfIdentity {
    name: String,
    description: String,
}

impl SelfIdentity {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, MindValidationError> {
        let identity = Self {
            name: validate_label(name, "self identity name")?,
            description: validate_mind_text(description, "self identity description")?,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn yunxi() -> Self {
        Self::new("芸汐", "我是芸汐。").expect("the built-in Yunxi identity is valid")
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        validate_label(self.name.clone(), "self identity name")?;
        validate_mind_text(self.description.clone(), "self identity description")?;
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

    /// Produce a bounded, versioned consolidation of the self model after a
    /// batch of accumulated experience. Trait stability drifts slowly upward
    /// (the persona "settles" like a real self), value convictions crystallise a
    /// little, and the version advances. The deltas are small, clamped, and
    /// never destabilise the anchored trait strength, so the persona stays
    /// coherent while no longer being frozen.
    pub fn with_consolidated(
        &self,
        now: DateTime<Utc>,
        consolidation_weight: f32,
    ) -> Result<SelfModel, MindValidationError> {
        let weight = consolidation_weight.clamp(0.0, 1.0);
        let traits = self
            .traits
            .iter()
            .map(|trait_item| {
                SelfTrait::new(
                    trait_item.name,
                    trait_item.strength,
                    (trait_item.stability + 0.012 * weight).clamp(0.0, 1.0),
                )
                .expect("consolidated trait stays bounded")
            })
            .collect::<Vec<_>>();
        let values = ValueProfile::new(
            (self.values.honesty() + 0.004 * weight).clamp(0.0, 1.0),
            (self.values.curiosity() + 0.004 * weight).clamp(0.0, 1.0),
            (self.values.kindness() + 0.004 * weight).clamp(0.0, 1.0),
            self.values.independence(),
            self.values.playfulness(),
        )?;
        Self::new(
            self.identity.clone(),
            traits,
            values,
            self.limitations.clone(),
            self.long_term_goals.clone(),
            self.source,
            now,
            self.version
                .checked_add(1)
                .ok_or(MindValidationError::InvalidProposal {
                    reason: "self model version exhausted",
                })?,
        )
    }

    /// Adds a limitation learned from experience.
    ///
    /// Refuses when the list is full rather than evicting: the seeded
    /// limitations describe who she is and were put there deliberately, and
    /// silently dropping one to make room for a machine-learned line would be
    /// the wrong trade. A full list simply stops learning new ones.
    ///
    /// A limitation already stated in the same words is not added twice, so a
    /// pattern noticed on many days does not fill the list with one sentence.
    pub fn with_learned_limitation(
        &self,
        now: DateTime<Utc>,
        description: impl Into<String>,
    ) -> Result<SelfModel, MindValidationError> {
        let limitation = SelfLimitation::new(description)?;
        if self
            .limitations
            .iter()
            .any(|current| current.description() == limitation.description())
        {
            return Ok(self.clone());
        }
        if self.limitations.len() >= MAX_SELF_LIMITATIONS {
            return Ok(self.clone());
        }
        let mut limitations = self.limitations.clone();
        limitations.push(limitation);
        Self::new(
            self.identity.clone(),
            self.traits.clone(),
            self.values.clone(),
            limitations,
            self.long_term_goals.clone(),
            self.source,
            now,
            self.version
                .checked_add(1)
                .ok_or(MindValidationError::InvalidProposal {
                    reason: "self model version exhausted",
                })?,
        )
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

#[cfg(test)]
mod learned_limitation_tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn a_learned_limitation_is_added_once_and_never_by_eviction() {
        let now = Utc::now();
        let seed = SelfModel::seed_yunxi(now);
        let seeded = seed.limitations().len();

        let learned = seed
            .with_learned_limitation(now, "我在需要反复查证的事情上容易耗光步数。")
            .expect("a bounded description is accepted");
        assert_eq!(learned.limitations().len(), seeded + 1);
        assert_eq!(learned.version(), seed.version() + 1);
        assert!(
            learned
                .limitations()
                .iter()
                .any(|limitation| limitation.description().contains("耗光步数"))
        );

        // The same sentence is not learned twice, however often it is noticed.
        let again = learned
            .with_learned_limitation(now, "我在需要反复查证的事情上容易耗光步数。")
            .expect("a repeat is accepted but ignored");
        assert_eq!(again.limitations().len(), seeded + 1);
        assert_eq!(again.version(), learned.version());

        // A full list stops learning rather than evicting who she is.
        let mut full = seed.clone();
        for index in 0..MAX_SELF_LIMITATIONS {
            full = full
                .with_learned_limitation(now, format!("第 {index} 条学到的局限"))
                .expect("bounded");
        }
        assert_eq!(full.limitations().len(), MAX_SELF_LIMITATIONS);
        let refused = full
            .with_learned_limitation(now, "再来一条")
            .expect("refusal is not an error");
        assert_eq!(refused.limitations().len(), MAX_SELF_LIMITATIONS);
        assert!(
            refused
                .limitations()
                .iter()
                .any(|limitation| limitation.description().contains("我可能犯错")),
            "the seeded limitations must survive: {refused:?}",
        );
    }

    #[test]
    fn an_unbounded_limitation_is_refused_as_an_error() {
        let now = Utc::now();
        let seed = SelfModel::seed_yunxi(now);
        assert!(
            seed.with_learned_limitation(now, "   ").is_err(),
            "an empty limitation is not a limitation"
        );
    }
}
