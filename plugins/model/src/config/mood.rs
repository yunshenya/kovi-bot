use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct MoodConfig {
    cache_ttl_secs: u64,
    cache_retention_secs: u64,
    natural_drift_after_secs: u64,
    natural_drift_check_secs: u64,
}

impl MoodConfig {
    pub fn cache_ttl_secs(&self) -> u64 {
        self.cache_ttl_secs
    }

    pub fn cache_retention_secs(&self) -> u64 {
        self.cache_retention_secs
    }

    pub fn natural_drift_after_secs(&self) -> u64 {
        self.natural_drift_after_secs
    }

    pub fn natural_drift_check_secs(&self) -> u64 {
        self.natural_drift_check_secs
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.cache_ttl_secs == 0
            || self.cache_retention_secs == 0
            || self.natural_drift_after_secs == 0
            || self.natural_drift_check_secs == 0
        {
            return Err(anyhow::anyhow!("mood 配置中的时间必须大于 0"));
        }
        if self.cache_retention_secs < self.cache_ttl_secs {
            return Err(anyhow::anyhow!(
                "mood.cache_retention_secs 不能小于 cache_ttl_secs"
            ));
        }
        Ok(())
    }
}

impl Default for MoodConfig {
    fn default() -> Self {
        Self {
            cache_ttl_secs: 300,
            cache_retention_secs: 3600,
            natural_drift_after_secs: 7200,
            natural_drift_check_secs: 1800,
        }
    }
}
