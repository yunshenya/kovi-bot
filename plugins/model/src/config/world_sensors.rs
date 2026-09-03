use serde::{Deserialize, Serialize};

/// World-sensor framework config. Additive and disabled by default so it never
/// disturbs the normal chat deployment. When enabled, a bounded background
/// scheduler polls each configured sensor and, on a meaningful state change,
/// feeds a durable world fact (and, when watched, a surfaceable open loop) into
/// the core via `observe_world_fact`.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct WorldSensorsConfig {
    enabled: bool,
    check_interval_secs: u64,
    max_sensors: usize,
    max_result_chars: usize,
    cooldown_secs: u64,
    sensors: Vec<WorldSensorConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct WorldSensorConfig {
    /// Stable name, e.g. "ci:main" — used in the world fact and as a dedupe key.
    name: String,
    /// Sensor kind; only `url_status` is built in for now.
    kind: String,
    /// Target, e.g. a URL (for url_status).
    target: String,
    /// For url_status: the expected HTTP status code that counts as "success".
    expected_status: u16,
    /// For url_status: run the fetch with this many seconds timeout.
    timeout_secs: u64,
    /// Should the core be able to surface this proactively (watched open loop)?
    watch: bool,
    /// Importance fed into the world-fact memory.
    importance: u8,
}

impl Default for WorldSensorsConfig {
    fn default() -> Self {
        Self {
            // On by default so a fresh deploy gets world awareness at no cost:
            // an empty sensor list is a harmless no-op scheduler.
            enabled: true,
            check_interval_secs: 300,
            max_sensors: 16,
            max_result_chars: 500,
            cooldown_secs: 600,
            sensors: Vec::new(),
        }
    }
}

impl Default for WorldSensorConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: "url_status".to_owned(),
            target: String::new(),
            expected_status: 200,
            timeout_secs: 10,
            watch: true,
            importance: 60,
        }
    }
}

impl WorldSensorsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.check_interval_secs >= 5,
            "world_sensors.check_interval_secs 必须 ≥5"
        );
        anyhow::ensure!(
            self.max_sensors >= 1 && self.max_sensors <= 128,
            "world_sensors.max_sensors 必须在 1..=128"
        );
        anyhow::ensure!(
            self.max_result_chars >= 64 && self.max_result_chars <= 4096,
            "world_sensors.max_result_chars 必须在 64..=4096"
        );
        anyhow::ensure!(
            self.cooldown_secs >= 30,
            "world_sensors.cooldown_secs 必须 ≥30"
        );
        anyhow::ensure!(
            self.sensors.len() <= self.max_sensors,
            "world_sensors.sensors 数量超过 max_sensors"
        );
        for sensor in &self.sensors {
            sensor.validate()?;
        }
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn check_interval_secs(&self) -> u64 {
        self.check_interval_secs
    }

    pub fn max_sensors(&self) -> usize {
        self.max_sensors
    }

    pub fn max_result_chars(&self) -> usize {
        self.max_result_chars
    }

    pub fn cooldown_secs(&self) -> u64 {
        self.cooldown_secs
    }

    pub fn sensors(&self) -> &[WorldSensorConfig] {
        &self.sensors
    }
}

impl WorldSensorConfig {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.name.trim().is_empty() && self.name.chars().count() <= 64,
            "world_sensors.sensor.name 必须非空且 ≤64 字符"
        );
        anyhow::ensure!(
            self.kind == "url_status",
            "world_sensors.sensor.kind 目前仅支持 url_status"
        );
        anyhow::ensure!(
            !self.target.trim().is_empty() && self.target.chars().count() <= 512,
            "world_sensors.sensor.target 必须非空且 ≤512 字符"
        );
        anyhow::ensure!(
            (100..=599).contains(&self.expected_status),
            "world_sensors.sensor.expected_status 必须在 100..=599"
        );
        anyhow::ensure!(
            (1..=60).contains(&self.timeout_secs),
            "world_sensors.sensor.timeout_secs 必须在 1..=60"
        );
        anyhow::ensure!(
            (0..=100).contains(&self.importance),
            "world_sensors.sensor.importance 必须在 0..=100"
        );
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn expected_status(&self) -> u16 {
        self.expected_status
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    pub fn watch(&self) -> bool {
        self.watch
    }

    pub fn importance(&self) -> u8 {
        self.importance
    }
}
