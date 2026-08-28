//! Minimal model health state. It is intentionally not a full circuit breaker.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelHealth {
    Loading,
    Healthy,
    Degraded,
    #[default]
    Unavailable,
}

impl ModelHealth {
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }

    #[must_use]
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Healthy)
    }

    #[must_use]
    pub const fn can_serve(self) -> bool {
        self.is_available()
    }
}
