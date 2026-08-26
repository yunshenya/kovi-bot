use super::common::{
    MindSource, MindValidationError, SCHEMA_VERSION, normalized_key, validate_mind_text,
    validate_signed_unit, validate_unit,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mind_id!(InterestId);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interest {
    id: InterestId,
    topic: String,
    topic_key: String,
    activation: f32,
    long_term_affinity: f32,
    novelty: f32,
    source: MindSource,
    last_triggered_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    version: u64,
    schema_version: u16,
}

impl Interest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: InterestId,
        topic: impl Into<String>,
        activation: f32,
        long_term_affinity: f32,
        novelty: f32,
        source: MindSource,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        let topic = validate_mind_text(topic, "interest topic")?;
        let interest = Self {
            id,
            topic_key: normalized_key(&topic),
            topic,
            activation: validate_unit(activation, "interest activation")?,
            long_term_affinity: validate_unit(long_term_affinity, "interest long-term affinity")?,
            novelty: validate_unit(novelty, "interest novelty")?,
            source,
            last_triggered_at: now,
            updated_at: now,
            version: 1,
            schema_version: SCHEMA_VERSION,
        };
        interest.validate()?;
        Ok(interest)
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        let topic = validate_mind_text(self.topic.clone(), "interest topic")?;
        if normalized_key(&topic) != self.topic_key {
            return Err(MindValidationError::InvalidProposal {
                reason: "interest topic key does not match its topic",
            });
        }
        validate_unit(self.activation, "interest activation")?;
        validate_unit(self.long_term_affinity, "interest long-term affinity")?;
        validate_unit(self.novelty, "interest novelty")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported interest schema version",
            });
        }
        if self.updated_at < self.last_triggered_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "interest update predates its trigger",
            });
        }
        Ok(())
    }

    pub fn activate(
        &self,
        activation_delta: f32,
        affinity_delta: f32,
        novelty: f32,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        validate_signed_unit(activation_delta, "interest activation delta")?;
        validate_signed_unit(affinity_delta, "interest affinity delta")?;
        validate_unit(novelty, "interest novelty")?;
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "interest activation predates stored state",
            });
        }
        let mut updated = self.clone();
        updated.activation = (updated.activation + activation_delta).clamp(0.0, 1.0);
        updated.long_term_affinity = (updated.long_term_affinity + affinity_delta).clamp(0.0, 1.0);
        updated.novelty = novelty;
        updated.last_triggered_at = now;
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    pub fn decay(
        &self,
        now: DateTime<Utc>,
        half_life_seconds: f64,
    ) -> Result<Self, MindValidationError> {
        if !half_life_seconds.is_finite() || half_life_seconds <= 0.0 {
            return Err(MindValidationError::InvalidProposal {
                reason: "interest half-life must be positive and finite",
            });
        }
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "interest decay predates stored state",
            });
        }
        let elapsed = (now - self.updated_at).num_milliseconds().max(0) as f64 / 1_000.0;
        let retention = (-std::f64::consts::LN_2 * elapsed / half_life_seconds).exp() as f32;
        let mut updated = self.clone();
        updated.activation *= retention;
        updated.novelty *= retention;
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    #[must_use]
    pub const fn id(&self) -> InterestId {
        self.id
    }

    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    #[must_use]
    pub fn topic_key(&self) -> &str {
        &self.topic_key
    }

    #[must_use]
    pub const fn activation(&self) -> f32 {
        self.activation
    }

    #[must_use]
    pub const fn long_term_affinity(&self) -> f32 {
        self.long_term_affinity
    }

    #[must_use]
    pub const fn novelty(&self) -> f32 {
        self.novelty
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
}
