//! 概览页数据：进程、存储、模型与配置文件的现状。

use super::{AdminState, ApiError};
use crate::utils::{format_process_uptime, system_snapshot};
use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::SystemTime;

/// `GET /api/status`
pub(crate) async fn status(State(state): State<Arc<AdminState>>) -> Result<Json<Value>, ApiError> {
    let config = crate::config::get();
    let config_path = crate::config::config_file_path();
    let metadata = std::fs::metadata(&config_path).ok();

    let (database, redis) = kovi::tokio::join!(database_health(), redis_health());
    let counts = super::memory_api::counts().await;
    let waiting_room = waiting_room_report().await;

    let snapshot = kovi::tokio::task::spawn_blocking(system_snapshot)
        .await
        .unwrap_or_else(|error| crate::utils::SystemSnapshot {
            // 采样失败不该让整个概览打不开：如实写"获取失败"，页面照旧能看别的。
            host_uptime: "获取失败".to_string(),
            process_uptime_secs: None,
            process_memory: format!("芸汐进程内存: 获取失败 ({error})"),
        });
    let process_uptime = snapshot.process_uptime_secs.map(format_process_uptime);

    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "revision": std::env::var("KOVI_DEPLOY_REVISION").unwrap_or_else(|_| "未知".to_string()),
        "pid": std::process::id(),
        // `uptime` 是**机器开机时长**（历史字段名，保留给老前端）；
        // `process_uptime` 才是芸汐自己跑了多久——两者混用会让人以为刚发的版没生效。
        "uptime": snapshot.host_uptime,
        "host_uptime": snapshot.host_uptime,
        "process_uptime_secs": snapshot.process_uptime_secs,
        "process_uptime": process_uptime,
        "process": snapshot.process_memory,
        "database": database,
        "redis": redis,
        "counts": counts,
        "admin": {
            "uptime_secs": state.started_at.elapsed().as_secs(),
            "sessions": state.sessions.len(),
            // 已保存、但要重启才生效的分区。
            "pending_restart": state.pending_restart(),
        },
        // 等待房间 / 会话票据的实时状态（2026-09-15 那次"群静默十小时"之后加的）。
        // 卡住时页面要能自己说出来：哪个会话、排队几条、最老一条等了多久、
        // 排空任务还在不在、这个回合活了多久。
        "waiting_room": waiting_room,
        "config": {
            "path": config_path.display().to_string(),
            "bytes": metadata.as_ref().map(std::fs::Metadata::len),
            "modified": metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok())
                .map(format_time),
        },
        "model": {
            "enabled": config.server_config().enabled(),
            "model_name": config.server_config().model_name(),
            "endpoint": config.server_config().endpoint(),
            "api_key_env": config.server_config().api_key_env(),
            // 密钥可能来自配置（后台模型页托管）或环境变量：概览页那行"Token 环境变量"
            // 会误导人以为只能走环境变量，所以把来源一并给出去，措辞由 config 层统一。
            "has_key": config.server_config().resolved_api_key().is_some(),
            "api_key_source": match config.server_config().api_key_source() {
                crate::config::ApiKeySource::Config => "config",
                crate::config::ApiKeySource::Environment(_) => "environment",
                crate::config::ApiKeySource::Missing => "missing",
            },
            "key_source_text": config.server_config().api_key_source().describe(
                config.server_config().enabled(),
                config.server_config().requires_auth(),
            ),
            "intrinsic_enabled": config.model().intrinsic().enabled(),
            "turn_gate_mode": config.model().turn_gate().mode(),
        },
        "scheduler": {
            "proactive_enabled": config.proactive().enabled(),
            "group_interjection_enabled": config.group_interjection().enabled(),
            "tools_enabled": config.tools().enabled(),
            "vision_provider": config.vision().provider(),
            "qq_call_enabled": config.qq_call().enabled(),
            "voice_enabled": config.qq_voice().enabled(),
            "sing_enabled": config.qq_sing().enabled(),
            // 表情包素材库：她"能不能发表情"取决于开关**和**素材数量，两个都给。
            "sticker_enabled": config.qq_sticker().enabled(),
            "sticker_files": crate::sticker_library::listing().len(),
            "sticker_labels": crate::sticker_library::available_labels().len(),
            "sticker_ready": crate::sticker_library::is_available(),
        },
    })))
}

/// 等待房间与会话票据的实时快照。
///
/// 口径在 `model::waiting_room` 里统一（判定"卡住"的阈值来自配置），这里只负责
/// 把它摊成 JSON。只报有内容的会话：空闲会话不占版面，页面上一眼就能看出
/// "现在到底有没有人在等、等了多久"。
async fn waiting_room_report() -> Value {
    let stalled_after =
        std::time::Duration::from_secs(crate::config::get().traffic().window_stall_secs());
    let reports = crate::model::waiting_room::report(stalled_after).await;
    let scopes: Vec<Value> = reports
        .iter()
        .map(|report| {
            json!({
                "kind": report.kind,
                "subject_id": report.subject_id,
                "queued": report.queued,
                "oldest_queued_secs": report.oldest_queued_secs,
                "oldest_sender": report.oldest_sender,
                "oldest_preview": report.oldest_preview,
                "processing": report.processing,
                "drain_active": report.drain_active,
                "drain_drained": report.drain_drained,
                "drain_last_progress_secs": report.drain_last_progress_secs,
                "ticket": report.ticket.as_ref().map(|ticket| json!({
                    "generation": ticket.generation,
                    "age_secs": ticket.age_secs,
                })),
                "reply": {
                    "generation": report.reply.generation,
                    "conversation_version": report.reply.conversation_version,
                    "active_secs": report.reply.active_secs,
                    "pending_incoming": report.reply.pending_incoming,
                    "active_incoming": report.reply.active_incoming,
                    "pending_incoming_expires_in_secs":
                        report.reply.pending_incoming_expires_in_secs,
                    "prepared_outgoing": report.reply.prepared_outgoing,
                    "oldest_prepared_secs": report.reply.oldest_prepared_secs,
                    "precommit_armed": report.reply.precommit_armed,
                    "collision_count": report.reply.collision_count,
                    "last_seen_secs": report.reply.last_seen_secs,
                },
                "stuck": report.stuck,
                "stuck_reason": report.stuck_reason,
                "summary": report.summary,
            })
        })
        .collect();
    json!({
        "stalled_after_secs": stalled_after.as_secs(),
        "stuck": reports.iter().filter(|report| report.stuck).count(),
        "scopes": scopes,
    })
}

async fn database_health() -> Value {
    match crate::memory::MEMORY_MANAGER.check_storage_health().await {
        Ok(()) => json!({
            "ok": true,
            "detail": "PostgreSQL 可读写",
            "size_bytes": crate::memory::MEMORY_MANAGER.storage_size_bytes().await,
        }),
        Err(error) => json!({ "ok": false, "detail": error.to_string() }),
    }
}

async fn redis_health() -> Value {
    let detail = crate::redis_store::health_status().await;
    let ready = crate::redis_store::check_readiness().await;
    json!({
        "ok": ready.is_ok(),
        "detail": match ready {
            Ok(()) => detail,
            Err(error) => format!("{detail}：{error}"),
        },
    })
}

fn format_time(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Local> = time.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

// ---------------------------------------------------------------- 系统信息

/// 主机与进程的实时快照。
///
/// `System` 需要至少两次刷新才知道 CPU 使用率（它算的是两次刷新之间的差值），
/// 所以这里常驻一份并复用，而不是每次请求新建——顺便也省掉重复枚举进程的开销。
pub(crate) struct SystemMonitor {
    system: std::sync::Mutex<sysinfo::System>,
}

impl SystemMonitor {
    pub(crate) fn new() -> Self {
        let mut system = sysinfo::System::new_all();
        system.refresh_all();
        Self {
            system: std::sync::Mutex::new(system),
        }
    }

    fn snapshot(&self) -> Value {
        let Ok(mut system) = self.system.lock() else {
            return json!({ "error": "系统信息暂时不可用" });
        };
        system.refresh_cpu_all();
        system.refresh_memory();
        let pid = sysinfo::get_current_pid().ok();
        if let Some(pid) = pid {
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        }
        let process = pid.and_then(|pid| system.process(pid));

        let cpu = system.cpus().first();
        let total_memory = system.total_memory();
        let used_memory = system.used_memory();

        json!({
            "host": {
                "os": sysinfo::System::long_os_version().unwrap_or_else(|| "未知".to_string()),
                "kernel": sysinfo::System::kernel_version().unwrap_or_else(|| "未知".to_string()),
                "arch": sysinfo::System::cpu_arch(),
                "hostname": sysinfo::System::host_name().unwrap_or_else(|| "未知".to_string()),
                "uptime_secs": sysinfo::System::uptime(),
            },
            "cpu": {
                "brand": cpu.map(|cpu| cpu.brand().trim().to_string()).unwrap_or_default(),
                "cores": system.cpus().len(),
                "physical_cores": sysinfo::System::physical_core_count(),
                "frequency_mhz": cpu.map(sysinfo::Cpu::frequency).unwrap_or_default(),
                "usage_percent": f64::from(system.global_cpu_usage()),
                "process_percent": process.map(|process| f64::from(process.cpu_usage())),
            },
            "memory": {
                "total": total_memory,
                "used": used_memory,
                "available": total_memory.saturating_sub(used_memory),
                "percent": percent_of(used_memory, total_memory),
                "swap_total": system.total_swap(),
                "swap_used": system.used_swap(),
                "process_rss": process.map(sysinfo::Process::memory),
                "process_virtual": process.map(sysinfo::Process::virtual_memory),
            },
            "process": {
                "pid": pid.map(sysinfo::Pid::as_u32),
                "uptime_secs": process.map(sysinfo::Process::run_time),
            },
        })
    }

    /// 磁盘与网络：都是"取一次就够"的静态列表，按需构造。
    fn storage(&self) -> Value {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        // macOS 上 / 与 /System/Volumes/Data 是同一份空间的两个视角，数字几乎相同
        // （可用空间能差几十 KB，精确比较去不掉）。按"容量(GiB) + 使用率千分位"
        // 分档去重：同一个文件系统的两种视角会撞在一起，真不同的盘不会。
        let mut seen: std::collections::BTreeSet<(u64, i64)> = std::collections::BTreeSet::new();
        let disks: Vec<Value> = disks
            .list()
            .iter()
            // 只报真实挂载点，跳过容器/临时文件系统那一堆噪音。
            .filter(|disk| {
                let mount = disk.mount_point().to_string_lossy();
                let kind = format!("{:?}", disk.kind());
                disk.total_space() > 0
                    && !mount.starts_with("/sys")
                    && !mount.starts_with("/proc")
                    && !kind.contains("Tmpfs")
            })
            .filter(|disk| {
                let total = disk.total_space();
                let used = total.saturating_sub(disk.available_space());
                let bucket = (
                    total / (1 << 30),
                    (percent_of(used, total) * 1000.0).round() as i64,
                );
                seen.insert(bucket)
            })
            .map(|disk| {
                let total = disk.total_space();
                let available = disk.available_space();
                json!({
                    "mount": disk.mount_point().to_string_lossy(),
                    "file_system": disk.file_system().to_string_lossy(),
                    "total": total,
                    "available": available,
                    "used": total.saturating_sub(available),
                    "percent": percent_of(total.saturating_sub(available), total),
                })
            })
            .collect();

        let networks = sysinfo::Networks::new_with_refreshed_list();
        let received: u64 = networks
            .list()
            .values()
            .map(sysinfo::NetworkData::total_received)
            .sum();
        let transmitted: u64 = networks
            .list()
            .values()
            .map(sysinfo::NetworkData::total_transmitted)
            .sum();

        json!({
            "disks": disks,
            "network": { "received": received, "transmitted": transmitted },
        })
    }
}

fn percent_of(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    (part as f64 / whole as f64 * 100.0 * 10.0).round() / 10.0
}

/// `GET /api/system`：系统信息页。
///
/// OneBot 侧的信息（登录号、在线状态、NapCat 版本）来自对服务端的真实调用，
/// 带 60 秒缓存与 3 秒超时：对方不可达时这一块显示"未连接"，不影响主机信息。
pub(crate) async fn system(State(state): State<Arc<AdminState>>) -> Result<Json<Value>, ApiError> {
    let config = crate::config::get();
    let monitor = Arc::clone(&state.monitor);
    let snapshot = kovi::tokio::task::spawn_blocking(move || {
        let mut value = monitor.snapshot();
        let storage = monitor.storage();
        if let Some(object) = value.as_object_mut()
            && let Some(storage) = storage.as_object()
        {
            for (key, item) in storage {
                object.insert(key.clone(), item.clone());
            }
        }
        value
    })
    .await
    .map_err(|error| ApiError::internal(format!("系统信息采集失败: {error}")))?;

    let mut payload = snapshot;
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "identity".to_string(),
            json!({
                "name": "芸汐",
                "version": env!("CARGO_PKG_VERSION"),
                "revision": std::env::var("KOVI_DEPLOY_REVISION").unwrap_or_else(|_| "未知".to_string()),
                "runtime_dir": crate::config::runtime_dir().display().to_string(),
                "config_file": crate::config::config_file_path().display().to_string(),
                "owner_person_id": config.identity().owner_person_id().map(|id| id.to_string()),
                "pid": std::process::id(),
            }),
        );
        object.insert("onebot".to_string(), super::onebot_info(&state).await);
        object.insert(
            "model".to_string(),
            json!({
                "model_name": config.server_config().model_name(),
                "endpoint": config.server_config().endpoint(),
                "wire_api": config.server_config().wire_api(),
                "thinking_mode": config.server_config().thinking_mode(),
                "intrinsic_asset_dir": config.model().intrinsic().asset_dir(),
                "turn_gate_mode": config.model().turn_gate().mode(),
            }),
        );
    }
    Ok(Json(payload))
}
