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
const MAX_AUTHORIZED_CALLERS: usize = 256;
const MAX_AUTHORIZED_FRIENDS: usize = 256;

static STATE: LazyLock<Mutex<Option<GroupAccessState>>> = LazyLock::new(|| Mutex::new(None));

struct GroupAccessState {
    plugin_name: String,
    /// Kovi 插件访问控制表里的静态好友名单；每次改动数据库授权后都要并回来，
    /// 因为白名单是整体覆盖写的。
    configured_friends: BTreeSet<i64>,
    friends: BTreeSet<i64>,
    configured_admins: BTreeSet<i64>,
    groups: BTreeSet<i64>,
    admins: BTreeSet<i64>,
    /// 被显式授权可以给芸汐打语音电话的 QQ 号。
    ///
    /// 与群白名单分开维护：群白名单决定"在哪里说话"，通话名单决定"谁能打电话
    /// 进来"——后者是私人通道，默认只有主管理员，其余人必须显式授权。
    callers: BTreeSet<i64>,
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
    AddCaller(i64),
    RemoveCaller(i64),
    ListCallers,
    CallerHelp,
    AddFriend(i64),
    RemoveFriend(i64),
    ListFriends,
    FriendHelp,
}

/// Create the table, seed it once from the static Kovi list, then apply the
/// PostgreSQL list before the plugin starts receiving events.
pub async fn initialize(bot: &RuntimeBot) -> Result<()> {
    let plugin_name = PluginBuilder::get_plugin_name();
    let configured_groups = configured_groups(bot, &plugin_name);
    let configured_friends = configured_friends(bot, &plugin_name);
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
    let configured_admins = bot
        .get_deputy_admins()
        .map_err(|error| anyhow!("读取 Kovi 副管理员失败: {}", error))?
        .into_iter()
        .collect::<Vec<_>>();
    let configured_admins = normalize_admins(configured_admins, main_admin)?;
    let configured_callers =
        normalize_callers(crate::config::get().qq_call().allowed_callers().to_vec())?;
    let pool = database_pool()?;
    initialize_schema(
        pool,
        &configured_groups,
        &configured_admins,
        &configured_callers,
        &configured_friends,
        main_admin,
    )
    .await?;
    let groups = load_groups(pool).await?;
    let mut admins = load_admins(pool, main_admin).await?;
    admins.extend(configured_admins.iter().copied());
    let admins = normalize_admins(admins.into_iter().collect(), main_admin)?;
    let mut callers = load_callers(pool).await?;
    callers.extend(configured_callers.iter().copied());
    let callers = normalize_callers(callers.into_iter().collect())?;
    // 私聊白名单 = 静态 Kovi 配置 ∪ 数据库授权 ∪ 主管理员（副管理员在应用时并入）。
    let mut friends = configured_friends.clone();
    friends.extend(load_friends(pool).await?);
    friends.insert(main_admin);
    let friends = normalize_friends(friends.into_iter().collect())?;
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
    println!(
        "[INFO] PostgreSQL 通话授权名单已加载 (表: kovi_bot_authorized_callers, 数量: {}，另加主/副管理员)",
        callers.len()
    );
    println!(
        "[INFO] PostgreSQL 私聊授权名单已加载 (表: kovi_bot_authorized_friends, 数量: {}，另加主/副管理员)",
        friends.len()
    );
    let mut state = STATE.lock().await;
    *state = Some(GroupAccessState {
        plugin_name,
        configured_friends,
        friends,
        configured_admins,
        groups,
        admins,
        callers,
        main_admin,
    });
    drop(state);
    publish_caller_allowlist().await;
    Ok(())
}

/// 把"有效通话授权"（授权名单 ∪ 副管理员 ∪ 主管理员）写成桥可读的 JSON 文件。
///
/// 桥在每个来电接听前读它，**名单外不接听**：QQ 的 1v1 通话没有对插件开放
/// "离开房间"，接通后只能静音，通话会一直留在 `connected`（2026-09-11 晚上就
/// 这样卡了一整夜），所以未授权来电必须在接通前拦掉。写失败只告警，不影响
/// 授权命令本身；文件用临时文件 + rename 原子替换，避免桥读到半个 JSON。
pub(crate) async fn publish_caller_allowlist() {
    let path = crate::config::get()
        .qq_call()
        .caller_allowlist_file()
        .to_owned();
    if path.is_empty() {
        return;
    }
    let callers = {
        let state = STATE.lock().await;
        let Some(state) = state.as_ref() else {
            return;
        };
        let mut all = state.callers.clone();
        all.extend(state.admins.iter().copied());
        all.insert(state.main_admin);
        all.into_iter().collect::<Vec<_>>()
    };
    // enabled=false 时桥会保持上游行为（接听任何来电、名单外播报婉拒），
    // 只有部署者显式打开才在接听前拦截。
    let payload = serde_json::json!({
        "enabled": crate::config::get().qq_call().caller_allowlist_enabled(),
        "callers": callers,
        "updatedAt": chrono::Local::now().to_rfc3339(),
    });
    let body = match serde_json::to_vec(&payload) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("[WARN] 通话授权名单序列化失败: {error}");
            return;
        }
    };
    let target = std::path::PathBuf::from(&path);
    let temporary = target.with_extension("json.tmp");
    let result =
        std::fs::write(&temporary, &body).and_then(|()| std::fs::rename(&temporary, &target));
    match result {
        Ok(()) => println!(
            "[INFO] 通话授权名单已同步给桥 ({} 人, 文件: {})",
            callers.len(),
            path
        ),
        Err(error) => eprintln!("[WARN] 通话授权名单写入桥失败 ({path}): {error}"),
    }
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
        || text == "#通话名单"
        || text == "#授权通话列表"
        || text == "#通话帮助"
        || text == "#授权通话帮助"
        || text == "#取消授权通话"
        || text == "#移除授权通话"
        || has_argument_prefix(text, "#授权通话")
        || has_argument_prefix(text, "#取消授权通话")
        || has_argument_prefix(text, "#移除授权通话")
        || text == "#好友名单"
        || text == "#授权好友列表"
        || text == "#好友帮助"
        || text == "#授权好友帮助"
        || text == "#取消授权好友"
        || text == "#移除授权好友"
        || has_argument_prefix(text, "#授权好友")
        || has_argument_prefix(text, "#取消授权好友")
        || has_argument_prefix(text, "#移除授权好友")
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
        AuthorizationCommand::AddCaller(user_id) => update_caller(user_id, true).await,
        AuthorizationCommand::RemoveCaller(user_id) => update_caller(user_id, false).await,
        AuthorizationCommand::ListCallers => list_callers().await,
        AuthorizationCommand::CallerHelp => Ok(caller_command_help().to_string()),
        AuthorizationCommand::AddFriend(user_id) => update_friend(bot, user_id, true).await,
        AuthorizationCommand::RemoveFriend(user_id) => update_friend(bot, user_id, false).await,
        AuthorizationCommand::ListFriends => list_friends().await,
        AuthorizationCommand::FriendHelp => Ok(friend_command_help().to_string()),
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
    if text == "#通话名单" || text == "#授权通话列表" {
        return Some(AuthorizationCommand::ListCallers);
    }
    if text == "#通话帮助" || text == "#授权通话帮助" {
        return Some(AuthorizationCommand::CallerHelp);
    }
    if let Some(user_id) = parse_user_id_argument(text, "#授权通话") {
        return Some(AuthorizationCommand::AddCaller(user_id));
    }
    if let Some(user_id) = parse_user_id_argument(text, "#取消授权通话")
        .or_else(|| parse_user_id_argument(text, "#移除授权通话"))
    {
        return Some(AuthorizationCommand::RemoveCaller(user_id));
    }
    if text == "#取消授权通话" || text == "#移除授权通话" {
        return Some(AuthorizationCommand::CallerHelp);
    }
    if text == "#好友名单" || text == "#授权好友列表" {
        return Some(AuthorizationCommand::ListFriends);
    }
    if text == "#好友帮助" || text == "#授权好友帮助" {
        return Some(AuthorizationCommand::FriendHelp);
    }
    if let Some(user_id) = parse_user_id_argument(text, "#授权好友") {
        return Some(AuthorizationCommand::AddFriend(user_id));
    }
    if let Some(user_id) = parse_user_id_argument(text, "#取消授权好友")
        .or_else(|| parse_user_id_argument(text, "#移除授权好友"))
    {
        return Some(AuthorizationCommand::RemoveFriend(user_id));
    }
    if text == "#取消授权好友" || text == "#移除授权好友" {
        return Some(AuthorizationCommand::FriendHelp);
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
    drop(state_guard);
    publish_caller_allowlist().await;
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

/// 增删「允许给芸汐打电话」的 QQ 号。
///
/// 与群白名单分开存：群白名单决定芸汐在哪里说话，通话名单决定谁能打进她的私人
/// 语音通道。主管理员与副管理员天然允许，其余人必须显式授权。
async fn update_caller(user_id: i64, add: bool) -> Result<String> {
    if user_id <= 0 {
        return Err(anyhow!("通话授权 QQ 号必须是正整数"));
    }
    let mut state_guard = STATE.lock().await;
    let state = state_guard
        .as_mut()
        .ok_or_else(|| anyhow!("授权状态尚未初始化"))?;
    if user_id == state.main_admin {
        return Ok(format!("{} 是主管理员，本来就可以打。", user_id));
    }
    if state.admins.contains(&user_id) {
        return Ok(format!("{} 是副管理员，本来就可以打。", user_id));
    }
    if add && state.callers.len() >= MAX_AUTHORIZED_CALLERS {
        return Err(anyhow!(
            "通话授权名单最多支持 {} 人",
            MAX_AUTHORIZED_CALLERS
        ));
    }

    let pool = database_pool()?;
    let mut transaction = pool.begin().await.context("开启通话授权事务")?;
    let result = if add {
        query(
            "INSERT INTO kovi_bot_authorized_callers (user_id) VALUES ($1)
             ON CONFLICT DO NOTHING",
        )
        .bind(user_id)
        .execute(&mut *transaction)
        .await
        .context("写入通话授权名单")?
    } else {
        query("DELETE FROM kovi_bot_authorized_callers WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *transaction)
            .await
            .context("删除通话授权名单")?
    };

    if result.rows_affected() == 0 {
        transaction.rollback().await.ok();
        return Ok(if add {
            format!("{} 已在通话授权名单中。", user_id)
        } else {
            format!("{} 不在通话授权名单中。", user_id)
        });
    }

    let new_callers = load_callers_from_transaction(&mut transaction).await?;
    if let Err(error) = transaction.commit().await {
        return Err(error).context("提交通话授权事务");
    }
    state.callers = new_callers;
    let caller_count = state.callers.len();
    drop(state_guard);
    publish_caller_allowlist().await;
    println!(
        "[INFO] 通话授权名单已更新 (操作: {}, QQ: {}, 数量: {})",
        if add { "添加" } else { "移除" },
        user_id,
        caller_count
    );
    Ok(if add {
        format!("已授权 {} 给芸汐打电话。", user_id)
    } else {
        format!("已取消 {} 的通话授权。", user_id)
    })
}

async fn list_callers() -> Result<String> {
    let state = STATE.lock().await;
    let state = state
        .as_ref()
        .ok_or_else(|| anyhow!("授权状态尚未初始化"))?;
    let mut entries = vec![format!("{}（主管理员）", state.main_admin)];
    entries.extend(state.admins.iter().map(|id| format!("{id}（副管理员）")));
    entries.extend(state.callers.iter().map(ToString::to_string));
    Ok(format!("当前可以和芸汐打电话的 QQ：{}", entries.join("、")))
}

pub(crate) fn caller_command_help() -> &'static str {
    "用法：#授权通话 QQ号、#取消授权通话 QQ号、#通话名单。主管理员与副管理员本来就可以通话，其他人需要显式授权；未授权的人打进来只会听到一句婉拒。仅机器人管理员可执行。"
}

/// 增删「允许私聊芸汐」的 QQ 号。
///
/// 私聊等于进入芸汐的私人对话（她会带着长期记忆说话），比群准入敏感，因此
/// 这三个命令限定主管理员执行——与 `#授权管理员` 同级，而不是 `#授权群` 同级。
async fn update_friend(bot: &RuntimeBot, user_id: i64, add: bool) -> Result<String> {
    if user_id <= 0 {
        return Err(anyhow!("好友授权 QQ 号必须是正整数"));
    }
    let mut state_guard = STATE.lock().await;
    let state = state_guard
        .as_mut()
        .ok_or_else(|| anyhow!("授权状态尚未初始化"))?;
    if user_id == state.main_admin {
        return Ok(format!("{} 是主管理员，本来就能私聊。", user_id));
    }
    if state.admins.contains(&user_id) {
        return Ok(format!("{} 是副管理员，本来就能私聊。", user_id));
    }
    if add && state.friends.len() >= MAX_AUTHORIZED_FRIENDS {
        return Err(anyhow!(
            "私聊授权名单最多支持 {} 人",
            MAX_AUTHORIZED_FRIENDS
        ));
    }

    // 应用白名单需要这几个字段，先取出来，避免与 state.friends 的可变借用冲突。
    let plugin_name = state.plugin_name.clone();
    let admins = state.admins.clone();
    let configured_friends = state.configured_friends.clone();
    let main_admin = state.main_admin;
    let old_friends = state.friends.clone();

    let pool = database_pool()?;
    let mut transaction = pool.begin().await.context("开启好友授权事务")?;
    let result = if add {
        query(
            "INSERT INTO kovi_bot_authorized_friends (user_id) VALUES ($1)
             ON CONFLICT DO NOTHING",
        )
        .bind(user_id)
        .execute(&mut *transaction)
        .await
        .context("写入好友授权名单")?
    } else {
        query("DELETE FROM kovi_bot_authorized_friends WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *transaction)
            .await
            .context("删除好友授权名单")?
    };

    if result.rows_affected() == 0 {
        transaction.rollback().await.ok();
        return Ok(if add {
            format!("{} 已在私聊授权名单中。", user_id)
        } else {
            format!("{} 不在私聊授权名单中。", user_id)
        });
    }

    let mut new_friends = load_friends_from_transaction(&mut transaction).await?;
    new_friends.extend(configured_friends);
    new_friends.insert(main_admin);
    if let Err(error) = apply_admins(bot, &plugin_name, &new_friends, &admins) {
        transaction.rollback().await.ok();
        return Err(error);
    }
    if let Err(error) = transaction.commit().await {
        let _ = apply_admins(bot, &plugin_name, &old_friends, &admins);
        return Err(error).context("提交好友授权事务");
    }

    state.friends = new_friends;
    println!(
        "[INFO] 私聊授权名单已更新 (操作: {}, QQ: {}, 数量: {})",
        if add { "添加" } else { "移除" },
        user_id,
        state.friends.len()
    );
    Ok(if add {
        format!("已授权 {} 私聊芸汐。", user_id)
    } else {
        format!("已取消 {} 的私聊授权。", user_id)
    })
}

async fn list_friends() -> Result<String> {
    let state = STATE.lock().await;
    let state = state
        .as_ref()
        .ok_or_else(|| anyhow!("授权状态尚未初始化"))?;
    let mut entries = vec![format!("{}（主管理员）", state.main_admin)];
    entries.extend(state.admins.iter().map(|id| format!("{id}（副管理员）")));
    entries.extend(
        state
            .friends
            .iter()
            .filter(|id| **id != state.main_admin && !state.admins.contains(id))
            .map(ToString::to_string),
    );
    Ok(format!("当前可以私聊芸汐的 QQ：{}", entries.join("、")))
}

pub(crate) fn friend_command_help() -> &'static str {
    "用法：#授权好友 QQ号、#取消授权好友 QQ号、#好友名单。授权后对方可以私聊芸汐（她会带着长期记忆说话）。主管理员与副管理员默认即可私聊。仅主管理员可执行。"
}

/// 该 QQ 号是否允许和芸汐通话。
///
/// 主管理员、副管理员、显式授权的名单都放行。授权状态尚未初始化时返回 false，
/// 由调用方回退到静态配置判断，避免初始化失败导致所有人都打不进来。
pub(crate) async fn is_authorized_caller(user_id: i64) -> bool {
    let state = STATE.lock().await;
    state
        .as_ref()
        .is_some_and(|state| caller_is_authorized(state, user_id))
}

/// 判定部分独立成纯函数：不依赖全局状态，测试之间不会互相踩。
fn caller_is_authorized(state: &GroupAccessState, user_id: i64) -> bool {
    user_id == state.main_admin
        || state.admins.contains(&user_id)
        || state.callers.contains(&user_id)
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
            // 私聊等于进入芸汐的私人对话，授权门槛与管理员同级。
            | AuthorizationCommand::AddFriend(_)
            | AuthorizationCommand::RemoveFriend(_)
            | AuthorizationCommand::ListFriends
            | AuthorizationCommand::FriendHelp
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
    configured_callers: &BTreeSet<i64>,
    configured_friends: &BTreeSet<i64>,
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
    query(
        "CREATE TABLE IF NOT EXISTS kovi_bot_authorized_callers (
            user_id BIGINT PRIMARY KEY CHECK (user_id > 0),
            authorized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&mut *transaction)
    .await
    .context("创建通话授权表")?;
    query(
        "CREATE TABLE IF NOT EXISTS kovi_bot_authorized_callers_meta (
            id SMALLINT PRIMARY KEY CHECK (id = 1),
            initialized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&mut *transaction)
    .await
    .context("创建通话授权元数据表")?;
    let first_caller_initialization = query(
        "INSERT INTO kovi_bot_authorized_callers_meta (id) VALUES (1)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .fetch_optional(&mut *transaction)
    .await
    .context("初始化通话授权元数据")?
    .is_some();
    if first_caller_initialization {
        // 静态配置里的 allowed_callers 只在首次初始化时迁移一次，之后以数据库为准。
        for user_id in configured_callers {
            if *user_id == main_admin {
                continue;
            }
            query(
                "INSERT INTO kovi_bot_authorized_callers (user_id) VALUES ($1)
                 ON CONFLICT DO NOTHING",
            )
            .bind(user_id)
            .execute(&mut *transaction)
            .await
            .context("迁移静态通话授权名单")?;
        }
    }
    query(
        "CREATE TABLE IF NOT EXISTS kovi_bot_authorized_friends (
            user_id BIGINT PRIMARY KEY CHECK (user_id > 0),
            authorized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&mut *transaction)
    .await
    .context("创建好友授权表")?;
    query(
        "CREATE TABLE IF NOT EXISTS kovi_bot_authorized_friends_meta (
            id SMALLINT PRIMARY KEY CHECK (id = 1),
            initialized_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&mut *transaction)
    .await
    .context("创建好友授权元数据表")?;
    let first_friend_initialization = query(
        "INSERT INTO kovi_bot_authorized_friends_meta (id) VALUES (1)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .fetch_optional(&mut *transaction)
    .await
    .context("初始化好友授权元数据")?
    .is_some();
    if first_friend_initialization {
        // 静态 KOVI_ALLOWED_FRIENDS 同样只在首次迁移一次；Kovi 访问控制表每次
        // 应用白名单时都会被覆盖，所以数据库才是持久的那一份。
        for user_id in configured_friends {
            if *user_id == main_admin {
                continue;
            }
            query(
                "INSERT INTO kovi_bot_authorized_friends (user_id) VALUES ($1)
                 ON CONFLICT DO NOTHING",
            )
            .bind(user_id)
            .execute(&mut *transaction)
            .await
            .context("迁移静态好友白名单")?;
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

async fn load_callers(pool: &PgPool) -> Result<BTreeSet<i64>> {
    let rows = query("SELECT user_id FROM kovi_bot_authorized_callers ORDER BY user_id")
        .fetch_all(pool)
        .await
        .context("读取通话授权名单")?;
    normalize_callers(
        rows.into_iter()
            .map(|row| row.try_get::<i64, _>("user_id"))
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

/// 读取允许通话的 QQ 名单；不做管理员去重，调用方按"并集"判断。
async fn load_callers_from_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
) -> Result<BTreeSet<i64>> {
    let rows = query("SELECT user_id FROM kovi_bot_authorized_callers ORDER BY user_id")
        .fetch_all(&mut **transaction)
        .await
        .context("读取事务中的通话授权名单")?;
    normalize_callers(
        rows.into_iter()
            .map(|row| row.try_get::<i64, _>("user_id"))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )
}

/// 读取允许私聊的 QQ 名单（数据库部分；调用方再并上静态配置）。
async fn load_friends_from_transaction(
    transaction: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
) -> Result<BTreeSet<i64>> {
    let rows = query("SELECT user_id FROM kovi_bot_authorized_friends ORDER BY user_id")
        .fetch_all(&mut **transaction)
        .await
        .context("读取事务中的好友授权名单")?;
    normalize_friends(
        rows.into_iter()
            .map(|row| row.try_get::<i64, _>("user_id"))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )
}

async fn load_friends(pool: &PgPool) -> Result<BTreeSet<i64>> {
    let rows = query("SELECT user_id FROM kovi_bot_authorized_friends ORDER BY user_id")
        .fetch_all(pool)
        .await
        .context("读取好友授权名单")?;
    normalize_friends(
        rows.into_iter()
            .map(|row| row.try_get::<i64, _>("user_id"))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )
}

fn normalize_friends(friends: Vec<i64>) -> Result<BTreeSet<i64>> {
    if friends.len() > MAX_AUTHORIZED_FRIENDS {
        return Err(anyhow!(
            "私聊授权名单最多支持 {} 人",
            MAX_AUTHORIZED_FRIENDS
        ));
    }
    let friends = friends.into_iter().collect::<BTreeSet<_>>();
    if friends.iter().any(|user_id| *user_id <= 0) {
        return Err(anyhow!("好友授权 QQ 号必须是正整数"));
    }
    Ok(friends)
}

fn normalize_callers(callers: Vec<i64>) -> Result<BTreeSet<i64>> {
    if callers.len() > MAX_AUTHORIZED_CALLERS {
        return Err(anyhow!(
            "通话授权名单最多支持 {} 人",
            MAX_AUTHORIZED_CALLERS
        ));
    }
    let callers = callers.into_iter().collect::<BTreeSet<_>>();
    if callers.iter().any(|user_id| *user_id <= 0) {
        return Err(anyhow!("通话授权 QQ 号必须是正整数"));
    }
    Ok(callers)
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
        AuthorizationCommand, GroupAccessState, STATE, authorize_group_send, caller_is_authorized,
        command_requires_main_admin, is_authorization_command, normalize_admins, normalize_callers,
        normalize_friends, normalize_groups, parse_command,
    };
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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
    fn parses_call_authorization_commands() {
        assert_eq!(
            parse_command("#授权通话 900000001"),
            Some(AuthorizationCommand::AddCaller(900000001))
        );
        assert_eq!(
            parse_command("#取消授权通话 900000001"),
            Some(AuthorizationCommand::RemoveCaller(900000001))
        );
        assert_eq!(
            parse_command("#移除授权通话 900000001"),
            Some(AuthorizationCommand::RemoveCaller(900000001))
        );
        assert_eq!(
            parse_command("#通话名单"),
            Some(AuthorizationCommand::ListCallers)
        );
        assert_eq!(
            parse_command("#授权通话列表"),
            Some(AuthorizationCommand::ListCallers)
        );
        assert_eq!(
            parse_command("#通话帮助"),
            Some(AuthorizationCommand::CallerHelp)
        );
        // 缺参数的取消命令退化成帮助，而不是静默失败。
        assert_eq!(
            parse_command("#取消授权通话"),
            Some(AuthorizationCommand::CallerHelp)
        );
        // 通话授权不是特权变更，普通管理员即可执行。
        assert!(!command_requires_main_admin(
            AuthorizationCommand::AddCaller(1)
        ));
        assert!(!command_requires_main_admin(
            AuthorizationCommand::RemoveCaller(1)
        ));
        assert!(!command_requires_main_admin(
            AuthorizationCommand::ListCallers
        ));
        // 参数校验与其它授权命令一致。
        assert_eq!(parse_command("#授权通话 900000001 extra"), None);
        assert_eq!(parse_command("#授权通话 -1"), None);
        assert_eq!(parse_command("#授权通话 0"), None);
        assert_eq!(parse_command("#授权通话 abc"), None);
        assert!(is_authorization_command("#授权通话 invalid"));
        assert!(is_authorization_command("#通话名单"));
        assert!(is_authorization_command("#取消授权通话"));
        assert!(!is_authorization_command("#通话abc"));
    }

    #[test]
    fn call_authorization_covers_admins_and_explicit_callers() {
        let main_admin = 9_130_001;
        let deputy = 9_130_002;
        let granted = 9_130_003;
        let stranger = 9_130_004;
        let state = GroupAccessState {
            plugin_name: "test".to_string(),
            configured_friends: BTreeSet::new(),
            friends: BTreeSet::new(),
            configured_admins: BTreeSet::new(),
            groups: BTreeSet::new(),
            admins: BTreeSet::from([deputy]),
            callers: BTreeSet::from([granted]),
            main_admin,
        };

        assert!(caller_is_authorized(&state, main_admin), "主管理员应可通话");
        assert!(caller_is_authorized(&state, deputy), "副管理员应可通话");
        assert!(caller_is_authorized(&state, granted), "显式授权者应可通话");
        assert!(
            !caller_is_authorized(&state, stranger),
            "未授权者不应可通话"
        );
    }

    #[test]
    fn parses_friend_authorization_commands() {
        assert_eq!(
            parse_command("#授权好友 900000001"),
            Some(AuthorizationCommand::AddFriend(900000001))
        );
        assert_eq!(
            parse_command("#取消授权好友 900000001"),
            Some(AuthorizationCommand::RemoveFriend(900000001))
        );
        assert_eq!(
            parse_command("#移除授权好友 900000001"),
            Some(AuthorizationCommand::RemoveFriend(900000001))
        );
        assert_eq!(
            parse_command("#好友名单"),
            Some(AuthorizationCommand::ListFriends)
        );
        assert_eq!(
            parse_command("#授权好友列表"),
            Some(AuthorizationCommand::ListFriends)
        );
        assert_eq!(
            parse_command("#好友帮助"),
            Some(AuthorizationCommand::FriendHelp)
        );
        assert_eq!(
            parse_command("#取消授权好友"),
            Some(AuthorizationCommand::FriendHelp)
        );
        // 私聊授权等于进入芸汐的私人对话，门槛与管理员同级：仅主管理员。
        assert!(command_requires_main_admin(
            AuthorizationCommand::AddFriend(1)
        ));
        assert!(command_requires_main_admin(
            AuthorizationCommand::RemoveFriend(1)
        ));
        assert!(command_requires_main_admin(
            AuthorizationCommand::ListFriends
        ));
        assert!(command_requires_main_admin(
            AuthorizationCommand::FriendHelp
        ));
        // 参数校验与其它授权命令一致。
        assert_eq!(parse_command("#授权好友 900000001 extra"), None);
        assert_eq!(parse_command("#授权好友 -1"), None);
        assert_eq!(parse_command("#授权好友 0"), None);
        assert_eq!(parse_command("#授权好友 abc"), None);
        assert!(is_authorization_command("#授权好友 invalid"));
        assert!(is_authorization_command("#好友名单"));
        assert!(!is_authorization_command("#好友abc"));
    }

    #[test]
    fn normalizes_and_validates_friend_ids() {
        let friends = normalize_friends(vec![7, 3, 7]).expect("重复 QQ 号应去重");
        assert_eq!(friends.into_iter().collect::<Vec<_>>(), vec![3, 7]);
        assert!(normalize_friends(vec![0]).is_err());
        assert!(normalize_friends(vec![-1]).is_err());
    }

    #[test]
    fn normalizes_and_validates_caller_ids() {
        let callers = normalize_callers(vec![7, 3, 7]).expect("重复 QQ 号应去重");
        assert_eq!(callers.into_iter().collect::<Vec<_>>(), vec![3, 7]);
        assert!(normalize_callers(vec![0]).is_err());
        assert!(normalize_callers(vec![-1]).is_err());
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

    #[test]
    fn group_send_authorization_pins_the_snapshot_until_commit() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 9_120_001;
                let groups = BTreeSet::from([group_id]);
                *STATE.lock().await = Some(GroupAccessState {
                    plugin_name: "test".to_string(),
                    configured_friends: BTreeSet::new(),
                    friends: BTreeSet::new(),
                    configured_admins: BTreeSet::new(),
                    groups,
                    admins: BTreeSet::new(),
                    callers: BTreeSet::new(),
                    main_admin: 9_120_002,
                });

                let authorization = authorize_group_send(group_id)
                    .await
                    .expect("发送应取得授权快照");
                let revoked = Arc::new(AtomicBool::new(false));
                let revoked_in_task = Arc::clone(&revoked);
                let revoke = kovi::tokio::spawn(async move {
                    let mut state = STATE.lock().await;
                    state
                        .as_mut()
                        .expect("测试授权状态应存在")
                        .groups
                        .remove(&group_id);
                    revoked_in_task.store(true, Ordering::Release);
                });
                kovi::tokio::task::yield_now().await;
                assert!(!revoked.load(Ordering::Acquire));

                drop(authorization);
                revoke.await.expect("撤销任务应完成");
                assert!(revoked.load(Ordering::Acquire));
                assert!(
                    authorize_group_send(group_id).await.is_err(),
                    "a fresh pre-effect authorization must observe revocation"
                );
                assert!(
                    !STATE
                        .lock()
                        .await
                        .as_ref()
                        .expect("测试授权状态应存在")
                        .groups
                        .contains(&group_id)
                );
                *STATE.lock().await = None;
            });
    }
}
