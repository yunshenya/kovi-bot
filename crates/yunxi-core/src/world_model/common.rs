//! Shared validation helpers and the World Model error type.

use thiserror::Error;

/// Bounded long text (observation payloads, situation detail, notes).
pub const MAX_WORLD_TEXT_BYTES: usize = 2 * 1_024;
pub const MAX_WORLD_TEXT_CHARS: usize = 1_024;
/// Bounded short value text (property values, hypothesis propositions).
pub const MAX_WORLD_VALUE_BYTES: usize = 512;
pub const MAX_WORLD_VALUE_CHARS: usize = 256;
/// Maximum observation references on a hypothesis.
pub const MAX_EVIDENCE_REFS: usize = 32;
/// Maximum related ids (goals / open loops) on one record.
pub const MAX_RELATED_IDS: usize = 16;

/// All World Model validation failures. State can never be constructed with
/// invalid content, even through serde (types validate in their constructors
/// and `validate()` methods; serde cannot bypass them via malicious JSON
/// because every deserialized instance is re-validated by callers).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum WorldValidationError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} is {length} bytes, above maximum {maximum}")]
    TooLong {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error("{field} is {length} characters, above maximum {maximum}")]
    TooManyCharacters {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error("{field} contains a NUL byte")]
    ContainsNul { field: &'static str },
    #[error("{field} contains a non-finite value")]
    NonFinite { field: &'static str },
    #[error("{field} is outside [{minimum}, {maximum}]")]
    OutOfRange {
        field: &'static str,
        minimum: f32,
        maximum: f32,
    },
    #[error("{field} contains too many items: {length}, maximum {maximum}")]
    TooManyItems {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error("duplicate item: {field}")]
    DuplicateItem { field: &'static str },
    #[error("timestamps are inconsistent: {reason}")]
    InvalidTimestamp { reason: &'static str },
    #[error("invalid transition from {from} to {to}")]
    InvalidTransition { from: &'static str, to: &'static str },
    #[error("state is invalid: {reason}")]
    InvalidState { reason: &'static str },
    #[error("version must be non-zero")]
    ZeroVersion,
    #[error("scope is invalid: {reason}")]
    InvalidScope { reason: &'static str },
    #[error("proposal is stale: expected version {expected}, actual {actual}")]
    StaleProposal { expected: u64, actual: u64 },
}

/// Validate bounded long/medium text shared by world records.
pub(crate) fn validate_text(
    value: impl Into<String>,
    field: &'static str,
) -> Result<String, WorldValidationError> {
    validate_text_with_limits(value, field, MAX_WORLD_TEXT_BYTES, MAX_WORLD_TEXT_CHARS)
}

/// Validate bounded short value text.
pub(crate) fn validate_value(
    value: impl Into<String>,
    field: &'static str,
) -> Result<String, WorldValidationError> {
    validate_text_with_limits(value, field, MAX_WORLD_VALUE_BYTES, MAX_WORLD_VALUE_CHARS)
}

fn validate_text_with_limits(
    value: impl Into<String>,
    field: &'static str,
    max_bytes: usize,
    max_chars: usize,
) -> Result<String, WorldValidationError> {
    let value = value.into();
    if value.trim().is_empty() {
        return Err(WorldValidationError::Empty { field });
    }
    if value.contains('\0') {
        return Err(WorldValidationError::ContainsNul { field });
    }
    if value.len() > max_bytes {
        return Err(WorldValidationError::TooLong {
            field,
            length: value.len(),
            maximum: max_bytes,
        });
    }
    if value.chars().count() > max_chars {
        return Err(WorldValidationError::TooManyCharacters {
            field,
            length: value.chars().count(),
            maximum: max_chars,
        });
    }
    Ok(value)
}

/// Validate a probability/confidence/score in [0, 1].
pub(crate) fn validate_unit(
    value: f32,
    field: &'static str,
) -> Result<f32, WorldValidationError> {
    if !value.is_finite() {
        return Err(WorldValidationError::NonFinite { field });
    }
    if !(0.0..=1.0).contains(&value) {
        return Err(WorldValidationError::OutOfRange {
            field,
            minimum: 0.0,
            maximum: 1.0,
        });
    }
    Ok(value)
}

/// Clamp a raw confidence into [0, 1] (never trust callers to pre-clamp).
#[must_use]
pub(crate) fn clamp_unit(value: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 1.0)
}

/// Deduplicate while preserving order; errors when duplicates are forbidden.
pub(crate) fn dedupe<T: PartialEq + Clone>(
    items: Vec<T>,
    field: &'static str,
    allow_duplicates: bool,
) -> Result<Vec<T>, WorldValidationError> {
    let mut seen = Vec::new();
    for item in items {
        if seen.contains(&item) {
            if allow_duplicates {
                continue;
            }
            return Err(WorldValidationError::DuplicateItem { field });
        }
        seen.push(item);
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_values_clamp_and_reject_out_of_range() {
        assert_eq!(clamp_unit(1.5), 1.0);
        assert_eq!(clamp_unit(-0.2), 0.0);
        assert_eq!(clamp_unit(f32::NAN), 0.0);
        assert_eq!(validate_unit(0.73, "x").expect("in range"), 0.73);
        assert!(validate_unit(1.1, "x").is_err());
        assert!(validate_unit(f32::INFINITY, "x").is_err());
    }

    #[test]
    fn text_is_bounded_and_nul_rejected() {
        assert!(validate_text("ok", "f").is_ok());
        assert!(validate_text("", "f").is_err());
        assert!(validate_text("  ", "f").is_err());
        assert!(validate_text("a\0b", "f").is_err());
        assert_eq!(
            validate_text("x".repeat(MAX_WORLD_TEXT_BYTES + 1), "f").unwrap_err(),
            WorldValidationError::TooLong {
                field: "f",
                length: MAX_WORLD_TEXT_BYTES + 1,
                maximum: MAX_WORLD_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn dedupe_forbids_and_allows() {
        assert!(dedupe(vec![1, 1], "x", false).is_err());
        assert_eq!(dedupe(vec![1, 1, 2], "x", true).expect("allowed"), vec![1, 2]);
    }
}
