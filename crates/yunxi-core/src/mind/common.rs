use crate::{ConversationId, PersonId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SCHEMA_VERSION: u16 = 1;
pub(crate) const MAX_LABEL_BYTES: usize = 128;
pub(crate) const MAX_LABEL_CHARS: usize = 64;
pub(crate) const MAX_MIND_TEXT_BYTES: usize = 2 * 1_024;
pub(crate) const MAX_MIND_TEXT_CHARS: usize = 1_024;
pub(crate) const MAX_SUMMARY_BYTES: usize = 8 * 1_024;
pub(crate) const MAX_SUMMARY_CHARS: usize = 4_096;
pub(crate) const MAX_EVIDENCE_REFS: usize = 16;
pub(crate) const MAX_RELATED_IDS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MindInfluenceMode {
    #[default]
    Disabled,
    Shadow,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MindScope {
    Global,
    Person { person_id: PersonId },
    Conversation { conversation_id: ConversationId },
}

impl MindScope {
    #[must_use]
    pub const fn person_id(self) -> Option<PersonId> {
        match self {
            Self::Person { person_id } => Some(person_id),
            Self::Global | Self::Conversation { .. } => None,
        }
    }

    #[must_use]
    pub const fn conversation_id(self) -> Option<ConversationId> {
        match self {
            Self::Conversation { conversation_id } => Some(conversation_id),
            Self::Global | Self::Person { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MindSource {
    Seed,
    Experience,
    Conversation,
    ToolResult,
    Reflection,
    Inference,
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MindReasonTag {
    RelatedOpenLoop,
    ActiveInterest,
    BeliefConflict,
    CuriosityTriggered,
    AgendaResume,
    RelationContext,
    LowSocialValue,
    StaleEvent,
    ExplicitCorrection,
    ReflectionConsolidation,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MindValidationError {
    #[error("{field} must not be empty")]
    EmptyText { field: &'static str },
    #[error("{field} is {length} bytes, above maximum {maximum}")]
    TextTooLong {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error("{field} is {length} characters, above maximum {maximum}")]
    TextTooManyCharacters {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error("{field} must not contain NUL")]
    TextContainsNul { field: &'static str },
    #[error("{field} contains a non-finite value")]
    NonFinite { field: &'static str },
    #[error("{field} is outside [{minimum}, {maximum}]")]
    OutOfRange {
        field: &'static str,
        minimum: i8,
        maximum: i8,
    },
    #[error("{field} contains too many items: {length}, maximum {maximum}")]
    TooManyItems {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error("invalid state transition from {from} to {to}")]
    InvalidTransition {
        from: &'static str,
        to: &'static str,
    },
    #[error("timestamps are inconsistent: {reason}")]
    InvalidTimestamp { reason: &'static str },
    #[error("scope is invalid: {reason}")]
    InvalidScope { reason: &'static str },
    #[error("version must be non-zero")]
    ZeroVersion,
    #[error("duplicate item: {field}")]
    Duplicate { field: &'static str },
    #[error("sensitive person inference is not allowed")]
    SensitivePersonInference,
    #[error("proposal is invalid: {reason}")]
    InvalidProposal { reason: &'static str },
}

pub(crate) fn validate_text(
    value: impl Into<String>,
    field: &'static str,
    max_bytes: usize,
    max_chars: usize,
) -> Result<String, MindValidationError> {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(MindValidationError::EmptyText { field });
    }
    if value.contains('\0') {
        return Err(MindValidationError::TextContainsNul { field });
    }
    if value.len() > max_bytes {
        return Err(MindValidationError::TextTooLong {
            field,
            length: value.len(),
            maximum: max_bytes,
        });
    }
    let chars = value.chars().count();
    if chars > max_chars {
        return Err(MindValidationError::TextTooManyCharacters {
            field,
            length: chars,
            maximum: max_chars,
        });
    }
    Ok(trimmed.to_owned())
}

pub(crate) fn validate_label(
    value: impl Into<String>,
    field: &'static str,
) -> Result<String, MindValidationError> {
    validate_text(value, field, MAX_LABEL_BYTES, MAX_LABEL_CHARS)
}

pub(crate) fn validate_mind_text(
    value: impl Into<String>,
    field: &'static str,
) -> Result<String, MindValidationError> {
    validate_text(value, field, MAX_MIND_TEXT_BYTES, MAX_MIND_TEXT_CHARS)
}

pub(crate) fn validate_summary(
    value: impl Into<String>,
    field: &'static str,
) -> Result<String, MindValidationError> {
    validate_text(value, field, MAX_SUMMARY_BYTES, MAX_SUMMARY_CHARS)
}

pub(crate) fn validate_unit(value: f32, field: &'static str) -> Result<f32, MindValidationError> {
    validate_range(value, 0.0, 1.0, field)
}

pub(crate) fn validate_signed_unit(
    value: f32,
    field: &'static str,
) -> Result<f32, MindValidationError> {
    validate_range(value, -1.0, 1.0, field)
}

fn validate_range(
    value: f32,
    minimum: f32,
    maximum: f32,
    field: &'static str,
) -> Result<f32, MindValidationError> {
    if !value.is_finite() {
        return Err(MindValidationError::NonFinite { field });
    }
    if !(minimum..=maximum).contains(&value) {
        return Err(MindValidationError::OutOfRange {
            field,
            minimum: minimum as i8,
            maximum: maximum as i8,
        });
    }
    Ok(value)
}

pub(crate) fn normalized_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub(crate) fn looks_sensitive(value: &str) -> bool {
    let normalized = value.to_lowercase();
    [
        "政治倾向",
        "宗教信仰",
        "性取向",
        "犯罪记录",
        "精神疾病",
        "健康诊断",
        "political affiliation",
        "religious belief",
        "sexual orientation",
        "criminal history",
        "medical diagnosis",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}
