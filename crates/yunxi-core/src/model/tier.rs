//! Cognitive capability levels used by the bounded model stack.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A capability level is deliberately independent from a provider name.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CognitiveTier {
    /// Rust-only deterministic behavior. No generative quality is promised.
    Reflex,
    /// The model shipped with Yunxi, when healthy enough for the request.
    #[default]
    Intrinsic,
    /// A host-provided normal model.
    Standard,
    /// A host-provided high-quality or high-cost model.
    Enhanced,
}

impl CognitiveTier {
    #[must_use]
    pub const fn is_generative(self) -> bool {
        !matches!(self, Self::Reflex)
    }

    #[must_use]
    pub const fn is_strong(self) -> bool {
        matches!(self, Self::Standard | Self::Enhanced)
    }

    #[must_use]
    pub const fn fallback(self) -> Option<Self> {
        match self {
            Self::Enhanced => Some(Self::Standard),
            Self::Standard => Some(Self::Intrinsic),
            Self::Intrinsic => Some(Self::Reflex),
            Self::Reflex => None,
        }
    }

    #[must_use]
    pub const fn at_least(self, other: Self) -> bool {
        (self as u8) >= (other as u8)
    }
}

impl fmt::Display for CognitiveTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Reflex => "reflex",
            Self::Intrinsic => "intrinsic",
            Self::Standard => "standard",
            Self::Enhanced => "enhanced",
        })
    }
}
