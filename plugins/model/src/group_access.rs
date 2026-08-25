//! Runtime management of the model plugin's group and administrator allowlists.
//!
//! Kovi filters events before the plugin handler runs, so both lists must be
//! updated through Kovi's runtime API. PostgreSQL is the source of truth; the
//! in-memory copy only avoids a query for each incoming command.

use crate::memory::MEMORY_MANAGER;
use anyhow::{Context, Result, anyhow};
use kovi::PluginBuilder;
use kovi::RuntimeBot;
use kovi::bot::runtimebot::kovi_api::{SetAccessControlList, SetAdmin};
use kovi::tokio::sync::{Mutex, MutexGuard};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::collections::BTreeSet;
use std::sync::LazyLock;

const MAX_AUTHORIZED_GROUPS: usize = 4096;
const MAX_AUTHORIZED_ADMINS: usize = 256;

static STATE: LazyLock<Mutex<Option<GroupAccessState>>> = LazyLock::new(|| Mutex::new(None));

struct GroupAccessState {
    plugin_name: String,
    friends: BTreeSet<i64>,
    configured_admins: BTreeSet<i64>,
    groups: BTreeSet<i64>,
    admins: BTreeSet<i64>,
    main_admin: i64,
}

/// Pins the runtime authorization snapshot through the outgoing commit point.
/// Allowlist mutations use the same mutex, so revocation and commit have a
/// deterministic order without holding the conversation lock across SQL or
/// platform calls.
#[must_use]
pub(crate) struct GroupSendAuthorization {
    _state: MutexGuard<'static, Option<GroupAccessState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorizationCommand {
    Add(i64),
    AddCurrent,
    Remove(i64),
    RemoveCurrent,
    List,
    Help,
    AddAdmin(i64),
    RemoveAdmin(i64),
    ListAdmins,
    AdminHelp,
}

/// Create the table, seed it once from the static Kovi list, then apply the
/// PostgreSQL list before the plugin starts receiving events.
pub async fn initialize(bot: &RuntimeBot) -> Result<()> {
    let plugin_name = PluginBuilder::get_plugin_name();
    let configured_groups = configured_groups(bot, &plugin_name);
    let mut friends = configured_friends(bot, &plugin_name);
    // Use the canonical PersonId owner route whenever configured. The Kovi
    // host administrator remains a compatibility source only for deployments
    // that have not migrated `[identity].owner_person_id` yet.
    let main_admin = match crate::yunxi::canonical_owner_qq_id() {
        Some(Some(owner)) => owner,
        Some(None) => {
            return Err(anyhow!(
                "[identity].owner_person_id 未绑定唯一 QQ，拒绝初始化管理员入口"
            ));
        }
        None => bot.get_main_admin().context("读取 Kovi 主管理员")?,
    };
    friends.insert(main_admin);
    let configured_admins = bot
        .get_deputy_admins()
        .map_err(|error| anyhow!("读取 Kovi 副管理员失败: {}", error))?
        .into_iter()
        .collect::<Vec<_>>();
    let configured_admins = normalize_admins(configured_admins, main_admin)?;
    let pool = database_pool()?;
    initialize_schema(pool, &configured_groups, &configured_admins, main_admin).await?;
    let groups = load_groups(pool).await?;
    let mut admins = load_admins(pool, main_admin).await?;
    admins.extend(configured_admins.iter().copied());
    let admins = normalize_admins(admins.into_iter().collect(), main_admin)?;
    apply_groups(bot, &plugin_name, &groups)?;
    apply_admins(bot, &plugin_name, &friends, &admins)?;

    println!(
        "[INFO] PostgreSQL 群聊白名单已加载 (表: kovi_bot_authorized_groups, 数量: {})",
        groups.len()
    );
    println!(
        "[INFO] PostgreSQL 管理员名单已加载 (表: kovi_bot_authorized_admins, 数量: {})",
        admins.len() + 1
    );
    let mut state = STATE.lock().await;
    *state = Some(GroupAccessState {
        plugin_name,
        friends,
        configured_admins,
        groups,
        admins,
        main_admin,
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
        || text == "#授权管理员"
        || text == "#授权管理员列表"
        || text == "#授权管理员帮助"
        || text == "#管理员授权"
        || text == "#管理员列表"
        || text == "#管理员帮助"
        || text == "#取消授权群"
        || text == "#移除授权群"
        || text == "#取消授权管理员"
        || text == "#移除授权管理员"
        || has_argument_prefix(text, "#授权群")
        || has_argument_prefix(text, "#取消授权群")
        || has_argument_prefix(text, "#移除授权群")
        || has_argument_prefix(text, "#授权管理员")
        || has_argument_prefix(text, "#管理员授权")
        || has_argument_prefix(text, "#取消授权管理员")
        || has_argument_prefix(text, "#移除授权管理员")
}

pub(crate) async fn is_authorized_group(group_id: i64) -> Result<bool> {
    let state = STATE.lock().await;
    let state = state
        .as_ref()
        .ok_or_else(|| anyhow!("群聊白名单尚未初始化"))?;
    Ok(state.groups.contains(&group_id))
}

pub(crate) async fn authorize_group_send(group_id: i64) -> Result<GroupSendAuthorization> {
    let state = STATE.lock().await;
    let initialized = state
        .as_ref()
        .ok_or_else(|| anyhow!("群聊白名单尚未初始化"))?;
    if !initialized.groups.contains(&group_id) {
        return Err(anyhow!("群聊不在授权白名单中"));
    }
    Ok(GroupSendAuthorization { _state: state })
}

pub(crate) async fn authorized_groups() -> Result<Vec<i64>> {
    let state = STATE.lock().await;
    let state = state
        .as_ref()
        .ok_or_else(|| anyhow!("群聊白名单尚未初始化"))?;
    Ok(state.groups.iter().copied().collect())
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
    actor_id: i64,
) -> Option<String> {
    let command = parse_command(message)?;
    if command_requires_main_admin(command) && !is_main_admin(actor_id).await.unwrap_or(false) {
        return Some("只有主管理员可以授权或取消授权管理员。".to_string());
    }
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
        AuthorizationCommand::AddAdmin(user_id) => update_admin(bot, user_id, true).await,
        AuthorizationCommand::RemoveAdmin(user_id) => update_admin(bot, user_id, false).await,
        AuthorizationCommand::ListAdmins => list_admins().await,
        AuthorizationCommand::AdminHelp => Ok(admin_command_help().to_string()),
    };
    Some(response.unwrap_or_else(|error| {
        eprintln!("[ERROR] 授权命令执行失败: {}", error);
        format!("授权操作失败：{}", error)
    }))
}

fn parse_command(message: &str) -> Option<AuthorizationCommand> {
    let text = message.trim();
    if text == "#授权管理员列表" || text == "#管理员列表" {
        return Some(AuthorizationCommand::ListAdmins);
    }
    if text == "#授权管理员帮助"
        || text == "#管理员帮助"
        || text == "#授权管理员"
        || text == "#管理员授权"
    {
        return Some(AuthorizationCommand::AdminHelp);
    }
    if let Some(user_id) = parse_user_id_argument(text, "#授权管理员")
        .or_else(|| parse_user_id_argument(text, "#管理员授权"))
    {
        return Some(AuthorizationCommand::AddAdmin(user_id));
    }
    if let Some(user_id) = parse_user_id_argument(text, "#取消授权管理员")
        .or_else(|| parse_user_id_argument(text, "#移除授权管理员"))
    {
        return Some(AuthorizationCommand::RemoveAdmin(user_id));
    }
    if text == "#取消授权管理员" || text == "#移除授权管理员" {
        return Some(AuthorizationCommand::AdminHelp);
    }
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
    parse_user_id_argument(text, prefix)
}

fn parse_user_id_argument(text: &str, prefix: &str) -> Option<i64> {
    let argument = text.strip_prefix(prefix)?.trim();
    if argument.is_empty() || !argument.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let user_id = argument.parse::<i64>().ok()?;
    (user_id > 0).then_some(user_id)
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

async fn update_admin(bot: &RuntimeBot, user_id: i64, add: bool) -> Result<String> {
    if user_id <= 0 {
        return Err(anyhow!("管理员 QQ 号必须是正整数"));
    }
    let mut state_guard = STATE.lock().await;
    let state = state_guard
        .as_mut()
        .ok_or_else(|| anyhow!("群聊白名单尚未初始化"))?;
    if user_id == state.main_admin {
        return Err(anyhow!("不能添加或移除主管理员"));
    }
    if add && state.configured_admins.contains(&user_id) {
        return Ok(format!("用户 {} 已经是配置中的副管理员。", user_id));
    }
    if !add && state.configured_admins.contains(&user_id) {
        return Err(anyhow!("不能移除配置文件中的副管理员"));
    }
    let old_admins = state.admins.clone();
    let pool = database_pool()?;
    let mut transaction = pool.begin().await.context("开启授权管理员事务")?;
    let result = if add {
        query("INSERT INTO kovi_bot_authorized_admins (user_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(user_id)
            .execute(&mut *transaction)
            .await
            .context("写入授权管理员")?
    } else {
        query("DELETE FROM kovi_bot_authorized_admins WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *transaction)
            .await
            .context("删除授权管理员")?
    };
    if result.rows_affected() == 0 {
        transaction.rollback().await.ok();
        return Ok(if add {
            format!("用户 {} 已经是副管理员。", user_id)
        } else {
            format!("用户 {} 不是动态副管理员。", user_id)
        });
    }
    let mut new_admins = load_admins_from_transaction(&mut transaction, state.main_admin).await?;
    new_admins.extend(state.configured_admins.iter().copied());
    let new_admins = normalize_admins(new_admins.into_iter().collect(), state.main_admin)?;
    if let Err(error) = apply_admins(bot, &state.plugin_name, &state.friends, &new_admins) {
        transaction.rollback().await.ok();
        let _ = apply_admins(bot, &state.plugin_name, &state.friends, &old_admins);
        return Err(error);
    }
    if let Err(error) = transaction.commit().await {
        let _ = apply_admins(bot, &state.plugin_name, &state.friends, &old_admins);
        return Err(error).context("提交授权管理员事务");
    }
    state.admins = new_admins;
    Ok(if add {
        format!("已授权 {} 为副管理员。", user_id)
    } else {
        format!("已取消 {} 的副管理员权限。", user_id)
    })
}

async fn list_admins() -> Result<String> {
    let state = STATE.lock().await;
    let state = state
        .as_ref()
        .ok_or_else(|| anyhow!("群聊白名单尚未初始化"))?;
    let mut admins = vec![format!("主管理员 {}", state.main_admin)];
    admins.extend(state.admins.iter().map(|user_id| user_id.to_string()));
    Ok(format!("当前管理员：{}", admins.join("、")))
}

pub(crate) fn command_help() -> &'static str {
    "用法：#授权群 群号、#取消授权群 群号、#授权群列表。仅机器人管理员可执行。"
}

pub(crate) fn admin_command_help() -> &'static str {
    "用法：#授权管理员 QQ号、#取消授权管理员 QQ号、#授权管理员列表。仅主管理员可执行。"
}

fn command_requires_main_admin(command: AuthorizationCommand) -> bool {
    matches!(
        command,
        AuthorizationCommand::AddAdmin(_)
            | AuthorizationCommand::RemoveAdmin(_)
            | AuthorizationCommand::ListAdmins
            | AuthorizationCommand::AdminHelp
    )
}

async fn is_main_admin(user_id: i64) -> Result<bool> {
    if let Some(is_owner) = crate::yunxi::canonical_owner_matches_authoritative(user_id).await {
        return Ok(is_owner);
    }
    let state = STATE.lock().await;
    let state = state
        .as_ref()
        .ok_or_else(|| anyhow!("群聊白名单尚未初始化"))?;
    Ok(state.main_admin == user_id)
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

fn configured_friends(bot: &RuntimeBot, plugin_name: &str) -> BTreeSet<i64> {
    let Ok(plugins) = bot.get_plugin_info() else {
        eprintln!("[ERROR] 读取 Kovi 插件信息失败，好友白名单按空集合启动");
        return BTreeSet::new();
    };
    plugins
        .into_iter()
        .find(|plugin| plugin.name == plugin_name)
        .map(|plugin| plugin.access_list.friends.into_iter().collect())
        .unwrap_or_default()
}

fn database_pool() -> Result<&'static PgPool> {
    MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 记忆连接池尚未初始化"))
}

async fn initialize_schema(
    pool: &PgPool,
    configured_groups: &BTreeSet<i64>,
    configured_admins: &BTreeSet<i64>,
    main_admin: i64,
) -> Result<()> {
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
    query(
        "CREATE TABLE IF NOT EXISTS kovi_bot_authorized_admins (
            user_id BIGINT PRIMARY KEY CHECK (user_id > 0),
            authorized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&mut *transaction)
    .await
    .context("创建授权管理员表")?;
    query(
        "CREATE TABLE IF NOT EXISTS kovi_bot_authorized_admins_meta (
            id SMALLINT PRIMARY KEY CHECK (id = 1),
            initialized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&mut *transaction)
    .await
    .context("创建授权管理员元数据表")?;
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
    let first_admin_initialization = query(
        "INSERT INTO kovi_bot_authorized_admins_meta (id) VALUES (1)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .fetch_optional(&mut *transaction)
    .await
    .context("初始化授权管理员元数据")?
    .is_some();
    if first_admin_initialization {
        for user_id in configured_admins {
            if *user_id == main_admin {
                continue;
            }
            query(
                "INSERT INTO kovi_bot_authorized_admins (user_id) VALUES ($1)
                 ON CONFLICT DO NOTHING",
            )
            .bind(user_id)
            .execute(&mut *transaction)
            .await
            .context("迁移静态授权管理员")?;
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

async fn load_admins(pool: &PgPool, main_admin: i64) -> Result<BTreeSet<i64>> {
    let rows = query("SELECT user_id FROM kovi_bot_authorized_admins ORDER BY user_id")
        .fetch_all(pool)
        .await
        .context("读取授权管理员")?;
    normalize_admins(
        rows.into_iter()
            .map(|row| row.try_get::<i64, _>("user_id"))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        main_admin,
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

async fn load_admins_from_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    main_admin: i64,
) -> Result<BTreeSet<i64>> {
    let rows = query("SELECT user_id FROM kovi_bot_authorized_admins ORDER BY user_id")
        .fetch_all(&mut **transaction)
        .await
        .context("读取事务中的授权管理员")?;
    normalize_admins(
        rows.into_iter()
            .map(|row| row.try_get::<i64, _>("user_id"))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        main_admin,
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

fn normalize_admins(admins: Vec<i64>, main_admin: i64) -> Result<BTreeSet<i64>> {
    if admins.len() > MAX_AUTHORIZED_ADMINS {
        return Err(anyhow!("授权管理员最多支持 {} 人", MAX_AUTHORIZED_ADMINS));
    }
    let admins = admins.into_iter().collect::<BTreeSet<_>>();
    if admins.iter().any(|user_id| *user_id <= 0) {
        return Err(anyhow!("管理员 QQ 号必须是正整数"));
    }
    if admins.contains(&main_admin) {
        return Err(anyhow!("主管理员不能存入副管理员名单"));
    }
    Ok(admins)
}

fn apply_groups(bot: &RuntimeBot, plugin_name: &str, groups: &BTreeSet<i64>) -> Result<()> {
    bot.set_plugin_access_control_list(
        plugin_name,
        true,
        SetAccessControlList::Changes(groups.iter().copied().collect()),
    )
    .map_err(|error| anyhow!("应用 Kovi 群聊白名单失败: {}", error))
}

fn apply_admins(
    bot: &RuntimeBot,
    plugin_name: &str,
    friends: &BTreeSet<i64>,
    admins: &BTreeSet<i64>,
) -> Result<()> {
    let mut allowed_friends = friends.clone();
    allowed_friends.extend(admins.iter().copied());
    bot.set_deputy_admins(SetAdmin::Changes(admins.iter().copied().collect()))
        .map_err(|error| anyhow!("应用 Kovi 管理员列表失败: {}", error))?;
    bot.set_plugin_access_control_list(
        plugin_name,
        false,
        SetAccessControlList::Changes(allowed_friends.into_iter().collect()),
    )
    .map_err(|error| anyhow!("应用 Kovi 好友白名单失败: {}", error))
}

#[cfg(test)]
mod tests {
    use super::{
        AuthorizationCommand, command_requires_main_admin, is_authorization_command,
        normalize_admins, normalize_groups, parse_command,
    };

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
        assert_eq!(
            parse_command("#授权管理员 900000001"),
            Some(AuthorizationCommand::AddAdmin(900000001))
        );
        assert_eq!(
            parse_command("#取消授权管理员 900000001"),
            Some(AuthorizationCommand::RemoveAdmin(900000001))
        );
        assert_eq!(
            parse_command("#授权管理员列表"),
            Some(AuthorizationCommand::ListAdmins)
        );
        assert_eq!(
            parse_command("#管理员帮助"),
            Some(AuthorizationCommand::AdminHelp)
        );
        assert!(command_requires_main_admin(AuthorizationCommand::AddAdmin(
            1
        )));
        assert!(command_requires_main_admin(
            AuthorizationCommand::RemoveAdmin(1)
        ));
        assert!(command_requires_main_admin(
            AuthorizationCommand::ListAdmins
        ));
        assert!(command_requires_main_admin(AuthorizationCommand::AdminHelp));
        assert!(!command_requires_main_admin(AuthorizationCommand::List));
        assert_eq!(parse_command("#授权群 641996763 extra"), None);
        assert_eq!(parse_command("#授权管理员 900000001 extra"), None);
        assert_eq!(parse_command("#授权群 -1"), None);
        assert_eq!(parse_command("#授权群 0"), None);
        assert_eq!(parse_command("#授权管理员 -1"), None);
        assert!(is_authorization_command("#授权群 invalid"));
        assert!(is_authorization_command("#授权管理员 invalid"));
        assert!(!is_authorization_command("#授权群abc"));
    }

    #[test]
    fn normalizes_and_validates_group_ids() {
        let groups = normalize_groups(vec![3, 1, 3]).expect("重复群号应去重");
        assert_eq!(groups.into_iter().collect::<Vec<_>>(), vec![1, 3]);
        assert!(normalize_groups(vec![0]).is_err());
        assert!(normalize_groups(vec![-1]).is_err());
    }

    #[test]
    fn normalizes_and_validates_admin_ids() {
        let admins = normalize_admins(vec![3, 1, 3], 99).expect("重复管理员应去重");
        assert_eq!(admins.into_iter().collect::<Vec<_>>(), vec![1, 3]);
        assert!(normalize_admins(vec![0], 99).is_err());
        assert!(normalize_admins(vec![-1], 99).is_err());
        assert!(normalize_admins(vec![99], 99).is_err());
    }
}
