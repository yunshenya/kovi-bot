use super::common::{
    MindScope, MindValidationError, SCHEMA_VERSION, normalized_key, validate_mind_text,
    validate_signed_unit, validate_unit,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mind_id!(PreferenceId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceSource {
    Seed,
    Experience,
    Reflection,
    DeliberateChange,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preference {
    id: PreferenceId,
    scope: MindScope,
    subject: String,
    subject_key: String,
    valence: f32,
    intensity: f32,
    confidence: f32,
    stability: f32,
    source: PreferenceSource,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    version: u64,
    schema_version: u16,
}

impl Preference {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PreferenceId,
        subject: impl Into<String>,
        valence: f32,
        intensity: f32,
        confidence: f32,
        stability: f32,
        source: PreferenceSource,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        let subject = validate_mind_text(subject, "preference subject")?;
        let preference = Self {
            id,
            scope: MindScope::Global,
            subject_key: normalized_key(&subject),
            subject,
            valence: validate_signed_unit(valence, "preference valence")?,
            intensity: validate_unit(intensity, "preference intensity")?,
            confidence: validate_unit(confidence, "preference confidence")?,
            stability: validate_unit(stability, "preference stability")?,
            source,
            created_at: now,
            updated_at: now,
            version: 1,
            schema_version: SCHEMA_VERSION,
        };
        preference.validate()?;
        Ok(preference)
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        if self.scope != MindScope::Global {
            return Err(MindValidationError::InvalidScope {
                reason: "Yunxi preferences must be global self state",
            });
        }
        let subject = validate_mind_text(self.subject.clone(), "preference subject")?;
        if normalized_key(&subject) != self.subject_key {
            return Err(MindValidationError::InvalidProposal {
                reason: "preference subject key does not match its subject",
            });
        }
        validate_signed_unit(self.valence, "preference valence")?;
        validate_unit(self.intensity, "preference intensity")?;
        validate_unit(self.confidence, "preference confidence")?;
        validate_unit(self.stability, "preference stability")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported preference schema version",
            });
        }
        if self.updated_at < self.created_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "preference updated_at predates created_at",
            });
        }
        Ok(())
    }

    pub fn apply_delta(
        &self,
        valence_delta: f32,
        intensity_delta: f32,
        confidence_delta: f32,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        validate_signed_unit(valence_delta, "preference valence delta")?;
        validate_signed_unit(intensity_delta, "preference intensity delta")?;
        validate_signed_unit(confidence_delta, "preference confidence delta")?;
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "preference update predates stored state",
            });
        }
        let mut updated = self.clone();
        updated.valence = (updated.valence + valence_delta).clamp(-1.0, 1.0);
        updated.intensity = (updated.intensity + intensity_delta).clamp(0.0, 1.0);
        updated.confidence = (updated.confidence + confidence_delta).clamp(0.0, 1.0);
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    #[must_use]
    pub const fn id(&self) -> PreferenceId {
        self.id
    }

    #[must_use]
    pub const fn scope(&self) -> MindScope {
        self.scope
    }

    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    #[must_use]
    pub fn subject_key(&self) -> &str {
        &self.subject_key
    }

    #[must_use]
    pub const fn valence(&self) -> f32 {
        self.valence
    }

    #[must_use]
    pub const fn intensity(&self) -> f32 {
        self.intensity
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn stability(&self) -> f32 {
        self.stability
    }

    #[must_use]
    pub const fn source(&self) -> PreferenceSource {
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
