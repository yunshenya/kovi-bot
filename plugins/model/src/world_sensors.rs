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

/// 传感器输出的保留上限。它只被用来做一次 `contains` 与 160 字的日志，1 MiB 已经
/// 远远够用；**超限之后仍然继续读**（只是不再保留），否则子进程会再次写满管道。
const MAX_SENSOR_OUTPUT_BYTES: usize = 1024 * 1024;

/// 读干一个管道，最多保留 `cap` 字节。
///
/// 两点都不能省：一是必须把管道读到 EOF，否则子进程写满缓冲区就卡在 `write()`；
/// 二是用 lossy 解码——`read_to_string` 碰到非 UTF-8 会整体失败并留下空串，于是
/// "命令成功、只是输出里带了二进制字节"会被判成"输出不匹配"，一条假的未达预期。
fn drain_bounded(reader: impl std::io::Read, cap: usize) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    let mut reader = reader;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if kept.len() < cap {
                    let room = cap - kept.len();
                    kept.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Spawn `sh -c <command>`, capture stdout+stderr, and kill it once `timeout`
/// elapses so a stuck check cannot wedge the scheduler.
async fn run_bounded_command(command: &str, timeout: Duration) -> anyhow::Result<(i32, String)> {
    let command = command.to_owned();
    kovi::tokio::task::spawn_blocking(move || {
        // 不再需要 `use std::io::Read`：读取挪进 `drain_bounded`，而它接收的是
        // `impl Read`，trait 方法随类型参数一起可见。
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| anyhow::anyhow!("无法启动命令: {error}"))?;
        // 两个管道必须在等待的同时**并发**排空。只在 `try_wait()` 报退出之后才去读
        // 是不行的：子进程写满管道缓冲区（Linux 64 KiB）就会阻塞在 `write()`，永远
        // 不会退出，于是无论命令多快都会走到超时分支——这类传感器根本不可能成功。
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdout 管道缺失"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("stderr 管道缺失"))?;
        let stdout_reader =
            std::thread::spawn(move || drain_bounded(stdout, MAX_SENSOR_OUTPUT_BYTES));
        let stderr_reader =
            std::thread::spawn(move || drain_bounded(stderr, MAX_SENSOR_OUTPUT_BYTES));

        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| anyhow::anyhow!("等待命令进程失败: {error}"))?
            {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                // 进程结束后管道关闭，两个读线程会自然返回。
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(anyhow::anyhow!("命令超时（>{}s）", timeout.as_secs()));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let mut output = stdout_reader.join().unwrap_or_default();
        output.push_str(&stderr_reader.join().unwrap_or_default());
        Ok((status.code().unwrap_or(-1), output))
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
            (
                matches!(&response, Ok(r) if r.status == sensor.expected_status()),
                None,
            )
        }
    };
    // A file_state sensor also fires when the file's mtime changes while it
    // remains in the expected state ("文件变更").
    let file_changed =
        sensor.kind() == "file_state" && state.last_mtime != mtime && mtime.is_some();
    let should_write = should_feed_core(state.last_ok, ok, file_changed);
    if should_write {
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
    // 无论这次写没写，都要把观测记下来：基线不建立，"首次观测"就会每轮重现。
    // `last_change` 只在真的写的时候推进，保留它原本的语义（写完之后静默一个 cooldown）。
    set_sensor_state(
        sensor.name(),
        SensorState {
            last_ok: Some(ok),
            last_change: if should_write {
                Some(std::time::SystemTime::now())
            } else {
                state.last_change
            },
            last_mtime: mtime,
        },
    );
    Ok(())
}

/// 这次观测该不该写进记忆。
///
/// - 启动后的**第一次**观测只当基线：正常就静默（否则每次重启都留一条
///   "一切正常"，而传感器状态是进程内的，重启风暴会把它刷成几百条重复记忆）；
///   异常必须写出来——"刚起来就发现服务没在跑"是有用的信息。
/// - 之后只在状态真的翻转、或 file_state 的文件在正常态下发生变化时才写。
///
/// 已知取舍：首次观测不看 `file_changed`。mtime 没有持久化，进程重启后第一次
/// 读到文件时它必然"和上次不同"，照写就还是每次重启一条。代价是"机器人离线期间
/// 文件变过"这一次不会被报出来——宁可漏一次，也不要重启风暴刷屏。
fn should_feed_core(previous: Option<bool>, ok: bool, file_changed: bool) -> bool {
    match previous {
        None => !ok,
        Some(previous) => previous != ok || (file_changed && ok),
    }
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
    fn first_observation_is_a_baseline_unless_it_is_bad_news() {
        // 启动后第一次观测：正常 → 静默建基线（重启不该留下"一切正常"的重复记忆）。
        assert!(!should_feed_core(None, true, false));
        // 但"刚起来就发现服务没跑"必须写出来。
        assert!(should_feed_core(None, false, false));
        // 首次观测即使 file_changed 也不写：mtime 没持久化，重启后它必然为真。
        assert!(!should_feed_core(None, true, true));
    }

    #[test]
    fn later_observations_only_fire_on_a_real_transition() {
        assert!(!should_feed_core(Some(true), true, false));
        assert!(!should_feed_core(Some(false), false, false));
        assert!(should_feed_core(Some(true), false, false));
        assert!(should_feed_core(Some(false), true, false));
        // 异常态下的文件变化不算"变化"，避免噪声。
        assert!(!should_feed_core(Some(false), false, true));
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

    /// 输出超过管道缓冲区（Linux 64 KiB / macOS 16 KiB）的命令必须能正常跑完。
    ///
    /// 修前这里必然失败：两个管道只在 `try_wait()` 报退出之后才被读，子进程写满
    /// 缓冲区就卡在 `write()`、永远不退出，于是无论命令多快都会走到超时分支——
    /// 这类传感器根本不可能成功。用 `seq` 产生约 240 KB 输出，稳稳超过两个平台的
    /// 缓冲区。超时给 10 秒，真出问题也不会把测试拖太久。
    #[test]
    fn command_sensor_survives_output_larger_than_the_pipe_buffer() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let (exit, output) = run_bounded_command("seq 1 40000", Duration::from_secs(10))
                    .await
                    .expect("大输出不应被判成超时");
                assert_eq!(exit, 0);
                assert!(output.len() > 64 * 1024, "确实产出了超过管道缓冲区的输出");
                assert!(output.contains("40000"), "末尾内容也要读到");
            });
    }

    /// 非 UTF-8 输出不能让整段读取失败。
    ///
    /// 修前用的是 `read_to_string`：它碰到非法字节会整体报错并留下空串，于是
    /// "命令成功、只是输出里带了二进制"会被判成"输出不匹配"，一条假的未达预期。
    #[test]
    fn command_sensor_keeps_non_utf8_output() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let (exit, output) = run_bounded_command(r"printf 'ÿþOK'", Duration::from_secs(10))
                    .await
                    .expect("应能执行");
                assert_eq!(exit, 0);
                assert!(
                    output.contains("OK"),
                    "非法字节不该把后面的可读内容一起丢掉: {output:?}"
                );
            });
    }
}
