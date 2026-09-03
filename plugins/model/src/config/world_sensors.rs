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
    /// Explicitly configured sensors; when omitted, the built-in
    /// `bot:service` command sensor applies (see `default_sensors`).
    #[serde(default = "default_sensors")]
    sensors: Vec<WorldSensorConfig>,
}

/// The built-in default sensor: checks that this bot's own systemd unit is
/// active. When a deployer configures `[[world_sensors.sensors]]` explicitly,
/// that list replaces this default.
fn default_sensors() -> Vec<WorldSensorConfig> {
    vec![WorldSensorConfig {
        name: "bot:service".to_owned(),
        kind: "command".to_owned(),
        target: String::new(),
        expected_status: 200,
        timeout_secs: 10,
        command: "systemctl is-active kovi-bot".to_owned(),
        expected_exit: 0,
        expected_output: String::new(),
        watch: true,
        importance: 70,
    }]
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct WorldSensorConfig {
    /// Stable name, e.g. "ci:main" — used in the world fact and as a dedupe key.
    name: String,
    /// Sensor kind: `url_status` (fetch a URL and compare status) or `command`
    /// (run a bounded shell command; ok = exit code matches and, when set, the
    /// output contains `expected_output`).
    kind: String,
    /// Target, e.g. a URL (for url_status). Ignored by `command`.
    target: String,
    /// For url_status: the expected HTTP status code that counts as "success".
    expected_status: u16,
    /// Bounded runtime in seconds for a url fetch or a command execution.
    timeout_secs: u64,
    /// For `command`: the shell command to run (under `sh -c`).
    command: String,
    /// For `command`: the exit code that counts as success (default 0).
    expected_exit: i32,
    /// For `command`: if non-empty, the command output must contain this text.
    expected_output: String,
    /// Should the core be able to surface this proactively (watched open loop)?
    watch: bool,
    /// Importance fed into the world-fact memory.
    importance: u8,
}

impl Default for WorldSensorsConfig {
    fn default() -> Self {
        Self {
            // On by default so a fresh deploy gets world awareness at no cost.
            // The default sensor checks this bot's own service health; an
            // explicitly configured sensor list replaces it.
            enabled: true,
            check_interval_secs: 300,
            max_sensors: 16,
            max_result_chars: 500,
            cooldown_secs: 600,
            sensors: default_sensors(),
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
            command: String::new(),
            expected_exit: 0,
            expected_output: String::new(),
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
            (0..=100).contains(&self.importance),
            "world_sensors.sensor.importance 必须在 0..=100"
        );
        anyhow::ensure!(
            (1..=60).contains(&self.timeout_secs),
            "world_sensors.sensor.timeout_secs 必须在 1..=60"
        );
        match self.kind.as_str() {
            "url_status" => {
                anyhow::ensure!(
                    !self.target.trim().is_empty() && self.target.chars().count() <= 512,
                    "world_sensors.sensor.target 必须非空且 ≤512 字符"
                );
                anyhow::ensure!(
                    (100..=599).contains(&self.expected_status),
                    "world_sensors.sensor.expected_status 必须在 100..=599"
                );
            }
            "command" => {
                anyhow::ensure!(
                    !self.command.trim().is_empty() && self.command.chars().count() <= 2048,
                    "world_sensors.sensor.command 必须非空且 ≤2048 字符"
                );
                anyhow::ensure!(
                    (0..=255).contains(&self.expected_exit),
                    "world_sensors.sensor.expected_exit 必须在 0..=255"
                );
                anyhow::ensure!(
                    self.expected_output.chars().count() <= 512,
                    "world_sensors.sensor.expected_output 必须 ≤512 字符"
                );
            }
            _ => {
                anyhow::bail!(
                    "world_sensors.sensor.kind 仅支持 url_status 或 command（当前: {}）",
                    self.kind
                )
            }
        }
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

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn expected_exit(&self) -> i32 {
        self.expected_exit
    }

    pub fn expected_output(&self) -> &str {
        &self.expected_output
    }

    pub fn watch(&self) -> bool {
        self.watch
    }

    pub fn importance(&self) -> u8 {
        self.importance
    }
}
