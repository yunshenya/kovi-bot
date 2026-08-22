//! Runtime management of the model plugin's group allowlist.
//!
//! Kovi filters group events before the plugin handler runs, so the list must
//! be updated through Kovi's runtime API. PostgreSQL is the source of truth;
//! the in-memory copy only avoids a query for each incoming command.

use crate::memory::MEMORY_MANAGER;
use anyhow::{Context, Result, anyhow};
use kovi::PluginBuilder;
use kovi::RuntimeBot;
use kovi::bot::runtimebot::kovi_api::SetAccessControlList;
use kovi::tokio::sync::Mutex;
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::collections::BTreeSet;
use std::sync::LazyLock;

const MAX_AUTHORIZED_GROUPS: usize = 4096;

static STATE: LazyLock<Mutex<Option<GroupAccessState>>> = LazyLock::new(|| Mutex::new(None));

struct GroupAccessState {
    plugin_name: String,
    groups: BTreeSet<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorizationCommand {
    Add(i64),
    AddCurrent,
    Remove(i64),
    RemoveCurrent,
    List,
    Help,
}

/// Create the table, seed it once from the static Kovi list, then apply the
/// PostgreSQL list before the plugin starts receiving events.
pub async fn initialize(bot: &RuntimeBot) -> Result<()> {
    let plugin_name = PluginBuilder::get_plugin_name();
    let configured_groups = configured_groups(bot, &plugin_name);
    let pool = database_pool()?;
    initialize_schema(pool, &configured_groups).await?;
    let groups = load_groups(pool).await?;
    apply_groups(bot, &plugin_name, &groups)?;

    println!(
        "[INFO] PostgreSQL 群聊白名单已加载 (表: kovi_bot_authorized_groups, 数量: {})",
        groups.len()
    );
    let mut state = STATE.lock().await;
    *state = Some(GroupAccessState {
        plugin_name,
        groups,
    });
    Ok(())
}

/// Whether a message is one of the allowlist management commands.
pub(crate) fn is_authorization_command(message: &str) -> bool {
    let text = message.trim();
    text == "#授权群"
        || text == "#授权群列表"
        || text == "#授权列表"
        || text == "#授权群帮助"
        || text == "#授权帮助"
        || text == "#取消授权群"
        || text == "#移除授权群"
        || has_argument_prefix(text, "#授权群")
        || has_argument_prefix(text, "#取消授权群")
        || has_argument_prefix(text, "#移除授权群")
}

fn has_argument_prefix(text: &str, prefix: &str) -> bool {
    text.strip_prefix(prefix)
        .and_then(|rest| rest.chars().next())
        .is_some_and(char::is_whitespace)
}

/// Execute an allowlist command and return the visible administrator response.
pub(crate) async fn handle_command(
    bot: &RuntimeBot,
    message: &str,
    current_group: Option<i64>,
) -> Option<String> {
    let command = parse_command(message)?;
    let response = match command {
        AuthorizationCommand::Add(group_id) => update_group(bot, group_id, true).await,
        AuthorizationCommand::AddCurrent => match current_group {
            Some(group_id) => update_group(bot, group_id, true).await,
            None => Err(anyhow!("私聊请提供群号，例如：#授权群 641996763")),
        },
        AuthorizationCommand::Remove(group_id) => update_group(bot, group_id, false).await,
        AuthorizationCommand::RemoveCurrent => match current_group {
            Some(group_id) => update_group(bot, group_id, false).await,
            None => Err(anyhow!("私聊请提供群号，例如：#取消授权群 641996763")),
        },
        AuthorizationCommand::List => list_groups().await,
        AuthorizationCommand::Help => Ok(command_help().to_string()),
    };
    Some(match response {
        Ok(message) => message,
        Err(error) => {
            eprintln!("[ERROR] 群聊白名单命令执行失败: {}", error);
            format!("群聊白名单操作失败：{}", error)
        }
    })
}

fn parse_command(message: &str) -> Option<AuthorizationCommand> {
    let text = message.trim();
    if text == "#授权群列表" || text == "#授权列表" {
        return Some(AuthorizationCommand::List);
    }
    if text == "#授权群帮助" || text == "#授权帮助" {
        return Some(AuthorizationCommand::Help);
    }
    if text == "#授权群" {
        return Some(AuthorizationCommand::AddCurrent);
    }
    if text == "#取消授权群" || text == "#移除授权群" {
        return Some(AuthorizationCommand::RemoveCurrent);
    }
    if let Some(group_id) = parse_group_id_argument(text, "#授权群") {
        return Some(AuthorizationCommand::Add(group_id));
    }
    if let Some(group_id) = parse_group_id_argument(text, "#取消授权群")
        .or_else(|| parse_group_id_argument(text, "#移除授权群"))
    {
        return Some(AuthorizationCommand::Remove(group_id));
    }
    None
}

fn parse_group_id_argument(text: &str, prefix: &str) -> Option<i64> {
    let argument = text.strip_prefix(prefix)?.trim();
    if argument.is_empty() || !argument.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let group_id = argument.parse::<i64>().ok()?;
    (group_id > 0).then_some(group_id)
}

async fn update_group(bot: &RuntimeBot, group_id: i64, add: bool) -> Result<String> {
    let mut state_guard = STATE.lock().await;
    let state = state_guard
        .as_mut()
        .ok_or_else(|| anyhow!("群聊白名单尚未初始化"))?;
    let old_groups = state.groups.clone();
    let pool = database_pool()?;
    let mut transaction = pool.begin().await.context("开启群聊白名单事务")?;
    let result = if add {
        query(
            "INSERT INTO kovi_bot_authorized_groups (group_id) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(group_id)
        .execute(&mut *transaction)
        .await
        .context("写入群聊白名单")?
    } else {
        query("DELETE FROM kovi_bot_authorized_groups WHERE group_id = $1")
            .bind(group_id)
            .execute(&mut *transaction)
            .await
            .context("删除群聊白名单")?
    };

    if result.rows_affected() == 0 {
        transaction.rollback().await.ok();
        return Ok(if add {
            format!("群聊 {} 已在群聊白名单中。", group_id)
        } else {
            format!("群聊 {} 不在群聊白名单中。", group_id)
        });
    }

    let new_groups = load_groups_from_transaction(&mut transaction).await?;
    if let Err(error) = apply_groups(bot, &state.plugin_name, &new_groups) {
        transaction.rollback().await.ok();
        return Err(error);
    }
    if let Err(error) = transaction.commit().await {
        let _ = apply_groups(bot, &state.plugin_name, &old_groups);
        return Err(error).context("提交群聊白名单事务");
    }

    state.groups = new_groups;
    println!(
        "[INFO] 群聊白名单已更新 (操作: {}, 群组: {}, 数量: {})",
        if add { "添加" } else { "移除" },
        group_id,
        state.groups.len()
    );
    Ok(if add {
        format!("已授权群聊 {}，现在可以接收群消息了。", group_id)
    } else {
        format!("已取消授权群聊 {}。", group_id)
    })
}

async fn list_groups() -> Result<String> {
    let state = STATE.lock().await;
    let state = state
        .as_ref()
        .ok_or_else(|| anyhow!("群聊白名单尚未初始化"))?;
    if state.groups.is_empty() {
        return Ok("当前群聊白名单为空。".to_string());
    }
    let groups = state
        .groups
        .iter()
        .take(200)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let suffix = if state.groups.len() > groups.len() {
        format!("（仅显示前 {} 个）", groups.len())
    } else {
        String::new()
    };
    Ok(format!("当前群聊白名单：{}{}", groups.join("、"), suffix))
}

pub(crate) fn command_help() -> &'static str {
    "用法：#授权群 群号、#取消授权群 群号、#授权群列表。仅机器人管理员可执行。"
}

fn configured_groups(bot: &RuntimeBot, plugin_name: &str) -> BTreeSet<i64> {
    let Ok(plugins) = bot.get_plugin_info() else {
        eprintln!("[ERROR] 读取 Kovi 插件信息失败，群聊白名单按空集合启动");
        return BTreeSet::new();
    };
    plugins
        .into_iter()
        .find(|plugin| plugin.name == plugin_name)
        .map(|plugin| plugin.access_list.groups.into_iter().collect())
        .unwrap_or_default()
}

fn database_pool() -> Result<&'static PgPool> {
    MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 记忆连接池尚未初始化"))
}

async fn initialize_schema(pool: &PgPool, configured_groups: &BTreeSet<i64>) -> Result<()> {
    let mut transaction = pool.begin().await.context("开启群聊白名单初始化事务")?;
    query(
        "CREATE TABLE IF NOT EXISTS kovi_bot_authorized_groups (
            group_id BIGINT PRIMARY KEY,
            authorized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&mut *transaction)
    .await
    .context("创建群聊白名单表")?;
    query(
        "CREATE TABLE IF NOT EXISTS kovi_bot_authorized_groups_meta (
            id SMALLINT PRIMARY KEY CHECK (id = 1),
            initialized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&mut *transaction)
    .await
    .context("创建群聊白名单元数据表")?;
    let first_initialization = query(
        "INSERT INTO kovi_bot_authorized_groups_meta (id) VALUES (1)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .fetch_optional(&mut *transaction)
    .await
    .context("初始化群聊白名单元数据")?
    .is_some();
    if first_initialization {
        for group_id in configured_groups {
            query(
                "INSERT INTO kovi_bot_authorized_groups (group_id) VALUES ($1)
                 ON CONFLICT DO NOTHING",
            )
            .bind(group_id)
            .execute(&mut *transaction)
            .await
            .context("迁移静态群聊白名单")?;
        }
    }
    transaction
        .commit()
        .await
        .context("提交群聊白名单初始化事务")
}

async fn load_groups(pool: &PgPool) -> Result<BTreeSet<i64>> {
    let rows = query("SELECT group_id FROM kovi_bot_authorized_groups ORDER BY group_id")
        .fetch_all(pool)
        .await
        .context("读取群聊白名单")?;
    normalize_groups(
        rows.into_iter()
            .map(|row| row.try_get::<i64, _>("group_id"))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )
}

async fn load_groups_from_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
) -> Result<BTreeSet<i64>> {
    let rows = query("SELECT group_id FROM kovi_bot_authorized_groups ORDER BY group_id")
        .fetch_all(&mut **transaction)
        .await
        .context("读取事务中的群聊白名单")?;
    normalize_groups(
        rows.into_iter()
            .map(|row| row.try_get::<i64, _>("group_id"))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )
}

fn normalize_groups(groups: Vec<i64>) -> Result<BTreeSet<i64>> {
    if groups.len() > MAX_AUTHORIZED_GROUPS {
        return Err(anyhow!("群聊白名单最多支持 {} 个群", MAX_AUTHORIZED_GROUPS));
    }
    let groups = groups.into_iter().collect::<BTreeSet<_>>();
    if groups.iter().any(|group_id| *group_id <= 0) {
        return Err(anyhow!("群号必须是正整数"));
    }
    Ok(groups)
}

fn apply_groups(bot: &RuntimeBot, plugin_name: &str, groups: &BTreeSet<i64>) -> Result<()> {
    bot.set_plugin_access_control_list(
        plugin_name,
        true,
        SetAccessControlList::Changes(groups.iter().copied().collect()),
    )
    .map_err(|error| anyhow!("应用 Kovi 群聊白名单失败: {}", error))
}

#[cfg(test)]
mod tests {
    use super::{AuthorizationCommand, is_authorization_command, normalize_groups, parse_command};

    #[test]
    fn parses_allowlist_commands_without_prefix_injection() {
        assert_eq!(
            parse_command("#授权群 641996763"),
            Some(AuthorizationCommand::Add(641996763))
        );
        assert_eq!(
            parse_command("#取消授权群 641996763"),
            Some(AuthorizationCommand::Remove(641996763))
        );
        assert_eq!(
            parse_command("#移除授权群 641996763"),
            Some(AuthorizationCommand::Remove(641996763))
        );
        assert_eq!(
            parse_command("#授权群列表"),
            Some(AuthorizationCommand::List)
        );
        assert_eq!(
            parse_command("#授权群"),
            Some(AuthorizationCommand::AddCurrent)
        );
        assert_eq!(
            parse_command("#取消授权群"),
            Some(AuthorizationCommand::RemoveCurrent)
        );
        assert_eq!(
            parse_command("#授权群帮助"),
            Some(AuthorizationCommand::Help)
        );
        assert_eq!(parse_command("#授权群 641996763 extra"), None);
        assert_eq!(parse_command("#授权群 -1"), None);
        assert_eq!(parse_command("#授权群 0"), None);
        assert!(is_authorization_command("#授权群 invalid"));
        assert!(!is_authorization_command("#授权群abc"));
    }

    #[test]
    fn normalizes_and_validates_group_ids() {
        let groups = normalize_groups(vec![3, 1, 3]).expect("重复群号应去重");
        assert_eq!(groups.into_iter().collect::<Vec<_>>(), vec![1, 3]);
        assert!(normalize_groups(vec![0]).is_err());
        assert!(normalize_groups(vec![-1]).is_err());
    }
}
