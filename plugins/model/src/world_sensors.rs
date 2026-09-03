//! Bounded world-sensor framework.
//!
//! When `[world_sensors].enabled` is set, a background scheduler polls each
//! configured sensor and, on a meaningful state change, feeds a durable world
//! fact (and, if watched, a surfaceable open loop) into the core via
//! [`crate::yunxi::observe_world_fact`]. This is how the bot learns durable
//! facts about your real world (a build/CI status, a URL becoming ready) rather
//! than only from chat. It is additive and off by default.

use crate::config;
use crate::model::tool_access::fetch_public_http_response;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SensorState {
    last_ok: Option<bool>,
    last_change: Option<std::time::SystemTime>,
}

static SENSOR_STATES: LazyLock<Mutex<HashMap<String, SensorState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Start the world-sensor scheduler (config-gated). No-op when disabled.
pub(crate) async fn start_scheduler(_bot: Arc<kovi::RuntimeBot>) {
    let config = config::get().world_sensors().clone();
    if !config.enabled() {
        println!("[INFO] World 传感器已关闭");
        return;
    }
    println!(
        "[INFO] World 传感器已启动（{} 个，间隔 {} 秒）",
        config.sensors().len(),
        config.check_interval_secs()
    );
    loop {
        if let Err(error) = poll_all().await {
            eprintln!("[ERROR] World 传感器轮询失败: {error}");
        }
        kovi::tokio::time::sleep(Duration::from_secs(config.check_interval_secs())).await;
    }
}

async fn poll_all() -> anyhow::Result<()> {
    let config = config::get().world_sensors().clone();
    let cooldown = Duration::from_secs(config.cooldown_secs());
    for sensor in config.sensors() {
        // url_status is the only built-in kind.
        ok_or_skip(sensor, cooldown).await?;
    }
    Ok(())
}

fn sensor_state(name: &str) -> Option<SensorState> {
    SENSOR_STATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(name)
        .copied()
}

fn set_sensor_state(name: &str, state: SensorState) {
    let mut states = SENSOR_STATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Bound the live set so an ever-changing sensor list cannot grow unbounded.
    if !states.contains_key(name) && states.len() >= config::get().world_sensors().max_sensors() {
        return;
    }
    states.insert(name.to_owned(), state);
}

async fn ok_or_skip(sensor: &config::WorldSensorConfig, cooldown: Duration) -> anyhow::Result<()> {
    let state = sensor_state(sensor.name()).unwrap_or_default();
    // Cooldown gates re-fires after a state change.
    if let Some(last_change) = state.last_change
        && last_change.elapsed().unwrap_or(Duration::MAX) < cooldown
    {
        return Ok(());
    }
    // url_status: fetch and compare status against the expected value.
    let response = fetch_public_http_response(
        sensor.target(),
        config::get().world_sensors().max_result_chars(),
        Duration::from_secs(sensor.timeout_secs()),
    )
    .await;
    let ok = matches!(&response, Ok(r) if r.status == sensor.expected_status());
    // Only feed the core on a state transition, not every poll.
    if state.last_ok != Some(ok) {
        let summary = format!(
            "{} 现在{}",
            sensor.name(),
            if ok {
                "达到预期状态"
            } else {
                "未达预期"
            }
        );
        let scope = yunxi_core::MemoryScope::Global;
        if let Err(error) = crate::yunxi::observe_world_fact(
            scope,
            &summary,
            sensor.importance(),
            sensor.watch(),
            Some(sensor.name()),
        )
        .await
        {
            eprintln!(
                "[WARN] World 传感器回喂核心失败 ({}): {error}",
                sensor.name()
            );
        } else {
            set_sensor_state(
                sensor.name(),
                SensorState {
                    last_ok: Some(ok),
                    last_change: Some(std::time::SystemTime::now()),
                },
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorldSensorsConfig;

    #[test]
    fn sensor_config_is_bounded_and_validated() {
        let ok_config: WorldSensorsConfig = kovi::toml::from_str(
            r#"
            enabled = true
            check_interval_secs = 300
            max_sensors = 16
            max_result_chars = 500
            cooldown_secs = 600
            sensors = [
                { name = "ci:main", kind = "url_status", target = "https://example.com/health", expected_status = 200, timeout_secs = 10, watch = true, importance = 60 },
            ]
            "#,
        )
        .expect("valid world-sensor config");
        assert!(ok_config.validate().is_ok());
        // A bad kind is rejected.
        let bad: WorldSensorsConfig = kovi::toml::from_str(
            r#"
            enabled = true
            check_interval_secs = 300
            max_sensors = 16
            max_result_chars = 500
            cooldown_secs = 600
            sensors = [
                { name = "x", kind = "shell", target = "x", expected_status = 200, timeout_secs = 10, watch = true, importance = 60 },
            ]
            "#,
        )
        .expect("deserializes");
        assert!(bad.validate().is_err());
    }

    #[test]
    fn sensor_state_transition_fires_only_on_change() {
        // Reset the registry for a deterministic test.
        SENSOR_STATES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        assert_eq!(sensor_state("t"), None);
        set_sensor_state(
            "t",
            SensorState {
                last_ok: Some(true),
                last_change: Some(std::time::SystemTime::now()),
            },
        );
        assert_eq!(
            sensor_state("t").map(|state| state.last_ok),
            Some(Some(true))
        );
    }
}
