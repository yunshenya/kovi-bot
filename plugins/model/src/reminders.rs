//! 持久化提醒任务。
//!
//! 提醒不是短期对话记忆：它必须在模型调用结束、进程重启甚至多个实例并行时仍然
//! 保持正确。因此创建、领取、发送和完成都通过 PostgreSQL 状态机完成，模型只接触
//! 受限的内置工具接口。

use crate::config;
use crate::memory::MEMORY_MANAGER;
use crate::model::{
    MessageDestination, MessageTransport, ReplyScope, record_standalone_bot_message,
};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use kovi::RuntimeBot;
use kovi::tokio::time::sleep;
use kovi::{Message, serde_json::Value};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, PgRow, Postgres};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_LIST_ITEMS: i64 = 20;
const CLAIM_BATCH_SIZE: i64 = 32;
const MAX_RETRY_DELAY_SECS: i64 = 300;
static REMINDER_TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

static LAST_CLEANUP: LazyLock<kovi::tokio::sync::Mutex<Option<Instant>>> =
    LazyLock::new(|| kovi::tokio::sync::Mutex::new(None));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepeatRule {
    None,
    Daily,
    Weekly,
}

impl RepeatRule {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "daily" => Ok(Self::Daily),
            "weekly" => Ok(Self::Weekly),
            _ => Err(anyhow!("repeat 只支持 none、daily 或 weekly")),
        }
    }
}

#[derive(Debug, Clone)]
struct CreateReminderRequest {
    due_at: DateTime<Utc>,
    timezone: String,
    message: String,
    repeat: RepeatRule,
}

#[derive(Debug, Clone)]
struct ClaimedReminder {
    id: i64,
    destination: MessageDestination,
    message: String,
    due_at: DateTime<Utc>,
    timezone: String,
    repeat: RepeatRule,
    lease_token: String,
    attempt_count: i32,
}

#[derive(Debug, Clone)]
struct ReminderListItem {
    id: i64,
    destination: MessageDestination,
    creator_user_id: i64,
    message: String,
    due_at: DateTime<Utc>,
    timezone: String,
    repeat: RepeatRule,
    status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryFailure {
    Retried,
    Failed,
    Stale,
}

/// 初始化提醒表。记忆模块已经完成数据库连接和基础 schema 初始化。
pub(crate) async fn initialize_database() -> Result<()> {
    let pool = database_pool()?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS kovi_bot_reminders (
            id BIGSERIAL PRIMARY KEY,
            scope_type TEXT NOT NULL CHECK (scope_type IN ('private', 'group')),
            scope_id BIGINT NOT NULL,
            creator_user_id BIGINT NOT NULL,
            message TEXT NOT NULL,
            due_at TIMESTAMPTZ NOT NULL,
            timezone TEXT NOT NULL,
            repeat_kind TEXT NOT NULL DEFAULT 'none'
                CHECK (repeat_kind IN ('none', 'daily', 'weekly')),
            status TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'delivering', 'sent', 'cancelled', 'failed')),
            attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
            lease_token TEXT,
            lease_until TIMESTAMPTZ,
            last_error TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            delivered_at TIMESTAMPTZ
        )
        "#,
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("创建提醒任务表失败: {error}"))?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_reminders_due_idx ON kovi_bot_reminders (status, due_at, id)",
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("创建提醒到期索引失败: {error}"))?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_reminders_scope_idx ON kovi_bot_reminders (scope_type, scope_id, status, due_at)",
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("创建提醒会话索引失败: {error}"))?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_reminders_creator_idx ON kovi_bot_reminders (creator_user_id, status, due_at)",
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("创建提醒创建者索引失败: {error}"))?;
    Ok(())
}

/// 启动长期提醒调度器。它不使用聊天回复 ticket，避免新消息打断已经创建的提醒。
pub(crate) async fn start_scheduler(bot: Arc<RuntimeBot>) {
    let reminder_config = config::get().reminders().clone();
    if !reminder_config.enabled() {
        println!("[INFO] 持久化提醒功能已关闭");
        return;
    }

    println!(
        "[INFO] 提醒调度器已启动，扫描间隔 {} 秒",
        reminder_config.poll_interval_secs()
    );
    loop {
        if let Err(error) = dispatch_due(&bot).await {
            eprintln!("[ERROR] 提醒调度失败: {error}");
        }
        maybe_cleanup_terminal_rows().await;
        sleep(Duration::from_secs(reminder_config.poll_interval_secs())).await;
    }
}

async fn dispatch_due(bot: &RuntimeBot) -> Result<()> {
    let claimed = claim_due(
        Utc::now(),
        CLAIM_BATCH_SIZE,
        config::get().reminders().lease_secs(),
    )
    .await?;
    for reminder in claimed {
        if !is_claim_current(&reminder).await? {
            continue;
        }
        let content = format!("⏰ {}", reminder.message);
        let destination = reminder.destination;
        let result = MessageTransport::new(bot)
            .send(destination, Message::from(content.clone()))
            .await;
        match result {
            Ok(message_id) => {
                let scope = destination_scope(destination);
                record_standalone_bot_message(scope, message_id, &content).await;
                if !complete_claim(&reminder).await? {
                    eprintln!(
                        "[WARN] 提醒已发送但完成状态未更新 (任务: {})，可能已被取消",
                        reminder.id
                    );
                }
            }
            Err(error) => {
                let outcome = fail_claim(
                    &reminder,
                    &format!("消息发送失败: {error:?}"),
                    config::get().reminders().max_attempts(),
                )
                .await?;
                if outcome == DeliveryFailure::Failed {
                    eprintln!(
                        "[ERROR] 提醒发送失败并停止重试 (任务: {}): {error:?}",
                        reminder.id
                    );
                }
            }
        }
    }
    Ok(())
}

async fn maybe_cleanup_terminal_rows() {
    let should_run = {
        let mut last_cleanup = LAST_CLEANUP.lock().await;
        let due = last_cleanup.is_none_or(|instant| instant.elapsed() >= Duration::from_secs(300));
        if due {
            *last_cleanup = Some(Instant::now());
        }
        due
    };
    if should_run
        && let Err(error) = cleanup_terminal_rows(config::get().memory().retention_days()).await
    {
        eprintln!("[WARN] 过期提醒清理失败: {error}");
    }
}

/// 由内置工具调用：创建一个当前私聊或当前群的提醒。
pub(crate) async fn create_from_tool(
    arguments: &serde_json::Map<String, Value>,
    destination: MessageDestination,
    actor_user_id: i64,
) -> Result<String> {
    let request = parse_create_request(arguments, Utc::now(), config::get().reminders())?;
    let id = create(destination, actor_user_id, request.clone()).await?;
    Ok(format_created_message(id, &request))
}

/// 由内置工具调用：列出当前私聊或当前群的未完成提醒。
pub(crate) async fn list_from_tool(
    arguments: &serde_json::Map<String, Value>,
    destination: MessageDestination,
    actor_user_id: i64,
) -> Result<String> {
    reject_unknown_arguments(arguments, &[])?;
    let items = list(destination).await?;
    if items.is_empty() {
        return Ok("当前会话没有未完成的提醒。".to_string());
    }
    let mut output = String::from("当前会话的提醒：");
    for item in items {
        let destination_label = match item.destination {
            MessageDestination::Private(_) => "私聊",
            MessageDestination::Group(_) => "群聊",
        };
        let repeat_label = match item.repeat {
            RepeatRule::None => "一次性",
            RepeatRule::Daily => "每天",
            RepeatRule::Weekly => "每周",
        };
        let creator_label = if item.creator_user_id == actor_user_id {
            "你"
        } else {
            "其他成员"
        };
        output.push_str(&format!(
            "\n- #{} [{}] {}，{}，{}，创建者 {}",
            item.id,
            destination_label,
            format_local_time(item.due_at, &item.timezone)?,
            repeat_label,
            item.message,
            creator_label
        ));
        if item.status != "pending" {
            let status = match item.status.as_str() {
                "delivering" => "发送中",
                other => other,
            };
            output.push_str(&format!("（{}）", status));
        }
    }
    Ok(output)
}

/// 由内置工具调用：只能取消当前会话内、且由当前用户创建的提醒。
pub(crate) async fn cancel_from_tool(
    arguments: &serde_json::Map<String, Value>,
    destination: MessageDestination,
    actor_user_id: i64,
) -> Result<String> {
    reject_unknown_arguments(arguments, &["reminder_id"])?;
    let id = arguments
        .get("reminder_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("参数 reminder_id 必须是正整数"))?;
    if id <= 0 {
        return Err(anyhow!("参数 reminder_id 必须是正整数"));
    }
    if cancel(destination, actor_user_id, id).await? {
        Ok(format!("提醒 #{} 已取消。", id))
    } else {
        Ok(format!(
            "没有找到可由你取消的提醒 #{}；它可能不存在、已发送、正在发送或属于其他用户。",
            id
        ))
    }
}

fn parse_create_request(
    arguments: &serde_json::Map<String, Value>,
    now: DateTime<Utc>,
    reminder_config: &config::ReminderConfig,
) -> Result<CreateReminderRequest> {
    reject_unknown_arguments(
        arguments,
        &[
            "mode",
            "after_seconds",
            "local_datetime",
            "timezone",
            "message",
            "repeat",
        ],
    )?;
    let mode = required_string(arguments, "mode", 10)?;
    let message = match arguments.get("message") {
        Some(value) => normalize_message(
            value
                .as_str()
                .ok_or_else(|| anyhow!("参数 message 必须是字符串"))?,
            reminder_config.max_message_chars(),
        )?,
        None => "时间到了，我来提醒你啦。".to_string(),
    };
    let timezone_name = match arguments.get("timezone") {
        Some(value) => value
            .as_str()
            .ok_or_else(|| anyhow!("参数 timezone 必须是字符串"))?
            .trim(),
        None => reminder_config.default_timezone(),
    };
    let timezone = timezone_name
        .parse::<Tz>()
        .map_err(|_| anyhow!("不支持的时区：{timezone_name}"))?;
    let repeat = match arguments.get("repeat") {
        Some(value) => RepeatRule::parse(
            value
                .as_str()
                .ok_or_else(|| anyhow!("参数 repeat 必须是字符串"))?,
        )?,
        None => RepeatRule::None,
    };
    let max_delay = ChronoDuration::days(reminder_config.max_delay_days() as i64);
    let due_at = match mode.as_str() {
        "after" => {
            let seconds = arguments
                .get("after_seconds")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("mode=after 时必须提供 after_seconds"))?;
            if !(5..=max_delay.num_seconds() as u64).contains(&seconds) {
                return Err(anyhow!(
                    "after_seconds 必须在 5 秒到 {} 天之间",
                    reminder_config.max_delay_days()
                ));
            }
            now + ChronoDuration::seconds(seconds as i64)
        }
        "at" => {
            let local_datetime = required_string(arguments, "local_datetime", 32)?;
            let naive = parse_local_datetime(&local_datetime)?;
            let local = timezone
                .from_local_datetime(&naive)
                .single()
                .ok_or_else(|| anyhow!("这个本地时间在时区 {} 中不存在或有歧义", timezone_name))?;
            let due_at = local.with_timezone(&Utc);
            if due_at <= now + ChronoDuration::seconds(4) {
                return Err(anyhow!("提醒时间已经过去，请提供未来的时间"));
            }
            if due_at > now + max_delay {
                return Err(anyhow!(
                    "提醒时间不能超过 {} 天",
                    reminder_config.max_delay_days()
                ));
            }
            due_at
        }
        _ => return Err(anyhow!("mode 只支持 after 或 at")),
    };
    Ok(CreateReminderRequest {
        due_at,
        timezone: timezone_name.to_string(),
        message,
        repeat,
    })
}

fn parse_local_datetime(value: &str) -> Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S"))
        .map_err(|_| anyhow!("local_datetime 必须是 YYYY-MM-DD HH:MM 格式"))
}

fn normalize_message(value: &str, max_chars: usize) -> Result<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(anyhow!("参数 message 不能为空"));
    }
    if normalized.chars().count() > max_chars {
        return Err(anyhow!("参数 message 不能超过 {} 个字符", max_chars));
    }
    Ok(normalized)
}

fn format_created_message(id: i64, request: &CreateReminderRequest) -> String {
    let repeat = match request.repeat {
        RepeatRule::None => "一次性",
        RepeatRule::Daily => "每天",
        RepeatRule::Weekly => "每周",
    };
    format!(
        "提醒已创建：#{}，{}，{}，内容：{}。",
        id,
        format_local_time(request.due_at, &request.timezone).unwrap_or_else(|_| "时间无效".into()),
        repeat,
        request.message
    )
}

fn format_local_time(due_at: DateTime<Utc>, timezone_name: &str) -> Result<String> {
    let timezone = timezone_name
        .parse::<Tz>()
        .map_err(|_| anyhow!("提醒保存了未知时区：{timezone_name}"))?;
    Ok(format!(
        "{} ({})",
        due_at.with_timezone(&timezone).format("%Y-%m-%d %H:%M"),
        timezone_name
    ))
}

async fn create(
    destination: MessageDestination,
    actor_user_id: i64,
    request: CreateReminderRequest,
) -> Result<i64> {
    let pool = database_pool()?;
    let (scope_type, scope_id) = destination_values(destination);
    let reminder_config = config::get().reminders().clone();
    let mut transaction = pool.begin().await?;
    lock_global(&mut transaction).await?;
    lock_scope(&mut transaction, scope_type, scope_id).await?;

    let scope_count = query_scalar::<Postgres, i64>(
        "SELECT COUNT(*) FROM kovi_bot_reminders WHERE scope_type = $1 AND scope_id = $2 AND status IN ('pending', 'delivering')",
    )
    .bind(scope_type)
    .bind(scope_id)
    .fetch_one(&mut *transaction)
    .await?;
    let scope_limit = match destination {
        MessageDestination::Private(_) => reminder_config.max_pending_per_user(),
        MessageDestination::Group(_) => reminder_config.max_pending_per_group(),
    } as i64;
    if scope_count >= scope_limit {
        return Err(anyhow!("当前会话的未完成提醒已达到上限 {}", scope_limit));
    }

    let total_count = query_scalar::<Postgres, i64>(
        "SELECT COUNT(*) FROM kovi_bot_reminders WHERE status IN ('pending', 'delivering')",
    )
    .fetch_one(&mut *transaction)
    .await?;
    if total_count >= reminder_config.max_pending_total() as i64 {
        return Err(anyhow!("系统当前的未完成提醒已达到全局上限，请稍后再试"));
    }

    let id = query_scalar::<Postgres, i64>(
        r#"
        INSERT INTO kovi_bot_reminders
            (scope_type, scope_id, creator_user_id, message, due_at, timezone, repeat_kind)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(scope_type)
    .bind(scope_id)
    .bind(actor_user_id)
    .bind(&request.message)
    .bind(request.due_at)
    .bind(&request.timezone)
    .bind(request.repeat.as_str())
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(id)
}

async fn list(destination: MessageDestination) -> Result<Vec<ReminderListItem>> {
    let pool = database_pool()?;
    let (scope_type, scope_id) = destination_values(destination);
    let rows = query(
        r#"
        SELECT id, scope_type, scope_id, creator_user_id, message, due_at, timezone,
               repeat_kind, status
        FROM kovi_bot_reminders
        WHERE scope_type = $1 AND scope_id = $2 AND status IN ('pending', 'delivering')
        ORDER BY due_at ASC, id ASC
        LIMIT $3
        "#,
    )
    .bind(scope_type)
    .bind(scope_id)
    .bind(MAX_LIST_ITEMS)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(parse_list_item).collect()
}

async fn cancel(destination: MessageDestination, actor_user_id: i64, id: i64) -> Result<bool> {
    let pool = database_pool()?;
    let (scope_type, scope_id) = destination_values(destination);
    let result = query(
        r#"
        UPDATE kovi_bot_reminders
        SET status = 'cancelled', lease_token = NULL, lease_until = NULL, updated_at = NOW()
        WHERE id = $1 AND scope_type = $2 AND scope_id = $3
          AND creator_user_id = $4 AND status = 'pending'
        "#,
    )
    .bind(id)
    .bind(scope_type)
    .bind(scope_id)
    .bind(actor_user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn claim_due(
    now: DateTime<Utc>,
    limit: i64,
    lease_secs: u64,
) -> Result<Vec<ClaimedReminder>> {
    let pool = database_pool()?;
    let lease_until = now + ChronoDuration::seconds(lease_secs as i64);
    let mut transaction = pool.begin().await?;
    let rows = query(
        r#"
        SELECT id, scope_type, scope_id, message, due_at, timezone, repeat_kind, attempt_count
        FROM kovi_bot_reminders
        WHERE due_at <= $1
          AND (
            status = 'pending'
            OR (status = 'delivering' AND lease_until IS NOT NULL AND lease_until <= $1)
          )
        ORDER BY due_at ASC, id ASC
        FOR UPDATE SKIP LOCKED
        LIMIT $2
        "#,
    )
    .bind(now)
    .bind(limit)
    .fetch_all(&mut *transaction)
    .await?;
    let mut claimed = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.get::<i64, _>("id");
        let lease_token = new_lease_token(id);
        query(
            r#"
            UPDATE kovi_bot_reminders
            SET status = 'delivering', lease_token = $2, lease_until = $3,
                attempt_count = attempt_count + 1, updated_at = $3
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(&lease_token)
        .bind(lease_until)
        .execute(&mut *transaction)
        .await?;
        let attempt_count = row.get::<i32, _>("attempt_count") + 1;
        claimed.push(ClaimedReminder {
            id,
            destination: destination_from_values(
                row.get::<String, _>("scope_type").as_str(),
                row.get("scope_id"),
            )?,
            message: row.get("message"),
            due_at: row.get("due_at"),
            timezone: row.get("timezone"),
            repeat: RepeatRule::parse(row.get::<String, _>("repeat_kind").as_str())?,
            lease_token,
            attempt_count,
        });
    }
    transaction.commit().await?;
    Ok(claimed)
}

async fn is_claim_current(reminder: &ClaimedReminder) -> Result<bool> {
    let pool = database_pool()?;
    let current = query_scalar::<Postgres, bool>(
        "SELECT EXISTS(SELECT 1 FROM kovi_bot_reminders WHERE id = $1 AND status = 'delivering' AND lease_token = $2 AND lease_until > NOW())",
    )
    .bind(reminder.id)
    .bind(&reminder.lease_token)
    .fetch_one(pool)
    .await?;
    Ok(current)
}

async fn complete_claim(reminder: &ClaimedReminder) -> Result<bool> {
    let pool = database_pool()?;
    let now = Utc::now();
    let result = match reminder.repeat {
        RepeatRule::None => {
            query(
                r#"
                UPDATE kovi_bot_reminders
                SET status = 'sent', delivered_at = $3, lease_token = NULL, lease_until = NULL,
                    updated_at = $3, last_error = NULL
                WHERE id = $1 AND status = 'delivering' AND lease_token = $2
                "#,
            )
            .bind(reminder.id)
            .bind(&reminder.lease_token)
            .bind(now)
            .execute(pool)
            .await?
        }
        repeat => {
            let next_due = next_occurrence(reminder.due_at, &reminder.timezone, repeat)?;
            query(
                r#"
                UPDATE kovi_bot_reminders
                SET status = 'pending', due_at = $3, attempt_count = 0,
                    lease_token = NULL, lease_until = NULL, updated_at = $4, last_error = NULL
                WHERE id = $1 AND status = 'delivering' AND lease_token = $2
                "#,
            )
            .bind(reminder.id)
            .bind(&reminder.lease_token)
            .bind(next_due)
            .bind(now)
            .execute(pool)
            .await?
        }
    };
    Ok(result.rows_affected() == 1)
}

async fn fail_claim(
    reminder: &ClaimedReminder,
    error: &str,
    max_attempts: u8,
) -> Result<DeliveryFailure> {
    let pool = database_pool()?;
    let now = Utc::now();
    let error = truncate_chars(error, 800);
    if reminder.attempt_count >= i32::from(max_attempts) {
        let result = query(
            r#"
            UPDATE kovi_bot_reminders
            SET status = 'failed', last_error = $3, lease_token = NULL, lease_until = NULL,
                updated_at = $4
            WHERE id = $1 AND status = 'delivering' AND lease_token = $2
            "#,
        )
        .bind(reminder.id)
        .bind(&reminder.lease_token)
        .bind(error)
        .bind(now)
        .execute(pool)
        .await?;
        return Ok(if result.rows_affected() == 1 {
            DeliveryFailure::Failed
        } else {
            DeliveryFailure::Stale
        });
    }

    let retry_exponent = reminder.attempt_count.clamp(1, 8) as u32;
    let retry_seconds = (1_i64 << retry_exponent).min(MAX_RETRY_DELAY_SECS);
    let retry_at = now + ChronoDuration::seconds(retry_seconds);
    let result = query(
        r#"
        UPDATE kovi_bot_reminders
        SET status = 'pending', due_at = $3, last_error = $4,
            lease_token = NULL, lease_until = NULL, updated_at = $5
        WHERE id = $1 AND status = 'delivering' AND lease_token = $2
        "#,
    )
    .bind(reminder.id)
    .bind(&reminder.lease_token)
    .bind(retry_at)
    .bind(error)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(if result.rows_affected() == 1 {
        DeliveryFailure::Retried
    } else {
        DeliveryFailure::Stale
    })
}

async fn cleanup_terminal_rows(retention_days: i64) -> Result<u64> {
    let pool = database_pool()?;
    let cutoff = Utc::now() - ChronoDuration::days(retention_days.max(1));
    Ok(query(
        "DELETE FROM kovi_bot_reminders WHERE status IN ('sent', 'cancelled', 'failed') AND updated_at < $1",
    )
    .bind(cutoff)
    .execute(pool)
    .await?
    .rows_affected())
}

pub(crate) async fn delete_user_data(user_id: i64) -> Result<u64> {
    let pool = database_pool()?;
    Ok(query(
        "DELETE FROM kovi_bot_reminders WHERE creator_user_id = $1 OR (scope_type = 'private' AND scope_id = $1)",
    )
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected())
}

pub(crate) async fn delete_group_data(group_id: i64) -> Result<u64> {
    let pool = database_pool()?;
    Ok(
        query("DELETE FROM kovi_bot_reminders WHERE scope_type = 'group' AND scope_id = $1")
            .bind(group_id)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

async fn lock_scope(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    scope_type: &str,
    scope_id: i64,
) -> Result<()> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("reminder:{scope_type}:{scope_id}"))
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn lock_global(
    transaction: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
) -> Result<()> {
    query("SELECT pg_advisory_xact_lock(hashtextextended('reminder:global', 0))")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn parse_list_item(row: PgRow) -> Result<ReminderListItem> {
    Ok(ReminderListItem {
        id: row.get("id"),
        destination: destination_from_values(
            row.get::<String, _>("scope_type").as_str(),
            row.get("scope_id"),
        )?,
        creator_user_id: row.get("creator_user_id"),
        message: row.get("message"),
        due_at: row.get("due_at"),
        timezone: row.get("timezone"),
        repeat: RepeatRule::parse(row.get::<String, _>("repeat_kind").as_str())?,
        status: row.get("status"),
    })
}

fn destination_values(destination: MessageDestination) -> (&'static str, i64) {
    match destination {
        MessageDestination::Private(user_id) => ("private", user_id),
        MessageDestination::Group(group_id) => ("group", group_id),
    }
}

fn destination_from_values(scope_type: &str, scope_id: i64) -> Result<MessageDestination> {
    match scope_type {
        "private" => Ok(MessageDestination::Private(scope_id)),
        "group" => Ok(MessageDestination::Group(scope_id)),
        _ => Err(anyhow!("提醒保存了未知会话类型")),
    }
}

fn destination_scope(destination: MessageDestination) -> ReplyScope {
    match destination {
        MessageDestination::Private(user_id) => ReplyScope::Private(user_id),
        MessageDestination::Group(group_id) => ReplyScope::Group(group_id),
    }
}

fn database_pool() -> Result<&'static PgPool> {
    MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 记忆连接池尚未初始化"))
}

fn new_lease_token(reminder_id: i64) -> String {
    let sequence = REMINDER_TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}:{reminder_id}:{sequence}:{nanos}", std::process::id())
}

fn next_occurrence(
    due_at: DateTime<Utc>,
    timezone_name: &str,
    repeat: RepeatRule,
) -> Result<DateTime<Utc>> {
    let timezone = timezone_name
        .parse::<Tz>()
        .map_err(|_| anyhow!("不支持的提醒时区：{timezone_name}"))?;
    let local = due_at.with_timezone(&timezone).naive_local();
    let next_local = match repeat {
        RepeatRule::None => return Err(anyhow!("一次性提醒没有下一次时间")),
        RepeatRule::Daily => local + ChronoDuration::days(1),
        RepeatRule::Weekly => local + ChronoDuration::days(7),
    };
    match timezone.from_local_datetime(&next_local) {
        chrono::LocalResult::Single(value) => Ok(value.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(_, latest) => Ok(latest.with_timezone(&Utc)),
        chrono::LocalResult::None => timezone
            .from_local_datetime(&(next_local + ChronoDuration::hours(1)))
            .earliest()
            .map(|value| value.with_timezone(&Utc))
            .ok_or_else(|| anyhow!("重复提醒的下一次本地时间无效")),
    }
}

fn required_string(
    arguments: &serde_json::Map<String, Value>,
    name: &str,
    max_chars: usize,
) -> Result<String> {
    let value = arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("参数 {name} 必须是字符串"))?
        .trim();
    if value.is_empty() {
        return Err(anyhow!("参数 {name} 不能为空"));
    }
    if value.chars().count() > max_chars {
        return Err(anyhow!("参数 {name} 过长"));
    }
    Ok(value.to_string())
}

fn reject_unknown_arguments(
    arguments: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<()> {
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !allowed.iter().any(|allowed| allowed == key))
    {
        return Err(anyhow!("不支持的工具参数：{unknown}"));
    }
    Ok(())
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

#[cfg(test)]
mod tests {
    use super::{RepeatRule, next_occurrence, parse_create_request};
    use crate::config::ReminderConfig;
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;

    #[test]
    fn parses_relative_reminders_with_bounded_text() {
        let arguments = serde_json::from_value(json!({
            "mode": "after",
            "after_seconds": 600,
            "message": "  记得   吃饭  ",
            "timezone": "Asia/Shanghai"
        }))
        .expect("参数应能构造");
        let now = Utc::now();
        let request = parse_create_request(&arguments, now, &ReminderConfig::default())
            .expect("相对提醒应能解析");
        assert_eq!(request.message, "记得 吃饭");
        assert_eq!(request.repeat, RepeatRule::None);
        assert_eq!(request.due_at - now, Duration::seconds(600));
    }

    #[test]
    fn missing_message_uses_a_safe_default() {
        let arguments = serde_json::from_value(json!({
            "mode": "after",
            "after_seconds": 600
        }))
        .expect("参数应能构造");
        let request = parse_create_request(&arguments, Utc::now(), &ReminderConfig::default())
            .expect("缺少正文时仍应创建提醒");
        assert_eq!(request.message, "时间到了，我来提醒你啦。");
    }

    #[test]
    fn rejects_ambiguous_or_past_absolute_reminders() {
        let arguments = serde_json::from_value(json!({
            "mode": "at",
            "local_datetime": "2020-01-01 08:00",
            "message": "吃饭"
        }))
        .expect("参数应能构造");
        let now = Utc.with_ymd_and_hms(2026, 8, 22, 0, 0, 0).unwrap();
        assert!(parse_create_request(&arguments, now, &ReminderConfig::default()).is_err());
    }

    #[test]
    fn daily_recurrence_preserves_local_clock() {
        let first = Utc.with_ymd_and_hms(2026, 8, 22, 0, 0, 0).unwrap();
        let next =
            next_occurrence(first, "Asia/Shanghai", RepeatRule::Daily).expect("每天提醒应有下一次");
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 8, 23, 0, 0, 0).unwrap());
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_claim_is_atomic_across_concurrent_workers() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let subject_id = Utc::now().timestamp_micros();
                crate::memory::MEMORY_MANAGER
                    .initialize_database()
                    .await
                    .expect("应初始化 PostgreSQL 记忆连接池");
                super::initialize_database().await.expect("应初始化提醒表");
                let request = super::CreateReminderRequest {
                    due_at: Utc::now() - chrono::Duration::seconds(1),
                    timezone: ReminderConfig::default().default_timezone().to_string(),
                    message: "并发领取测试".to_string(),
                    repeat: RepeatRule::None,
                };
                let id = super::create(
                    super::MessageDestination::Private(subject_id),
                    subject_id,
                    request,
                )
                .await
                .expect("应创建测试提醒");
                let now = Utc::now();
                let (first, second) =
                    kovi::tokio::join!(super::claim_due(now, 1, 60), super::claim_due(now, 1, 60),);
                let first = first.expect("第一个领取者不应失败");
                let second = second.expect("第二个领取者不应失败");
                let claimed = first
                    .iter()
                    .chain(second.iter())
                    .filter(|reminder| reminder.id == id)
                    .count();
                assert_eq!(claimed, 1, "同一提醒只能被一个并发领取者拿到");
                super::query("DELETE FROM kovi_bot_reminders WHERE id = $1")
                    .bind(id)
                    .execute(super::database_pool().expect("连接池应存在"))
                    .await
                    .expect("应清理测试提醒");
            });
    }
}
