//! Observation: what the system actually observed from the external world.
//!
//! "用户发来『面试过了』" is an observation. "用户很开心" is an inference.
//! Observations keep their source reliability, confidence, and TTL; they are
//! never silently promoted to facts (v4 §10–14, §178).

use super::{
    WorldScope, WorldValidationError,
    common::{
        MAX_WORLD_TEXT_BYTES, MAX_WORLD_TEXT_CHARS, clamp_unit, validate_text, validate_unit,
    },
};
use crate::EventId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Bounded size of one payload.
pub const MAX_OBSERVATION_PAYLOAD_BYTES: usize = MAX_WORLD_TEXT_BYTES;
pub const MAX_OBSERVATION_PAYLOAD_CHARS: usize = MAX_WORLD_TEXT_CHARS;
/// Maximum observations derived from a single world event.
pub const MAX_OBSERVATIONS_PER_EVENT: usize = 8;
/// Runtime observation retention cap (bounded, TTL-aware).
pub const MAX_RUNTIME_OBSERVATIONS: usize = 4_096;
/// Hard cap on observation TTL (1 year): observations are never eternal-ish.
pub const MAX_OBSERVATION_TTL_SECONDS: u64 = 31_536_000;

/// What kind of world event produced this observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// Inbound user/host message text.
    MessageReceived,
    /// Tool execution returned a result.
    ToolResult,
    /// An action (send/deliver) completed.
    ActionResult,
    /// Host connectivity/state changed.
    HostState,
    /// System/sensor state (build status, file state, url status).
    SystemState,
    /// Conversation-level event (collision, floor change, answered pending).
    ConversationEvent,
}

/// Where the observation came from. Different sources carry different weight
/// (v4 §12): a statement by the user is not the same as a model extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    DirectUserStatement,
    ToolResult,
    PlatformEvent,
    SystemState,
    ModelExtraction,
    DerivedObservation,
}

impl ObservationSource {
    /// Weight of this source: how much it can raise confidence on its own.
    #[must_use]
    pub const fn weight(self) -> f32 {
        match self {
            Self::DirectUserStatement => 0.95,
            Self::ToolResult => 1.0,
            Self::PlatformEvent => 0.9,
            Self::SystemState => 0.85,
            Self::ModelExtraction => 0.6,
            Self::DerivedObservation => 0.5,
        }
    }

    /// Direct evidence: first-hand, not model-inferred.
    #[must_use]
    pub const fn is_direct(self) -> bool {
        matches!(
            self,
            Self::DirectUserStatement | Self::ToolResult | Self::PlatformEvent | Self::SystemState
        )
    }
}

/// Serializable reliability profile of one source.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ObservationSourceReliability {
    source: ObservationSource,
    weight: f32,
}

impl ObservationSourceReliability {
    pub fn new(source: ObservationSource) -> Result<Self, WorldValidationError> {
        let reliability = Self {
            source,
            weight: source.weight(),
        };
        reliability.validate()?;
        Ok(reliability)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.weight, "source reliability weight")?;
        if (self.weight - self.source.weight()).abs() > f32::EPSILON {
            return Err(WorldValidationError::InvalidState {
                reason: "source reliability weight does not match its source",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn source(&self) -> ObservationSource {
        self.source
    }

    #[must_use]
    pub const fn weight(&self) -> f32 {
        self.weight
    }
}

/// Bounded, validated payload carried by an observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationPayload {
    content: String,
    facet: Option<String>,
}

impl ObservationPayload {
    pub fn new(
        content: impl Into<String>,
        facet: Option<impl Into<String>>,
    ) -> Result<Self, WorldValidationError> {
        let content = validate_text(content, "observation payload")?;
        let facet = match facet {
            Some(facet) => Some(validate_text(facet, "observation facet")?),
            None => None,
        };
        Ok(Self { content, facet })
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_text(self.content.clone(), "observation payload")?;
        if let Some(facet) = &self.facet {
            validate_text(facet.clone(), "observation facet")?;
        }
        Ok(())
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    #[must_use]
    pub fn facet(&self) -> Option<&str> {
        self.facet.as_deref()
    }
}

/// What the world model proposes to record. Validated by Rust into an
/// [`Observation`]; the model never writes raw state (v4 §87).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationDraft {
    scope: WorldScope,
    kind: ObservationKind,
    source: ObservationSource,
    payload: ObservationPayload,
    confidence: f32,
    ttl_seconds: Option<u64>,
}

impl ObservationDraft {
    pub fn new(
        scope: WorldScope,
        kind: ObservationKind,
        source: ObservationSource,
        payload: ObservationPayload,
        confidence: f32,
        ttl_seconds: Option<u64>,
    ) -> Result<Self, WorldValidationError> {
        let confidence = validate_unit(clamp_unit(confidence), "observation confidence")?;
        if let Some(ttl_seconds) = ttl_seconds
            && ttl_seconds > MAX_OBSERVATION_TTL_SECONDS
        {
            return Err(WorldValidationError::InvalidState {
                reason: "observation TTL is above the hard cap",
            });
        }
        Ok(Self {
            scope,
            kind,
            source,
            payload,
            confidence,
            ttl_seconds,
        })
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.confidence, "observation confidence")?;
        self.payload.validate()?;
        Ok(())
    }

    #[must_use]
    pub const fn scope(&self) -> WorldScope {
        self.scope
    }

    #[must_use]
    pub const fn kind(&self) -> ObservationKind {
        self.kind
    }

    #[must_use]
    pub const fn source(&self) -> ObservationSource {
        self.source
    }

    #[must_use]
    pub fn payload(&self) -> &ObservationPayload {
        &self.payload
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn ttl_seconds(&self) -> Option<u64> {
        self.ttl_seconds
    }

    /// Build a validated observation at `observed_at`, linked to the source
    /// world event. `expires_at` = now + ttl (checked for overflow).
    pub fn build(
        &self,
        id: super::ObservationId,
        source_event_id: EventId,
        observed_at: DateTime<Utc>,
    ) -> Result<Observation, WorldValidationError> {
        Observation::new(
            id,
            source_event_id,
            self.scope,
            self.kind,
            self.source,
            self.payload.clone(),
            self.confidence,
            observed_at,
            self.expires_at(observed_at)?,
        )
    }

    fn expires_at(
        &self,
        observed_at: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, WorldValidationError> {
        match self.ttl_seconds {
            Some(seconds) => observed_at
                .checked_add_signed(chrono::Duration::seconds(seconds as i64))
                .map(Some)
                .ok_or(WorldValidationError::InvalidTimestamp {
                    reason: "observation TTL overflows",
                }),
            None => Ok(None),
        }
    }
}

/// A single structured observation (v4 §11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    id: super::ObservationId,
    source_event_id: EventId,
    scope: WorldScope,
    kind: ObservationKind,
    source: ObservationSource,
    payload: ObservationPayload,
    confidence: f32,
    observed_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    version: u64,
}

impl Observation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: super::ObservationId,
        source_event_id: EventId,
        scope: WorldScope,
        kind: ObservationKind,
        source: ObservationSource,
        payload: ObservationPayload,
        confidence: f32,
        observed_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Self, WorldValidationError> {
        let observation = Self {
            id,
            source_event_id,
            scope,
            kind,
            source,
            payload,
            confidence: clamp_unit(confidence),
            observed_at,
            expires_at,
            version: 1,
        };
        observation.validate()?;
        Ok(observation)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.confidence, "observation confidence")?;
        self.payload.validate()?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        if let Some(expires_at) = self.expires_at
            && expires_at < self.observed_at
        {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "observation expires before it was observed",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::ObservationId {
        self.id
    }

    #[must_use]
    pub const fn source_event_id(&self) -> EventId {
        self.source_event_id
    }

    #[must_use]
    pub const fn scope(&self) -> WorldScope {
        self.scope
    }

    #[must_use]
    pub const fn kind(&self) -> ObservationKind {
        self.kind
    }

    #[must_use]
    pub const fn source(&self) -> ObservationSource {
        self.source
    }

    #[must_use]
    pub fn payload(&self) -> &ObservationPayload {
        &self.payload
    }

    /// Stored confidence, already clamped to [0, 1].
    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    /// Confidence the system is allowed to act on, capped by source weight
    /// (ModelExtraction cannot reach 1.0 no matter what the model claims).
    #[must_use]
    pub fn effective_confidence(&self) -> f32 {
        self.confidence.min(self.source.weight())
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Canonical dedupe fingerprint: same scope+kind+facet+content is the
    /// same observation even with a different id.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        observation_fingerprint(self.scope, self.kind, self.payload.facet(), self.payload.content())
    }

    /// Replace this observation with a newer one of the same fingerprint,
    /// keeping the stable id and bumping the version.
    pub fn replace_with(&mut self, newer: Observation) -> Result<(), WorldValidationError> {
        newer.validate()?;
        if self.fingerprint() != newer.fingerprint() {
            return Err(WorldValidationError::InvalidState {
                reason: "replacement observation fingerprint differs",
            });
        }
        let version = self.version.saturating_add(1);
        self.source_event_id = newer.source_event_id;
        self.scope = newer.scope;
        self.kind = newer.kind;
        self.source = newer.source;
        self.payload = newer.payload;
        self.confidence = newer.confidence;
        self.observed_at = newer.observed_at;
        self.expires_at = newer.expires_at;
        self.version = version;
        self.validate()
    }

    #[must_use]
    pub fn freshness_at(&self, now: DateTime<Utc>) -> super::Freshness {
        super::temporal::freshness_at(self.observed_at, self.expires_at, now)
    }

    #[must_use]
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        matches!(self.freshness_at(now), super::Freshness::Expired)
    }
}

/// Stable fingerprint of a prospective observation (payload-level dedupe).
#[must_use]
pub fn observation_fingerprint(
    scope: WorldScope,
    kind: ObservationKind,
    facet: Option<&str>,
    content: &str,
) -> String {
    match facet {
        Some(facet) => format!("{scope:?}|{kind:?}|{facet}|{content}"),
        None => format!("{scope:?}|{kind:?}|{content}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventId;
    use chrono::Duration;

    fn sample_draft(confidence: f32, ttl: Option<u64>) -> ObservationDraft {
        let payload = ObservationPayload::new("面试过了", None::<&str>).expect("payload");
        ObservationDraft::new(
            WorldScope::Global,
            ObservationKind::MessageReceived,
            ObservationSource::DirectUserStatement,
            payload,
            confidence,
            ttl,
        )
        .expect("draft")
    }

    #[test]
    fn source_reliability_caps_effective_confidence() {
        assert_eq!(ObservationSource::DirectUserStatement.weight(), 0.95);
        assert_eq!(ObservationSource::ModelExtraction.weight(), 0.6);
        assert!(ObservationSource::DirectUserStatement.is_direct());
        assert!(!ObservationSource::DerivedObservation.is_direct());
        let reliability =
            ObservationSourceReliability::new(ObservationSource::ToolResult).expect("valid");
        assert_eq!(reliability.weight(), 1.0);
        assert!(matches!(reliability.validate(), Ok(())));
    }

    #[test]
    fn confidence_is_clamped_on_observation() {
        let payload = ObservationPayload::new("x", None::<&str>).expect("payload");
        let observation = Observation::new(
            super::super::ObservationId::new(),
            EventId::new(),
            WorldScope::Global,
            ObservationKind::SystemState,
            ObservationSource::ModelExtraction,
            payload,
            1.7,
            Utc::now(),
            None,
        )
        .expect("observation with clamped confidence");
        assert_eq!(observation.confidence(), 1.0);
        // Model extraction can never reach full effectiveness.
        assert_eq!(observation.effective_confidence(), 0.6);
    }

    #[test]
    fn ttl_drives_freshness_and_expiry() {
        let now = Utc::now();
        let draft = sample_draft(0.8, Some(3600));
        let observation = draft
            .build(super::super::ObservationId::new(), EventId::new(), now)
            .expect("observation");
        assert_eq!(
            observation.freshness_at(now + Duration::minutes(1)),
            super::super::Freshness::Fresh
        );
        // 20% stale window → last 12 minutes of a 60-minute TTL.
        assert_eq!(
            observation.freshness_at(now + Duration::minutes(40)),
            super::super::Freshness::Fresh
        );
        assert_eq!(
            observation.freshness_at(now + Duration::minutes(52)),
            super::super::Freshness::Stale
        );
        assert_eq!(
            observation.freshness_at(now + Duration::minutes(61)),
            super::super::Freshness::Expired
        );
        // Future observation is unknown, never fresh.
        assert_eq!(
            observation.freshness_at(now - Duration::minutes(1)),
            super::super::Freshness::Unknown
        );
    }

    #[test]
    fn fingerprint_dedupes_and_replace_keeps_id() {
        let now = Utc::now();
        let a = sample_draft(0.5, None)
            .build(super::super::ObservationId::new(), EventId::new(), now)
            .expect("a");
        let b = sample_draft(0.9, None)
            .build(super::super::ObservationId::new(), EventId::new(), now)
            .expect("b");
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.id(), b.id());
        let mut a = a;
        a.replace_with(b.clone()).expect("replace");
        let id = a.id();
        assert_eq!(a.id(), id);
        assert_eq!(a.confidence(), 0.9);
        assert_eq!(a.version(), 2);
    }

    #[test]
    fn invalid_ttl_and_inverted_expiry_are_rejected() {
        let now = Utc::now();
        let payload = ObservationPayload::new("x", None::<&str>).expect("payload");
        assert!(ObservationDraft::new(
            WorldScope::Global,
            ObservationKind::SystemState,
            ObservationSource::SystemState,
            payload.clone(),
            0.5,
            Some(MAX_OBSERVATION_TTL_SECONDS + 1),
        )
        .is_err());
        assert!(Observation::new(
            super::super::ObservationId::new(),
            EventId::new(),
            WorldScope::Global,
            ObservationKind::SystemState,
            ObservationSource::SystemState,
            payload,
            0.5,
            now,
            Some(now - Duration::seconds(1)),
        )
        .is_err());
    }

    #[test]
    fn serde_roundtrip_stays_valid() {
        let now = Utc::now();
        let observation = sample_draft(0.8, Some(60))
            .build(super::super::ObservationId::new(), EventId::new(), now)
            .expect("observation");
        let encoded = serde_json::to_string(&observation).expect("json");
        let decoded: Observation = serde_json::from_str(&encoded).expect("decode");
        decoded.validate().expect("still valid");
        assert_eq!(decoded, observation);
    }
}
