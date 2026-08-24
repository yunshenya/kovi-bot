//! 通用持久化 Agent Run Runtime。
//!
//! Run 保存长期目标和下一次唤醒时间，Event 记录状态转换，Action 记录能力调用。
//! 当前执行器只开放受限的 `http.get`，后续 MCP 或内置动作可以沿同一状态机扩展。

use crate::config;
use crate::memory::MEMORY_MANAGER;
use crate::model::tool_access::{fetch_public_http_response, validate_public_url};
use crate::model::{
    MessageDestination, MessageTransport, ReplyScope, record_standalone_bot_message,
};
use anyhow::{Context, Result, anyhow, ensure};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use chrono_tz::Tz;
use kovi::tokio::sync::Notify;
use kovi::{Message, RuntimeBot};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, Postgres};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_LIST_ITEMS: i64 = 20;
const MAX_URL_CHARS: usize = 2_000;
const MAX_EXPECTED_TEXT_CHARS: usize = 2_000;
const MAX_JSON_EXPECTED_CHARS: usize = 4_000;
const MAX_JSON_POINTER_CHARS: usize = 500;
const NOTIFICATION_SEND_TIMEOUT: Duration = Duration::from_secs(5);

static RUN_WAKEUP: LazyLock<Notify> = LazyLock::new(Notify::new);
static LEASE_TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UrlConditionKind {
    TextContains,
    TextNotContains,
    TextEquals,
    StatusEquals,
    JsonPointerEquals,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct UrlWatchSpec {
    url: String,
    interval_seconds: u64,
    condition: UrlConditionKind,
    expected: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    json_pointer: Option<String>,
    notification_message: String,
    stop_after_minutes: u64,
    max_executions: u32,
}

#[derive(Debug, Clone)]
struct CreateRunRequest {
    spec: UrlWatchSpec,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct ClaimedRun {
    id: i64,
    owner_user_id: i64,
    kind: String,
    spec: Value,
    expires_at: DateTime<Utc>,
    execution_count: i32,
    max_executions: i32,
    consecutive_failure_count: i32,
    lease_token: String,
    http_action_id: Option<i64>,
    reason: ClaimReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimReason {
    Execute,
    Expired,
    Exhausted,
}

#[derive(Debug, Clone)]
struct HttpObservation {
    status: u16,
    content_type: String,
    body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalOutcome {
    Matched,
    Expired,
    Exhausted,
    Failed,
}

impl TerminalOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Expired => "expired",
            Self::Exhausted => "max_executions",
            Self::Failed => "failed",
        }
    }

    fn final_status(self) -> &'static str {
        match self {
            Self::Matched => "completed",
            Self::Expired | Self::Exhausted => "expired",
            Self::Failed => "failed",
        }
    }
}

enum ActionCompletion {
    Succeeded(Value),
    Failed(String),
}

pub(crate) async fn initialize_database() -> Result<()> {
    let pool = database_pool()?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS kovi_bot_agent_runs (
            id BIGSERIAL PRIMARY KEY,
            request_key TEXT NOT NULL UNIQUE,
            owner_user_id BIGINT NOT NULL,
            source_scope TEXT NOT NULL CHECK (source_scope IN ('private')),
            source_id BIGINT NOT NULL,
            source_message_id INTEGER NOT NULL,
            kind TEXT NOT NULL,
            spec JSONB NOT NULL,
            state JSONB NOT NULL DEFAULT '{}'::jsonb,
            status TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active', 'running', 'notifying', 'completed', 'cancelled', 'failed', 'expired')),
            next_wake_at TIMESTAMPTZ,
            expires_at TIMESTAMPTZ NOT NULL,
            execution_count INTEGER NOT NULL DEFAULT 0 CHECK (execution_count >= 0),
            max_executions INTEGER NOT NULL CHECK (max_executions > 0),
            consecutive_failure_count INTEGER NOT NULL DEFAULT 0
                CHECK (consecutive_failure_count >= 0),
            lease_token TEXT,
            lease_until TIMESTAMPTZ,
            notification_status TEXT NOT NULL DEFAULT 'none'
                CHECK (notification_status IN ('none', 'sending', 'sent', 'failed', 'unknown')),
            notification_started_at TIMESTAMPTZ,
            notification_delivered_at TIMESTAMPTZ,
            notification_message_id INTEGER,
            final_outcome TEXT,
            result JSONB,
            last_error TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            completed_at TIMESTAMPTZ
        )
        "#,
    )
    .execute(pool)
    .await
    .context("创建 Agent Run 表")?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS kovi_bot_agent_run_events (
            id BIGSERIAL PRIMARY KEY,
            run_id BIGINT NOT NULL REFERENCES kovi_bot_agent_runs(id) ON DELETE CASCADE,
            event_key TEXT NOT NULL,
            event_type TEXT NOT NULL,
            payload JSONB NOT NULL DEFAULT '{}'::jsonb,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (run_id, event_key)
        )
        "#,
    )
    .execute(pool)
    .await
    .context("创建 Agent Run 事件表")?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS kovi_bot_agent_run_actions (
            id BIGSERIAL PRIMARY KEY,
            run_id BIGINT NOT NULL REFERENCES kovi_bot_agent_runs(id) ON DELETE CASCADE,
            idempotency_key TEXT NOT NULL,
            capability TEXT NOT NULL,
            effect_class TEXT NOT NULL CHECK (effect_class IN ('read_only', 'irreversible')),
            arguments JSONB NOT NULL DEFAULT '{}'::jsonb,
            status TEXT NOT NULL DEFAULT 'started'
                CHECK (status IN ('started', 'succeeded', 'failed', 'unknown')),
            result JSONB,
            last_error TEXT,
            started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            completed_at TIMESTAMPTZ,
            UNIQUE (run_id, idempotency_key)
        )
        "#,
    )
    .execute(pool)
    .await
    .context("创建 Agent Run 动作表")?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_agent_runs_wake_idx ON kovi_bot_agent_runs (status, next_wake_at, id)",
    )
    .execute(pool)
    .await
    .context("创建 Agent Run 唤醒索引")?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_agent_runs_owner_idx ON kovi_bot_agent_runs (owner_user_id, status, created_at DESC)",
    )
    .execute(pool)
    .await
    .context("创建 Agent Run 所有者索引")?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_agent_run_events_run_idx ON kovi_bot_agent_run_events (run_id, created_at, id)",
    )
    .execute(pool)
    .await
    .context("创建 Agent Run 事件索引")?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_agent_run_actions_run_idx ON kovi_bot_agent_run_actions (run_id, started_at, id)",
    )
    .execute(pool)
    .await
    .context("创建 Agent Run 动作索引")?;

    recover_stale_claims(Utc::now()).await?;
    Ok(())
}

pub(crate) async fn start_scheduler(bot: Arc<RuntimeBot>) {
    if !config::get().agent_runs().enabled() {
        println!("[INFO] Agent Run Runtime 已关闭");
        return;
    }
    println!("[INFO] Agent Run Runtime 已启动（事件唤醒 + 自适应恢复扫描）");
    loop {
        if let Err(error) = dispatch_due(Arc::clone(&bot)).await {
            eprintln!("[ERROR] Agent Run 调度失败: {error}");
        }
        let delay = match next_scheduler_delay().await {
            Ok(delay) => delay,
            Err(error) => {
                eprintln!("[WARN] Agent Run 下一次唤醒时间读取失败: {error}");
                Duration::from_secs(config::get().agent_runs().recovery_scan_secs())
            }
        };
        kovi::tokio::select! {
            _ = RUN_WAKEUP.notified() => {}
            _ = kovi::tokio::time::sleep(delay) => {}
        }
    }
}

async fn dispatch_due(bot: Arc<RuntimeBot>) -> Result<()> {
    recover_stale_claims(Utc::now()).await?;
    let run_config = config::get().agent_runs().clone();
    let claimed = claim_due(
        Utc::now(),
        run_config.claim_batch_size() as i64,
        run_config.lease_secs(),
    )
    .await?;
    let mut workers = kovi::tokio::task::JoinSet::new();
    for run in claimed {
        let bot = Arc::clone(&bot);
        workers.spawn(async move {
            if let Err(error) = process_claim(bot.as_ref(), run).await {
                eprintln!("[ERROR] Agent Run 执行失败: {error}");
            }
        });
    }
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result {
            eprintln!("[ERROR] Agent Run worker 异常退出: {error}");
        }
    }
    Ok(())
}

async fn process_claim(bot: &RuntimeBot, run: ClaimedRun) -> Result<()> {
    match run.reason {
        ClaimReason::Expired => {
            return begin_notification(
                bot,
                &run,
                TerminalOutcome::Expired,
                None,
                None,
                json!({"reason": "deadline_reached"}),
            )
            .await;
        }
        ClaimReason::Exhausted => {
            return begin_notification(
                bot,
                &run,
                TerminalOutcome::Exhausted,
                None,
                None,
                json!({"reason": "max_executions_reached"}),
            )
            .await;
        }
        ClaimReason::Execute => {}
    }

    let spec = match parse_stored_spec(&run.kind, &run.spec) {
        Ok(spec) => spec,
        Err(error) => {
            return begin_notification(
                bot,
                &run,
                TerminalOutcome::Failed,
                run.http_action_id,
                Some(ActionCompletion::Failed(error.to_string())),
                json!({"reason": "invalid_stored_spec"}),
            )
            .await;
        }
    };
    let run_config = config::get().agent_runs().clone();
    let response = fetch_public_http_response(
        &spec.url,
        run_config.max_response_bytes(),
        Duration::from_secs(run_config.request_timeout_secs()),
    )
    .await;
    let now = Utc::now();
    match response {
        Ok(response) => {
            let observation = HttpObservation {
                status: response.status,
                content_type: response.content_type,
                body: String::from_utf8_lossy(&response.body).into_owned(),
            };
            let summary = observation_summary(&observation, run_config.max_body_preview_chars());
            if condition_matches(&spec, &observation) {
                return begin_notification(
                    bot,
                    &run,
                    TerminalOutcome::Matched,
                    run.http_action_id,
                    Some(ActionCompletion::Succeeded(summary.clone())),
                    summary,
                )
                .await;
            }
            if now >= run.expires_at {
                return begin_notification(
                    bot,
                    &run,
                    TerminalOutcome::Expired,
                    run.http_action_id,
                    Some(ActionCompletion::Succeeded(summary.clone())),
                    summary,
                )
                .await;
            }
            if run.execution_count >= run.max_executions {
                return begin_notification(
                    bot,
                    &run,
                    TerminalOutcome::Exhausted,
                    run.http_action_id,
                    Some(ActionCompletion::Succeeded(summary.clone())),
                    summary,
                )
                .await;
            }
            let next_wake_at = std::cmp::min(
                now + ChronoDuration::seconds(spec.interval_seconds as i64),
                run.expires_at,
            );
            reschedule_claim(
                &run,
                next_wake_at,
                0,
                None,
                ActionCompletion::Succeeded(summary),
                "condition_false",
            )
            .await?;
        }
        Err(error) => {
            let error_text = truncate_chars(&error.to_string(), 800);
            let failures = run.consecutive_failure_count.saturating_add(1);
            let outcome = if now >= run.expires_at {
                Some(TerminalOutcome::Expired)
            } else if run.execution_count >= run.max_executions {
                Some(TerminalOutcome::Exhausted)
            } else if failures >= run_config.max_consecutive_failures() as i32 {
                Some(TerminalOutcome::Failed)
            } else {
                None
            };
            if let Some(outcome) = outcome {
                return begin_notification(
                    bot,
                    &run,
                    outcome,
                    run.http_action_id,
                    Some(ActionCompletion::Failed(error_text.clone())),
                    json!({"last_error": error_text, "consecutive_failures": failures}),
                )
                .await;
            }
            let next_wake_at = std::cmp::min(
                now + ChronoDuration::seconds(spec.interval_seconds as i64),
                run.expires_at,
            );
            reschedule_claim(
                &run,
                next_wake_at,
                failures,
                Some(error_text.clone()),
                ActionCompletion::Failed(error_text),
                "action_failed",
            )
            .await?;
        }
    }
    RUN_WAKEUP.notify_one();
    Ok(())
}

async fn reschedule_claim(
    run: &ClaimedRun,
    next_wake_at: DateTime<Utc>,
    consecutive_failures: i32,
    last_error: Option<String>,
    action_completion: ActionCompletion,
    event_type: &str,
) -> Result<bool> {
    let pool = database_pool()?;
    let mut transaction = pool.begin().await?;
    let state = match &action_completion {
        ActionCompletion::Succeeded(result) => json!({"last_observation": result}),
        ActionCompletion::Failed(error) => json!({"last_error": error}),
    };
    let updated = query_scalar::<Postgres, i64>(
        r#"
        UPDATE kovi_bot_agent_runs
        SET status = 'active', state = $3, next_wake_at = $4,
            consecutive_failure_count = $5, last_error = $6,
            lease_token = NULL, lease_until = NULL, updated_at = NOW()
        WHERE id = $1 AND status = 'running' AND lease_token = $2
        RETURNING id
        "#,
    )
    .bind(run.id)
    .bind(&run.lease_token)
    .bind(&state)
    .bind(next_wake_at)
    .bind(consecutive_failures)
    .bind(last_error.as_deref())
    .fetch_optional(&mut *transaction)
    .await?;
    if updated.is_none() {
        transaction.rollback().await?;
        return Ok(false);
    }
    if let Some(action_id) = run.http_action_id {
        finish_action(&mut transaction, action_id, &action_completion).await?;
    }
    insert_event(
        &mut transaction,
        run.id,
        &format!("execution:{}:{event_type}", run.execution_count),
        event_type,
        json!({
            "execution": run.execution_count,
            "next_wake_at": next_wake_at,
            "consecutive_failures": consecutive_failures,
        }),
    )
    .await?;
    transaction.commit().await?;
    Ok(true)
}

async fn begin_notification(
    bot: &RuntimeBot,
    run: &ClaimedRun,
    outcome: TerminalOutcome,
    completed_action_id: Option<i64>,
    action_completion: Option<ActionCompletion>,
    final_result: Value,
) -> Result<()> {
    let content = terminal_notification_content(run, outcome);
    let pool = database_pool()?;
    let lease_until =
        Utc::now() + ChronoDuration::seconds(config::get().agent_runs().lease_secs() as i64);
    let mut transaction = pool.begin().await?;
    let updated = query_scalar::<Postgres, i64>(
        r#"
        UPDATE kovi_bot_agent_runs
        SET status = 'notifying', notification_status = 'sending',
            notification_started_at = NOW(), final_outcome = $3, result = $4,
            next_wake_at = NULL, lease_until = $5, updated_at = NOW()
        WHERE id = $1 AND status = 'running' AND lease_token = $2
        RETURNING id
        "#,
    )
    .bind(run.id)
    .bind(&run.lease_token)
    .bind(outcome.as_str())
    .bind(&final_result)
    .bind(lease_until)
    .fetch_optional(&mut *transaction)
    .await?;
    if updated.is_none() {
        transaction.rollback().await?;
        return Ok(());
    }
    if let (Some(action_id), Some(completion)) = (completed_action_id, action_completion.as_ref()) {
        finish_action(&mut transaction, action_id, completion).await?;
    }
    let notification_action_id = query_scalar::<Postgres, i64>(
        r#"
        INSERT INTO kovi_bot_agent_run_actions
            (run_id, idempotency_key, capability, effect_class, arguments)
        VALUES ($1, 'final:private.message.send', 'private.message.send', 'irreversible', $2)
        RETURNING id
        "#,
    )
    .bind(run.id)
    .bind(json!({
        "destination": "private",
        "user_id": run.owner_user_id,
        "content": content,
    }))
    .fetch_one(&mut *transaction)
    .await?;
    insert_event(
        &mut transaction,
        run.id,
        &format!("final:{}:notification_started", outcome.as_str()),
        "notification_started",
        json!({"outcome": outcome.as_str()}),
    )
    .await?;
    transaction.commit().await?;

    // `sending` 是不可逆闸门：从这里开始即使超时或重启也不会自动重放。
    let destination = MessageDestination::Private(run.owner_user_id);
    let send_result = match kovi::tokio::time::timeout(
        NOTIFICATION_SEND_TIMEOUT,
        MessageTransport::new(bot).send(destination, Message::from(content.clone())),
    )
    .await
    {
        Ok(Ok(message_id)) => Ok(message_id),
        Ok(Err(error)) => Err(format!("消息发送失败: {error:?}")),
        Err(_) => Err("消息发送超时，投递结果不确定".to_string()),
    };
    match send_result {
        Ok(message_id) => {
            record_standalone_bot_message(
                ReplyScope::Private(run.owner_user_id),
                message_id,
                &content,
            )
            .await;
            finish_notification(run, notification_action_id, outcome, Some(message_id), None)
                .await?;
        }
        Err(error) => {
            finish_notification(
                run,
                notification_action_id,
                outcome,
                None,
                Some(error.clone()),
            )
            .await?;
            eprintln!(
                "[WARN] Agent Run 终态通知结果不确定，已禁止自动重放 (Run: {}, 错误: {})",
                run.id, error
            );
        }
    }
    RUN_WAKEUP.notify_one();
    Ok(())
}

async fn finish_notification(
    run: &ClaimedRun,
    notification_action_id: i64,
    outcome: TerminalOutcome,
    message_id: Option<i32>,
    error: Option<String>,
) -> Result<()> {
    let pool = database_pool()?;
    let mut transaction = pool.begin().await?;
    let (status, notification_status, action_status) = if error.is_none() {
        (outcome.final_status(), "sent", "succeeded")
    } else {
        ("failed", "unknown", "unknown")
    };
    let updated = query_scalar::<Postgres, i64>(
        r#"
        UPDATE kovi_bot_agent_runs
        SET status = $3, notification_status = $4,
            notification_delivered_at = CASE WHEN $5::INTEGER IS NULL THEN NULL ELSE NOW() END,
            notification_message_id = $5, last_error = $6,
            lease_token = NULL, lease_until = NULL, completed_at = NOW(), updated_at = NOW()
        WHERE id = $1 AND status = 'notifying' AND lease_token = $2
          AND notification_status = 'sending'
        RETURNING id
        "#,
    )
    .bind(run.id)
    .bind(&run.lease_token)
    .bind(status)
    .bind(notification_status)
    .bind(message_id)
    .bind(error.as_deref())
    .fetch_optional(&mut *transaction)
    .await?;
    if updated.is_none() {
        transaction.rollback().await?;
        return Ok(());
    }
    query(
        r#"
        UPDATE kovi_bot_agent_run_actions
        SET status = $2, result = $3, last_error = $4, completed_at = NOW()
        WHERE id = $1 AND status = 'started'
        "#,
    )
    .bind(notification_action_id)
    .bind(action_status)
    .bind(message_id.map(|id| json!({"message_id": id})))
    .bind(error.as_deref())
    .execute(&mut *transaction)
    .await?;
    insert_event(
        &mut transaction,
        run.id,
        if error.is_none() {
            "final:notification_sent"
        } else {
            "final:notification_unknown"
        },
        if error.is_none() {
            "notification_sent"
        } else {
            "notification_unknown"
        },
        json!({
            "outcome": outcome.as_str(),
            "message_id": message_id,
            "error": error,
        }),
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn finish_action(
    transaction: &mut Transaction<'_, Postgres>,
    action_id: i64,
    completion: &ActionCompletion,
) -> Result<()> {
    match completion {
        ActionCompletion::Succeeded(result) => {
            query(
                r#"
                UPDATE kovi_bot_agent_run_actions
                SET status = 'succeeded', result = $2, last_error = NULL, completed_at = NOW()
                WHERE id = $1 AND status = 'started'
                "#,
            )
            .bind(action_id)
            .bind(result)
            .execute(&mut **transaction)
            .await?;
        }
        ActionCompletion::Failed(error) => {
            query(
                r#"
                UPDATE kovi_bot_agent_run_actions
                SET status = 'failed', last_error = $2, completed_at = NOW()
                WHERE id = $1 AND status = 'started'
                "#,
            )
            .bind(action_id)
            .bind(error)
            .execute(&mut **transaction)
            .await?;
        }
    }
    Ok(())
}

async fn claim_due(now: DateTime<Utc>, limit: i64, lease_secs: u64) -> Result<Vec<ClaimedRun>> {
    let pool = database_pool()?;
    let lease_until = now + ChronoDuration::seconds(lease_secs as i64);
    let mut transaction = pool.begin().await?;
    let rows = query(
        r#"
        SELECT id, owner_user_id, kind, spec, expires_at, execution_count,
               max_executions, consecutive_failure_count
        FROM kovi_bot_agent_runs
        WHERE status = 'active'
          AND (
            COALESCE(next_wake_at, expires_at) <= $1
            OR expires_at <= $1
            OR execution_count >= max_executions
          )
        ORDER BY LEAST(COALESCE(next_wake_at, expires_at), expires_at), id
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
        let expires_at = row.get::<DateTime<Utc>, _>("expires_at");
        let previous_execution_count = row.get::<i32, _>("execution_count");
        let max_executions = row.get::<i32, _>("max_executions");
        let reason = if expires_at <= now {
            ClaimReason::Expired
        } else if previous_execution_count >= max_executions {
            ClaimReason::Exhausted
        } else {
            ClaimReason::Execute
        };
        let execution_count = if reason == ClaimReason::Execute {
            previous_execution_count.saturating_add(1)
        } else {
            previous_execution_count
        };
        let lease_token = new_lease_token(id);
        query(
            r#"
            UPDATE kovi_bot_agent_runs
            SET status = 'running', execution_count = $2, next_wake_at = NULL,
                lease_token = $3, lease_until = $4, updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(execution_count)
        .bind(&lease_token)
        .bind(lease_until)
        .execute(&mut *transaction)
        .await?;
        let spec = row.get::<Value, _>("spec");
        let http_action_id = if reason == ClaimReason::Execute {
            let action_id = query_scalar::<Postgres, i64>(
                r#"
                INSERT INTO kovi_bot_agent_run_actions
                    (run_id, idempotency_key, capability, effect_class, arguments)
                VALUES ($1, $2, 'http.get', 'read_only', $3)
                RETURNING id
                "#,
            )
            .bind(id)
            .bind(format!("execution:{execution_count}:http.get"))
            .bind(json!({
                "method": "GET",
                "url": spec.get("url").cloned().unwrap_or(Value::Null),
            }))
            .fetch_one(&mut *transaction)
            .await?;
            insert_event(
                &mut transaction,
                id,
                &format!("execution:{execution_count}:started"),
                "timer_fired",
                json!({"execution": execution_count, "capability": "http.get"}),
            )
            .await?;
            Some(action_id)
        } else {
            insert_event(
                &mut transaction,
                id,
                &format!("terminal:{}", reason_label(reason)),
                "run_limit_reached",
                json!({"reason": reason_label(reason)}),
            )
            .await?;
            None
        };
        claimed.push(ClaimedRun {
            id,
            owner_user_id: row.get("owner_user_id"),
            kind: row.get("kind"),
            spec,
            expires_at,
            execution_count,
            max_executions,
            consecutive_failure_count: row.get("consecutive_failure_count"),
            lease_token,
            http_action_id,
            reason,
        });
    }
    transaction.commit().await?;
    Ok(claimed)
}

fn reason_label(reason: ClaimReason) -> &'static str {
    match reason {
        ClaimReason::Execute => "execute",
        ClaimReason::Expired => "expired",
        ClaimReason::Exhausted => "max_executions",
    }
}

async fn recover_stale_claims(now: DateTime<Utc>) -> Result<()> {
    let pool = database_pool()?;
    let mut transaction = pool.begin().await?;
    query(
        r#"
        UPDATE kovi_bot_agent_run_actions action
        SET status = 'failed', last_error = 'worker lease expired before action completion',
            completed_at = $1
        FROM kovi_bot_agent_runs run
        WHERE action.run_id = run.id AND action.status = 'started'
          AND action.effect_class = 'read_only' AND run.status = 'running'
          AND run.lease_until IS NOT NULL AND run.lease_until <= $1
        "#,
    )
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    query(
        r#"
        WITH recovered AS (
            UPDATE kovi_bot_agent_runs
            SET status = 'active', next_wake_at = $1, lease_token = NULL, lease_until = NULL,
                last_error = '上一次执行租约过期，已安排安全恢复', updated_at = $1
            WHERE status = 'running' AND lease_until IS NOT NULL AND lease_until <= $1
            RETURNING id, execution_count
        )
        INSERT INTO kovi_bot_agent_run_events (run_id, event_key, event_type, payload)
        SELECT id, 'execution:' || execution_count || ':lease_recovered',
               'lease_recovered', jsonb_build_object('execution', execution_count)
        FROM recovered
        ON CONFLICT (run_id, event_key) DO NOTHING
        "#,
    )
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    query(
        r#"
        UPDATE kovi_bot_agent_run_actions action
        SET status = 'unknown', last_error = 'notification lease expired; delivery was not replayed',
            completed_at = $1
        FROM kovi_bot_agent_runs run
        WHERE action.run_id = run.id AND action.status = 'started'
          AND action.effect_class = 'irreversible' AND run.status = 'notifying'
          AND run.lease_until IS NOT NULL AND run.lease_until <= $1
        "#,
    )
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    query(
        r#"
        WITH abandoned AS (
            UPDATE kovi_bot_agent_runs
            SET status = 'failed', notification_status = 'unknown',
                last_error = '通知发送期间运行时中断，投递结果不确定且不会自动重放',
                lease_token = NULL, lease_until = NULL, completed_at = $1, updated_at = $1
            WHERE status = 'notifying' AND notification_status = 'sending'
              AND lease_until IS NOT NULL AND lease_until <= $1
            RETURNING id
        )
        INSERT INTO kovi_bot_agent_run_events (run_id, event_key, event_type, payload)
        SELECT id, 'final:notification_abandoned', 'notification_unknown',
               jsonb_build_object('reason', 'worker_lease_expired')
        FROM abandoned
        ON CONFLICT (run_id, event_key) DO NOTHING
        "#,
    )
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn next_scheduler_delay() -> Result<Duration> {
    let recovery = Duration::from_secs(config::get().agent_runs().recovery_scan_secs());
    let next = query_scalar::<Postgres, Option<DateTime<Utc>>>(
        r#"
        SELECT MIN(candidate_at) FROM (
            SELECT LEAST(COALESCE(next_wake_at, expires_at), expires_at) AS candidate_at
            FROM kovi_bot_agent_runs WHERE status = 'active'
            UNION ALL
            SELECT lease_until AS candidate_at
            FROM kovi_bot_agent_runs
            WHERE status IN ('running', 'notifying') AND lease_until IS NOT NULL
        ) candidates
        "#,
    )
    .fetch_one(database_pool()?)
    .await?;
    let Some(next) = next else {
        return Ok(recovery);
    };
    let millis = (next - Utc::now()).num_milliseconds().max(0) as u64;
    Ok(std::cmp::min(Duration::from_millis(millis), recovery))
}

pub(crate) async fn create_from_tool(
    arguments: &Map<String, Value>,
    actor_user_id: i64,
    source_message_id: i32,
) -> Result<String> {
    ensure!(
        config::get().agent_runs().enabled(),
        "Agent Run Runtime 当前未启用"
    );
    ensure!(actor_user_id > 0, "创建者账号无效");
    let now = Utc::now();
    let request = parse_create_request(arguments, now, config::get().agent_runs())?;
    let request_key = format!("private:{actor_user_id}:{source_message_id}:agent.run.create");
    let spec_value = serde_json::to_value(&request.spec)?;
    let pool = database_pool()?;
    let mut transaction = pool.begin().await?;
    lock_global(&mut transaction).await?;
    lock_owner(&mut transaction, actor_user_id).await?;

    if let Some(row) = query(
        r#"
        SELECT id, owner_user_id, spec, status, expires_at, max_executions
        FROM kovi_bot_agent_runs WHERE request_key = $1
        "#,
    )
    .bind(&request_key)
    .fetch_optional(&mut *transaction)
    .await?
    {
        ensure!(
            row.get::<i64, _>("owner_user_id") == actor_user_id
                && row.get::<Value, _>("spec") == spec_value,
            "同一条来源消息已经绑定了不同的 Agent Run"
        );
        let id = row.get::<i64, _>("id");
        let status = row.get::<String, _>("status");
        let expires_at = row.get::<DateTime<Utc>, _>("expires_at");
        transaction.commit().await?;
        return Ok(format_created_run(
            id,
            &request.spec,
            expires_at,
            &status,
            true,
        ));
    }

    let owner_count = query_scalar::<Postgres, i64>(
        "SELECT COUNT(*) FROM kovi_bot_agent_runs WHERE owner_user_id = $1 AND status IN ('active', 'running', 'notifying')",
    )
    .bind(actor_user_id)
    .fetch_one(&mut *transaction)
    .await?;
    let run_config = config::get().agent_runs().clone();
    ensure!(
        owner_count < run_config.max_active_per_user() as i64,
        "你的未完成 Agent Run 已达到上限 {}",
        run_config.max_active_per_user()
    );
    let total_count = query_scalar::<Postgres, i64>(
        "SELECT COUNT(*) FROM kovi_bot_agent_runs WHERE status IN ('active', 'running', 'notifying')",
    )
    .fetch_one(&mut *transaction)
    .await?;
    ensure!(
        total_count < run_config.max_active_total() as i64,
        "系统未完成 Agent Run 已达到上限，请稍后再试"
    );

    let id = query_scalar::<Postgres, i64>(
        r#"
        INSERT INTO kovi_bot_agent_runs
            (request_key, owner_user_id, source_scope, source_id, source_message_id,
             kind, spec, next_wake_at, expires_at, max_executions)
        VALUES ($1, $2, 'private', $2, $3, 'url_watch', $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(&request_key)
    .bind(actor_user_id)
    .bind(source_message_id)
    .bind(&spec_value)
    .bind(now)
    .bind(request.expires_at)
    .bind(request.spec.max_executions as i32)
    .fetch_one(&mut *transaction)
    .await?;
    insert_event(
        &mut transaction,
        id,
        "run:created",
        "run_created",
        json!({
            "kind": "url_watch",
            "next_wake_at": now,
            "expires_at": request.expires_at,
        }),
    )
    .await?;
    transaction.commit().await?;
    RUN_WAKEUP.notify_one();
    Ok(format_created_run(
        id,
        &request.spec,
        request.expires_at,
        "active",
        false,
    ))
}

pub(crate) async fn status_from_tool(
    arguments: &Map<String, Value>,
    actor_user_id: i64,
) -> Result<String> {
    reject_unknown_arguments(arguments, &["run_id"])?;
    let run_id = optional_positive_i64(arguments, "run_id")?;
    let rows = if let Some(run_id) = run_id {
        query(
            r#"
            SELECT id, kind, spec, state, status, next_wake_at, expires_at,
                   execution_count, max_executions, consecutive_failure_count,
                   notification_status, final_outcome, last_error, created_at
            FROM kovi_bot_agent_runs
            WHERE id = $1 AND owner_user_id = $2
            "#,
        )
        .bind(run_id)
        .bind(actor_user_id)
        .fetch_all(database_pool()?)
        .await?
    } else {
        query(
            r#"
            SELECT id, kind, spec, state, status, next_wake_at, expires_at,
                   execution_count, max_executions, consecutive_failure_count,
                   notification_status, final_outcome, last_error, created_at
            FROM kovi_bot_agent_runs
            WHERE owner_user_id = $1
            ORDER BY CASE WHEN status IN ('active', 'running', 'notifying') THEN 0 ELSE 1 END,
                     created_at DESC
            LIMIT $2
            "#,
        )
        .bind(actor_user_id)
        .bind(MAX_LIST_ITEMS)
        .fetch_all(database_pool()?)
        .await?
    };
    if rows.is_empty() {
        return Ok(match run_id {
            Some(id) => format!("没有找到属于你的 Agent Run #{id}。"),
            None => "你还没有 Agent Run。".to_string(),
        });
    }
    let mut output = if run_id.is_some() {
        String::new()
    } else {
        String::from("你的 Agent Run：")
    };
    for row in rows {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format_run_status(&row));
    }
    Ok(output)
}

pub(crate) async fn cancel_from_tool(
    arguments: &Map<String, Value>,
    actor_user_id: i64,
) -> Result<String> {
    reject_unknown_arguments(arguments, &["run_id"])?;
    let requested_id = optional_positive_i64(arguments, "run_id")?;
    let pool = database_pool()?;
    let run_id = if let Some(id) = requested_id {
        id
    } else {
        let rows = query_scalar::<Postgres, i64>(
            r#"
            SELECT id FROM kovi_bot_agent_runs
            WHERE owner_user_id = $1 AND status IN ('active', 'running')
            ORDER BY created_at DESC LIMIT 2
            "#,
        )
        .bind(actor_user_id)
        .fetch_all(pool)
        .await?;
        match rows.as_slice() {
            [id] => *id,
            [] => return Ok("你当前没有可取消的 Agent Run。".to_string()),
            _ => return Ok("你有多个运行中的 Agent Run，请先查看状态并指定 run_id。".to_string()),
        }
    };
    let mut transaction = pool.begin().await?;
    let cancelled = query_scalar::<Postgres, i64>(
        r#"
        UPDATE kovi_bot_agent_runs
        SET status = 'cancelled', next_wake_at = NULL, lease_token = NULL, lease_until = NULL,
            completed_at = NOW(), updated_at = NOW(), last_error = NULL
        WHERE id = $1 AND owner_user_id = $2 AND status IN ('active', 'running')
        RETURNING id
        "#,
    )
    .bind(run_id)
    .bind(actor_user_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if cancelled.is_none() {
        transaction.rollback().await?;
        return Ok(format!(
            "没有找到可取消的 Agent Run #{run_id}；它可能已经结束，或通知发送已经开始。"
        ));
    }
    query(
        r#"
        UPDATE kovi_bot_agent_run_actions
        SET status = 'failed', last_error = 'run cancelled by owner', completed_at = NOW()
        WHERE run_id = $1 AND status = 'started' AND effect_class = 'read_only'
        "#,
    )
    .bind(run_id)
    .execute(&mut *transaction)
    .await?;
    insert_event(
        &mut transaction,
        run_id,
        "run:cancelled",
        "run_cancelled",
        json!({"actor_user_id": actor_user_id}),
    )
    .await?;
    transaction.commit().await?;
    RUN_WAKEUP.notify_one();
    Ok(format!("Agent Run #{run_id} 已取消。"))
}

pub(crate) fn looks_like_agent_run_request(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if [
        "取消监控",
        "取消监测",
        "停止监控",
        "停止监测",
        "不用盯",
        "查看状态",
        "任务状态",
    ]
    .iter()
    .any(|marker| text.contains(marker))
    {
        return false;
    }
    let recurring = [
        "每隔",
        "每过",
        "每秒",
        "每分钟",
        "每小时",
        "定期",
        "持续",
        "反复",
        "轮询",
        "监控",
        "监测",
        "盯着",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || lower.contains("poll ")
        || lower.contains("every ");
    let web_target = text.contains("接口")
        || text.contains("链接")
        || text.contains("网站")
        || text.contains("网页")
        || lower.contains("url")
        || lower.contains("http://")
        || lower.contains("https://");
    let completion = [
        "直到",
        "等到",
        "返回",
        "出现",
        "变成",
        "满足",
        "之后告诉",
        "就告诉",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || lower.contains("until ");
    recurring && web_target && completion
}

pub(crate) async fn compact_expired() -> Result<u64> {
    let cutoff = Utc::now() - ChronoDuration::days(config::get().memory().retention_days().max(1));
    Ok(query(
        r#"
        DELETE FROM kovi_bot_agent_runs
        WHERE status IN ('completed', 'cancelled', 'failed', 'expired')
          AND completed_at IS NOT NULL AND completed_at < $1
        "#,
    )
    .bind(cutoff)
    .execute(database_pool()?)
    .await?
    .rows_affected())
}

pub(crate) async fn delete_user_data(user_id: i64) -> Result<u64> {
    Ok(query(
        "DELETE FROM kovi_bot_agent_runs WHERE owner_user_id = $1 OR (source_scope = 'private' AND source_id = $1)",
    )
    .bind(user_id)
    .execute(database_pool()?)
    .await?
    .rows_affected())
}

fn parse_create_request(
    arguments: &Map<String, Value>,
    now: DateTime<Utc>,
    run_config: &config::AgentRunConfig,
) -> Result<CreateRunRequest> {
    reject_unknown_arguments(
        arguments,
        &[
            "url",
            "interval_seconds",
            "condition",
            "expected",
            "json_pointer",
            "notification_message",
            "stop_after_minutes",
            "max_executions",
        ],
    )?;
    let raw_url = required_string(arguments, "url", MAX_URL_CHARS)?;
    let mut url = validate_public_url(&raw_url)?;
    url.set_fragment(None);
    let interval_seconds = arguments
        .get("interval_seconds")
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| anyhow!("interval_seconds 必须是正整数"))
        })
        .transpose()?
        .unwrap_or_else(|| run_config.default_interval_secs());
    ensure!(
        (run_config.min_interval_secs()..=run_config.max_interval_secs())
            .contains(&interval_seconds),
        "interval_seconds 必须在 {} 到 {} 秒之间",
        run_config.min_interval_secs(),
        run_config.max_interval_secs()
    );
    let condition = match required_string(arguments, "condition", 40)?.as_str() {
        "text_contains" => UrlConditionKind::TextContains,
        "text_not_contains" => UrlConditionKind::TextNotContains,
        "text_equals" => UrlConditionKind::TextEquals,
        "status_equals" => UrlConditionKind::StatusEquals,
        "json_pointer_equals" => UrlConditionKind::JsonPointerEquals,
        _ => return Err(anyhow!("condition 不受支持")),
    };
    let raw_expected = arguments
        .get("expected")
        .ok_or_else(|| anyhow!("缺少 expected"))?;
    let expected = match condition {
        UrlConditionKind::TextContains
        | UrlConditionKind::TextNotContains
        | UrlConditionKind::TextEquals => {
            let value = raw_expected
                .as_str()
                .ok_or_else(|| anyhow!("文本条件的 expected 必须是字符串"))?;
            ensure!(!value.is_empty(), "文本条件的 expected 不能为空");
            ensure!(
                value.chars().count() <= MAX_EXPECTED_TEXT_CHARS,
                "expected 过长"
            );
            Value::String(value.to_string())
        }
        UrlConditionKind::StatusEquals => {
            let value = raw_expected
                .as_u64()
                .filter(|value| (100..=599).contains(value))
                .ok_or_else(|| anyhow!("status_equals 的 expected 必须是 100 到 599"))?;
            json!(value)
        }
        UrlConditionKind::JsonPointerEquals => {
            ensure!(
                serde_json::to_string(raw_expected)?.chars().count() <= MAX_JSON_EXPECTED_CHARS,
                "JSON expected 过长"
            );
            raw_expected.clone()
        }
    };
    let json_pointer = match condition {
        UrlConditionKind::JsonPointerEquals => {
            let pointer = required_string(arguments, "json_pointer", MAX_JSON_POINTER_CHARS)?;
            ensure!(
                valid_json_pointer(&pointer),
                "json_pointer 不是合法的 JSON Pointer"
            );
            Some(pointer)
        }
        _ => {
            ensure!(
                !arguments.contains_key("json_pointer"),
                "只有 json_pointer_equals 支持 json_pointer"
            );
            None
        }
    };
    let stop_after_minutes = arguments
        .get("stop_after_minutes")
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| anyhow!("stop_after_minutes 必须是正整数"))
        })
        .transpose()?
        .unwrap_or_else(|| run_config.default_stop_after_minutes());
    ensure!(
        (1..=run_config.max_stop_after_minutes()).contains(&stop_after_minutes),
        "stop_after_minutes 必须在 1 到 {} 分钟之间",
        run_config.max_stop_after_minutes()
    );
    let max_executions = arguments
        .get("max_executions")
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| anyhow!("max_executions 必须是正整数"))
        })
        .transpose()?
        .unwrap_or_else(|| run_config.default_max_executions());
    ensure!(
        (1..=run_config.max_executions_per_run()).contains(&max_executions),
        "max_executions 必须在 1 到 {} 之间",
        run_config.max_executions_per_run()
    );
    let default_notification = format!(
        "你让我盯着的接口已经满足条件：{}。",
        condition_description(condition, &expected, json_pointer.as_deref())
    );
    let notification_message = match arguments.get("notification_message") {
        Some(value) => normalize_notification(
            value
                .as_str()
                .ok_or_else(|| anyhow!("notification_message 必须是字符串"))?,
            run_config.max_notification_chars(),
        )?,
        None => normalize_notification(&default_notification, run_config.max_notification_chars())?,
    };
    let spec = UrlWatchSpec {
        url: url.to_string(),
        interval_seconds,
        condition,
        expected,
        json_pointer,
        notification_message,
        stop_after_minutes,
        max_executions,
    };
    Ok(CreateRunRequest {
        expires_at: now + ChronoDuration::minutes(stop_after_minutes as i64),
        spec,
    })
}

fn parse_stored_spec(kind: &str, value: &Value) -> Result<UrlWatchSpec> {
    ensure!(kind == "url_watch", "当前 Runtime 不支持 Run kind={kind}");
    let spec: UrlWatchSpec = serde_json::from_value(value.clone())
        .map_err(|error| anyhow!("URL Watch 参数损坏: {error}"))?;
    ensure!(spec.interval_seconds > 0, "URL Watch 间隔无效");
    Ok(spec)
}

fn condition_matches(spec: &UrlWatchSpec, observation: &HttpObservation) -> bool {
    match spec.condition {
        UrlConditionKind::StatusEquals => spec
            .expected
            .as_u64()
            .is_some_and(|expected| u64::from(observation.status) == expected),
        UrlConditionKind::TextContains => {
            (200..300).contains(&observation.status)
                && spec
                    .expected
                    .as_str()
                    .is_some_and(|expected| observation.body.contains(expected))
        }
        UrlConditionKind::TextNotContains => {
            (200..300).contains(&observation.status)
                && spec
                    .expected
                    .as_str()
                    .is_some_and(|expected| !observation.body.contains(expected))
        }
        UrlConditionKind::TextEquals => {
            (200..300).contains(&observation.status)
                && spec
                    .expected
                    .as_str()
                    .is_some_and(|expected| observation.body.trim() == expected)
        }
        UrlConditionKind::JsonPointerEquals => {
            if !(200..300).contains(&observation.status) {
                return false;
            }
            serde_json::from_str::<Value>(&observation.body)
                .ok()
                .and_then(|body| {
                    body.pointer(spec.json_pointer.as_deref().unwrap_or_default())
                        .cloned()
                })
                .is_some_and(|value| value == spec.expected)
        }
    }
}

fn observation_summary(observation: &HttpObservation, max_preview_chars: usize) -> Value {
    json!({
        "status": observation.status,
        "content_type": observation.content_type,
        "body_chars": observation.body.chars().count(),
        "body_preview": truncate_chars(&observation.body, max_preview_chars),
    })
}

fn terminal_notification_content(run: &ClaimedRun, outcome: TerminalOutcome) -> String {
    let custom = parse_stored_spec(&run.kind, &run.spec)
        .ok()
        .map(|spec| spec.notification_message);
    let content = match outcome {
        TerminalOutcome::Matched => {
            custom.unwrap_or_else(|| "你让我盯着的接口已经满足目标条件了。".to_string())
        }
        TerminalOutcome::Expired => {
            "我盯到设定的截止时间了，但接口还没有满足目标条件，这次监测已经停止。".to_string()
        }
        TerminalOutcome::Exhausted => {
            "接口监测已经达到你设定的最大检查次数，但还没有满足目标条件，这次监测已经停止。"
                .to_string()
        }
        TerminalOutcome::Failed => {
            "接口连续多次无法可靠读取，我已经停止这次监测，避免继续无效请求。".to_string()
        }
    };
    truncate_chars(&format!("{content}\nAgent Run #{}", run.id), 2_000)
}

fn format_created_run(
    id: i64,
    spec: &UrlWatchSpec,
    expires_at: DateTime<Utc>,
    status: &str,
    existing: bool,
) -> String {
    let schedule = if matches!(status, "active" | "running" | "notifying") {
        if existing {
            format!("该 Run 已经在处理，检查间隔 {} 秒", spec.interval_seconds)
        } else {
            format!(
                "首次检查已立即安排，之后每 {} 秒检查一次",
                spec.interval_seconds
            )
        }
    } else {
        format!("该 Run 已结束，原检查间隔 {} 秒", spec.interval_seconds)
    };
    format!(
        "Agent Run {}：#{}，状态 {}；{}；条件：{}；截止 {}；最多执行 {} 次。",
        if existing { "已存在" } else { "已创建" },
        id,
        status_label(status),
        schedule,
        condition_description(spec.condition, &spec.expected, spec.json_pointer.as_deref()),
        format_time(expires_at),
        spec.max_executions,
    )
}

fn format_run_status(row: &sqlx_postgres::PgRow) -> String {
    let id = row.get::<i64, _>("id");
    let status = row.get::<String, _>("status");
    let spec_value = row.get::<Value, _>("spec");
    let spec = serde_json::from_value::<UrlWatchSpec>(spec_value).ok();
    let execution_count = row.get::<i32, _>("execution_count");
    let max_executions = row.get::<i32, _>("max_executions");
    let expires_at = row.get::<DateTime<Utc>, _>("expires_at");
    let next_wake_at = row.get::<Option<DateTime<Utc>>, _>("next_wake_at");
    let failures = row.get::<i32, _>("consecutive_failure_count");
    let notification_status = row.get::<String, _>("notification_status");
    let state = row.get::<Value, _>("state");
    let last_status = state
        .pointer("/last_observation/status")
        .and_then(Value::as_u64)
        .map(|status| format!("，最近 HTTP {status}"))
        .unwrap_or_default();
    let target = spec
        .as_ref()
        .map(|spec| {
            format!(
                "{}，{}",
                display_url(&spec.url),
                condition_description(spec.condition, &spec.expected, spec.json_pointer.as_deref())
            )
        })
        .unwrap_or_else(|| "参数不可读".to_string());
    let next = next_wake_at
        .map(|time| format!("，下次 {}", format_time(time)))
        .unwrap_or_default();
    let failure = if failures > 0 {
        format!("，连续失败 {failures} 次")
    } else {
        String::new()
    };
    let notification = if notification_status == "unknown" {
        "，最终通知投递结果不确定且未重放"
    } else {
        ""
    };
    format!(
        "- #{} [{}] {}；检查 {}/{} 次{}{}{}；截止 {}{}",
        id,
        status_label(&status),
        target,
        execution_count,
        max_executions,
        last_status,
        failure,
        next,
        format_time(expires_at),
        notification,
    )
}

fn condition_description(
    condition: UrlConditionKind,
    expected: &Value,
    json_pointer: Option<&str>,
) -> String {
    match condition {
        UrlConditionKind::TextContains => {
            format!("正文包含 {}", compact_json_value(expected))
        }
        UrlConditionKind::TextNotContains => {
            format!("正文不包含 {}", compact_json_value(expected))
        }
        UrlConditionKind::TextEquals => {
            format!("正文等于 {}", compact_json_value(expected))
        }
        UrlConditionKind::StatusEquals => {
            format!("HTTP 状态等于 {}", compact_json_value(expected))
        }
        UrlConditionKind::JsonPointerEquals => format!(
            "JSON {} 等于 {}",
            json_pointer.unwrap_or(""),
            compact_json_value(expected)
        ),
    }
}

fn compact_json_value(value: &Value) -> String {
    truncate_chars(
        &serde_json::to_string(value).unwrap_or_else(|_| "?".to_string()),
        120,
    )
}

fn display_url(raw_url: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw_url) else {
        return "URL 不可读".to_string();
    };
    url.set_query(None);
    url.set_fragment(None);
    truncate_chars(url.as_str(), 160)
}

fn status_label(status: &str) -> &'static str {
    match status {
        "active" => "等待中",
        "running" => "检查中",
        "notifying" => "通知中",
        "completed" => "已完成",
        "cancelled" => "已取消",
        "failed" => "失败",
        "expired" => "已结束",
        _ => "未知",
    }
}

fn format_time(time: DateTime<Utc>) -> String {
    let timezone = config::get()
        .reminders()
        .default_timezone()
        .parse::<Tz>()
        .unwrap_or(chrono_tz::Asia::Shanghai);
    format!(
        "{} ({})",
        time.with_timezone(&timezone).format("%Y-%m-%d %H:%M:%S"),
        timezone
    )
}

fn valid_json_pointer(pointer: &str) -> bool {
    if !pointer.starts_with('/') {
        return false;
    }
    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            if index + 1 >= bytes.len() || !matches!(bytes[index + 1], b'0' | b'1') {
                return false;
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    true
}

fn normalize_notification(value: &str, max_chars: usize) -> Result<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(!value.is_empty(), "notification_message 不能为空");
    ensure!(
        value.chars().count() <= max_chars,
        "notification_message 不能超过 {} 个字符",
        max_chars
    );
    Ok(value)
}

fn required_string(arguments: &Map<String, Value>, name: &str, max_chars: usize) -> Result<String> {
    let value = arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("参数 {name} 必须是字符串"))?
        .trim();
    ensure!(!value.is_empty(), "参数 {name} 不能为空");
    ensure!(value.chars().count() <= max_chars, "参数 {name} 过长");
    Ok(value.to_string())
}

fn optional_positive_i64(arguments: &Map<String, Value>, name: &str) -> Result<Option<i64>> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };
    value
        .as_i64()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| anyhow!("参数 {name} 必须是正整数"))
}

fn reject_unknown_arguments(arguments: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
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

async fn insert_event(
    transaction: &mut Transaction<'_, Postgres>,
    run_id: i64,
    event_key: &str,
    event_type: &str,
    payload: Value,
) -> Result<()> {
    query(
        r#"
        INSERT INTO kovi_bot_agent_run_events (run_id, event_key, event_type, payload)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (run_id, event_key) DO NOTHING
        "#,
    )
    .bind(run_id)
    .bind(event_key)
    .bind(event_type)
    .bind(payload)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn lock_global(transaction: &mut Transaction<'_, Postgres>) -> Result<()> {
    query("SELECT pg_advisory_xact_lock(hashtextextended('agent-run:global', 0))")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn lock_owner(transaction: &mut Transaction<'_, Postgres>, owner_user_id: i64) -> Result<()> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("agent-run:owner:{owner_user_id}"))
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn database_pool() -> Result<&'static PgPool> {
    MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 记忆连接池尚未初始化"))
}

fn new_lease_token(run_id: i64) -> String {
    let sequence = LEASE_TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}:{run_id}:{sequence}:{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::{
        HttpObservation, UrlConditionKind, UrlWatchSpec, condition_matches,
        looks_like_agent_run_request, parse_create_request, valid_json_pointer,
    };
    use crate::config::AgentRunConfig;
    use chrono::{TimeZone, Utc};
    use serde_json::{Map, Value, json};
    use sqlx_core::row::Row;
    use sqlx_postgres::Postgres;

    fn spec(condition: UrlConditionKind, expected: Value) -> UrlWatchSpec {
        UrlWatchSpec {
            url: "https://example.com/health".to_string(),
            interval_seconds: 30,
            condition,
            expected,
            json_pointer: None,
            notification_message: "好了".to_string(),
            stop_after_minutes: 60,
            max_executions: 100,
        }
    }

    #[test]
    fn detects_continuous_url_work_without_confusing_status_or_cancel_requests() {
        assert!(looks_like_agent_run_request(
            "每隔30秒请求一下 https://example.com/health，直到返回 ready 之后告诉我"
        ));
        assert!(looks_like_agent_run_request(
            "持续监控这个接口，等到状态变成200就告诉我"
        ));
        assert!(looks_like_agent_run_request(
            "30秒后开始每隔一分钟请求这个接口，直到返回ready告诉我"
        ));
        assert!(looks_like_agent_run_request(
            "每分钟检查这个接口，HTTP 状态变成 200 就通知我"
        ));
        assert!(!looks_like_agent_run_request("查看接口监控任务状态"));
        assert!(!looks_like_agent_run_request("停止监控这个链接"));
        assert!(!looks_like_agent_run_request("帮我请求一下这个链接"));
    }

    #[test]
    fn evaluates_text_status_and_json_conditions() {
        let observation = HttpObservation {
            status: 200,
            content_type: "application/json".to_string(),
            body: r#"{"state":"ready","nested":{"ok":true}}"#.to_string(),
        };
        assert!(condition_matches(
            &spec(UrlConditionKind::TextContains, json!("ready")),
            &observation
        ));
        assert!(condition_matches(
            &spec(UrlConditionKind::StatusEquals, json!(200)),
            &observation
        ));
        let mut json_spec = spec(UrlConditionKind::JsonPointerEquals, json!(true));
        json_spec.json_pointer = Some("/nested/ok".to_string());
        assert!(condition_matches(&json_spec, &observation));
    }

    #[test]
    fn body_conditions_do_not_match_error_responses() {
        let observation = HttpObservation {
            status: 503,
            content_type: "text/plain".to_string(),
            body: "not ready".to_string(),
        };
        assert!(!condition_matches(
            &spec(UrlConditionKind::TextNotContains, json!("ready")),
            &observation
        ));
    }

    #[test]
    fn parses_bounded_create_request() {
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            json!("https://example.com/status#fragment"),
        );
        arguments.insert("interval_seconds".to_string(), json!(15));
        arguments.insert("condition".to_string(), json!("status_equals"));
        arguments.insert("expected".to_string(), json!(204));
        arguments.insert("stop_after_minutes".to_string(), json!(60));
        arguments.insert("max_executions".to_string(), json!(100));
        let now = Utc.with_ymd_and_hms(2026, 8, 25, 12, 0, 0).unwrap();
        let request = parse_create_request(&arguments, now, &AgentRunConfig::default()).unwrap();
        assert_eq!(request.spec.interval_seconds, 15);
        assert_eq!(request.spec.url, "https://example.com/status");
        assert_eq!(request.expires_at, now + chrono::Duration::minutes(60));
    }

    #[test]
    fn validates_json_pointer_escape_sequences() {
        assert!(valid_json_pointer("/result/items/0/state"));
        assert!(valid_json_pointer("/a~1b/~0value"));
        assert!(!valid_json_pointer("result/state"));
        assert!(!valid_json_pointer("/bad~2escape"));
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_run_creation_and_claim_are_atomic() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                crate::memory::MEMORY_MANAGER
                    .initialize_database()
                    .await
                    .expect("应初始化 PostgreSQL 记忆连接池");
                super::initialize_database()
                    .await
                    .expect("应初始化 Agent Run 表");
                let actor_user_id = Utc::now().timestamp_micros();
                let source_message_id =
                    ((actor_user_id % i64::from(i32::MAX - 1)) as i32).max(1);
                let arguments: Map<String, Value> = serde_json::from_value(json!({
                    "url": "https://example.com/health",
                    "interval_seconds": 30,
                    "condition": "status_equals",
                    "expected": 204,
                    "stop_after_minutes": 60,
                    "max_executions": 20
                }))
                .expect("应构造工具参数");
                let (first, second) = kovi::tokio::join!(
                    super::create_from_tool(&arguments, actor_user_id, source_message_id),
                    super::create_from_tool(&arguments, actor_user_id, source_message_id),
                );
                first.expect("第一次创建不应失败");
                second.expect("同一来源消息重放应返回原 Run");

                let request_key =
                    format!("private:{actor_user_id}:{source_message_id}:agent.run.create");
                let pool = super::database_pool().expect("连接池应存在");
                let run_ids = sqlx_core::query_scalar::query_scalar::<Postgres, i64>(
                    "SELECT id FROM kovi_bot_agent_runs WHERE request_key = $1",
                )
                .bind(&request_key)
                .fetch_all(pool)
                .await
                .expect("应读取测试 Run");
                assert_eq!(run_ids.len(), 1, "同一来源消息只能创建一个 Run");
                let run_id = run_ids[0];

                let now = Utc::now();
                let (first_claim, second_claim) = kovi::tokio::join!(
                    super::claim_due(now, 32, 60),
                    super::claim_due(now, 32, 60),
                );
                let claimed = first_claim
                    .expect("第一个 worker 领取不应失败")
                    .into_iter()
                    .chain(
                        second_claim
                            .expect("第二个 worker 领取不应失败"),
                    )
                    .filter(|run| run.id == run_id)
                    .collect::<Vec<_>>();
                assert_eq!(claimed.len(), 1, "同一个 tick 只能被一个 worker 领取");
                let run = claimed.into_iter().next().expect("测试 Run 应被领取");
                assert_eq!(run.execution_count, 1);

                let action_count =
                    sqlx_core::query_scalar::query_scalar::<Postgres, i64>(
                        "SELECT COUNT(*) FROM kovi_bot_agent_run_actions WHERE run_id = $1 AND idempotency_key = 'execution:1:http.get'",
                    )
                    .bind(run_id)
                    .fetch_one(pool)
                    .await
                    .expect("应读取动作数量");
                let event_count = sqlx_core::query_scalar::query_scalar::<Postgres, i64>(
                    "SELECT COUNT(*) FROM kovi_bot_agent_run_events WHERE run_id = $1 AND event_key = 'execution:1:started'",
                )
                .bind(run_id)
                .fetch_one(pool)
                .await
                .expect("应读取事件数量");
                assert_eq!(action_count, 1);
                assert_eq!(event_count, 1);

                super::reschedule_claim(
                    &run,
                    Utc::now() + chrono::Duration::seconds(30),
                    0,
                    None,
                    super::ActionCompletion::Succeeded(json!({"status": 200})),
                    "condition_false",
                )
                .await
                .expect("应完成并重排测试 tick");

                sqlx_core::query::query(
                    r#"
                    UPDATE kovi_bot_agent_runs
                    SET status = 'notifying', notification_status = 'sending',
                        lease_token = $2, lease_until = NOW() + INTERVAL '60 seconds',
                        final_outcome = 'matched'
                    WHERE id = $1
                    "#,
                )
                .bind(run_id)
                .bind(&run.lease_token)
                .execute(pool)
                .await
                .expect("应模拟通知发送闸门");
                let notification_action_id =
                    sqlx_core::query_scalar::query_scalar::<Postgres, i64>(
                        r#"
                        INSERT INTO kovi_bot_agent_run_actions
                            (run_id, idempotency_key, capability, effect_class, arguments)
                        VALUES ($1, 'final:private.message.send', 'private.message.send',
                                'irreversible', '{}'::jsonb)
                        RETURNING id
                        "#,
                    )
                    .bind(run_id)
                    .fetch_one(pool)
                    .await
                    .expect("应写入通知动作");
                super::finish_notification(
                    &run,
                    notification_action_id,
                    super::TerminalOutcome::Matched,
                    Some(70_000_001),
                    None,
                )
                .await
                .expect("应完成通知状态回写");
                let terminal = sqlx_core::query::query(
                    "SELECT status, notification_status FROM kovi_bot_agent_runs WHERE id = $1",
                )
                .bind(run_id)
                .fetch_one(pool)
                .await
                .expect("应读取终态");
                assert_eq!(terminal.get::<String, _>("status"), "completed");
                assert_eq!(
                    terminal.get::<String, _>("notification_status"),
                    "sent"
                );

                sqlx_core::query::query("DELETE FROM kovi_bot_agent_runs WHERE id = $1")
                    .bind(run_id)
                    .execute(pool)
                    .await
                    .expect("应清理测试 Run");
            });
    }
}
