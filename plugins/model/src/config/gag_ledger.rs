use serde::{Deserialize, Serialize};

/// Gag-ledger (梗账本) config: structured "promises / running gags / grudges"
/// the bot owes or holds. Additive and bounded by default.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct GagLedgerConfig {
    enabled: bool,
    max_entries_per_scope: usize,
    max_global_entries: usize,
    entry_ttl_days: u64,
}

impl Default for GagLedgerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries_per_scope: 32,
            max_global_entries: 128,
            entry_ttl_days: 180,
        }
    }
}

impl GagLedgerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=512).contains(&self.max_entries_per_scope),
            "gag_ledger.max_entries_per_scope 必须在 1..=512"
        );
        anyhow::ensure!(
            (1..=4096).contains(&self.max_global_entries),
            "gag_ledger.max_global_entries 必须在 1..=4096"
        );
        anyhow::ensure!(
            (1..=3650).contains(&self.entry_ttl_days),
            "gag_ledger.entry_ttl_days 必须在 1..=3650"
        );
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn max_entries_per_scope(&self) -> usize {
        self.max_entries_per_scope
    }

    pub fn max_global_entries(&self) -> usize {
        self.max_global_entries
    }

    pub fn entry_ttl_days(&self) -> u64 {
        self.entry_ttl_days
    }
}

#[cfg(test)]
mod tests {
    use super::GagLedgerConfig;

    #[test]
    fn defaults_are_bounded_and_valid() {
        let config = GagLedgerConfig::default();
        assert!(config.enabled());
        assert!(config.validate().is_ok());
        assert_eq!(config.max_entries_per_scope(), 32);
    }

    #[test]
    fn toml_parses_and_out_of_bounds_is_rejected() {
        let config: GagLedgerConfig = kovi::toml::from_str(
            r#"
            enabled = true
            max_entries_per_scope = 16
            max_global_entries = 64
            entry_ttl_days = 90
            "#,
        )
        .expect("valid gag ledger config");
        assert!(config.validate().is_ok());
        assert_eq!(config.max_entries_per_scope(), 16);

        let bad: GagLedgerConfig = kovi::toml::from_str(
            r#"
            enabled = true
            max_entries_per_scope = 0
            "#,
        )
        .expect("deserializes");
        assert!(bad.validate().is_err());
    }
}
