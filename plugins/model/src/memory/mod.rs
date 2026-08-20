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
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

static MEMORY_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    /// 机器人人格状态
    bot_personality: Arc<Mutex<BotPersonality>>,
    /// 旧版记忆文件路径，仅用于迁移和无数据库的单元测试实例
    memory_file: String,
    /// 串行化持久化操作，避免多个任务同时覆盖记忆快照。
    save_lock: Arc<Mutex<()>>,
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

        Self {
            memories: Arc::new(Mutex::new(data.memories)),
            user_profiles: Arc::new(Mutex::new(data.user_profiles)),
            group_profiles: Arc::new(Mutex::new(data.group_profiles)),
            conversation_summaries: Arc::new(Mutex::new(data.conversation_summaries)),
            bot_personality: Arc::new(Mutex::new(data.bot_personality)),
            memory_file: memory_file.to_string(),
            save_lock: Arc::new(Mutex::new(())),
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

    async fn replace_data(&self, data: MemoryData) {
        *self.memories.lock().await = data.memories;
        *self.user_profiles.lock().await = data.user_profiles;
        *self.group_profiles.lock().await = data.group_profiles;
        *self.conversation_summaries.lock().await = data.conversation_summaries;
        *self.bot_personality.lock().await = data.bot_personality;
    }

    async fn snapshot(&self) -> MemoryData {
        MemoryData {
            memories: self.memories.lock().await.clone(),
            user_profiles: self.user_profiles.lock().await.clone(),
            group_profiles: self.group_profiles.lock().await.clone(),
            conversation_summaries: self.conversation_summaries.lock().await.clone(),
            bot_personality: self.bot_personality.lock().await.clone(),
        }
    }

    async fn create_normalized_schema(pool: &PgPool) -> Result<()> {
        query(
            r#"
            CREATE TABLE IF NOT EXISTS kovi_bot_memories (
                id TEXT PRIMARY KEY,
                subject_id BIGINT,
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
        query(
            "CREATE INDEX IF NOT EXISTS kovi_bot_memories_subject_context_time_idx ON kovi_bot_memories (subject_id, context, occurred_at DESC)",
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
        for row in query("SELECT summary_key, summary FROM kovi_bot_conversation_summaries")
            .fetch_all(pool)
            .await?
        {
            data.conversation_summaries
                .insert(row.get("summary_key"), row.get("summary"));
        }
        if let Some(row) = query("SELECT payload FROM kovi_bot_personality WHERE id = 1")
            .fetch_optional(pool)
            .await?
        {
            data.bot_personality = serde_json::from_value(row.get("payload"))?;
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
                (id, subject_id, context, occurred_at, importance, payload)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (id) DO UPDATE SET
                subject_id = EXCLUDED.subject_id,
                context = EXCLUDED.context,
                occurred_at = EXCLUDED.occurred_at,
                importance = EXCLUDED.importance,
                payload = EXCLUDED.payload
            "#,
        )
        .bind(&memory.id)
        .bind(memory.subject_id)
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

    async fn persist_memory(&self, memory: &MemoryEntry) -> Result<()> {
        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::upsert_memory(&mut transaction, memory).await?;
            transaction.commit().await?;
            return Ok(());
        }
        self.save_file_snapshot().await
    }

    async fn persist_user_profile(&self, profile: &UserProfile) -> Result<()> {
        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::upsert_user_profile(&mut transaction, profile).await?;
            transaction.commit().await?;
            return Ok(());
        }
        self.save_file_snapshot().await
    }

    async fn persist_group_profile(&self, profile: &GroupProfile) -> Result<()> {
        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::upsert_group_profile(&mut transaction, profile).await?;
            transaction.commit().await?;
            return Ok(());
        }
        self.save_file_snapshot().await
    }

    async fn persist_summary(&self, summary_key: &str, summary: &str) -> Result<()> {
        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::upsert_summary(&mut transaction, summary_key, summary).await?;
            transaction.commit().await?;
            return Ok(());
        }
        self.save_file_snapshot().await
    }

    async fn persist_personality(&self, personality: &BotPersonality) -> Result<()> {
        if let Some(pool) = self.database_pool.get() {
            let mut transaction = pool.begin().await?;
            Self::upsert_personality(&mut transaction, personality).await?;
            transaction.commit().await?;
            return Ok(());
        }
        self.save_file_snapshot().await
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
        let persisted_memory = memory.clone();
        let duplicate_id = {
            let mut memories = self.memories.lock().await;
            let normalized_content = normalize_memory_content(&memory.content);
            let duplicate_id = memories
                .values()
                .find(|existing| {
                    existing.subject_id == memory.subject_id
                        && existing.context == memory.context
                        && normalize_memory_content(&existing.content) == normalized_content
                })
                .map(|existing| existing.id.clone());
            if let Some(duplicate_id) = &duplicate_id {
                memories.remove(duplicate_id);
            }
            memories.insert(memory.id.clone(), memory);
            duplicate_id
        };
        self.persist_memory(&persisted_memory).await?;
        if let Some(duplicate_id) = duplicate_id
            && let Some(pool) = self.database_pool.get()
        {
            query("DELETE FROM kovi_bot_memories WHERE id = $1")
                .bind(duplicate_id)
                .execute(pool)
                .await?;
        }
        if self.memories.lock().await.len() > crate::config::get().memory().max_entries() {
            self.compact_memories().await?;
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
        let requested_scope = context_scope(context);

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
            if requested_scope.is_some() && context_scope(&memory.context) != requested_scope {
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
        let requested_scope = context_scope(context).unwrap_or(context).to_string();
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
                        WHEN $2 = 'private' THEN POSITION('private' IN context) > 0
                        WHEN $2 = 'group' THEN POSITION('group' IN context) > 0
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
            .bind(&requested_scope)
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
                context_scope(&memory.context).unwrap_or(&memory.context) == requested_scope
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
        let persisted_profile = profile.clone();
        {
            let mut profiles = self.user_profiles.lock().await;
            profiles.insert(user_id, profile);
        }
        self.persist_user_profile(&persisted_profile).await
    }

    pub async fn get_user_profile(&self, user_id: i64) -> Option<UserProfile> {
        let profiles = self.user_profiles.lock().await;
        profiles.get(&user_id).cloned()
    }

    pub async fn update_group_profile(&self, group_id: i64, profile: GroupProfile) -> Result<()> {
        let persisted_profile = profile.clone();
        {
            let mut profiles = self.group_profiles.lock().await;
            profiles.insert(group_id, profile);
        }
        self.persist_group_profile(&persisted_profile).await
    }

    pub async fn get_group_profile(&self, group_id: i64) -> Option<GroupProfile> {
        let profiles = self.group_profiles.lock().await;
        profiles.get(&group_id).cloned()
    }

    /// 获取某段私聊或群聊的滚动摘要。
    pub async fn get_conversation_summary(&self, context: &str, subject_id: i64) -> Option<String> {
        self.conversation_summaries
            .lock()
            .await
            .get(&conversation_summary_key(context, subject_id))
            .cloned()
    }

    /// 保存某段私聊或群聊的滚动摘要。每段会覆盖旧摘要，因此不会随轮次无限增长。
    pub async fn update_conversation_summary(
        &self,
        context: &str,
        subject_id: i64,
        summary: String,
    ) -> Result<()> {
        let summary_key = conversation_summary_key(context, subject_id);
        {
            let mut summaries = self.conversation_summaries.lock().await;
            summaries.insert(summary_key.clone(), summary.clone());
        }
        self.persist_summary(&summary_key, &summary).await
    }

    pub async fn get_all_user_profiles(&self) -> Vec<UserProfile> {
        let profiles = self.user_profiles.lock().await;
        profiles.values().cloned().collect()
    }

    pub async fn get_all_group_profiles(&self) -> Vec<GroupProfile> {
        let profiles = self.group_profiles.lock().await;
        profiles.values().cloned().collect()
    }

    pub async fn update_bot_personality(&self, personality: BotPersonality) -> Result<()> {
        let persisted_personality = personality.clone();
        {
            let mut bot_personality = self.bot_personality.lock().await;
            *bot_personality = personality;
        }
        self.persist_personality(&persisted_personality).await
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
        serde_json::to_vec(&self.snapshot().await)
            .map(|data| data.len() as u64)
            .unwrap_or(0)
    }

    /// 主动执行去重、过期清理和持久化，供后台维护任务调用。
    pub async fn compact_memories(&self) -> Result<()> {
        let _save_guard = self.save_lock.lock().await;
        let removed_ids = self.cleanup_old_memories().await?;
        if let Some(pool) = self.database_pool.get() {
            if !removed_ids.is_empty() {
                query("DELETE FROM kovi_bot_memories WHERE id = ANY($1::TEXT[])")
                    .bind(&removed_ids)
                    .execute(pool)
                    .await?;
            }
            return Ok(());
        }
        self.save_file_snapshot_locked().await
    }

    pub async fn add_conversation_memory(
        &self,
        user_id: i64,
        content: &str,
        context: &str,
    ) -> Result<()> {
        let sequence = MEMORY_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let memory = MemoryEntry {
            id: format!(
                "conv_{}_{}_{}",
                user_id,
                Local::now().timestamp_micros(),
                sequence
            ),
            content: content.to_string(),
            timestamp: Local::now(),
            memory_type: MemoryType::Conversation,
            importance: self.calculate_importance(content),
            tags: self.extract_tags(content),
            context: context.to_string(),
            subject_id: Some(user_id),
        };
        self.add_memory(memory).await
    }

    pub async fn add_emotion_memory(&self, mood: &str, intensity: u8, context: &str) -> Result<()> {
        let sequence = MEMORY_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.add_memory(MemoryEntry {
            id: format!("emotion_{}_{}", Local::now().timestamp_micros(), sequence),
            content: format!("情绪变为 {}，强度 {}/10", mood, intensity.min(10)),
            timestamp: Local::now(),
            memory_type: MemoryType::Emotion,
            importance: intensity.clamp(1, 10),
            tags: vec!["情绪".to_string(), mood.to_string()],
            context: context.to_string(),
            subject_id: None,
        })
        .await
    }

    /// 计算记忆内容的重要性评分
    ///
    /// 使用多维度分析算法评估记忆的重要性，考虑以下因素：
    ///
    /// ## 关键词权重
    /// - **高重要性关键词** (+4分)：喜欢、讨厌、重要、秘密、梦想、目标、家人、朋友、爱、恨、害怕、担心
    /// - **中等重要性关键词** (+2分)：工作、学习、游戏、电影、音乐、食物、旅行、运动、健康
    /// - **低重要性关键词** (-1分)：天气、今天、昨天、明天、现在、刚才
    ///
    /// ## 内容特征
    /// - **长度权重**：>150字符(+2分)，>100字符(+1分)
    /// - **情感表达** (+2分)：开心、难过、生气、兴奋、害怕、担心、惊讶、失望
    /// - **个人信息** (+1分)：我、我的、自己、个人、私人的
    ///
    /// # 参数
    /// * `content` - 要分析的内容文本
    ///
    /// # 返回值
    /// 重要性评分 (0-10)，10表示最重要
    fn calculate_importance(&self, content: &str) -> u8 {
        let mut importance: u8 = 3; // 基础重要性

        // 检查关键词
        let high_importance_keywords = [
            "喜欢", "讨厌", "重要", "秘密", "梦想", "目标", "家人", "朋友", "爱", "恨", "害怕",
            "担心",
        ];
        let medium_importance_keywords = [
            "工作", "学习", "游戏", "电影", "音乐", "食物", "旅行", "运动", "健康",
        ];
        let low_importance_keywords = ["天气", "今天", "昨天", "明天", "现在", "刚才"];

        for keyword in &high_importance_keywords {
            if content.contains(keyword) {
                importance += 4;
            }
        }

        for keyword in &medium_importance_keywords {
            if content.contains(keyword) {
                importance += 2;
            }
        }

        for keyword in &low_importance_keywords {
            if content.contains(keyword) {
                importance = importance.saturating_sub(1);
            }
        }

        // 根据长度调整
        let character_count = content.chars().count();
        if character_count > 150 {
            importance += 2;
        } else if character_count > 100 {
            importance += 1;
        }

        // 检查是否包含情感表达
        let emotional_keywords = [
            "开心", "难过", "生气", "兴奋", "害怕", "担心", "惊讶", "失望",
        ];
        for keyword in &emotional_keywords {
            if content.contains(keyword) {
                importance += 2;
            }
        }

        // 检查是否包含个人信息
        let personal_keywords = ["我", "我的", "自己", "个人", "私人的"];
        for keyword in &personal_keywords {
            if content.contains(keyword) {
                importance += 1;
            }
        }

        importance.min(10)
    }

    fn extract_tags(&self, content: &str) -> Vec<String> {
        let mut tags = Vec::new();

        // 简单的关键词提取
        let common_tags = [
            "游戏", "学习", "工作", "生活", "情感", "技术", "科技", "娱乐", "美食", "旅行", "运动",
            "健康", "音乐", "电影", "读书", "家人", "朋友",
        ];
        for tag in &common_tags {
            if content.contains(tag) {
                tags.push(tag.to_string());
            }
        }

        tags
    }

    async fn save_file_snapshot(&self) -> Result<()> {
        let _save_guard = self.save_lock.lock().await;
        self.save_file_snapshot_locked().await
    }

    async fn save_file_snapshot_locked(&self) -> Result<()> {
        let data = self.snapshot().await;
        let json = serde_json::to_string_pretty(&data)?;
        let memory_file = self.memory_file.clone();
        kovi::tokio::task::spawn_blocking(move || -> Result<()> {
            let temporary_file = format!("{}.tmp", memory_file);
            fs::write(&temporary_file, json)?;
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

    /// 清理旧记忆，避免内存过度使用
    ///
    /// 执行以下清理策略：
    /// 1. 移除配置保留期之外的低重要性记忆（重要性 < 7）
    /// 2. 压缩同一对象、上下文和内容的重复记忆
    /// 3. 如果超过配置容量，只保留最重要且较新的记忆
    ///
    /// # 清理规则
    /// - 保留所有高重要性记忆（重要性 >= 7）
    /// - 保留期和最大数量均由 `bot.conf.toml` 的 `[memory]` 控制
    ///
    /// # 返回值
    /// 成功时返回 `Ok(())`，失败时返回错误信息
    async fn cleanup_old_memories(&self) -> Result<Vec<String>> {
        let mut memories = self.memories.lock().await;
        let original_count = memories.len();
        let original_ids = memories.keys().cloned().collect::<HashSet<_>>();
        let now = Local::now();
        let memory_config = crate::config::get().memory().clone();
        let retention_boundary = now - chrono::Duration::days(memory_config.retention_days());

        // 移除保留期之外的低重要性记忆。
        memories
            .retain(|_, memory| memory.timestamp > retention_boundary || memory.importance >= 7);

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
        Ok(original_ids
            .into_iter()
            .filter(|id| !memories.contains_key(id))
            .collect())
    }
}

fn context_scope(context: &str) -> Option<&'static str> {
    if context.contains("private") {
        Some("private")
    } else if context.contains("group") {
        Some("group")
    } else {
        None
    }
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

fn conversation_summary_key(context: &str, subject_id: i64) -> String {
    format!(
        "{}:{}",
        context_scope(context).unwrap_or(context),
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
    use super::{MemoryLookup, MemoryLookupType, MemoryManager, UserProfile};
    use chrono::Local;

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

                std::fs::remove_file(path).expect("应清理测试记忆文件");
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
    fn normalized_postgres_storage_round_trips_when_enabled() {
        if std::env::var("KOVI_RUN_POSTGRES_TEST").as_deref() != Ok("1") {
            return;
        }
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
