//! Platform-neutral identity configuration.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct IdentityConfig {
    /// Canonical Yunxi owner. External platform identities are only a host
    /// routing detail and are used as a fallback while this remains unset.
    owner_person_id: Option<Uuid>,
}

impl IdentityConfig {
    #[must_use]
    pub const fn owner_person_id(&self) -> Option<Uuid> {
        self.owner_person_id
    }
}

#[cfg(test)]
mod tests {
    use super::IdentityConfig;
    use uuid::Uuid;

    #[test]
    fn owner_person_id_is_optional_and_round_trips() {
        let owner = Uuid::new_v4();
        let config = IdentityConfig {
            owner_person_id: Some(owner),
        };
        let encoded = kovi::toml::to_string(&config).expect("identity config should serialize");
        let decoded: IdentityConfig =
            kovi::toml::from_str(&encoded).expect("identity config parses");
        assert_eq!(decoded.owner_person_id(), Some(owner));
        assert_eq!(IdentityConfig::default().owner_person_id(), None);
    }
}
