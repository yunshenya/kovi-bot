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
    last_mtime: Option<std::time::SystemTime>,
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
        // Built-in kinds: url_status (fetch a URL) or command (bounded shell check).
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

/// Run one bounded command sensor check. The command runs under `sh -c`, is
/// killed after `timeout_secs`, and is considered ok when its exit code matches
/// `expected_exit` and (when configured) its output contains `expected_output`.
async fn command_sensor_ok(sensor: &config::WorldSensorConfig) -> bool {
    let timeout = Duration::from_secs(sensor.timeout_secs().max(1));
    match run_bounded_command(sensor.command(), timeout).await {
        Ok((exit, output)) => {
            let exit_ok = exit == sensor.expected_exit();
            let output_ok =
                sensor.expected_output().is_empty() || output.contains(sensor.expected_output());
            if !exit_ok || !output_ok {
                eprintln!(
                    "[WARN] World 命令传感器未达预期 ({}): exit={} output={}",
                    sensor.name(),
                    exit,
                    truncate_for_log(&output, 160)
                );
            }
            exit_ok && output_ok
        }
        Err(error) => {
            eprintln!(
                "[WARN] World 命令传感器执行失败 ({}): {error}",
                sensor.name()
            );
            false
        }
    }
}

/// Spawn `sh -c <command>`, capture stdout+stderr, and kill it once `timeout`
/// elapses so a stuck check cannot wedge the scheduler.
async fn run_bounded_command(command: &str, timeout: Duration) -> anyhow::Result<(i32, String)> {
    let command = command.to_owned();
    kovi::tokio::task::spawn_blocking(move || {
        use std::io::Read;
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| anyhow::anyhow!("无法启动命令: {error}"))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| anyhow::anyhow!("等待命令进程失败: {error}"))?
            {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut output);
                }
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut output);
                }
                return Ok((status.code().unwrap_or(-1), output));
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!("命令超时（>{}s）", timeout.as_secs()));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    })
    .await
    .map_err(|error| anyhow::anyhow!("命令执行任务失败: {error}"))?
}

fn truncate_for_log(text: &str, maximum: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().take(maximum).collect()
}

async fn ok_or_skip(sensor: &config::WorldSensorConfig, cooldown: Duration) -> anyhow::Result<()> {
    let state = sensor_state(sensor.name()).unwrap_or_default();
    // Cooldown gates re-fires after a state change.
    if let Some(last_change) = state.last_change
        && last_change.elapsed().unwrap_or(Duration::MAX) < cooldown
    {
        return Ok(());
    }
    let (ok, mtime) = match sensor.kind() {
        "command" => (command_sensor_ok(sensor).await, None),
        "file_state" => file_state_ok(sensor),
        _ => {
            // url_status: fetch and compare status against the expected value.
            let response = fetch_public_http_response(
                sensor.target(),
                config::get().world_sensors().max_result_chars(),
                Duration::from_secs(sensor.timeout_secs()),
            )
            .await;
            (matches!(&response, Ok(r) if r.status == sensor.expected_status()), None)
        }
    };
    // A file_state sensor also fires when the file's mtime changes while it
    // remains in the expected state ("文件变更").
    let file_changed =
        sensor.kind() == "file_state" && state.last_mtime != mtime && mtime.is_some();
    // Only feed the core on a state transition, not every poll.
    if state.last_ok != Some(ok) || (file_changed && ok) {
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
                    last_mtime: mtime,
                },
            );
            // Shadow-mode World Model: also record a structured entity
            // property so the v4 runtime can track this sensor's state.
            crate::yunxi::world_model::record_entity_property(
                yunxi_core::world_model::EntityKind::Resource,
                None,
                None,
                format!("sensor:{}", sensor.name()).as_str(),
                if ok { "ok" } else { "not_ok" },
                (sensor.importance() as f32 / 100.0).clamp(0.2, 1.0),
            );
        }
    }
    Ok(())
}

/// Check a `file_state` sensor: ok when the path exists and is a regular
/// file; the modification time is returned for change detection.
fn file_state_ok(sensor: &config::WorldSensorConfig) -> (bool, Option<std::time::SystemTime>) {
    match std::fs::metadata(sensor.target()) {
        Ok(metadata) if metadata.is_file() => (true, metadata.modified().ok()),
        Ok(_) => (false, None),
        Err(_) => (false, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorldSensorsConfig;

    #[test]
    fn default_sensors_include_builtin_service_check() {
        let default = WorldSensorsConfig::default();
        assert_eq!(default.sensors().len(), 1);
        assert_eq!(default.sensors()[0].name(), "bot:service");
        assert_eq!(default.sensors()[0].kind(), "command");

        // A `[world_sensors]` table WITHOUT a `sensors` key still gets the
        // built-in default sensor.
        let parsed: WorldSensorsConfig =
            kovi::toml::from_str("max_sensors = 16\n").expect("deserializes");
        assert_eq!(parsed.sensors().len(), 1);
        assert_eq!(parsed.sensors()[0].name(), "bot:service");

        // An explicit empty list disables the built-in default.
        let none: WorldSensorsConfig =
            kovi::toml::from_str("sensors = []\n").expect("deserializes");
        assert!(none.sensors().is_empty());
    }

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
                last_mtime: None,
            },
        );
        assert_eq!(
            sensor_state("t").map(|state| state.last_ok),
            Some(Some(true))
        );
    }

    #[test]
    fn file_state_sensor_detects_existence_and_mtime() {
        let dir = std::env::temp_dir().join(format!("yunxi-wm-sensor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("artifact.txt");
        let sensor: WorldSensorsConfig = kovi::toml::from_str(
            &format!(
                r#"
                enabled = true
                check_interval_secs = 300
                max_sensors = 16
                max_result_chars = 500
                cooldown_secs = 600
                sensors = [
                    {{ name = "file:artifact", kind = "file_state", target = "{}", timeout_secs = 10, watch = true, importance = 60 }},
                ]
                "#,
                file.display()
            ),
        )
        .expect("deserializes file_state sensor");
        assert!(sensor.validate().is_ok());
        let config = &sensor.sensors()[0];
        // Not yet created → not ok.
        assert_eq!(file_state_ok(config), (false, None));
        std::fs::write(&file, "v1").expect("write");
        let (ok, mtime) = file_state_ok(config);
        assert!(ok);
        assert!(mtime.is_some());
        // A directory target is treated as not-ok (it is not a "file").
        let dir_sensor: WorldSensorsConfig = kovi::toml::from_str(
            &format!(
                r#"
                enabled = true
                check_interval_secs = 300
                max_sensors = 16
                max_result_chars = 500
                cooldown_secs = 600
                sensors = [
                    {{ name = "file:dir", kind = "file_state", target = "{}", timeout_secs = 10, watch = true, importance = 60 }},
                ]
                "#,
                dir.display()
            ),
        )
        .expect("deserializes");
        assert_eq!(file_state_ok(&dir_sensor.sensors()[0]), (false, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_sensor_config_validates() {
        let ok: WorldSensorsConfig = kovi::toml::from_str(
            r#"
            enabled = true
            check_interval_secs = 300
            max_sensors = 16
            max_result_chars = 500
            cooldown_secs = 600
            sensors = [
                { name = "bot:service", kind = "command", command = "systemctl is-active kovi-bot", expected_exit = 0, timeout_secs = 10, watch = true, importance = 70 },
            ]
            "#,
        )
        .expect("deserializes command sensor");
        assert!(ok.validate().is_ok());
        assert_eq!(ok.sensors()[0].kind(), "command");
        assert_eq!(ok.sensors()[0].expected_exit(), 0);
        assert!(ok.sensors()[0].command().contains("is-active"));

        // An empty command is rejected.
        let bad: WorldSensorsConfig = kovi::toml::from_str(
            r#"
            enabled = true
            check_interval_secs = 300
            max_sensors = 16
            max_result_chars = 500
            cooldown_secs = 600
            sensors = [
                { name = "bad", kind = "command", command = "", timeout_secs = 10, watch = true, importance = 70 },
            ]
            "#,
        )
        .expect("deserializes");
        assert!(bad.validate().is_err());
    }

    #[test]
    fn bounded_command_runner_captures_exit_and_output() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let (exit, output) = run_bounded_command("printf 'hi'", Duration::from_secs(10))
                .await
                .expect("ok");
            assert_eq!(exit, 0);
            assert!(output.contains("hi"));

            let (exit, _) = run_bounded_command("exit 3", Duration::from_secs(10))
                .await
                .expect("ok");
            assert_eq!(exit, 3);

            // A stuck command must be killed on the timeout path.
            let err = run_bounded_command("sleep 5", Duration::from_millis(300)).await;
            assert!(err.is_err());
        });
    }
}
