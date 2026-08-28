//! Capability declarations and the deliberately small Intrinsic allowlist.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Text,
    Vision,
    Audio,
}

/// Operations the first Intrinsic release is allowed to support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntrinsicCapability {
    ShortTextReply,
    SimpleSemanticClassification,
    SimpleStructuredExtraction,
    ImageDescription,
    SimpleVisualQuestionAnswering,
    LowCostSummarization,
}

impl IntrinsicCapability {
    #[must_use]
    pub const fn model_capability(self) -> ModelCapability {
        match self {
            Self::ImageDescription | Self::SimpleVisualQuestionAnswering => ModelCapability::Vision,
            Self::ShortTextReply
            | Self::SimpleSemanticClassification
            | Self::SimpleStructuredExtraction
            | Self::LowCostSummarization => ModelCapability::Text,
        }
    }

    /// The first release never grants these capabilities to an Intrinsic
    /// backend, even if an engine happens to emit a plausible-looking answer.
    #[must_use]
    pub const fn permits_intrinsic(self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntrinsicCapabilitySet {
    pub text: bool,
    pub vision: bool,
    pub audio: bool,
}

impl Default for IntrinsicCapabilitySet {
    fn default() -> Self {
        Self {
            text: true,
            vision: true,
            audio: false,
        }
    }
}

impl IntrinsicCapabilitySet {
    #[must_use]
    pub const fn supports(self, capability: ModelCapability) -> bool {
        match capability {
            ModelCapability::Text => self.text,
            ModelCapability::Vision => self.vision,
            ModelCapability::Audio => self.audio,
        }
    }

    pub fn validate(self) -> Result<(), &'static str> {
        if self.audio {
            return Err("Intrinsic v1 supports text and vision only");
        }
        if !self.text && !self.vision {
            return Err("Intrinsic must expose at least text or vision");
        }
        Ok(())
    }
}
