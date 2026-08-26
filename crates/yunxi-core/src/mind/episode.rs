use super::common::{
    MAX_RELATED_IDS, MindScope, MindSource, MindValidationError, SCHEMA_VERSION,
    validate_signed_unit, validate_summary, validate_unit,
};
use crate::{EventId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

mind_id!(EpisodeId);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    id: EpisodeId,
    scope: MindScope,
    participants: Vec<PersonId>,
    source_events: Vec<EventId>,
    summary: String,
    salience: f32,
    emotional_weight: f32,
    unresolved: bool,
    source: MindSource,
    occurred_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
    version: u64,
    schema_version: u16,
}

impl Episode {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: EpisodeId,
        scope: MindScope,
        participants: Vec<PersonId>,
        source_events: Vec<EventId>,
        summary: impl Into<String>,
        salience: f32,
        emotional_weight: f32,
        unresolved: bool,
        source: MindSource,
        occurred_at: DateTime<Utc>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        let episode = Self {
            id,
            scope,
            participants,
            source_events,
            summary: validate_summary(summary, "episode summary")?,
            salience: validate_unit(salience, "episode salience")?,
            emotional_weight: validate_signed_unit(emotional_weight, "episode emotional weight")?,
            unresolved,
            source,
            occurred_at,
            created_at,
            version: 1,
            schema_version: SCHEMA_VERSION,
        };
        episode.validate()?;
        Ok(episode)
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        validate_summary(self.summary.clone(), "episode summary")?;
        validate_unit(self.salience, "episode salience")?;
        validate_signed_unit(self.emotional_weight, "episode emotional weight")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported episode schema version",
            });
        }
        if self.created_at < self.occurred_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "episode creation predates occurrence",
            });
        }
        for (field, length) in [
            ("episode participants", self.participants.len()),
            ("episode source events", self.source_events.len()),
        ] {
            if length > MAX_RELATED_IDS {
                return Err(MindValidationError::TooManyItems {
                    field,
                    length,
                    maximum: MAX_RELATED_IDS,
                });
            }
        }
        let mut participants = HashSet::new();
        if self
            .participants
            .iter()
            .any(|person| !participants.insert(*person))
        {
            return Err(MindValidationError::Duplicate {
                field: "episode participant",
            });
        }
        let mut events = HashSet::new();
        if self
            .source_events
            .iter()
            .any(|event| !events.insert(*event))
        {
            return Err(MindValidationError::Duplicate {
                field: "episode source event",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> EpisodeId {
        self.id
    }

    #[must_use]
    pub const fn scope(&self) -> MindScope {
        self.scope
    }

    #[must_use]
    pub fn participants(&self) -> &[PersonId] {
        &self.participants
    }

    #[must_use]
    pub fn source_events(&self) -> &[EventId] {
        &self.source_events
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub const fn salience(&self) -> f32 {
        self.salience
    }

    #[must_use]
    pub const fn emotional_weight(&self) -> f32 {
        self.emotional_weight
    }

    #[must_use]
    pub const fn unresolved(&self) -> bool {
        self.unresolved
    }

    #[must_use]
    pub const fn source(&self) -> MindSource {
        self.source
    }

    #[must_use]
    pub const fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}
