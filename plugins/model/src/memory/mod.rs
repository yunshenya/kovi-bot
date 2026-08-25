//! # 记忆管理系统
//!
//! 提供智能的长期记忆存储和检索功能，支持：
//! - 多类型记忆分类存储
//! - 智能重要性评分
//! - 上下文相关记忆检索
//! - 用户和群组档案管理
//! - 机器人人格状态维护
//! - 自动记忆清理和优化

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use kovi::tokio::sync::Mutex;
use serde::{Deserialize, Serialize};
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, PgPoolOptions, Postgres};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use uuid::Uuid;

static MEMORY_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MAX_USER_PROFILES: usize = 10_000;
const MAX_GROUP_PROFILES: usize = 2_000;

/// 全局记忆管理器实例
///
/// 使用LazyLock确保线程安全的单例模式，在首次访问时初始化
/// PostgreSQL 为空时会从 "bot_memory.json" 自动迁移旧数据
pub static MEMORY_MANAGER: LazyLock<Arc<MemoryManager>> =
    LazyLock::new(|| Arc::new(MemoryManager::new("bot_memory.json")));

/// 记忆条目结构体
///
/// 存储单条记忆的完整信息，包括内容、时间戳、类型、重要性等
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MemoryEntry {
    /// 记忆唯一标识符
    pub id: String,
    /// 记忆内容文本
    pub content: String,
    /// 记忆创建时间
    pub timestamp: DateTime<Local>,
    /// 记忆类型分类
    pub memory_type: MemoryType,
    /// 重要性评分 (0-10)，10表示最重要
    pub importance: u8,
    /// 标签列表，用于快速检索和分类
    pub tags: Vec<String>,
    /// 上下文信息，描述记忆产生的环境
    pub context: String,
    /// 该记忆所属的用户或群组。旧版记忆反序列化时为 `None`。
    #[serde(default)]
    pub subject_id: Option<i64>,
}

/// 主动消息的持久化限频状态。
///
/// 这类状态不能放在普通记忆里，否则记忆达到容量上限时会被清理，
/// 服务重启后就可能重新触发主动消息。
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct ProactiveState {
    pub(crate) last_decision_at: Option<DateTime<Local>>,
    pub(crate) last_sent_at: Option<DateTime<Local>>,
    pub(crate) day_key: String,
    pub(crate) daily_sent_count: u32,
}

impl ProactiveState {
    pub(crate) fn daily_count_for(&self, day_key: &str) -> u32 {
        if self.day_key == day_key {
            self.daily_sent_count
        } else {
            0
        }
    }
}

/// 记忆类型枚举
///
/// 定义不同类型的记忆，用于分类存储和检索
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum MemoryType {
    /// 对话记忆：存储用户与机器人的对话内容
    Conversation,
    /// 用户档案：存储用户的基本信息和偏好
    UserProfile,
    /// 群组信息：存储群组的基本信息和活跃状态
    GroupInfo,
    /// 事件记忆：存储重要事件和里程碑
    Event,
    /// 偏好设置：存储用户或系统的偏好配置
    Preference,
    /// 情绪状态：存储机器人的情绪变化记录
    Emotion,
}

/// 模型可选择的记忆类型。查询范围和所属对象不在协议中，由程序强制指定。
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoryLookupType {
    Conversation,
    UserProfile,
    GroupInfo,
    Event,
    Preference,
    Emotion,
}

/// 持久化会话范围。自由文本 `context` 只负责描述事件类型，用户/群隔离由该类型决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationScope {
    Private,
    Group,
}

impl ConversationScope {
    fn parse(context: &str) -> Option<Self> {
        if context == "private"
            || context.starts_with("private_")
            || context.starts_with("proactive_private_")
        {
            Some(Self::Private)
        } else if context == "group"
            || context.starts_with("group_")
            || context.starts_with("proactive_group_")
        {
            Some(Self::Group)
        } else {
            None
        }
    }

    const fn database_value(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
        }
    }
}

impl MemoryLookupType {
    fn database_value(&self) -> &'static str {
        match self {
            Self::Conversation => "Conversation",
            Self::UserProfile => "UserProfile",
            Self::GroupInfo => "GroupInfo",
            Self::Event => "Event",
            Self::Preference => "Preference",
            Self::Emotion => "Emotion",
        }
    }
}

fn default_memory_lookup_limit() -> usize {
    5
}

/// 模型能提交的受限查询参数。故意不包含 SQL、表名、会话对象或聊天范围。
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct MemoryLookup {
    pub(crate) keywords: Vec<String>,
    pub(crate) since_days: Option<u32>,
    pub(crate) memory_types: Vec<MemoryLookupType>,
    pub(crate) min_importance: Option<u8>,
    #[serde(default = "default_memory_lookup_limit")]
    pub(crate) limit: usize,
}

impl Default for MemoryLookup {
    fn default() -> Self {
        Self {
            keywords: Vec::new(),
            since_days: None,
            memory_types: Vec::new(),
            min_importance: None,
            limit: default_memory_lookup_limit(),
        }
    }
}

impl MemoryLookup {
    fn normalized(mut self, max_results: usize, max_days: u32) -> Self {
        let mut seen_keywords = HashSet::new();
        self.keywords = self
            .keywords
            .into_iter()
            .map(|keyword| keyword.trim().chars().take(48).collect::<String>())
            .filter(|keyword| !keyword.is_empty())
            .filter(|keyword| seen_keywords.insert(keyword.to_lowercase()))
            .take(5)
            .collect();
        self.memory_types.truncate(6);
        self.since_days = self.since_days.map(|days| days.clamp(1, max_days));
        self.min_importance = self.min_importance.map(|importance| importance.min(10));
        self.limit = self.limit.clamp(1, max_results);

        // 完全空的查询默认只取最近一周，避免模型无条件遍历全部历史。
        if self.keywords.is_empty()
            && self.since_days.is_none()
            && self.memory_types.is_empty()
            && self.min_importance.is_none()
        {
            self.since_days = Some(7);
        }
        self
    }
}

/// 用户档案结构体
///
/// 存储用户的详细信息，用于个性化交互和关系管理
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserProfile {
    /// 用户唯一标识符
    pub user_id: i64,
    /// 用户昵称
    pub nickname: String,
    /// 用户性格特征列表
    pub personality_traits: Vec<String>,
    /// 用户兴趣标签列表
    pub interests: Vec<String>,
    /// 关系亲密度 (0-10)，10表示最亲密
    pub relationship_level: u8,
    /// 最后互动时间
    pub last_interaction: DateTime<Local>,
    /// 总互动次数
    pub interaction_count: u32,
    /// 最近一次私聊时间。只有真正私聊过的用户才会成为主动私聊候选。
    #[serde(default)]
    pub last_private_interaction: Option<DateTime<Local>>,
    /// 情绪历史记录
    pub mood_history: Vec<MoodEntry>,
}

/// 情绪记录条目
///
/// 记录单次情绪变化的信息
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MoodEntry {
    /// 情绪名称
    pub mood: String,
    /// 情绪强度 (0-10)，10表示最强烈
    pub intensity: u8,
    /// 情绪变化时间
    pub timestamp: DateTime<Local>,
    /// 情绪触发原因
    pub trigger: String,
}

/// 群组档案结构体
///
/// 存储群组的基本信息和活跃状态
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GroupProfile {
    /// 群组唯一标识符
    pub group_id: i64,
    /// 群组名称
    pub group_name: String,
    /// 活跃成员ID列表
    pub active_members: Vec<i64>,
    /// 群组整体性格特征
    pub group_personality: String,
    /// 群组常讨论的话题列表
    pub conversation_topics: Vec<String>,
    /// 最后活跃时间
    pub last_activity: DateTime<Local>,
    /// 活跃度等级 (0-10)，10表示最活跃
    pub activity_level: u8,
}

/// 机器人人格结构体
///
/// 存储机器人的当前状态和人格特征
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BotPersonality {
    /// 当前情绪状态
    pub current_mood: String,
    /// 情绪强度 (0-10)
    pub mood_intensity: u8,
    /// 能量水平 (0-10)，影响回复的积极性
    pub energy_level: u8,
    /// 社交信心 (0-10)，影响主动聊天的频率
    pub social_confidence: u8,
    /// 好奇心水平 (0-10)，影响话题探索的积极性
    pub curiosity_level: u8,
    /// 最后情绪变化时间
    pub last_mood_change: DateTime<Local>,
    /// 人格特征列表
    pub personality_traits: Vec<String>,
}

/// 记忆管理器结构体
///
/// 负责管理所有类型的记忆数据，包括：
/// - 对话记忆的存储和检索
/// - 用户和群组档案的管理
/// - 机器人人格状态的维护
/// - 记忆的持久化存储和加载
/// - 智能记忆清理和优化
#[derive(Clone)]
pub struct MemoryManager {
    /// 记忆条目存储 (ID -> MemoryEntry)
    memories: Arc<Mutex<HashMap<String, MemoryEntry>>>,
    /// 用户档案存储 (UserID -> UserProfile)
    user_profiles: Arc<Mutex<HashMap<i64, UserProfile>>>,
    /// 群组档案存储 (GroupID -> GroupProfile)
    group_profiles: Arc<Mutex<HashMap<i64, GroupProfile>>>,
    /// 滚动压缩后的对话摘要（`group:<id>` 或 `private:<id>` -> 摘要）。
    conversation_summaries: Arc<Mutex<HashMap<String, String>>>,
    /// 摘要最后更新时间；与摘要分开保存以兼容旧 JSON 快照。
    conversation_summary_updated_at: Arc<Mutex<HashMap<String, DateTime<Local>>>>,
    /// 主动消息限频状态，独立于可压缩的普通记忆。
    proactive_states: Arc<Mutex<HashMap<String, ProactiveState>>>,
    /// 机器人人格状态
    bot_personality: Arc<Mutex<BotPersonality>>,
    /// 旧版记忆文件路径，仅用于迁移和无数据库的单元测试实例
    memory_file: String,
    /// 串行化持久化操作，避免多个任务同时覆盖记忆快照。
    save_lock: Arc<Mutex<()>>,
    /// 串行化 PostgreSQL 初始化，避免并发调用重复建立连接池和迁移 schema。
    database_init_lock: Arc<Mutex<()>>,
    /// PostgreSQL 连接池。测试和未初始化的独立实例仍可使用 JSON 文件后端。
    database_pool: Arc<OnceLock<PgPool>>,
}

impl MemoryManager {
    /// 创建新的记忆管理器实例
    ///
    /// # 参数
    /// * `memory_file` - 旧版 JSON 记忆路径（数据库迁移源和测试后端）
    ///
    /// # 返回值
    /// 返回初始化的MemoryManager实例，包含默认的机器人人格设置
    ///
    /// # 默认人格特征
    /// - 当前情绪：中性
    /// - 情绪强度：5/10
    /// - 能量水平：7/10
    /// - 社交信心：6/10
    /// - 好奇心：8/10
    /// - 性格特征：好奇、顽皮、有同理心、轻微傲娇
    pub fn new(memory_file: &str) -> Self {
        harden_memory_file_permissions(memory_file);
        // 构造阶段同步读取，保证第一条消息不会和后台加载任务竞态并覆盖旧数据。
        let data = match fs::read_to_string(memory_file) {
            Ok(json) => match serde_json::from_str::<MemoryData>(&json) {
                Ok(data) => data,
                Err(error) => {
                    let backup_path =
                        format!("{}.corrupt.{}", memory_file, Local::now().timestamp());
                    match fs::copy(memory_file, &backup_path) {
                        Ok(_) => eprintln!(
                            "[ERROR] 记忆文件解析失败，已备份到 {}: {}",
                            backup_path, error
                        ),
                        Err(backup_error) => eprintln!(
                            "[ERROR] 记忆文件解析失败且备份失败 ({}): {}",
                            backup_error, error
                        ),
                    }
                    MemoryData::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => MemoryData::default(),
            Err(error) => {
                eprintln!("[ERROR] 记忆文件读取失败，将使用空记忆: {}", error);
                MemoryData::default()
            }
        };

        let mut data = data;
        let now = Local::now();
        for summary_key in data.conversation_summaries.keys() {
            data.conversation_summary_updated_at
                .entry(summary_key.clone())
                .or_insert(now);
        }
        Self {
            memories: Arc::new(Mutex::new(data.memories)),
            user_profiles: Arc::new(Mutex::new(data.user_profiles)),
            group_profiles: Arc::new(Mutex::new(data.group_profiles)),
            conversation_summaries: Arc::new(Mutex::new(data.conversation_summaries)),
            conversation_summary_updated_at: Arc::new(Mutex::new(
                data.conversation_summary_updated_at,
            )),
            proactive_states: Arc::new(Mutex::new(data.proactive_states)),
            bot_personality: Arc::new(Mutex::new(data.bot_personality)),
            memory_file: memory_file.to_string(),
            save_lock: Arc::new(Mutex::new(())),
            database_init_lock: Arc::new(Mutex::new(())),
            database_pool: Arc::new(OnceLock::new()),
        }
    }

    /// 初始化 PostgreSQL 存储，并在数据库为空时自动导入旧 JSON 记忆。
    ///
    /// 连接串只从 `DATABASE_URL` 环境变量读取，避免凭据进入配置文件或源码。
    pub async fn initialize_database(&self) -> Result<()> {
        if self.database_pool.get().is_some() {
            return Ok(());
        }

        // `OnceLock` only serializes the final write. Hold an async guard over
        // the full initialization so concurrent callers reuse the same pool.
        let _init_guard = self.database_init_lock.lock().await;
        if self.database_pool.get().is_some() {
            return Ok(());
        }

        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("未设置 DATABASE_URL，无法启用 PostgreSQL 记忆存储"))?;
        if database_url.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "DATABASE_URL 为空，无法启用 PostgreSQL 记忆存储"
            ));
        }

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .map_err(|error| anyhow::anyhow!("连接 PostgreSQL 失败: {}", error))?;

        // 保留旧快照表作为一次性迁移源，新写入改用按实体拆分的表。
        query(
            r#"
            CREATE TABLE IF NOT EXISTS kovi_bot_memory (
                id SMALLINT PRIMARY KEY CHECK (id = 1),
                payload JSONB NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(&pool)
        .await
        .map_err(|error| anyhow::anyhow!("创建 PostgreSQL 记忆表失败: {}", error))?;

        Self::create_normalized_schema(&pool).await?;
        let data = if Self::normalized_storage_has_data(&pool).await? {
            println!("[INFO] 已从 PostgreSQL 分表加载记忆");
            Self::load_normalized_data(&pool).await?
        } else {
            let stored_payload = query("SELECT payload FROM kovi_bot_memory WHERE id = 1")
                .fetch_optional(&pool)
                .await
                .map_err(|error| anyhow::anyhow!("读取旧 PostgreSQL 记忆失败: {}", error))?
                .map(|row| row.get::<serde_json::Value, _>("payload"));
            let data = if let Some(payload) = stored_payload {
                println!("[INFO] 正在将旧 PostgreSQL JSONB 快照迁移到分表存储");
                serde_json::from_value(payload)
                    .map_err(|error| anyhow::anyhow!("解析旧 PostgreSQL 记忆失败: {}", error))?
            } else {
                let data = self.snapshot().await;
                if std::path::Path::new(&self.memory_file).exists() {
                    println!(
                        "[INFO] 正在将旧记忆文件 {} 导入 PostgreSQL 分表（原文件保留）",
                        self.memory_file
                    );
                }
                data
            };
            Self::write_normalized_snapshot(&pool, &data).await?;
            data
        };
        self.replace_data(data).await;

        self.database_pool
            .set(pool)
            .map_err(|_| anyhow::anyhow!("PostgreSQL 记忆存储已被并发初始化"))?;
        Ok(())
    }

    /// 返回已初始化的 PostgreSQL 连接池，供独立的持久化功能复用同一数据库。
    pub(crate) fn database_pool(&self) -> Option<&PgPool> {
        self.database_pool.get()
    }

    /// 获取主动消息的独立限频状态。
    pub(crate) async fn get_proactive_state(&self, state_key: &str) -> Option<ProactiveState> {
        if let Some(pool) = self.database_pool.get() {
            match query(
                "SELECT last_decision_at, last_sent_at, day_key, daily_sent_count FROM kovi_bot_proactive_state WHERE state_key = $1",
            )
            .bind(state_key)
            .fetch_optional(pool)
            .await
            {
                Ok(Some(row)) => {
                    return Some(ProactiveState {
                        last_decision_at: row
                            .get::<Option<DateTime<Utc>>, _>("last_decision_at")
                            .map(|value| value.with_timezone(&Local)),
                        last_sent_at: row
                            .get::<Option<DateTime<Utc>>, _>("last_sent_at")
                            .map(|value| value.with_timezone(&Local)),
                        day_key: row.get("day_key"),
                        daily_sent_count: u32::try_from(
                            row.get::<i32, _>("daily_sent_count"),
                        )
                        .unwrap_or_default(),
                    });
                }
                Ok(None) => return None,
                Err(error) => {
                    eprintln!("[WARN] 查询主动消息限频状态失败，回退内存缓存: {}", error);
                }
            }
        }

        self.proactive_states.lock().await.get(state_key).cloned()
    }

    /// 记录一次主动决策和/或成功发送。
    ///
    /// `decision_key` 用于限制模型决策频率；`sent_keys` 同时更新全局、目标和
    /// 主人专属计数。所有状态都在同一事务中写入，避免重启或记忆压缩造成丢失。
    pub(crate) async fn record_proactive_event(
        &self,
        decision_key: Option<&str>,
        sent_keys: &[String],
        occurred_at: DateTime<Local>,
    ) -> Result<()> {
        let day_key = occurred_at.format("%Y-%m-%d").to_string();
        let mut unique_sent_keys = Vec::new();
        for key in sent_keys {
            if !unique_sent_keys.contains(&key) {
                unique_sent_keys.push(key);
            }
        }
        let _save_guard = self.save_lock.lock().await;

        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            if let Some(state_key) = decision_key {
                query(
                    r#"
                    INSERT INTO kovi_bot_proactive_state
                        (state_key, last_decision_at, last_sent_at, day_key, daily_sent_count)
                    VALUES ($1, $2, NULL, '', 0)
                    ON CONFLICT (state_key) DO UPDATE SET
                        last_decision_at = EXCLUDED.last_decision_at,
                        updated_at = NOW()
                    "#,
                )
                .bind(state_key)
                .bind(occurred_at)
                .execute(&mut *transaction)
                .await?;
            }
            for state_key in &unique_sent_keys {
                query(
                    r#"
                    INSERT INTO kovi_bot_proactive_state
                        (state_key, last_decision_at, last_sent_at, day_key, daily_sent_count)
                    VALUES ($1, NULL, $2, $3, 1)
                    ON CONFLICT (state_key) DO UPDATE SET
                        last_sent_at = EXCLUDED.last_sent_at,
                        day_key = EXCLUDED.day_key,
                        daily_sent_count = CASE
                            WHEN kovi_bot_proactive_state.day_key = EXCLUDED.day_key
                                THEN kovi_bot_proactive_state.daily_sent_count + 1
                            ELSE 1
                        END,
                        updated_at = NOW()
                    "#,
                )
                .bind(state_key.as_str())
                .bind(occurred_at)
                .bind(&day_key)
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;

            let mut states = self.proactive_states.lock().await;
            if let Some(state_key) = decision_key {
                states
                    .entry(state_key.to_string())
                    .or_default()
                    .last_decision_at = Some(occurred_at);
            }
            for state_key in unique_sent_keys {
                let state = states.entry(state_key.clone()).or_default();
                if state.day_key == day_key {
                    state.daily_sent_count = state.daily_sent_count.saturating_add(1);
                } else {
                    state.day_key = day_key.clone();
                    state.daily_sent_count = 1;
                }
                state.last_sent_at = Some(occurred_at);
            }
            return Ok(());
        }

        let mut data = self.snapshot().await;
        if let Some(state_key) = decision_key {
            data.proactive_states
                .entry(state_key.to_string())
                .or_default()
                .last_decision_at = Some(occurred_at);
        }
        for state_key in unique_sent_keys {
            let state = data
                .proactive_states
                .entry(state_key.to_string())
                .or_default();
            if state.day_key == day_key {
                state.daily_sent_count = state.daily_sent_count.saturating_add(1);
            } else {
                state.day_key = day_key.clone();
                state.daily_sent_count = 1;
            }
            state.last_sent_at = Some(occurred_at);
        }
        self.persist_file_snapshot_locked(&data).await?;
        *self.proactive_states.lock().await = data.proactive_states;
        Ok(())
    }

    async fn replace_data(&self, data: MemoryData) {
        let mut data = data;
        let now = Local::now();
        for summary_key in data.conversation_summaries.keys() {
            data.conversation_summary_updated_at
                .entry(summary_key.clone())
                .or_insert(now);
        }
        *self.memories.lock().await = data.memories;
        *self.user_profiles.lock().await = data.user_profiles;
        *self.group_profiles.lock().await = data.group_profiles;
        *self.conversation_summaries.lock().await = data.conversation_summaries;
        *self.conversation_summary_updated_at.lock().await = data.conversation_summary_updated_at;
        *self.proactive_states.lock().await = data.proactive_states;
        *self.bot_personality.lock().await = data.bot_personality;
    }

    async fn snapshot(&self) -> MemoryData {
        MemoryData {
            memories: self.memories.lock().await.clone(),
            user_profiles: self.user_profiles.lock().await.clone(),
            group_profiles: self.group_profiles.lock().await.clone(),
            conversation_summaries: self.conversation_summaries.lock().await.clone(),
            conversation_summary_updated_at: self
                .conversation_summary_updated_at
                .lock()
                .await
                .clone(),
            proactive_states: self.proactive_states.lock().await.clone(),
            bot_personality: self.bot_personality.lock().await.clone(),
        }
    }

    async fn create_normalized_schema(pool: &PgPool) -> Result<()> {
        query(
            r#"
            CREATE TABLE IF NOT EXISTS kovi_bot_memories (
                id TEXT PRIMARY KEY,
                subject_id BIGINT,
                scope_type TEXT CHECK (scope_type IN ('private', 'group') OR scope_type IS NULL),
                context TEXT NOT NULL,
                occurred_at TIMESTAMPTZ NOT NULL,
                importance SMALLINT NOT NULL,
                payload JSONB NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await
        .map_err(|error| anyhow::anyhow!("创建记忆明细表失败: {}", error))?;
        // 非破坏式迁移：旧部署没有 scope_type 时补列，并按受控 context 映射回填。
        query(
            "ALTER TABLE kovi_bot_memories ADD COLUMN IF NOT EXISTS scope_type TEXT CHECK (scope_type IN ('private', 'group') OR scope_type IS NULL)",
        )
        .execute(pool)
        .await?;
        query(
            r#"
            UPDATE kovi_bot_memories
            SET scope_type = CASE
                WHEN context = 'private'
                  OR context LIKE 'private\_%' ESCAPE '\'
                  OR context LIKE 'proactive\_private\_%' ESCAPE '\' THEN 'private'
                WHEN context = 'group'
                  OR context LIKE 'group\_%' ESCAPE '\'
                  OR context LIKE 'proactive\_group\_%' ESCAPE '\' THEN 'group'
                ELSE NULL
            END
            WHERE scope_type IS NULL
            "#,
        )
        .execute(pool)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS kovi_bot_memories_subject_context_time_idx ON kovi_bot_memories (subject_id, context, occurred_at DESC)",
        )
        .execute(pool)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS kovi_bot_memories_subject_scope_time_idx ON kovi_bot_memories (subject_id, scope_type, occurred_at DESC)",
        )
        .execute(pool)
        .await?;
        query(
            r#"
            CREATE TABLE IF NOT EXISTS kovi_bot_user_profiles (
                user_id BIGINT PRIMARY KEY,
                payload JSONB NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(pool)
        .await?;
        query(
            r#"
            CREATE TABLE IF NOT EXISTS kovi_bot_group_profiles (
                group_id BIGINT PRIMARY KEY,
                payload JSONB NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(pool)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS kovi_bot_user_profiles_updated_idx ON kovi_bot_user_profiles (updated_at DESC)",
        )
        .execute(pool)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS kovi_bot_group_profiles_updated_idx ON kovi_bot_group_profiles (updated_at DESC)",
        )
        .execute(pool)
        .await?;
        query(
            r#"
            CREATE TABLE IF NOT EXISTS kovi_bot_conversation_summaries (
                summary_key TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(pool)
        .await?;
        query(
            r#"
            CREATE TABLE IF NOT EXISTS kovi_bot_personality (
                id SMALLINT PRIMARY KEY CHECK (id = 1),
                payload JSONB NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(pool)
        .await?;
        query(
            r#"
            CREATE TABLE IF NOT EXISTS kovi_bot_proactive_state (
                state_key TEXT PRIMARY KEY,
                last_decision_at TIMESTAMPTZ,
                last_sent_at TIMESTAMPTZ,
                day_key TEXT NOT NULL DEFAULT '',
                daily_sent_count INTEGER NOT NULL DEFAULT 0,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn normalized_storage_has_data(pool: &PgPool) -> Result<bool> {
        query_scalar::<Postgres, bool>(
            r#"
            SELECT EXISTS(SELECT 1 FROM kovi_bot_personality WHERE id = 1)
                OR EXISTS(SELECT 1 FROM kovi_bot_memories LIMIT 1)
                OR EXISTS(SELECT 1 FROM kovi_bot_user_profiles LIMIT 1)
                OR EXISTS(SELECT 1 FROM kovi_bot_group_profiles LIMIT 1)
                OR EXISTS(SELECT 1 FROM kovi_bot_conversation_summaries LIMIT 1)
                OR EXISTS(SELECT 1 FROM kovi_bot_proactive_state LIMIT 1)
            "#,
        )
        .fetch_one(pool)
        .await
        .map_err(Into::into)
    }

    async fn load_normalized_data(pool: &PgPool) -> Result<MemoryData> {
        let mut data = MemoryData::default();
        for row in query("SELECT payload FROM kovi_bot_memories")
            .fetch_all(pool)
            .await?
        {
            let memory: MemoryEntry = serde_json::from_value(row.get("payload"))?;
            data.memories.insert(memory.id.clone(), memory);
        }
        for row in query("SELECT user_id, payload FROM kovi_bot_user_profiles")
            .fetch_all(pool)
            .await?
        {
            data.user_profiles.insert(
                row.get("user_id"),
                serde_json::from_value(row.get("payload"))?,
            );
        }
        for row in query("SELECT group_id, payload FROM kovi_bot_group_profiles")
            .fetch_all(pool)
            .await?
        {
            data.group_profiles.insert(
                row.get("group_id"),
                serde_json::from_value(row.get("payload"))?,
            );
        }
        for row in
            query("SELECT summary_key, summary, updated_at FROM kovi_bot_conversation_summaries")
                .fetch_all(pool)
                .await?
        {
            let summary_key = row.get::<String, _>("summary_key");
            data.conversation_summaries
                .insert(summary_key.clone(), row.get("summary"));
            data.conversation_summary_updated_at.insert(
                summary_key,
                row.get::<DateTime<Utc>, _>("updated_at")
                    .with_timezone(&Local),
            );
        }
        if let Some(row) = query("SELECT payload FROM kovi_bot_personality WHERE id = 1")
            .fetch_optional(pool)
            .await?
        {
            data.bot_personality = serde_json::from_value(row.get("payload"))?;
        }
        for row in query(
            "SELECT state_key, last_decision_at, last_sent_at, day_key, daily_sent_count FROM kovi_bot_proactive_state",
        )
        .fetch_all(pool)
        .await?
        {
            let last_decision_at = row
                .get::<Option<DateTime<Utc>>, _>("last_decision_at")
                .map(|value| value.with_timezone(&Local));
            let last_sent_at = row
                .get::<Option<DateTime<Utc>>, _>("last_sent_at")
                .map(|value| value.with_timezone(&Local));
            let daily_sent_count = u32::try_from(row.get::<i32, _>("daily_sent_count"))
                .unwrap_or_default();
            data.proactive_states.insert(
                row.get("state_key"),
                ProactiveState {
                    last_decision_at,
                    last_sent_at,
                    day_key: row.get("day_key"),
                    daily_sent_count,
                },
            );
        }
        Ok(data)
    }

    async fn write_normalized_snapshot(pool: &PgPool, data: &MemoryData) -> Result<()> {
        let mut transaction = pool.begin().await?;
        for memory in data.memories.values() {
            Self::upsert_memory(&mut transaction, memory).await?;
        }
        for profile in data.user_profiles.values() {
            Self::upsert_user_profile(&mut transaction, profile).await?;
        }
        for profile in data.group_profiles.values() {
            Self::upsert_group_profile(&mut transaction, profile).await?;
        }
        for (summary_key, summary) in &data.conversation_summaries {
            Self::upsert_summary(&mut transaction, summary_key, summary).await?;
        }
        for (state_key, state) in &data.proactive_states {
            Self::upsert_proactive_state(&mut transaction, state_key, state).await?;
        }
        Self::upsert_personality(&mut transaction, &data.bot_personality).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn upsert_memory(
        transaction: &mut Transaction<'_, Postgres>,
        memory: &MemoryEntry,
    ) -> Result<()> {
        query(
            r#"
            INSERT INTO kovi_bot_memories
                (id, subject_id, scope_type, context, occurred_at, importance, payload)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (id) DO UPDATE SET
                subject_id = EXCLUDED.subject_id,
                scope_type = EXCLUDED.scope_type,
                context = EXCLUDED.context,
                occurred_at = EXCLUDED.occurred_at,
                importance = EXCLUDED.importance,
                payload = EXCLUDED.payload
            "#,
        )
        .bind(&memory.id)
        .bind(memory.subject_id)
        .bind(ConversationScope::parse(&memory.context).map(ConversationScope::database_value))
        .bind(&memory.context)
        .bind(memory.timestamp)
        .bind(i16::from(memory.importance))
        .bind(serde_json::to_value(memory)?)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    async fn upsert_user_profile(
        transaction: &mut Transaction<'_, Postgres>,
        profile: &UserProfile,
    ) -> Result<()> {
        query(
            r#"
            INSERT INTO kovi_bot_user_profiles (user_id, payload, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (user_id) DO UPDATE SET payload = EXCLUDED.payload, updated_at = NOW()
            "#,
        )
        .bind(profile.user_id)
        .bind(serde_json::to_value(profile)?)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    async fn upsert_proactive_state(
        transaction: &mut Transaction<'_, Postgres>,
        state_key: &str,
        state: &ProactiveState,
    ) -> Result<()> {
        query(
            r#"
            INSERT INTO kovi_bot_proactive_state
                (state_key, last_decision_at, last_sent_at, day_key, daily_sent_count)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (state_key) DO UPDATE SET
                last_decision_at = EXCLUDED.last_decision_at,
                last_sent_at = EXCLUDED.last_sent_at,
                day_key = EXCLUDED.day_key,
                daily_sent_count = EXCLUDED.daily_sent_count,
                updated_at = NOW()
            "#,
        )
        .bind(state_key)
        .bind(state.last_decision_at)
        .bind(state.last_sent_at)
        .bind(&state.day_key)
        .bind(i32::try_from(state.daily_sent_count).unwrap_or(i32::MAX))
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    /// 跨进程串行化同一档案的读改写；仅依赖 PostgreSQL 内置 advisory lock。
    async fn lock_profile_entity(
        transaction: &mut Transaction<'_, Postgres>,
        kind: &str,
        entity_id: i64,
    ) -> Result<()> {
        query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{kind}:{entity_id}"))
            .execute(&mut **transaction)
            .await?;
        Ok(())
    }

    async fn upsert_group_profile(
        transaction: &mut Transaction<'_, Postgres>,
        profile: &GroupProfile,
    ) -> Result<()> {
        query(
            r#"
            INSERT INTO kovi_bot_group_profiles (group_id, payload, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (group_id) DO UPDATE SET payload = EXCLUDED.payload, updated_at = NOW()
            "#,
        )
        .bind(profile.group_id)
        .bind(serde_json::to_value(profile)?)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    async fn upsert_summary(
        transaction: &mut Transaction<'_, Postgres>,
        summary_key: &str,
        summary: &str,
    ) -> Result<()> {
        query(
            r#"
            INSERT INTO kovi_bot_conversation_summaries (summary_key, summary, updated_at)
            VALUES ($1, $2, NOW())
            ON CONFLICT (summary_key) DO UPDATE SET summary = EXCLUDED.summary, updated_at = NOW()
            "#,
        )
        .bind(summary_key)
        .bind(summary)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    async fn upsert_personality(
        transaction: &mut Transaction<'_, Postgres>,
        personality: &BotPersonality,
    ) -> Result<()> {
        query(
            r#"
            INSERT INTO kovi_bot_personality (id, payload, updated_at)
            VALUES (1, $1, NOW())
            ON CONFLICT (id) DO UPDATE SET payload = EXCLUDED.payload, updated_at = NOW()
            "#,
        )
        .bind(serde_json::to_value(personality)?)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }

    /// 添加新的记忆条目
    ///
    /// # 参数
    /// * `memory` - 要添加的记忆条目
    ///
    /// # 返回值
    /// 成功时返回 `Ok(())`，失败时返回错误信息
    ///
    /// # 注意
    /// 添加记忆后会自动保存到当前持久化后端
    pub async fn add_memory(&self, memory: MemoryEntry) -> Result<()> {
        let _save_guard = self.save_lock.lock().await;
        let duplicate_id = {
            let memories = self.memories.lock().await;
            let normalized_content = normalize_memory_content(&memory.content);
            memories
                .values()
                .find(|existing| {
                    existing.subject_id == memory.subject_id
                        && existing.context == memory.context
                        && normalize_memory_content(&existing.content) == normalized_content
                })
                .map(|existing| existing.id.clone())
        };

        if let Some(pool) = self.database_pool.get() {
            // 新记忆写入与旧重复项删除属于同一个事务。提交成功后才发布到内存，
            // 避免数据库失败时进程内看见一个重启后会消失的状态。
            let mut transaction = pool.begin().await?;
            Self::upsert_memory(&mut transaction, &memory).await?;
            if let Some(duplicate_id) = &duplicate_id
                && *duplicate_id != memory.id
            {
                query("DELETE FROM kovi_bot_memories WHERE id = $1")
                    .bind(duplicate_id)
                    .execute(&mut *transaction)
                    .await?;
            }
            transaction.commit().await?;
            let mut memories = self.memories.lock().await;
            if let Some(duplicate_id) = &duplicate_id {
                memories.remove(duplicate_id);
            }
            memories.insert(memory.id.clone(), memory);
        } else {
            let mut data = self.snapshot().await;
            if let Some(duplicate_id) = &duplicate_id {
                data.memories.remove(duplicate_id);
            }
            data.memories.insert(memory.id.clone(), memory);
            self.persist_file_snapshot_locked(&data).await?;
            *self.memories.lock().await = data.memories;
        }

        let needs_compaction =
            self.memories.lock().await.len() > crate::config::get().memory().max_entries();
        drop(_save_guard);
        if needs_compaction && let Err(error) = self.compact_memories().await {
            // 记忆本身已经原子提交，压缩失败不应向调用方谎报“写入失败”并诱发重试。
            eprintln!("[WARN] 记忆已保存，但后续压缩失败: {}", error);
        }
        Ok(())
    }

    /// 根据类型获取记忆条目
    ///
    /// # 参数
    /// * `memory_type` - 要查询的记忆类型
    ///
    /// # 返回值
    /// 返回指定类型的所有记忆条目
    pub async fn get_memories_by_type(&self, memory_type: &MemoryType) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        memories
            .values()
            .filter(|m| {
                std::mem::discriminant(&m.memory_type) == std::mem::discriminant(memory_type)
            })
            .cloned()
            .collect()
    }

    /// 获取最近的记忆条目
    ///
    /// # 参数
    /// * `limit` - 返回的最大记忆条目数量
    ///
    /// # 返回值
    /// 按时间倒序排列的最近记忆条目列表
    pub async fn get_recent_memories(&self, limit: usize) -> Vec<MemoryEntry> {
        let mut memories: Vec<MemoryEntry> = self.memories.lock().await.values().cloned().collect();
        memories.sort_by_key(|memory| Reverse(memory.timestamp));
        // limit=0 用于健康检查，表示返回全部记忆。
        if limit > 0 {
            memories.truncate(limit);
        }
        memories
    }

    /// 获取单个会话对象的近期记忆，避免为了一个用户或群组克隆并排序全局记忆。
    /// `context_prefix` 为空时匹配该对象的全部上下文，否则只匹配指定前缀。
    pub async fn get_recent_memories_for_subject(
        &self,
        subject_id: i64,
        context_prefix: Option<&str>,
        limit: usize,
    ) -> Vec<MemoryEntry> {
        if let Some(pool) = self.database_pool.get() {
            let database_limit = if limit == 0 {
                i64::MAX
            } else {
                i64::try_from(limit).unwrap_or(i64::MAX)
            };
            let fetch = query(
                r#"
                SELECT payload
                FROM kovi_bot_memories
                WHERE subject_id = $1
                  AND ($2::TEXT IS NULL OR STRPOS(context, $2) = 1)
                ORDER BY occurred_at DESC
                LIMIT $3
                "#,
            )
            .bind(subject_id)
            .bind(context_prefix)
            .bind(database_limit)
            .fetch_all(pool)
            .await;
            match fetch {
                Ok(rows) => {
                    let parsed = rows
                        .into_iter()
                        .map(|row| serde_json::from_value(row.get("payload")))
                        .collect::<std::result::Result<Vec<MemoryEntry>, _>>();
                    match parsed {
                        Ok(memories) => return memories,
                        Err(error) => {
                            eprintln!("[WARN] 解析范围记忆失败，回退内存缓存: {}", error);
                        }
                    }
                }
                Err(error) => {
                    eprintln!("[WARN] 查询范围记忆失败，回退内存缓存: {}", error);
                }
            }
        }

        let memories = self.memories.lock().await;
        let mut scoped = memories
            .values()
            .filter(|memory| memory.subject_id == Some(subject_id))
            .filter(|memory| context_prefix.is_none_or(|prefix| memory.context.starts_with(prefix)))
            .cloned()
            .collect::<Vec<_>>();
        scoped.sort_by_key(|memory| Reverse(memory.timestamp));
        if limit > 0 {
            scoped.truncate(limit);
        }
        scoped
    }

    /// Bounded domain-scope lookup used by the Yunxi MemoryStore adapter.
    /// `subject_id` stays an infrastructure detail; Core callers provide only
    /// a validated Person/Conversation/Global scope.
    pub(crate) async fn get_recent_memories_for_domain_scope(
        &self,
        subject_id: Option<i64>,
        context_prefix: &str,
        limit: usize,
    ) -> Vec<MemoryEntry> {
        let limit = limit.clamp(1, 128);
        if let Some(pool) = self.database_pool.get() {
            let fetch = query(
                r#"
                SELECT payload
                FROM kovi_bot_memories
                WHERE subject_id IS NOT DISTINCT FROM $1
                  AND STRPOS(context, $2) = 1
                ORDER BY occurred_at DESC, id DESC
                LIMIT $3
                "#,
            )
            .bind(subject_id)
            .bind(context_prefix)
            .bind(i64::try_from(limit).unwrap_or(128))
            .fetch_all(pool)
            .await;
            if let Ok(rows) = fetch
                && let Ok(memories) = rows
                    .into_iter()
                    .map(|row| serde_json::from_value(row.get("payload")))
                    .collect::<std::result::Result<Vec<MemoryEntry>, _>>()
            {
                return memories;
            }
        }

        let mut memories = self
            .memories
            .lock()
            .await
            .values()
            .filter(|memory| memory.subject_id == subject_id)
            .filter(|memory| memory.context.starts_with(context_prefix))
            .cloned()
            .collect::<Vec<_>>();
        memories.sort_by_key(|memory| Reverse(memory.timestamp));
        memories.truncate(limit);
        memories
    }

    pub(crate) async fn delete_memory_for_domain_scope(
        &self,
        id: &str,
        subject_id: Option<i64>,
        context_prefix: &str,
    ) -> Result<bool> {
        let _save_guard = self.save_lock.lock().await;
        let actual_id = self
            .memories
            .lock()
            .await
            .iter()
            .find_map(|(actual_id, memory)| {
                let in_scope =
                    memory.subject_id == subject_id && memory.context.starts_with(context_prefix);
                (in_scope
                    && (actual_id == id
                        || Uuid::parse_str(id)
                            .ok()
                            .is_some_and(|requested| stable_memory_uuid(actual_id) == requested)))
                .then_some(actual_id.clone())
            });
        let Some(actual_id) = actual_id else {
            return Ok(false);
        };

        if let Some(pool) = self.database_pool.get() {
            let deleted = query(
                r#"
                DELETE FROM kovi_bot_memories
                WHERE id = $1
                  AND subject_id IS NOT DISTINCT FROM $2
                  AND STRPOS(context, $3) = 1
                "#,
            )
            .bind(&actual_id)
            .bind(subject_id)
            .bind(context_prefix)
            .execute(pool)
            .await?
            .rows_affected();
            if deleted > 0 {
                self.memories.lock().await.remove(&actual_id);
                return Ok(true);
            }
            return Ok(false);
        }

        let mut data = self.snapshot().await;
        data.memories.remove(&actual_id);
        self.persist_file_snapshot_locked(&data).await?;
        *self.memories.lock().await = data.memories;
        Ok(true)
    }

    /// 判断给定上下文中是否存在指定时间之后的记忆，不克隆或排序全局集合。
    pub(crate) async fn has_memory_since_in_contexts(
        &self,
        contexts: &[&str],
        since: DateTime<Local>,
    ) -> bool {
        self.memories.lock().await.values().any(|memory| {
            memory.timestamp > since && contexts.iter().any(|context| memory.context == *context)
        })
    }

    /// 统计指定时间后的普通互动；达到 `limit` 后提前停止。
    pub(crate) async fn count_non_proactive_memories_since(
        &self,
        since: DateTime<Local>,
        limit: usize,
    ) -> usize {
        self.memories
            .lock()
            .await
            .values()
            .filter(|memory| memory.timestamp > since && !memory.context.starts_with("proactive_"))
            .take(limit)
            .count()
    }

    /// 获取重要性达到指定阈值的记忆条目
    ///
    /// # 参数
    /// * `min_importance` - 最小重要性阈值 (0-10)
    ///
    /// # 返回值
    /// 重要性大于等于阈值的记忆条目列表
    pub async fn get_important_memories(&self, min_importance: u8) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        memories
            .values()
            .filter(|m| m.importance >= min_importance)
            .cloned()
            .collect()
    }

    /// 智能搜索记忆条目
    ///
    /// 使用多因素评分算法搜索相关记忆，考虑以下因素：
    /// - 内容完全匹配 (10分)
    /// - 标签匹配 (5分)
    /// - 记忆重要性 (0-10分)
    /// - 时间权重：7天内(3分)，30天内(2分)，90天内(1分)
    ///
    /// # 参数
    /// * `query` - 搜索查询字符串
    ///
    /// # 返回值
    /// 按相关性得分排序的记忆条目列表
    pub async fn search_memories(&self, query: &str) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        let query_lower = query.trim().to_lowercase();
        if query_lower.is_empty() {
            return Vec::new();
        }
        let query_terms: Vec<&str> = query_lower
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .collect();

        let mut results: Vec<(MemoryEntry, u8)> = memories
            .values()
            .filter_map(|m| {
                let mut score = 0u8;
                let content_lower = m.content.to_lowercase();
                let content_match = content_lower.contains(&query_lower)
                    || query_terms.iter().any(|term| content_lower.contains(term));
                let tag_matches = m
                    .tags
                    .iter()
                    .filter(|tag| {
                        let tag = tag.to_lowercase();
                        tag.contains(&query_lower)
                            || query_terms.iter().any(|term| tag.contains(term))
                    })
                    .count();

                // 无内容或标签匹配的记忆不应仅凭重要性进入搜索结果。
                if !content_match && tag_matches == 0 {
                    return None;
                }

                // 完全匹配得分最高
                if content_match {
                    score += 10;
                }

                // 标签匹配
                score = score.saturating_add((tag_matches.min(3) as u8) * 5);

                // 重要性权重
                score += m.importance;

                // 时间权重（越近越重要）
                let now = Local::now();
                let days_ago = now.signed_duration_since(m.timestamp).num_days();
                if days_ago < 7 {
                    score += 3;
                } else if days_ago < 30 {
                    score += 2;
                } else if days_ago < 90 {
                    score += 1;
                }

                Some((m.clone(), score))
            })
            .collect();

        // 按得分排序
        results.sort_by_key(|result| Reverse(result.1));

        results.into_iter().map(|(memory, _)| memory).collect()
    }

    pub async fn get_contextual_memories(
        &self,
        user_id: i64,
        context: &str,
        query: &str,
        limit: usize,
    ) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        let mut contextual_memories: Vec<(MemoryEntry, u8, u8)> = Vec::new();
        let requested_scope = ConversationScope::parse(context);

        for memory in memories.values() {
            let mut relevance_score = 0u8;

            // 新版数据精确匹配所属对象；旧版数据仍可通过上下文参与检索。
            match memory.subject_id {
                Some(subject_id) if subject_id == user_id => relevance_score += 5,
                Some(_) => continue,
                // 旧版数据没有所属对象，不能安全注入其他人的上下文。
                None => continue,
            }

            // 用户号和群号都使用 i64，数值偶然相同时也不能跨私聊/群聊注入记忆。
            if requested_scope.is_some()
                && ConversationScope::parse(&memory.context) != requested_scope
            {
                continue;
            }

            // 检查上下文匹配
            if memory.context == context {
                relevance_score += 3;
            }

            // 当前消息与记忆正文/标签的词面相关度优先于单纯的重要性和新旧程度。
            let query_relevance = memory_query_relevance(memory, query);
            relevance_score = relevance_score.saturating_add(query_relevance);

            // 重要性权重
            relevance_score += memory.importance;

            if relevance_score > 0 {
                contextual_memories.push((memory.clone(), relevance_score, query_relevance));
            }
        }

        let has_query_match = contextual_memories
            .iter()
            .any(|(_, _, query_relevance)| *query_relevance > 0);
        if has_query_match {
            contextual_memories.retain(|(_, _, query_relevance)| *query_relevance > 0);
        }
        contextual_memories.sort_by_key(|result| Reverse(result.1));
        // 没有词面命中时只回退少量高价值记忆，避免每轮塞入一批无关旧消息。
        let limit = if has_query_match { limit } else { limit.min(2) };
        contextual_memories.truncate(limit);

        contextual_memories
            .into_iter()
            .map(|(memory, _, _)| memory)
            .collect()
    }

    /// 执行模型提出的受限长期记忆查询。
    ///
    /// `subject_id` 和 `context` 由当前事件决定，不接受模型输入；数据库查询始终参数化，
    /// 并设置短超时与结果上限。未初始化数据库的单元测试实例使用相同规则查询内存副本。
    pub(crate) async fn query_memories_for_model(
        &self,
        subject_id: i64,
        context: &str,
        lookup: MemoryLookup,
        max_results: usize,
        max_days: u32,
    ) -> Result<Vec<MemoryEntry>> {
        let lookup = lookup.normalized(max_results, max_days);
        let requested_scope = ConversationScope::parse(context);
        let requested_context = requested_scope
            .map(ConversationScope::database_value)
            .unwrap_or(context)
            .to_string();
        let since = lookup
            .since_days
            .map(|days| Utc::now() - ChronoDuration::days(i64::from(days)));
        let memory_types = lookup
            .memory_types
            .iter()
            .map(MemoryLookupType::database_value)
            .collect::<Vec<_>>();
        let min_importance = i16::from(lookup.min_importance.unwrap_or(0));

        if let Some(pool) = self.database_pool.get() {
            let fetch = query(
                r#"
                SELECT payload
                FROM kovi_bot_memories
                WHERE subject_id = $1
                  AND CASE
                        WHEN $2 IN ('private', 'group') THEN scope_type = $2
                        ELSE context = $2
                      END
                  AND ($3::TIMESTAMPTZ IS NULL OR occurred_at >= $3)
                  AND (CARDINALITY($4::TEXT[]) = 0 OR payload->>'memory_type' = ANY($4::TEXT[]))
                  AND importance >= $5
                  AND (
                        CARDINALITY($6::TEXT[]) = 0
                        OR EXISTS (
                            SELECT 1
                            FROM UNNEST($6::TEXT[]) AS terms(keyword)
                            WHERE STRPOS(LOWER(COALESCE(payload->>'content', '')), LOWER(keyword)) > 0
                               OR STRPOS(LOWER(COALESCE((payload->'tags')::TEXT, '')), LOWER(keyword)) > 0
                        )
                      )
                ORDER BY (
                    SELECT COUNT(*)
                    FROM UNNEST($6::TEXT[]) AS ranked_terms(keyword)
                    WHERE STRPOS(LOWER(COALESCE(payload->>'content', '')), LOWER(keyword)) > 0
                       OR STRPOS(LOWER(COALESCE((payload->'tags')::TEXT, '')), LOWER(keyword)) > 0
                ) DESC, importance DESC, occurred_at DESC
                LIMIT $7
                "#,
            )
            .bind(subject_id)
            .bind(&requested_context)
            .bind(since)
            .bind(&memory_types)
            .bind(min_importance)
            .bind(&lookup.keywords)
            .bind(lookup.limit as i64)
            .fetch_all(pool);
            let rows = kovi::tokio::time::timeout(Duration::from_secs(2), fetch)
                .await
                .map_err(|_| anyhow::anyhow!("自主记忆查询超时"))??;
            return rows
                .into_iter()
                .map(|row| serde_json::from_value(row.get("payload")).map_err(Into::into))
                .collect();
        }

        let since_local = lookup
            .since_days
            .map(|days| Local::now() - ChronoDuration::days(i64::from(days)));
        let mut results = self
            .memories
            .lock()
            .await
            .values()
            .filter(|memory| memory.subject_id == Some(subject_id))
            .filter(|memory| {
                if let Some(requested_scope) = requested_scope {
                    ConversationScope::parse(&memory.context) == Some(requested_scope)
                } else {
                    memory.context == requested_context
                }
            })
            .filter(|memory| since_local.is_none_or(|since| memory.timestamp >= since))
            .filter(|memory| memory.importance >= lookup.min_importance.unwrap_or(0))
            .filter(|memory| {
                lookup.memory_types.is_empty()
                    || lookup.memory_types.iter().any(|kind| {
                        kind.database_value() == memory_type_database_value(&memory.memory_type)
                    })
            })
            .filter_map(|memory| {
                let searchable =
                    format!("{} {}", memory.content, memory.tags.join(" ")).to_lowercase();
                let matches = lookup
                    .keywords
                    .iter()
                    .filter(|keyword| searchable.contains(&keyword.to_lowercase()))
                    .count();
                if !lookup.keywords.is_empty() && matches == 0 {
                    None
                } else {
                    Some((memory.clone(), matches))
                }
            })
            .collect::<Vec<_>>();
        results.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| right.0.importance.cmp(&left.0.importance))
                .then_with(|| right.0.timestamp.cmp(&left.0.timestamp))
        });
        results.truncate(lookup.limit);
        Ok(results.into_iter().map(|(memory, _)| memory).collect())
    }

    pub async fn update_user_profile(&self, user_id: i64, profile: UserProfile) -> Result<()> {
        self.mutate_user_profile(user_id, |_| profile).await?;
        Ok(())
    }

    /// 在持久化串行区内基于最新档案执行一次原子读改写。
    /// 调用方应优先使用该接口，避免“先 get、后 update”覆盖并发消息的更新。
    pub(crate) async fn mutate_user_profile<F>(
        &self,
        user_id: i64,
        mutate: F,
    ) -> Result<UserProfile>
    where
        F: FnOnce(Option<UserProfile>) -> UserProfile,
    {
        let _save_guard = self.save_lock.lock().await;

        let next = if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::lock_profile_entity(&mut transaction, "user", user_id).await?;
            let current =
                query("SELECT payload FROM kovi_bot_user_profiles WHERE user_id = $1 FOR UPDATE")
                    .bind(user_id)
                    .fetch_optional(&mut *transaction)
                    .await?
                    .map(|row| serde_json::from_value(row.get::<serde_json::Value, _>("payload")))
                    .transpose()?;
            let mut next = mutate(current);
            next.user_id = user_id;
            minimize_user_profile(&mut next);
            Self::upsert_user_profile(&mut transaction, &next).await?;
            transaction.commit().await?;
            self.user_profiles
                .lock()
                .await
                .insert(user_id, next.clone());
            next
        } else {
            let current = self.user_profiles.lock().await.get(&user_id).cloned();
            let mut next = mutate(current);
            next.user_id = user_id;
            minimize_user_profile(&mut next);
            let mut data = self.snapshot().await;
            data.user_profiles.insert(user_id, next.clone());
            self.persist_file_snapshot_locked(&data).await?;
            self.user_profiles
                .lock()
                .await
                .insert(user_id, next.clone());
            next
        };
        Ok(next)
    }

    pub async fn get_user_profile(&self, user_id: i64) -> Option<UserProfile> {
        let boundary =
            Local::now() - ChronoDuration::days(crate::config::get().memory().profile_ttl_days());
        let profiles = self.user_profiles.lock().await;
        profiles
            .get(&user_id)
            .filter(|profile| profile.last_interaction > boundary)
            .cloned()
    }

    pub async fn update_group_profile(&self, group_id: i64, profile: GroupProfile) -> Result<()> {
        self.mutate_group_profile(group_id, |_| profile).await?;
        Ok(())
    }

    /// 在持久化串行区内基于最新档案执行一次原子读改写。
    pub(crate) async fn mutate_group_profile<F>(
        &self,
        group_id: i64,
        mutate: F,
    ) -> Result<GroupProfile>
    where
        F: FnOnce(Option<GroupProfile>) -> GroupProfile,
    {
        let _save_guard = self.save_lock.lock().await;

        let next = if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::lock_profile_entity(&mut transaction, "group", group_id).await?;
            let current =
                query("SELECT payload FROM kovi_bot_group_profiles WHERE group_id = $1 FOR UPDATE")
                    .bind(group_id)
                    .fetch_optional(&mut *transaction)
                    .await?
                    .map(|row| serde_json::from_value(row.get::<serde_json::Value, _>("payload")))
                    .transpose()?;
            let mut next = mutate(current);
            next.group_id = group_id;
            minimize_group_profile(&mut next);
            Self::upsert_group_profile(&mut transaction, &next).await?;
            transaction.commit().await?;
            self.group_profiles
                .lock()
                .await
                .insert(group_id, next.clone());
            next
        } else {
            let current = self.group_profiles.lock().await.get(&group_id).cloned();
            let mut next = mutate(current);
            next.group_id = group_id;
            minimize_group_profile(&mut next);
            let mut data = self.snapshot().await;
            data.group_profiles.insert(group_id, next.clone());
            self.persist_file_snapshot_locked(&data).await?;
            self.group_profiles
                .lock()
                .await
                .insert(group_id, next.clone());
            next
        };
        Ok(next)
    }

    pub async fn get_group_profile(&self, group_id: i64) -> Option<GroupProfile> {
        let boundary =
            Local::now() - ChronoDuration::days(crate::config::get().memory().profile_ttl_days());
        let profiles = self.group_profiles.lock().await;
        profiles
            .get(&group_id)
            .filter(|profile| profile.last_activity > boundary)
            .cloned()
    }

    /// 获取某段私聊或群聊的滚动摘要。
    pub async fn get_conversation_summary(&self, context: &str, subject_id: i64) -> Option<String> {
        let summary_key = conversation_summary_key(context, subject_id);
        let boundary =
            Local::now() - ChronoDuration::days(crate::config::get().memory().summary_ttl_days());
        let summaries = self.conversation_summaries.lock().await;
        let updated_at = self
            .conversation_summary_updated_at
            .lock()
            .await
            .get(&summary_key)
            .copied();
        if updated_at.is_some_and(|updated_at| updated_at <= boundary) {
            return None;
        }
        summaries.get(&summary_key).cloned()
    }

    /// 保存某段私聊或群聊的滚动摘要。每段会覆盖旧摘要，因此不会随轮次无限增长。
    pub async fn update_conversation_summary(
        &self,
        context: &str,
        subject_id: i64,
        summary: String,
    ) -> Result<()> {
        let summary_key = conversation_summary_key(context, subject_id);
        let summary = truncate_chars(&summary, crate::config::get().memory().summary_max_chars());
        let _save_guard = self.save_lock.lock().await;
        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::upsert_summary(&mut transaction, &summary_key, &summary).await?;
            transaction.commit().await?;
            self.conversation_summaries
                .lock()
                .await
                .insert(summary_key.clone(), summary);
            self.conversation_summary_updated_at
                .lock()
                .await
                .insert(summary_key, Local::now());
            return Ok(());
        }

        let mut data = self.snapshot().await;
        data.conversation_summaries
            .insert(summary_key.clone(), summary.clone());
        self.persist_file_snapshot_locked(&data).await?;
        self.conversation_summaries
            .lock()
            .await
            .insert(summary_key.clone(), summary);
        self.conversation_summary_updated_at
            .lock()
            .await
            .insert(summary_key, Local::now());
        Ok(())
    }

    pub async fn get_all_user_profiles(&self) -> Vec<UserProfile> {
        let profiles = self.user_profiles.lock().await;
        profiles.values().cloned().collect()
    }

    pub async fn get_all_group_profiles(&self) -> Vec<GroupProfile> {
        let profiles = self.group_profiles.lock().await;
        profiles.values().cloned().collect()
    }

    /// Fetch a bounded, recently updated private-profile candidate set. The
    /// database index prevents proactive ticks from cloning every profile.
    pub(crate) async fn get_proactive_user_candidates(
        &self,
        active_after: DateTime<Local>,
        excluded_user_id: Option<i64>,
        limit: usize,
    ) -> Vec<UserProfile> {
        let limit = limit.min(32);
        if limit == 0 {
            return Vec::new();
        }
        if let Some(pool) = self.database_pool.get() {
            let rows = query(
                r#"
                SELECT payload
                FROM kovi_bot_user_profiles
                WHERE updated_at > $1
                  AND ($2::BIGINT IS NULL OR user_id <> $2)
                  AND COALESCE((payload->>'relationship_level')::SMALLINT, 0) > 2
                  AND NULLIF(payload->>'last_private_interaction', '')::TIMESTAMPTZ > $1
                ORDER BY updated_at DESC, user_id
                LIMIT $3
                "#,
            )
            .bind(active_after)
            .bind(excluded_user_id)
            .bind(limit as i64)
            .fetch_all(pool)
            .await;
            match rows {
                Ok(rows) => {
                    let parsed = rows
                        .into_iter()
                        .map(|row| serde_json::from_value(row.get("payload")))
                        .collect::<std::result::Result<Vec<_>, _>>();
                    if let Ok(profiles) = parsed {
                        return profiles;
                    }
                }
                Err(error) => {
                    eprintln!("[WARN] 查询主动私聊候选失败，回退内存缓存: {error}");
                }
            }
        }

        let mut profiles = self
            .user_profiles
            .lock()
            .await
            .values()
            .filter(|profile| Some(profile.user_id) != excluded_user_id)
            .filter(|profile| profile.relationship_level > 2)
            .filter(|profile| {
                profile
                    .last_private_interaction
                    .is_some_and(|last| last > active_after)
            })
            .cloned()
            .collect::<Vec<_>>();
        profiles.sort_by_key(|profile| Reverse(profile.last_private_interaction));
        profiles.truncate(limit);
        profiles
    }

    /// Fetch a bounded, recently updated group-profile candidate set.
    pub(crate) async fn get_proactive_group_candidates(
        &self,
        active_after: DateTime<Local>,
        limit: usize,
    ) -> Vec<GroupProfile> {
        let limit = limit.min(32);
        if limit == 0 {
            return Vec::new();
        }
        if let Some(pool) = self.database_pool.get() {
            let rows = query(
                r#"
                SELECT payload
                FROM kovi_bot_group_profiles
                WHERE updated_at > $1
                  AND COALESCE((payload->>'activity_level')::SMALLINT, 0) > 3
                  AND NULLIF(payload->>'last_activity', '')::TIMESTAMPTZ > $1
                ORDER BY updated_at DESC, group_id
                LIMIT $2
                "#,
            )
            .bind(active_after)
            .bind(limit as i64)
            .fetch_all(pool)
            .await;
            match rows {
                Ok(rows) => {
                    let parsed = rows
                        .into_iter()
                        .map(|row| serde_json::from_value(row.get("payload")))
                        .collect::<std::result::Result<Vec<_>, _>>();
                    if let Ok(profiles) = parsed {
                        return profiles;
                    }
                }
                Err(error) => {
                    eprintln!("[WARN] 查询主动群聊候选失败，回退内存缓存: {error}");
                }
            }
        }

        let mut profiles = self
            .group_profiles
            .lock()
            .await
            .values()
            .filter(|profile| profile.activity_level > 3 && profile.last_activity > active_after)
            .cloned()
            .collect::<Vec<_>>();
        profiles.sort_by_key(|profile| Reverse(profile.last_activity));
        profiles.truncate(limit);
        profiles
    }

    /// 删除一个私聊用户在记忆仓储中的全部可归属数据。
    /// 群消息目前只以群为 subject，无法可靠拆分其中某位成员的历史文本。
    pub(crate) async fn delete_user_data(&self, user_id: i64) -> Result<u64> {
        self.delete_subject_data(user_id, ConversationScope::Private)
            .await
    }

    /// 删除一个群组在记忆仓储中的全部可归属数据。
    pub(crate) async fn delete_group_data(&self, group_id: i64) -> Result<u64> {
        self.delete_subject_data(group_id, ConversationScope::Group)
            .await
    }

    async fn delete_subject_data(&self, subject_id: i64, scope: ConversationScope) -> Result<u64> {
        let _save_guard = self.save_lock.lock().await;
        let summary_key = format!("{}:{}", scope.database_value(), subject_id);

        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            let memories = match scope {
                ConversationScope::Private => query(
                    "DELETE FROM kovi_bot_memories WHERE subject_id = $1 AND (scope_type = 'private' OR context = 'proactive_main_admin_decision')",
                ),
                ConversationScope::Group => query(
                    "DELETE FROM kovi_bot_memories WHERE subject_id = $1 AND scope_type = 'group'",
                ),
            }
            .bind(subject_id)
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            let profile = match scope {
                ConversationScope::Private => {
                    query("DELETE FROM kovi_bot_user_profiles WHERE user_id = $1")
                }
                ConversationScope::Group => {
                    query("DELETE FROM kovi_bot_group_profiles WHERE group_id = $1")
                }
            }
            .bind(subject_id)
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            let summary =
                query("DELETE FROM kovi_bot_conversation_summaries WHERE summary_key = $1")
                    .bind(&summary_key)
                    .execute(&mut *transaction)
                    .await?
                    .rows_affected();
            transaction.commit().await?;

            self.publish_subject_deletion(subject_id, scope, &summary_key)
                .await;
            return Ok(memories + profile + summary);
        }

        let mut data = self.snapshot().await;
        let before_memories = data.memories.len();
        data.memories
            .retain(|_, memory| !memory_belongs_to_subject(memory, subject_id, scope));
        let profile_removed = match scope {
            ConversationScope::Private => data.user_profiles.remove(&subject_id).is_some(),
            ConversationScope::Group => data.group_profiles.remove(&subject_id).is_some(),
        };
        let summary_removed = data.conversation_summaries.remove(&summary_key).is_some();
        data.conversation_summary_updated_at.remove(&summary_key);
        let removed = (before_memories - data.memories.len()) as u64
            + u64::from(profile_removed)
            + u64::from(summary_removed);
        self.persist_file_snapshot_locked(&data).await?;
        *self.memories.lock().await = data.memories;
        *self.user_profiles.lock().await = data.user_profiles;
        *self.group_profiles.lock().await = data.group_profiles;
        *self.conversation_summaries.lock().await = data.conversation_summaries;
        Ok(removed)
    }

    async fn publish_subject_deletion(
        &self,
        subject_id: i64,
        scope: ConversationScope,
        summary_key: &str,
    ) {
        self.memories
            .lock()
            .await
            .retain(|_, memory| !memory_belongs_to_subject(memory, subject_id, scope));
        match scope {
            ConversationScope::Private => {
                self.user_profiles.lock().await.remove(&subject_id);
            }
            ConversationScope::Group => {
                self.group_profiles.lock().await.remove(&subject_id);
            }
        }
        self.conversation_summaries.lock().await.remove(summary_key);
        self.conversation_summary_updated_at
            .lock()
            .await
            .remove(summary_key);
    }

    pub async fn update_bot_personality(&self, personality: BotPersonality) -> Result<()> {
        self.mutate_bot_personality(|_| personality).await?;
        Ok(())
    }

    /// 基于最新人格状态串行执行读改写，并在数据库提交后才发布到内存。
    pub(crate) async fn mutate_bot_personality<F>(&self, mutate: F) -> Result<BotPersonality>
    where
        F: FnOnce(BotPersonality) -> BotPersonality,
    {
        let _save_guard = self.save_lock.lock().await;
        let current = self.bot_personality.lock().await.clone();
        let next = mutate(current);

        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::upsert_personality(&mut transaction, &next).await?;
            transaction.commit().await?;
            *self.bot_personality.lock().await = next.clone();
        } else {
            let mut data = self.snapshot().await;
            data.bot_personality = next.clone();
            self.persist_file_snapshot_locked(&data).await?;
            *self.bot_personality.lock().await = next.clone();
        }
        Ok(next)
    }

    /// 原子更新人格并记录对应情绪历史，避免其中一步成功、另一步失败。
    pub(crate) async fn mutate_bot_personality_and_record_emotion<F>(
        &self,
        mood: &str,
        context: &str,
        mutate: F,
    ) -> Result<BotPersonality>
    where
        F: FnOnce(BotPersonality) -> BotPersonality,
    {
        let save_guard = self.save_lock.lock().await;
        let current = self.bot_personality.lock().await.clone();
        let next = mutate(current);
        let memory = new_emotion_memory(mood, next.mood_intensity, context);
        let duplicate_id = self
            .memories
            .lock()
            .await
            .values()
            .find(|existing| {
                existing.subject_id == memory.subject_id
                    && existing.context == memory.context
                    && normalize_memory_content(&existing.content)
                        == normalize_memory_content(&memory.content)
            })
            .map(|existing| existing.id.clone());

        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::upsert_personality(&mut transaction, &next).await?;
            Self::upsert_memory(&mut transaction, &memory).await?;
            if let Some(duplicate_id) = &duplicate_id
                && *duplicate_id != memory.id
            {
                query("DELETE FROM kovi_bot_memories WHERE id = $1")
                    .bind(duplicate_id)
                    .execute(&mut *transaction)
                    .await?;
            }
            transaction.commit().await?;
        } else {
            let mut data = self.snapshot().await;
            data.bot_personality = next.clone();
            if let Some(duplicate_id) = &duplicate_id {
                data.memories.remove(duplicate_id);
            }
            data.memories.insert(memory.id.clone(), memory.clone());
            self.persist_file_snapshot_locked(&data).await?;
        }

        *self.bot_personality.lock().await = next.clone();
        {
            let mut memories = self.memories.lock().await;
            if let Some(duplicate_id) = &duplicate_id {
                memories.remove(duplicate_id);
            }
            memories.insert(memory.id.clone(), memory);
        }
        let needs_compaction =
            self.memories.lock().await.len() > crate::config::get().memory().max_entries();
        drop(save_guard);
        if needs_compaction && let Err(error) = self.compact_memories().await {
            eprintln!("[WARN] 情绪记录已保存，但后续记忆压缩失败: {}", error);
        }
        Ok(next)
    }

    pub async fn get_bot_personality(&self) -> BotPersonality {
        let bot_personality = self.bot_personality.lock().await;
        bot_personality.clone()
    }

    pub async fn check_storage_health(&self) -> Result<()> {
        let pool = self
            .database_pool
            .get()
            .ok_or_else(|| anyhow::anyhow!("PostgreSQL 记忆存储尚未初始化"))?;
        query("SELECT 1")
            .execute(pool)
            .await
            .map_err(|error| anyhow::anyhow!("PostgreSQL 记忆存储不可用: {}", error))?;
        Ok(())
    }

    pub async fn storage_size_bytes(&self) -> u64 {
        if let Some(pool) = self.database_pool.get()
            && let Ok(size) = query_scalar::<Postgres, i64>(
                r#"
                SELECT pg_total_relation_size('kovi_bot_memories')
                     + pg_total_relation_size('kovi_bot_user_profiles')
                     + pg_total_relation_size('kovi_bot_group_profiles')
                     + pg_total_relation_size('kovi_bot_conversation_summaries')
                     + pg_total_relation_size('kovi_bot_personality')
                "#,
            )
            .fetch_one(pool)
            .await
        {
            return u64::try_from(size).unwrap_or(0);
        }

        let memory_file = self.memory_file.clone();
        kovi::tokio::task::spawn_blocking(move || {
            fs::metadata(memory_file)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        })
        .await
        .unwrap_or(0)
    }

    /// 返回运行态对象数量，不克隆档案或对全量记忆排序。
    pub(crate) async fn runtime_counts(&self) -> (usize, usize, usize) {
        let memories = self.memories.lock().await.len();
        let user_profiles = self.user_profiles.lock().await.len();
        let group_profiles = self.group_profiles.lock().await.len();
        (memories, user_profiles, group_profiles)
    }

    /// 主动执行去重、过期清理和持久化，供后台维护任务调用。
    pub async fn compact_memories(&self) -> Result<()> {
        let _save_guard = self.save_lock.lock().await;
        let mut data = self.snapshot().await;
        let removed_ids = cleanup_old_memories(&mut data.memories);
        let (removed_user_ids, removed_group_ids, removed_summary_keys) =
            cleanup_old_profiles(&mut data);
        if removed_ids.is_empty()
            && removed_user_ids.is_empty()
            && removed_group_ids.is_empty()
            && removed_summary_keys.is_empty()
        {
            return Ok(());
        }
        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            if !removed_ids.is_empty() {
                query("DELETE FROM kovi_bot_memories WHERE id = ANY($1::TEXT[])")
                    .bind(&removed_ids)
                    .execute(&mut *transaction)
                    .await?;
            }
            if !removed_user_ids.is_empty() {
                query("DELETE FROM kovi_bot_user_profiles WHERE user_id = ANY($1::BIGINT[])")
                    .bind(&removed_user_ids)
                    .execute(&mut *transaction)
                    .await?;
            }
            if !removed_group_ids.is_empty() {
                query("DELETE FROM kovi_bot_group_profiles WHERE group_id = ANY($1::BIGINT[])")
                    .bind(&removed_group_ids)
                    .execute(&mut *transaction)
                    .await?;
            }
            if !removed_summary_keys.is_empty() {
                query(
                    "DELETE FROM kovi_bot_conversation_summaries WHERE summary_key = ANY($1::TEXT[])",
                )
                .bind(&removed_summary_keys)
                .execute(&mut *transaction)
                .await?;
            }
            transaction.commit().await?;
            *self.memories.lock().await = data.memories;
            *self.user_profiles.lock().await = data.user_profiles;
            *self.group_profiles.lock().await = data.group_profiles;
            *self.conversation_summaries.lock().await = data.conversation_summaries;
            *self.conversation_summary_updated_at.lock().await =
                data.conversation_summary_updated_at;
            return Ok(());
        }

        self.persist_file_snapshot_locked(&data).await?;
        *self.memories.lock().await = data.memories;
        *self.user_profiles.lock().await = data.user_profiles;
        *self.group_profiles.lock().await = data.group_profiles;
        *self.conversation_summaries.lock().await = data.conversation_summaries;
        *self.conversation_summary_updated_at.lock().await = data.conversation_summary_updated_at;
        Ok(())
    }

    pub async fn add_conversation_memory(
        &self,
        user_id: i64,
        content: &str,
        context: &str,
    ) -> Result<()> {
        self.add_conversation_memory_with_hints(user_id, content, context, None, &[])
            .await
    }

    pub async fn add_conversation_memory_with_hints(
        &self,
        user_id: i64,
        content: &str,
        context: &str,
        importance: Option<u8>,
        tags: &[String],
    ) -> Result<()> {
        let stored_content = minimize_memory_content(content);
        let sequence = MEMORY_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let memory = MemoryEntry {
            id: format!(
                "conv_{}_{}_{}",
                user_id,
                Local::now().timestamp_micros(),
                sequence
            ),
            content: stored_content.clone(),
            timestamp: Local::now(),
            memory_type: MemoryType::Conversation,
            importance: importance
                .unwrap_or_else(|| default_conversation_importance(&stored_content, context))
                .clamp(0, 10),
            tags: normalize_memory_tags(tags),
            context: context.to_string(),
            subject_id: Some(user_id),
        };
        self.add_memory(memory).await
    }

    pub async fn add_emotion_memory(&self, mood: &str, intensity: u8, context: &str) -> Result<()> {
        self.add_memory(new_emotion_memory(mood, intensity, context))
            .await
    }

    async fn persist_file_snapshot_locked(&self, data: &MemoryData) -> Result<()> {
        let json = serde_json::to_string_pretty(&data)?;
        let memory_file = self.memory_file.clone();
        kovi::tokio::task::spawn_blocking(move || -> Result<()> {
            let temporary_file = format!("{}.tmp", memory_file);
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary_file)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            #[cfg(unix)]
            fs::set_permissions(&temporary_file, fs::Permissions::from_mode(0o600))?;
            #[cfg(windows)]
            if std::path::Path::new(&memory_file).exists() {
                fs::remove_file(&memory_file)?;
            }
            fs::rename(&temporary_file, &memory_file)?;
            Ok(())
        })
        .await
        .map_err(|error| anyhow::anyhow!("记忆持久化任务失败: {}", error))??;
        Ok(())
    }
}

fn harden_memory_file_permissions(memory_file: &str) {
    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(memory_file)
        && metadata.permissions().mode() & 0o077 != 0
        && let Err(error) = fs::set_permissions(memory_file, fs::Permissions::from_mode(0o600))
    {
        eprintln!("[WARN] 收紧记忆文件权限失败 ({}): {}", memory_file, error);
    }
    #[cfg(not(unix))]
    let _ = memory_file;
}

fn cleanup_old_memories(memories: &mut HashMap<String, MemoryEntry>) -> Vec<String> {
    let original_count = memories.len();
    let original_ids = memories.keys().cloned().collect::<HashSet<_>>();
    let now = Local::now();
    let memory_config = crate::config::get().memory().clone();
    let retention_boundary = now - chrono::Duration::days(memory_config.retention_days());

    // 移除保留期之外的低重要性记忆。
    memories.retain(|_, memory| memory.timestamp > retention_boundary || memory.importance >= 7);

    // 对相同对象、上下文和内容的重复记忆去重，保留更新且更重要的一条。
    let mut entries: Vec<_> = memories.drain().collect();
    entries.sort_by(|left, right| {
        right
            .1
            .timestamp
            .cmp(&left.1.timestamp)
            .then_with(|| right.1.importance.cmp(&left.1.importance))
    });
    let mut seen = HashSet::new();
    entries.retain(|(_, memory)| {
        let normalized_content = normalize_memory_content(&memory.content);
        seen.insert((
            memory.subject_id,
            memory.context.clone(),
            normalized_content,
        ))
    });

    // 超出容量时综合重要性与时间保留价值最高的记忆。
    if entries.len() > memory_config.max_entries() {
        entries.sort_by(|left, right| {
            right
                .1
                .importance
                .cmp(&left.1.importance)
                .then_with(|| right.1.timestamp.cmp(&left.1.timestamp))
        });
        entries.truncate(memory_config.max_entries());
    }
    *memories = entries.into_iter().collect();

    if memories.len() < original_count {
        println!(
            "[INFO] 记忆清理完成，移除 {} 条，当前保留 {} 条",
            original_count - memories.len(),
            memories.len()
        );
    }
    original_ids
        .into_iter()
        .filter(|id| !memories.contains_key(id))
        .collect()
}

fn cleanup_old_profiles(data: &mut MemoryData) -> (Vec<i64>, Vec<i64>, Vec<String>) {
    let memory_config = crate::config::get().memory().clone();
    let profile_boundary = Local::now() - chrono::Duration::days(memory_config.profile_ttl_days());
    let summary_boundary = Local::now() - chrono::Duration::days(memory_config.summary_ttl_days());
    let mut removed_users = data
        .user_profiles
        .values()
        .filter(|profile| profile.last_interaction <= profile_boundary)
        .map(|profile| profile.user_id)
        .collect::<HashSet<_>>();
    let mut remaining_users = data
        .user_profiles
        .values()
        .filter(|profile| !removed_users.contains(&profile.user_id))
        .map(|profile| (profile.user_id, profile.last_interaction))
        .collect::<Vec<_>>();
    if remaining_users.len() > MAX_USER_PROFILES {
        remaining_users.sort_by_key(|(_, last_interaction)| *last_interaction);
        removed_users.extend(
            remaining_users
                .into_iter()
                .take(data.user_profiles.len() - removed_users.len() - MAX_USER_PROFILES)
                .map(|(user_id, _)| user_id),
        );
    }

    let mut removed_groups = data
        .group_profiles
        .values()
        .filter(|profile| profile.last_activity <= profile_boundary)
        .map(|profile| profile.group_id)
        .collect::<HashSet<_>>();
    let mut remaining_groups = data
        .group_profiles
        .values()
        .filter(|profile| !removed_groups.contains(&profile.group_id))
        .map(|profile| (profile.group_id, profile.last_activity))
        .collect::<Vec<_>>();
    if remaining_groups.len() > MAX_GROUP_PROFILES {
        remaining_groups.sort_by_key(|(_, last_activity)| *last_activity);
        removed_groups.extend(
            remaining_groups
                .into_iter()
                .take(data.group_profiles.len() - removed_groups.len() - MAX_GROUP_PROFILES)
                .map(|(group_id, _)| group_id),
        );
    }

    data.user_profiles
        .retain(|user_id, _| !removed_users.contains(user_id));
    data.group_profiles
        .retain(|group_id, _| !removed_groups.contains(group_id));
    let removed_summary_keys = removed_users
        .iter()
        .map(|user_id| format!("private:{user_id}"))
        .chain(
            removed_groups
                .iter()
                .map(|group_id| format!("group:{group_id}")),
        )
        .filter(|summary_key| data.conversation_summaries.remove(summary_key).is_some())
        .collect::<Vec<_>>();

    let mut removed_summary_keys = removed_summary_keys;
    for summary_key in data
        .conversation_summaries
        .keys()
        .cloned()
        .collect::<Vec<_>>()
    {
        let updated_at = data
            .conversation_summary_updated_at
            .get(&summary_key)
            .copied()
            .unwrap_or_else(Local::now);
        if updated_at <= summary_boundary {
            data.conversation_summaries.remove(&summary_key);
            removed_summary_keys.push(summary_key);
        }
    }
    for summary_key in &removed_summary_keys {
        data.conversation_summary_updated_at.remove(summary_key);
    }
    data.conversation_summary_updated_at
        .retain(|summary_key, _| data.conversation_summaries.contains_key(summary_key));

    if !removed_users.is_empty() || !removed_groups.is_empty() {
        println!(
            "[INFO] 档案清理完成，移除 {} 个用户档案与 {} 个群档案",
            removed_users.len(),
            removed_groups.len()
        );
    }
    (
        removed_users.into_iter().collect(),
        removed_groups.into_iter().collect(),
        removed_summary_keys,
    )
}

fn memory_belongs_to_subject(
    memory: &MemoryEntry,
    subject_id: i64,
    scope: ConversationScope,
) -> bool {
    if memory.subject_id != Some(subject_id) {
        return false;
    }
    ConversationScope::parse(&memory.context) == Some(scope)
        || (scope == ConversationScope::Private
            && memory.context == "proactive_main_admin_decision")
}

fn new_emotion_memory(mood: &str, intensity: u8, context: &str) -> MemoryEntry {
    let sequence = MEMORY_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    MemoryEntry {
        id: format!("emotion_{}_{}", Local::now().timestamp_micros(), sequence),
        content: format!("情绪变为 {}，强度 {}/10", mood, intensity.min(10)),
        timestamp: Local::now(),
        memory_type: MemoryType::Emotion,
        importance: intensity.clamp(1, 10),
        tags: vec!["情绪".to_string(), mood.to_string()],
        context: context.to_string(),
        subject_id: None,
    }
}

fn default_conversation_importance(content: &str, context: &str) -> u8 {
    let mut score = if context == "group_observation" { 1 } else { 2 };
    let character_count = content.chars().count();
    if character_count > 160 {
        score += 2;
    } else if character_count > 80 {
        score += 1;
    }
    if context.starts_with("proactive_") {
        score += 1;
    }
    score.min(10)
}

fn normalize_memory_tags(tags: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() || normalized.iter().any(|existing| existing == tag) {
            continue;
        }
        normalized.push(tag.chars().take(48).collect());
        if normalized.len() >= 8 {
            break;
        }
    }
    normalized
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn minimize_memory_content(content: &str) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let max_chars = if crate::config::get().memory().data_minimization() {
        600
    } else {
        1_600
    };
    truncate_chars(&normalized, max_chars)
}

fn minimize_user_profile(profile: &mut UserProfile) {
    let minimal = crate::config::get().memory().data_minimization();
    profile.nickname = truncate_chars(profile.nickname.trim(), if minimal { 80 } else { 160 });
    profile.personality_traits = normalize_profile_values(
        &profile.personality_traits,
        if minimal { 4 } else { 12 },
        if minimal { 48 } else { 80 },
    );
    profile.interests = normalize_profile_values(
        &profile.interests,
        if minimal { 6 } else { 16 },
        if minimal { 48 } else { 80 },
    );
    let mood_limit = if minimal { 8 } else { 16 };
    if profile.mood_history.len() > mood_limit {
        let remove_count = profile.mood_history.len() - mood_limit;
        profile.mood_history.drain(..remove_count);
    }
    for entry in &mut profile.mood_history {
        entry.mood = truncate_chars(entry.mood.trim(), 32);
        entry.trigger = truncate_chars(entry.trigger.trim(), if minimal { 80 } else { 160 });
        entry.intensity = entry.intensity.min(10);
    }
    profile.relationship_level = profile.relationship_level.min(10);
}

fn minimize_group_profile(profile: &mut GroupProfile) {
    let minimal = crate::config::get().memory().data_minimization();
    profile.group_name = truncate_chars(profile.group_name.trim(), if minimal { 80 } else { 160 });
    profile.group_personality = truncate_chars(
        profile.group_personality.trim(),
        if minimal { 120 } else { 240 },
    );
    profile.conversation_topics = normalize_profile_values(
        &profile.conversation_topics,
        if minimal { 8 } else { 20 },
        if minimal { 48 } else { 80 },
    );
    let member_limit = if minimal { 32 } else { 64 };
    let mut seen = HashSet::new();
    profile
        .active_members
        .retain(|member_id| *member_id > 0 && seen.insert(*member_id));
    profile.active_members.truncate(member_limit);
    profile.activity_level = profile.activity_level.min(10);
}

fn normalize_profile_values(values: &[String], max_items: usize, max_chars: usize) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values {
        let value = truncate_chars(value.trim(), max_chars);
        if value.is_empty() || normalized.iter().any(|existing| existing == &value) {
            continue;
        }
        normalized.push(value);
        if normalized.len() >= max_items {
            break;
        }
    }
    normalized
}

fn memory_type_database_value(memory_type: &MemoryType) -> &'static str {
    match memory_type {
        MemoryType::Conversation => "Conversation",
        MemoryType::UserProfile => "UserProfile",
        MemoryType::GroupInfo => "GroupInfo",
        MemoryType::Event => "Event",
        MemoryType::Preference => "Preference",
        MemoryType::Emotion => "Emotion",
    }
}

fn memory_query_relevance(memory: &MemoryEntry, query: &str) -> u8 {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return 0;
    }
    let searchable = format!("{} {}", memory.content, memory.tags.join(" ")).to_lowercase();
    if searchable.contains(&query) {
        return 12;
    }

    let query_chars = query
        .chars()
        .filter(|character| !character.is_whitespace() && !character.is_ascii_punctuation())
        .collect::<Vec<_>>();
    if query_chars.len() < 2 {
        return u8::from(searchable.contains(*query_chars.first().unwrap_or(&'\0')));
    }
    let matching_bigrams = query_chars
        .windows(2)
        .filter(|pair| searchable.contains(&pair.iter().collect::<String>()))
        .count();
    if matching_bigrams == 0 {
        0
    } else {
        (matching_bigrams.min(6) as u8).saturating_add(2)
    }
}

fn normalize_memory_content(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<String>()
        .to_lowercase()
}

fn stable_memory_uuid(legacy_id: &str) -> Uuid {
    Uuid::parse_str(legacy_id)
        .unwrap_or_else(|_| Uuid::new_v5(&Uuid::NAMESPACE_URL, legacy_id.as_bytes()))
}

fn conversation_summary_key(context: &str, subject_id: i64) -> String {
    format!(
        "{}:{}",
        ConversationScope::parse(context)
            .map(ConversationScope::database_value)
            .unwrap_or(context),
        subject_id
    )
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(default)]
struct MemoryData {
    memories: HashMap<String, MemoryEntry>,
    user_profiles: HashMap<i64, UserProfile>,
    group_profiles: HashMap<i64, GroupProfile>,
    conversation_summaries: HashMap<String, String>,
    #[serde(default)]
    conversation_summary_updated_at: HashMap<String, DateTime<Local>>,
    #[serde(default)]
    proactive_states: HashMap<String, ProactiveState>,
    bot_personality: BotPersonality,
}

impl Default for BotPersonality {
    fn default() -> Self {
        Self {
            current_mood: "neutral".to_string(),
            mood_intensity: 5,
            energy_level: 7,
            social_confidence: 6,
            curiosity_level: 8,
            last_mood_change: Local::now(),
            personality_traits: vec![
                "curious".to_string(),
                "playful".to_string(),
                "empathetic".to_string(),
                "slightly_tsundere".to_string(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationScope, GroupProfile, MemoryEntry, MemoryLookup, MemoryLookupType,
        MemoryManager, MemoryType, MoodEntry, ProactiveState, UserProfile,
        conversation_summary_key,
    };
    use chrono::{Duration as ChronoDuration, Local};
    use std::sync::Arc;
    use uuid::Uuid;

    fn temporary_memory_path(test_name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kovi-bot-{}-{}-{}.json",
            test_name,
            std::process::id(),
            Local::now().timestamp_micros(),
        ))
    }

    #[test]
    fn profile_updates_persist_without_deadlock() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("profile");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                let profile = UserProfile {
                    user_id: 42,
                    nickname: "tester".to_string(),
                    personality_traits: Vec::new(),
                    interests: vec!["Rust".to_string()],
                    relationship_level: 3,
                    last_interaction: Local::now(),
                    interaction_count: 2,
                    last_private_interaction: Some(Local::now()),
                    mood_history: Vec::new(),
                };

                manager
                    .update_user_profile(42, profile)
                    .await
                    .expect("档案应成功持久化");

                let reloaded = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                let saved = reloaded.get_user_profile(42).await.expect("应读回用户档案");
                assert_eq!(saved.nickname, "tester");
                assert_eq!(saved.interests, vec!["Rust"]);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(&path)
                        .expect("应读取记忆文件权限")
                        .permissions()
                        .mode()
                        & 0o777;
                    assert_eq!(mode, 0o600);
                }

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn proactive_state_survives_restart_and_rolls_daily_count() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("proactive-state");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                let occurred_at = Local::now();
                manager
                    .record_proactive_event(
                        Some("proactive:main_admin:42"),
                        &["proactive:global".to_string()],
                        occurred_at,
                    )
                    .await
                    .expect("主动决策状态应成功保存");
                manager
                    .record_proactive_event(
                        None,
                        &["proactive:global".to_string()],
                        occurred_at + ChronoDuration::minutes(1),
                    )
                    .await
                    .expect("主动发送状态应成功保存");

                let reloaded = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                let state = reloaded
                    .get_proactive_state("proactive:global")
                    .await
                    .expect("重启后应读回主动状态");
                assert_eq!(state.daily_sent_count, 2);
                assert_eq!(
                    reloaded
                        .get_proactive_state("proactive:main_admin:42")
                        .await
                        .expect("应读回决策状态")
                        .last_decision_at,
                    Some(occurred_at)
                );
                assert_eq!(
                    state.daily_count_for(
                        &(occurred_at + ChronoDuration::days(1))
                            .format("%Y-%m-%d")
                            .to_string()
                    ),
                    0
                );

                let empty = ProactiveState::default();
                assert_eq!(empty.daily_count_for("2026-08-23"), 0);
                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn atomic_profile_mutations_do_not_lose_concurrent_increments() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("profile-concurrency");
                let manager = Arc::new(MemoryManager::new(
                    path.to_str().expect("临时路径应为 UTF-8"),
                ));
                let mut tasks = Vec::new();
                for _ in 0..8 {
                    let manager = Arc::clone(&manager);
                    tasks.push(kovi::tokio::spawn(async move {
                        manager
                            .mutate_user_profile(42, |current| {
                                let mut profile = current.unwrap_or_else(|| UserProfile {
                                    user_id: 42,
                                    nickname: "tester".to_string(),
                                    personality_traits: Vec::new(),
                                    interests: Vec::new(),
                                    relationship_level: 1,
                                    last_interaction: Local::now(),
                                    interaction_count: 0,
                                    last_private_interaction: None,
                                    mood_history: Vec::new(),
                                });
                                profile.interaction_count =
                                    profile.interaction_count.saturating_add(1);
                                profile
                            })
                            .await
                    }));
                }
                for task in tasks {
                    task.await
                        .expect("并发更新任务不应崩溃")
                        .expect("并发更新应持久化");
                }
                assert_eq!(
                    manager
                        .get_user_profile(42)
                        .await
                        .expect("档案应存在")
                        .interaction_count,
                    8
                );
                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn failed_file_persistence_rolls_back_published_profile() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let parent = std::env::temp_dir().join(format!(
                    "kovi-missing-dir-{}-{}",
                    std::process::id(),
                    Local::now().timestamp_micros()
                ));
                let path = parent.join("memory.json");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                let profile = UserProfile {
                    user_id: 7,
                    nickname: "rollback".to_string(),
                    personality_traits: Vec::new(),
                    interests: Vec::new(),
                    relationship_level: 1,
                    last_interaction: Local::now(),
                    interaction_count: 1,
                    last_private_interaction: None,
                    mood_history: Vec::new(),
                };
                assert!(manager.update_user_profile(7, profile).await.is_err());
                assert!(manager.get_user_profile(7).await.is_none());
            });
    }

    #[test]
    fn contextual_memories_are_isolated_by_subject() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("isolation");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));

                manager
                    .add_conversation_memory(1, "用户一的秘密", "private_chat")
                    .await
                    .expect("应写入用户一记忆");
                manager
                    .add_conversation_memory(2, "用户二的秘密", "private_chat")
                    .await
                    .expect("应写入用户二记忆");

                let user_one = manager
                    .get_contextual_memories(1, "private_chat", "秘密", 10)
                    .await;
                assert_eq!(user_one.len(), 1);
                assert_eq!(user_one[0].subject_id, Some(1));
                assert_eq!(manager.get_recent_memories(0).await.len(), 2);

                manager
                    .add_conversation_memory(1, "同号群聊内容", "group_chat")
                    .await
                    .expect("应写入同号群聊记忆");
                let private_context = manager
                    .get_contextual_memories(1, "private_chat", "秘密", 10)
                    .await;
                assert_eq!(private_context.len(), 1);
                assert!(!private_context[0].content.contains("群聊"));

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn scoped_delete_accepts_the_stable_id_of_a_legacy_memory() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("legacy-delete");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                manager
                    .add_memory(MemoryEntry {
                        id: "legacy-conversation-1".to_string(),
                        content: "可删除的旧记忆".to_string(),
                        timestamp: Local::now(),
                        memory_type: MemoryType::Event,
                        importance: 5,
                        tags: Vec::new(),
                        context: "private_chat".to_string(),
                        subject_id: Some(7),
                    })
                    .await
                    .expect("旧记忆应写入");
                let stable_id =
                    Uuid::new_v5(&Uuid::NAMESPACE_URL, b"legacy-conversation-1").to_string();
                assert!(
                    manager
                        .delete_memory_for_domain_scope(&stable_id, Some(7), "private_chat")
                        .await
                        .expect("旧记忆应可按稳定 ID 删除")
                );
                assert!(manager.get_recent_memories(0).await.is_empty());
                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn autonomous_lookup_is_scoped_and_parameter_limited() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("autonomous-lookup");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                manager
                    .add_conversation_memory(11, "小林喜欢爵士音乐", "private_chat")
                    .await
                    .expect("应写入当前用户记忆");
                manager
                    .add_conversation_memory(12, "另一个人的音乐秘密", "private_chat")
                    .await
                    .expect("应写入其他用户记忆");
                manager
                    .add_conversation_memory(11, "同号群聊里的音乐话题", "group_chat")
                    .await
                    .expect("应写入同号群聊记忆");

                let memories = manager
                    .query_memories_for_model(
                        11,
                        "private_chat",
                        MemoryLookup {
                            keywords: vec!["音乐".to_string()],
                            since_days: None,
                            memory_types: vec![MemoryLookupType::Conversation],
                            min_importance: None,
                            limit: 999,
                        },
                        8,
                        3_650,
                    )
                    .await
                    .expect("自主查询应成功");
                assert_eq!(memories.len(), 1);
                assert_eq!(memories[0].subject_id, Some(11));
                assert_eq!(memories[0].context, "private_chat");

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn conversation_summaries_persist_and_keep_scopes_separate() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("summary");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                manager
                    .update_conversation_summary(
                        "private_chat",
                        42,
                        "用户偏好 Rust，正在准备考试。".to_string(),
                    )
                    .await
                    .expect("摘要应成功持久化");

                let reloaded = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                assert_eq!(
                    reloaded.get_conversation_summary("private_chat", 42).await,
                    Some("用户偏好 Rust，正在准备考试。".to_string())
                );
                assert_eq!(
                    reloaded.get_conversation_summary("group_chat", 42).await,
                    None
                );

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn subject_deletion_is_transactional_and_scope_isolated() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("subject-deletion");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                manager
                    .add_conversation_memory(42, "私聊数据", "private_chat")
                    .await
                    .expect("应写入私聊记忆");
                manager
                    .add_conversation_memory(42, "同号群数据", "group_chat")
                    .await
                    .expect("应写入群记忆");
                manager
                    .update_user_profile(
                        42,
                        UserProfile {
                            user_id: 42,
                            nickname: "user".to_string(),
                            personality_traits: Vec::new(),
                            interests: Vec::new(),
                            relationship_level: 1,
                            last_interaction: Local::now(),
                            interaction_count: 1,
                            last_private_interaction: Some(Local::now()),
                            mood_history: Vec::new(),
                        },
                    )
                    .await
                    .expect("应写入用户档案");
                manager
                    .update_group_profile(
                        42,
                        GroupProfile {
                            group_id: 42,
                            group_name: "group".to_string(),
                            active_members: vec![42],
                            group_personality: "friendly".to_string(),
                            conversation_topics: Vec::new(),
                            last_activity: Local::now(),
                            activity_level: 1,
                        },
                    )
                    .await
                    .expect("应写入群档案");
                manager
                    .update_conversation_summary("private_chat", 42, "私聊摘要".to_string())
                    .await
                    .expect("应写入私聊摘要");
                manager
                    .update_conversation_summary("group_chat", 42, "群摘要".to_string())
                    .await
                    .expect("应写入群摘要");

                assert!(manager.delete_user_data(42).await.expect("应删除用户数据") >= 3);
                assert!(manager.get_user_profile(42).await.is_none());
                assert!(
                    manager
                        .get_conversation_summary("private_chat", 42)
                        .await
                        .is_none()
                );
                assert!(manager.get_group_profile(42).await.is_some());
                assert_eq!(
                    manager.get_conversation_summary("group_chat", 42).await,
                    Some("群摘要".to_string())
                );
                let remaining = manager.get_recent_memories_for_subject(42, None, 0).await;
                assert_eq!(remaining.len(), 1);
                assert_eq!(remaining[0].context, "group_chat");

                assert!(manager.delete_group_data(42).await.expect("应删除群数据") >= 3);
                assert!(manager.get_group_profile(42).await.is_none());
                assert!(
                    manager
                        .get_recent_memories_for_subject(42, None, 0)
                        .await
                        .is_empty()
                );
                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn maintenance_expires_stale_profiles_and_their_summaries() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("profile-retention");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                manager
                    .update_user_profile(
                        77,
                        UserProfile {
                            user_id: 77,
                            nickname: "stale".to_string(),
                            personality_traits: Vec::new(),
                            interests: Vec::new(),
                            relationship_level: 1,
                            last_interaction: Local::now() - chrono::Duration::days(365),
                            interaction_count: 1,
                            last_private_interaction: None,
                            mood_history: Vec::new(),
                        },
                    )
                    .await
                    .expect("应写入过期档案");
                manager
                    .update_conversation_summary("private_chat", 77, "过期摘要".to_string())
                    .await
                    .expect("应写入摘要");
                assert!(manager.get_user_profile(77).await.is_none());
                manager.compact_memories().await.expect("应执行保留期清理");
                assert!(manager.get_user_profile(77).await.is_none());
                assert!(
                    manager
                        .get_conversation_summary("private_chat", 77)
                        .await
                        .is_none()
                );
                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn data_minimization_bounds_profiles_and_summary_ttl_is_enforced() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("data-minimization");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                let now = Local::now();
                manager
                    .update_user_profile(
                        88,
                        UserProfile {
                            user_id: 88,
                            nickname: "昵称".repeat(100),
                            personality_traits: (0..12)
                                .map(|index| format!("trait-{index}"))
                                .collect(),
                            interests: (0..16).map(|index| format!("interest-{index}")).collect(),
                            relationship_level: 99,
                            last_interaction: now,
                            interaction_count: 1,
                            last_private_interaction: None,
                            mood_history: (0..16)
                                .map(|index| MoodEntry {
                                    mood: format!("mood-{index}"),
                                    intensity: 99,
                                    timestamp: now,
                                    trigger: "trigger".repeat(100),
                                })
                                .collect(),
                        },
                    )
                    .await
                    .expect("应写入档案");
                let profile = manager.get_user_profile(88).await.expect("档案应存在");
                assert_eq!(profile.nickname.chars().count(), 80);
                assert_eq!(profile.personality_traits.len(), 4);
                assert_eq!(profile.interests.len(), 6);
                assert_eq!(profile.mood_history.len(), 8);
                assert!(
                    profile
                        .mood_history
                        .iter()
                        .all(|entry| entry.intensity <= 10)
                );
                assert!(
                    profile
                        .mood_history
                        .iter()
                        .all(|entry| entry.trigger.chars().count() <= 80)
                );

                let summary_key = conversation_summary_key("private_chat", 88);
                manager
                    .update_conversation_summary("private_chat", 88, "摘要".repeat(1_000))
                    .await
                    .expect("应写入摘要");
                assert_eq!(
                    manager
                        .get_conversation_summary("private_chat", 88)
                        .await
                        .expect("摘要应存在")
                        .chars()
                        .count(),
                    1_500
                );
                manager
                    .conversation_summary_updated_at
                    .lock()
                    .await
                    .insert(summary_key, now - chrono::Duration::days(31));
                assert!(
                    manager
                        .get_conversation_summary("private_chat", 88)
                        .await
                        .is_none()
                );
                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn duplicate_memories_are_compressed_and_search_requires_a_match() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("compression");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));

                manager
                    .add_conversation_memory(7, "我喜欢音乐", "private_chat")
                    .await
                    .expect("应写入记忆");
                manager
                    .add_conversation_memory(7, "我 喜欢 音乐", "private_chat")
                    .await
                    .expect("应压缩空白不同的重复记忆");

                assert_eq!(manager.get_recent_memories(0).await.len(), 1);
                assert_eq!(manager.search_memories("音乐").await.len(), 1);
                assert!(manager.search_memories("量子计算").await.is_empty());

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn legacy_profile_defaults_private_interaction() {
        let json = r#"{
            "user_id": 9,
            "nickname": "legacy",
            "personality_traits": [],
            "interests": [],
            "relationship_level": 1,
            "last_interaction": "2026-08-19T22:00:00+08:00",
            "interaction_count": 1,
            "mood_history": []
        }"#;
        let profile: UserProfile = serde_json::from_str(json).expect("旧档案应能迁移");
        assert!(profile.last_private_interaction.is_none());
    }

    #[test]
    fn context_scope_uses_explicit_prefixes_instead_of_substring_matches() {
        assert_eq!(
            ConversationScope::parse("private_chat"),
            Some(ConversationScope::Private)
        );
        assert_eq!(
            ConversationScope::parse("proactive_group_chat"),
            Some(ConversationScope::Group)
        );
        assert_eq!(
            ConversationScope::parse("not_private_but_contains_word"),
            None
        );
        assert_eq!(ConversationScope::parse("ungrouped_note"), None);
    }

    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn normalized_postgres_storage_round_trips() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let subject_id = Local::now().timestamp_micros();
                let content = format!("PostgreSQL roundtrip {subject_id}");
                let first = MemoryManager::new("/tmp/kovi-postgres-integration-source.json");
                first
                    .initialize_database()
                    .await
                    .expect("应初始化 PostgreSQL 分表");
                first
                    .add_conversation_memory(subject_id, &content, "private_chat")
                    .await
                    .expect("应写入 PostgreSQL");

                let reloaded = MemoryManager::new("/tmp/kovi-postgres-integration-reload.json");
                reloaded
                    .initialize_database()
                    .await
                    .expect("应从 PostgreSQL 重新加载");
                let memories = reloaded
                    .get_contextual_memories(subject_id, "private_chat", &content, 3)
                    .await;
                assert!(memories.iter().any(|memory| memory.content == content));

                let autonomous_results = reloaded
                    .query_memories_for_model(
                        subject_id,
                        "private_chat",
                        MemoryLookup {
                            keywords: vec!["PostgreSQL".to_string()],
                            since_days: Some(1),
                            memory_types: vec![MemoryLookupType::Conversation],
                            min_importance: Some(1),
                            limit: 3,
                        },
                        8,
                        3_650,
                    )
                    .await
                    .expect("参数化 PostgreSQL 自主查询应成功");
                assert!(
                    autonomous_results
                        .iter()
                        .any(|memory| memory.content == content)
                );
            });
    }
}
