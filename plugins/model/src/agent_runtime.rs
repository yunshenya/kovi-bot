//! 持久化角色目标与受控动作执行边界。
//!
//! 模型只能提出动作；权限、幂等、持久化和真实副作用全部由本模块处理。

use crate::group_access;
use crate::memory::MEMORY_MANAGER;
use crate::model::{
    MessageDestination, OutgoingSource, ReplyTicket, is_current,
    send_tracked_message_with_revalidation,
};
use anyhow::{Context, Result, anyhow, ensure};
use chrono::{Duration as ChronoDuration, Utc};
use kovi::{Message, RuntimeBot};
use serde_json::{Value, json};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, Postgres};

const MAX_GROUP_MESSAGE_CHARS: usize = 1_000;
const MAX_GROUP_ACTIONS_PER_MINUTE: i64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentAction {
    SendGroupMessage {
        group_id: i64,
        content: String,
        collect_replies_minutes: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentActionContext {
    pub(crate) actor_user_id: i64,
    pub(crate) source: MessageDestination,
    pub(crate) source_message_id: i32,
}

#[derive(Debug)]
struct StoredGoal {
    id: i64,
    status: String,
    payload: Value,
    result: Option<Value>,
    last_error: Option<String>,
}

enum GoalReservation {
    New(i64),
    Existing(StoredGoal),
}

pub(crate) async fn initialize_database() -> Result<()> {
    let pool = database_pool()?;
    query(
        r#"
        CREATE TABLE IF NOT EXISTS kovi_bot_agent_goals (
            id BIGSERIAL PRIMARY KEY,
            request_key TEXT NOT NULL UNIQUE,
            actor_user_id BIGINT NOT NULL,
            source_scope TEXT NOT NULL CHECK (source_scope IN ('private')),
            source_id BIGINT NOT NULL,
            source_message_id INTEGER NOT NULL,
            action_kind TEXT NOT NULL CHECK (action_kind IN ('send_group_message')),
            target_scope TEXT NOT NULL CHECK (target_scope IN ('group')),
            target_id BIGINT NOT NULL,
            payload JSONB NOT NULL,
            status TEXT NOT NULL DEFAULT 'received'
                CHECK (status IN ('received', 'executing', 'completed', 'failed')),
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
    .context("创建角色目标表")?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_agent_goals_actor_idx ON kovi_bot_agent_goals (actor_user_id, status, created_at DESC)",
    )
    .execute(pool)
    .await
    .context("创建角色目标操作者索引")?;
    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_agent_goals_target_idx ON kovi_bot_agent_goals (target_scope, target_id, created_at DESC)",
    )
    .execute(pool)
    .await
    .context("创建角色目标对象索引")?;

    settle_stale_goals(pool).await?;
    Ok(())
}

pub(crate) async fn compact_expired() -> Result<u64> {
    let pool = database_pool()?;
    settle_stale_goals(pool).await?;
    let retention_days = crate::config::get().memory().retention_days().max(1);
    let cutoff = Utc::now() - ChronoDuration::days(retention_days);
    let removed = query(
        "DELETE FROM kovi_bot_agent_goals WHERE status IN ('completed', 'failed') AND completed_at < $1",
    )
    .bind(cutoff)
    .execute(pool)
    .await
    .context("清理过期角色目标")?
    .rows_affected();
    Ok(removed)
}

pub(crate) async fn delete_user_data(user_id: i64) -> Result<u64> {
    let removed_tasks = crate::agent_tasks::delete_user_data(user_id).await?;
    let removed = query(
        "DELETE FROM kovi_bot_agent_goals WHERE actor_user_id = $1 OR (source_scope = 'private' AND source_id = $1)",
    )
    .bind(user_id)
    .execute(database_pool()?)
    .await
    .context("删除用户角色目标")?
    .rows_affected();
    Ok(removed + removed_tasks)
}

pub(crate) async fn delete_group_data(group_id: i64) -> Result<u64> {
    let removed_tasks = crate::agent_tasks::delete_group_data(group_id).await?;
    let removed =
        query("DELETE FROM kovi_bot_agent_goals WHERE target_scope = 'group' AND target_id = $1")
            .bind(group_id)
            .execute(database_pool()?)
            .await
            .context("删除群聊角色目标")?
            .rows_affected();
    Ok(removed + removed_tasks)
}

pub(crate) async fn execute_action(
    bot: &RuntimeBot,
    context: AgentActionContext,
    action: AgentAction,
    reply_ticket: ReplyTicket,
) -> Result<String> {
    ensure_private_main_admin(bot, context)?;
    ensure!(
        is_current(reply_ticket).await,
        "这条指令已经被更新的私聊消息打断"
    );

    match action {
        AgentAction::SendGroupMessage {
            group_id,
            content,
            collect_replies_minutes,
        } => {
            execute_send_group_message(
                bot,
                context,
                group_id,
                &content,
                collect_replies_minutes,
                reply_ticket,
            )
            .await
        }
    }
}

pub(crate) async fn list_group_targets(
    bot: &RuntimeBot,
    actor_user_id: i64,
    max_result_chars: usize,
) -> Result<String> {
    ensure!(
        is_main_admin(bot, actor_user_id)?,
        "只有主管理员可以查看跨群动作目标"
    );
    let authorized = group_access::authorized_groups().await?;
    if authorized.is_empty() {
        return Ok(group_targets_result(Vec::new(), max_result_chars));
    }

    let response = bot.get_group_list().await.map_err(|error| {
        anyhow!(
            "暂时读取不到机器人已加入的群列表（retcode={}）",
            error.retcode
        )
    })?;
    ensure!(response.status == "ok", "暂时读取不到机器人已加入的群列表");
    let groups = response
        .data
        .as_array()
        .ok_or_else(|| anyhow!("群列表返回格式无效"))?;
    let authorized = authorized
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut targets = groups
        .iter()
        .filter_map(|group| {
            let group_id = json_i64(group.get("group_id")?)?;
            if !authorized.contains(&group_id) {
                return None;
            }
            let group_name = group
                .get("group_name")
                .and_then(Value::as_str)
                .map(normalize_group_name)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("群 {group_id}"));
            Some((group_id, group_name))
        })
        .collect::<Vec<_>>();
    targets.sort_by_key(|(group_id, _)| *group_id);
    targets.dedup_by_key(|(group_id, _)| *group_id);
    Ok(group_targets_result(targets, max_result_chars))
}

async fn execute_send_group_message(
    bot: &RuntimeBot,
    context: AgentActionContext,
    group_id: i64,
    content: &str,
    collect_replies_minutes: Option<u64>,
    reply_ticket: ReplyTicket,
) -> Result<String> {
    ensure!(group_id > 0, "目标群号必须是正整数");
    ensure!(
        group_access::is_authorized_group(group_id).await?,
        "目标群不在机器人的授权群白名单中"
    );
    let content = normalize_group_message(content)?;
    let payload = json!({
        "group_id": group_id,
        "content": content,
        "collect_replies_minutes": collect_replies_minutes,
    });
    let request_key = action_request_key(context, "send_group_message")?;
    let pool = database_pool()?;
    let reservation = reserve_goal(
        pool,
        &request_key,
        context,
        "send_group_message",
        group_id,
        &payload,
    )
    .await?;
    let goal_id = match reservation {
        GoalReservation::New(goal_id) => goal_id,
        GoalReservation::Existing(goal) => return existing_goal_result(goal, &payload),
    };

    let task_id = if let Some(collect_minutes) = collect_replies_minutes {
        let MessageDestination::Private(source_id) = context.source else {
            mark_goal_failed(pool, goal_id, "闭环任务来源不是私聊").await;
            return Err(anyhow!("跨群问答任务只能从私聊发起"));
        };
        match crate::agent_tasks::reserve_task(crate::agent_tasks::TaskReservationRequest {
            goal_id,
            request_key: &request_key,
            actor_user_id: context.actor_user_id,
            source_id,
            source_message_id: context.source_message_id,
            target_group_id: group_id,
            question: &content,
            collect_minutes,
        })
        .await
        {
            Ok(task_id) => Some(task_id),
            Err(error) => {
                mark_goal_failed(pool, goal_id, &format!("创建跨群问答任务失败：{error}")).await;
                return Err(error);
            }
        }
    } else {
        None
    };

    if let Err(error) = mark_goal_executing(pool, goal_id).await {
        if let Some(task_id) = task_id {
            crate::agent_tasks::fail_pending_task(task_id, &format!("领取角色目标失败：{error}"))
                .await;
        }
        return Err(error);
    }
    if !is_current(reply_ticket).await {
        mark_goal_failed(pool, goal_id, "发送前被更新的私聊消息打断").await;
        if let Some(task_id) = task_id {
            crate::agent_tasks::fail_pending_task(task_id, "发送前被更新的私聊消息打断").await;
        }
        return Err(anyhow!("这条指令已经被更新的私聊消息打断"));
    }
    if let Err(error) = ensure_group_joined(bot, group_id).await {
        mark_goal_failed(pool, goal_id, &format!("发送前目标校验失败：{error}")).await;
        if let Some(task_id) = task_id {
            crate::agent_tasks::fail_pending_task(task_id, &format!("发送前目标校验失败：{error}"))
                .await;
        }
        return Err(error);
    }
    if !is_current(reply_ticket).await {
        mark_goal_failed(pool, goal_id, "群状态校验后被更新的私聊消息打断").await;
        if let Some(task_id) = task_id {
            crate::agent_tasks::fail_pending_task(task_id, "群状态校验后被更新的私聊消息打断")
                .await;
        }
        return Err(anyhow!("这条指令已经被更新的私聊消息打断"));
    }
    let delivery_key = format!("agent-goal:{goal_id}:group-send");
    let send_result = send_tracked_message_with_revalidation(
        bot,
        MessageDestination::Group(group_id),
        Message::from(content.clone()),
        OutgoingSource::Reply,
        Some(&delivery_key),
        || async {
            if !is_current(reply_ticket).await || ensure_group_joined(bot, group_id).await.is_err() {
                return false;
            }
            match task_id {
                Some(task_id) => crate::agent_tasks::begin_question_send(task_id)
                    .await
                    .is_ok(),
                None => true,
            }
        },
    )
    .await;
    let message_id = match send_result {
        Ok(message_id) => message_id,
        Err(error) => {
            let detail = format!("跨群消息在发送前校验或投递时失败：{error}");
            eprintln!(
                "[ERROR] 角色跨群动作发送失败 (目标: {}, 任务: {}): {}",
                group_id, goal_id, error
            );
            mark_goal_failed(pool, goal_id, &detail).await;
            if let Some(task_id) = task_id {
                crate::agent_tasks::fail_pending_task(task_id, &detail).await;
            }
            return Err(anyhow!(
                "没有成功发到群 {group_id}，请确认机器人仍在群里并具有发言权限"
            ));
        }
    };

    let collect_until = if let (Some(task_id), Some(collect_minutes)) =
        (task_id, collect_replies_minutes)
    {
        match crate::agent_tasks::activate_after_send(task_id, message_id, collect_minutes).await {
            Ok(collect_until) => Some((task_id, collect_until)),
            Err(error) => {
                // 群问题已经送达；任务状态不确定时绝不能自动重发问题。
                crate::agent_tasks::fail_pending_task(
                    task_id,
                    &format!("问题已发送，但收集任务状态保存失败：{error}"),
                )
                .await;
                mark_goal_failed(
                    pool,
                    goal_id,
                    &format!("问题已发送，但收集任务未建立：{error}"),
                )
                .await;
                return Err(anyhow!(
                    "问题已经发到群 {group_id}，但我没能确认后续收集任务已建立；为避免重复提问没有自动重试"
                ));
            }
        }
    } else {
        None
    };

    crate::model::utils::record_external_group_message(group_id, &content).await;
    if let Err(error) = MEMORY_MANAGER
        .add_conversation_memory(
            group_id,
            &format!("芸汐受私聊指令发出：{content}"),
            "group_agent_action",
        )
        .await
    {
        eprintln!(
            "[ERROR] 角色跨群动作写入群聊记忆失败 (目标: {}, 任务: {}): {}",
            group_id, goal_id, error
        );
    }

    let result = match collect_until {
        Some((task_id, collect_until)) => json!({
            "status": "completed",
            "task_status": "collecting",
            "goal_id": goal_id,
            "task_id": task_id,
            "group_id": group_id,
            "message_id": message_id,
            "collect_until": collect_until.to_rfc3339(),
            "min_valid_replies": crate::config::get().agent_tasks().min_valid_replies(),
            "quiet_period_secs": crate::config::get().agent_tasks().quiet_period_secs(),
            "status_command": format!("#群问答状态 {}", task_id),
            "cancel_command": format!("#取消群问答 {}", task_id),
        }),
        None => json!({
            "status": "completed",
            "goal_id": goal_id,
            "group_id": group_id,
            "message_id": message_id,
        }),
    };
    if let Err(error) = mark_goal_completed(pool, goal_id, &result).await {
        // 外部发送已经成功，不能因为审计状态更新失败而向用户谎报失败并诱导重发。
        eprintln!(
            "[ERROR] 角色跨群动作已发送但状态保存失败 (目标: {}, 任务: {}): {}",
            group_id, goal_id, error
        );
    }
    println!(
        "[INFO] 角色跨群动作完成 (操作者: {}, 目标: {}, 任务: {}, 消息: {})",
        context.actor_user_id, group_id, goal_id, message_id
    );
    Ok(result.to_string())
}

fn ensure_private_main_admin(bot: &RuntimeBot, context: AgentActionContext) -> Result<()> {
    let MessageDestination::Private(source_user_id) = context.source else {
        return Err(anyhow!("跨会话动作只能从私聊发起"));
    };
    ensure!(
        source_user_id == context.actor_user_id,
        "动作操作者与私聊来源不一致"
    );
    ensure!(
        is_main_admin(bot, context.actor_user_id)?,
        "只有主管理员可以执行跨会话动作"
    );
    ensure!(
        context.source_message_id > 0,
        "缺少可用于幂等校验的来源消息编号"
    );
    Ok(())
}

fn is_main_admin(bot: &RuntimeBot, actor_user_id: i64) -> Result<bool> {
    if crate::yunxi::canonical_owner_matches(actor_user_id).is_some() {
        return Ok(crate::model::utils::is_main_admin(bot, actor_user_id));
    }
    Ok(bot.get_main_admin().context("读取 Kovi 主管理员")? == actor_user_id)
}

fn normalize_group_message(content: &str) -> Result<String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.trim();
    ensure!(!normalized.is_empty(), "发送正文不能为空");
    ensure!(!normalized.contains('\0'), "发送正文包含无效控制字符");
    ensure!(
        normalized.chars().count() <= MAX_GROUP_MESSAGE_CHARS,
        "发送正文不能超过 {MAX_GROUP_MESSAGE_CHARS} 个字符"
    );
    Ok(normalized.to_string())
}

fn action_request_key(context: AgentActionContext, action_kind: &str) -> Result<String> {
    let MessageDestination::Private(source_user_id) = context.source else {
        return Err(anyhow!("动作来源必须是私聊"));
    };
    ensure!(context.source_message_id > 0, "来源消息编号无效");
    Ok(format!(
        "private:{source_user_id}:{}:{action_kind}",
        context.source_message_id
    ))
}

async fn reserve_goal(
    pool: &PgPool,
    request_key: &str,
    context: AgentActionContext,
    action_kind: &str,
    target_id: i64,
    payload: &Value,
) -> Result<GoalReservation> {
    let MessageDestination::Private(source_id) = context.source else {
        return Err(anyhow!("动作来源必须是私聊"));
    };
    let mut transaction = pool.begin().await.context("开启角色目标事务")?;
    query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("agent:group-send:{}", context.actor_user_id))
        .execute(&mut *transaction)
        .await
        .context("锁定角色动作操作者")?;

    if let Some(row) = query(
        "SELECT id, status, payload, result, last_error FROM kovi_bot_agent_goals WHERE request_key = $1",
    )
    .bind(request_key)
    .fetch_optional(&mut *transaction)
    .await
    .context("读取已有角色目标")?
    {
        let goal = StoredGoal {
            id: row.get("id"),
            status: row.get("status"),
            payload: row.get("payload"),
            result: row.get("result"),
            last_error: row.get("last_error"),
        };
        transaction.commit().await.context("结束角色目标读取事务")?;
        return Ok(GoalReservation::Existing(goal));
    }

    let recent_count = query_scalar::<Postgres, i64>(
        r#"
        SELECT COUNT(*)
        FROM kovi_bot_agent_goals
        WHERE actor_user_id = $1
          AND action_kind = 'send_group_message'
          AND created_at > NOW() - INTERVAL '60 seconds'
        "#,
    )
    .bind(context.actor_user_id)
    .fetch_one(&mut *transaction)
    .await
    .context("统计近期角色动作")?;
    ensure!(
        recent_count < MAX_GROUP_ACTIONS_PER_MINUTE,
        "跨群发言过于频繁，请稍后再试"
    );

    let inserted = query(
        r#"
        INSERT INTO kovi_bot_agent_goals
            (request_key, actor_user_id, source_scope, source_id, source_message_id,
             action_kind, target_scope, target_id, payload)
        VALUES ($1, $2, 'private', $3, $4, $5, 'group', $6, $7)
        ON CONFLICT (request_key) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(request_key)
    .bind(context.actor_user_id)
    .bind(source_id)
    .bind(context.source_message_id)
    .bind(action_kind)
    .bind(target_id)
    .bind(payload)
    .fetch_optional(&mut *transaction)
    .await
    .context("创建角色目标")?;
    if let Some(row) = inserted {
        let goal_id = row.get("id");
        transaction.commit().await.context("提交角色目标事务")?;
        return Ok(GoalReservation::New(goal_id));
    }

    let row = query(
        "SELECT id, status, payload, result, last_error FROM kovi_bot_agent_goals WHERE request_key = $1",
    )
    .bind(request_key)
    .fetch_one(&mut *transaction)
    .await
    .context("读取已有角色目标")?;
    let goal = StoredGoal {
        id: row.get("id"),
        status: row.get("status"),
        payload: row.get("payload"),
        result: row.get("result"),
        last_error: row.get("last_error"),
    };
    transaction.commit().await.context("提交角色目标事务")?;
    Ok(GoalReservation::Existing(goal))
}

fn existing_goal_result(goal: StoredGoal, requested_payload: &Value) -> Result<String> {
    ensure!(
        goal.payload == *requested_payload,
        "同一条私聊消息已经绑定了另一个跨群动作，不能重复执行"
    );
    match goal.status.as_str() {
        "completed" => {
            let mut result = goal.result.unwrap_or_else(|| json!({}));
            if let Some(result) = result.as_object_mut() {
                result.insert("status".to_string(), json!("already_completed"));
                result
                    .entry("goal_id".to_string())
                    .or_insert(json!(goal.id));
            }
            Ok(result.to_string())
        }
        "failed" => Err(anyhow!(
            "这条跨群指令此前执行失败：{}。如需重试，请重新发送一条指令",
            goal.last_error.as_deref().unwrap_or("原因未知")
        )),
        "received" | "executing" => Err(anyhow!("这条跨群指令已经在处理中，请不要重复发送")),
        _ => Err(anyhow!("角色目标保存了未知状态")),
    }
}

async fn mark_goal_executing(pool: &PgPool, goal_id: i64) -> Result<()> {
    let result = query(
        "UPDATE kovi_bot_agent_goals SET status = 'executing', updated_at = NOW() WHERE id = $1 AND status = 'received'",
    )
    .bind(goal_id)
    .execute(pool)
    .await
    .context("领取角色目标")?;
    ensure!(result.rows_affected() == 1, "角色目标已被其他执行器领取");
    Ok(())
}

async fn mark_goal_completed(pool: &PgPool, goal_id: i64, result: &Value) -> Result<()> {
    let updated = query(
        r#"
        UPDATE kovi_bot_agent_goals
        SET status = 'completed', result = $2, last_error = NULL,
            updated_at = NOW(), completed_at = NOW()
        WHERE id = $1 AND status = 'executing'
        "#,
    )
    .bind(goal_id)
    .bind(result)
    .execute(pool)
    .await
    .context("完成角色目标")?;
    ensure!(updated.rows_affected() == 1, "角色目标状态已经发生变化");
    Ok(())
}

async fn mark_goal_failed(pool: &PgPool, goal_id: i64, error: &str) {
    if let Err(database_error) = query(
        r#"
        UPDATE kovi_bot_agent_goals
        SET status = 'failed', last_error = $2, updated_at = NOW(), completed_at = NOW()
        WHERE id = $1 AND status IN ('received', 'executing')
        "#,
    )
    .bind(goal_id)
    .bind(error.chars().take(800).collect::<String>())
    .execute(pool)
    .await
    {
        eprintln!(
            "[ERROR] 标记角色目标失败状态时出错 (任务: {}): {}",
            goal_id, database_error
        );
    }
}

async fn settle_stale_goals(pool: &PgPool) -> Result<()> {
    // 外部消息是否已经送达无法可靠判断，因此过期执行不自动重放。
    query(
        r#"
        UPDATE kovi_bot_agent_goals
        SET status = 'failed',
            last_error = '动作执行中断；为避免重复发送，未自动重试',
            updated_at = NOW(),
            completed_at = NOW()
        WHERE status IN ('received', 'executing')
          AND updated_at < NOW() - INTERVAL '10 minutes'
        "#,
    )
    .execute(pool)
    .await
    .context("收敛过期角色目标")?;
    Ok(())
}

fn normalize_group_name(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

async fn ensure_group_joined(bot: &RuntimeBot, group_id: i64) -> Result<()> {
    let response = bot.get_group_info(group_id, true).await.map_err(|error| {
        anyhow!(
            "机器人当前不在群 {group_id} 中，或暂时无法确认群状态（retcode={}）",
            error.retcode
        )
    })?;
    ensure!(
        response.status == "ok"
            && response
                .data
                .get("group_id")
                .and_then(json_i64)
                .is_some_and(|value| value == group_id),
        "机器人当前不在群 {group_id} 中，或群信息返回无效"
    );
    Ok(())
}

fn group_targets_result(targets: Vec<(i64, String)>, max_result_chars: usize) -> String {
    let total = targets.len();
    let mut selected = Vec::new();
    for (group_id, group_name) in targets {
        selected.push(json!({
            "group_id": group_id,
            "group_name": group_name,
        }));
        let candidate = json!({
            "targets": selected,
            "total": total,
            "truncated": false,
        })
        .to_string();
        if candidate.chars().count() > max_result_chars {
            selected.pop();
            break;
        }
    }
    json!({
        "targets": selected,
        "total": total,
        "truncated": selected.len() < total,
    })
    .to_string()
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse::<i64>().ok())
        .filter(|value| *value > 0)
}

fn database_pool() -> Result<&'static PgPool> {
    MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 记忆连接池尚未初始化"))
}

#[cfg(test)]
mod tests {
    use super::{
        AgentActionContext, GoalReservation, action_request_key, group_targets_result, json_i64,
        normalize_group_message, normalize_group_name,
    };
    use crate::model::MessageDestination;
    use serde_json::json;

    #[test]
    fn group_message_validation_preserves_text_and_enforces_bounds() {
        assert_eq!(
            normalize_group_message("  今晚八点开会\r\n别迟到  ").unwrap(),
            "今晚八点开会\n别迟到"
        );
        assert!(normalize_group_message("   ").is_err());
        assert!(normalize_group_message("包含\0空字符").is_err());
        assert!(normalize_group_message(&"好".repeat(1_001)).is_err());
    }

    #[test]
    fn action_request_key_is_stable_per_private_source_message() {
        let context = AgentActionContext {
            actor_user_id: 42,
            source: MessageDestination::Private(42),
            source_message_id: 108,
        };
        assert_eq!(
            action_request_key(context, "send_group_message").unwrap(),
            "private:42:108:send_group_message"
        );
        assert!(
            action_request_key(
                AgentActionContext {
                    source: MessageDestination::Group(42),
                    ..context
                },
                "send_group_message"
            )
            .is_err()
        );
    }

    #[test]
    fn group_target_values_are_normalized_without_guessing_ids() {
        assert_eq!(json_i64(&json!(123)), Some(123));
        assert_eq!(json_i64(&json!("456")), Some(456));
        assert_eq!(json_i64(&json!("unknown")), None);
        assert_eq!(normalize_group_name("  主群  测试  "), "主群 测试");
    }

    #[test]
    fn group_target_results_remain_valid_json_within_the_tool_budget() {
        let result = group_targets_result(
            vec![
                (100, "主群".to_string()),
                (200, "测试群".repeat(30)),
                (300, "备用群".to_string()),
            ],
            150,
        );
        let value: serde_json::Value = serde_json::from_str(&result).expect("结果应是合法 JSON");
        assert!(result.chars().count() <= 150);
        assert_eq!(value["total"], 3);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["targets"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_goal_reservation_is_atomic_and_idempotent() {
        crate::database_test_support::block_on(async {
            crate::memory::MEMORY_MANAGER
                .initialize_database()
                .await
                .expect("应初始化 PostgreSQL 记忆连接池");
            super::initialize_database()
                .await
                .expect("应初始化角色目标表");
            let actor_user_id = chrono::Utc::now().timestamp_micros();
            let source_message_id = ((actor_user_id % i64::from(i32::MAX - 1)) as i32).max(1);
            let context = AgentActionContext {
                actor_user_id,
                source: MessageDestination::Private(actor_user_id),
                source_message_id,
            };
            let request_key = action_request_key(context, "send_group_message").unwrap();
            let payload = json!({"group_id": 778899, "content": "并发幂等测试"});
            let pool = super::database_pool().expect("连接池应存在");
            let (first, second) = kovi::tokio::join!(
                super::reserve_goal(
                    pool,
                    &request_key,
                    context,
                    "send_group_message",
                    778899,
                    &payload,
                ),
                super::reserve_goal(
                    pool,
                    &request_key,
                    context,
                    "send_group_message",
                    778899,
                    &payload,
                ),
            );
            let first = first.expect("第一个目标预留不应失败");
            let second = second.expect("第二个目标预留不应失败");
            let new_count = usize::from(matches!(first, GoalReservation::New(_)))
                + usize::from(matches!(second, GoalReservation::New(_)));
            assert_eq!(new_count, 1, "同一来源消息只能创建一个角色目标");

            let row_count = sqlx_core::query_scalar::query_scalar::<sqlx_postgres::Postgres, i64>(
                "SELECT COUNT(*) FROM kovi_bot_agent_goals WHERE request_key = $1",
            )
            .bind(&request_key)
            .fetch_one(pool)
            .await
            .expect("应读取角色目标数量");
            assert_eq!(row_count, 1);
            sqlx_core::query::query("DELETE FROM kovi_bot_agent_goals WHERE request_key = $1")
                .bind(&request_key)
                .execute(pool)
                .await
                .expect("应清理测试角色目标");
        });
    }
}
