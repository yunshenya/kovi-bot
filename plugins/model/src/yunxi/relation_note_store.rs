//! 「相处结论」的存储：她跟某个人相处下来，观察到这个人怎么对她。
//!
//! 为什么需要它：关系张力（`yunxi_relations.tension`）只是数值信号，说不出可读的
//! 结论；而立场管线（`yunxi_beliefs`）明确禁止写关于具体人的判断——"那是记忆，
//! 不是看法"。于是"跟某人相处下来的结论"一直**没有出口**。这张表就是它的出口，
//! 内容由既有的立场形成管线（`MindRuntime::form_stances`）顺带产出：同一个模型
//! 调用、同一个冷却与超时，不新增任何模型调用。
//!
//! **不参与硬门控**：回不回由代码里的关系张力阈值决定（见 `silence_gate_plan`），
//! 这里只留可读记录与将来的语气参考。不要把它接进"接不接人"的判断——模型的一句
//! 印象不稳定、不可解释，拿它当闸只会把误判变成沉默。这条约束是设计的一部分，
//! 不是"暂时没接"。
//!
//! 写入是有界的（照 `candidate_preview` / `validate_summary` 的量级）：
//! - 同一（作用域, 对象）**覆盖**，不追加——同一个人攒出十条互相打架的印象没有意义；
//! - 每个作用域条数有上限 [`MAX_RELATION_NOTES_PER_SCOPE`]，超出丢最早的；
//! - 正文与对象标识都有字数上限，超长截断。
//!
//! 表里没有外键：对象标识是模型给的显示名/QQ 原文，不对齐 `yunxi_persons`
//! （解析身份失败会让整条结论丢掉，而这条记录本来就不需要身份对齐）。代价是它
//! 还不参与"按人删除数据"，要接那条路得先设计"显示名 → PersonId"的解析。

use chrono::{DateTime, Utc};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;
use yunxi_core::{EventId, MindScope, normalized_key};

/// 一条相处结论最多多少字（含截断省略号）。
///
/// 量级对齐 `candidate_preview`（日志里 40 字）与 Mind 的文本校验：结论是**一句话**，
/// 不是段子。超长内容不代表更可信，只会把提示词里的私事整段抄进库里。
pub(crate) const MAX_RELATION_NOTE_CHARS: usize = 200;
/// 对象标识（显示名或 QQ 号）最多多少字。
pub(crate) const MAX_RELATION_TARGET_CHARS: usize = 80;
/// 每个作用域最多留几条结论；超出时丢观察时间最早的。
///
/// 32 与 `MAX_WORLD_INTERESTS` 同量级：结论按"人"去重，一个群里能有名字有脾气的
/// 人本来就有限，再多的边际价值还不如让提示词短一点。
pub(crate) const MAX_RELATION_NOTES_PER_SCOPE: usize = 32;

/// 一条待落库的相处结论。字段在 [`RelationNoteDraft::new`] 里定界，外部拿不到
/// 没校验过的值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelationNoteDraft {
    scope_kind: &'static str,
    scope_id: Option<Uuid>,
    scope_key: String,
    target_key: String,
    target_label: String,
    note: String,
    confidence_milli: i32,
    source_event_id: Option<Uuid>,
    observed_at: DateTime<Utc>,
}

impl RelationNoteDraft {
    /// 校验并定界一条相处结论；任何一项不成立就返回 `None`（坏条目直接丢，不猜）。
    ///
    /// - 作用域取**这次反思的作用域**（群会话或人），不是 Global：相处结论属于
    ///   "在哪儿跟谁相处"。`scope_key` 是主键的一部分，`global` / `person:<uuid>` /
    ///   `conversation:<uuid>`。
    /// - `target` 只存模型给的显示名/QQ 原文与归一化键；不去解析 `PersonId`。
    /// - 正文与对象标识截断到上限，置信度夹在 0..=200（与立场候选同一个刻度）。
    pub(crate) fn new(
        scope: MindScope,
        target: &str,
        note: &str,
        confidence_milli: i32,
        source_event_id: Option<EventId>,
        observed_at: DateTime<Utc>,
    ) -> Option<Self> {
        let target_label = bound_chars(target, MAX_RELATION_TARGET_CHARS);
        if target_label.is_empty() {
            return None;
        }
        let note = bound_chars(note, MAX_RELATION_NOTE_CHARS);
        if note.is_empty() {
            return None;
        }
        let (scope_kind, scope_id, scope_key) = note_scope_parts(scope);
        Some(Self {
            scope_kind,
            scope_id,
            scope_key,
            // 归一化键与立场那边共用 `yunxi_core::normalized_key`：自己写一套
            // 迟早会漂移，少一个空格就认不出是同一个人。
            target_key: normalized_key(&target_label),
            target_label,
            note,
            confidence_milli: confidence_milli.clamp(0, 200),
            source_event_id: source_event_id.map(EventId::into_uuid),
            observed_at,
        })
    }
}

/// 读访问器只给测试用：生产路径要么写库（直接读字段），要么用
/// [`RelationNoteRecord`]。放在 `#[cfg(test)]` 里而不是挂 `#[allow(dead_code)]`，
/// 是为了让"没人读它"这件事在编译期就看得见。
#[cfg(test)]
impl RelationNoteDraft {
    pub(crate) fn scope_key(&self) -> &str {
        &self.scope_key
    }
    pub(crate) fn target_label(&self) -> &str {
        &self.target_label
    }

    pub(crate) fn note(&self) -> &str {
        &self.note
    }

    pub(crate) fn confidence_milli(&self) -> i32 {
        self.confidence_milli
    }

    pub(crate) fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
}

/// 读出来的一条相处结论。
///
/// 现在只有集成测试与将来的可读出口用它（比如把结论摆进 `#相处结论` 或语气参考）；
/// **没有任何调用点拿它做判断**——这是设计约束，不是"暂时没接"。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelationNoteRecord {
    pub(crate) scope_key: String,
    pub(crate) target_label: String,
    pub(crate) note: String,
    pub(crate) confidence_milli: i32,
    pub(crate) observed_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

/// 作用域 → (kind, id, key)。key 进主键，所以它必须是稳定可比的字符串。
fn note_scope_parts(scope: MindScope) -> (&'static str, Option<Uuid>, String) {
    match scope {
        MindScope::Global => ("global", None, "global".to_string()),
        MindScope::Person { person_id } => (
            "person",
            Some(person_id.into_uuid()),
            format!("person:{person_id}"),
        ),
        MindScope::Conversation { conversation_id } => (
            "conversation",
            Some(conversation_id.into_uuid()),
            format!("conversation:{conversation_id}"),
        ),
    }
}

/// 压掉换行与多余空白，并截断到 `max_chars` 字。
///
/// 截断时补一个省略号：让"这条被截过"在库里也看得出来（与 `candidate_preview`
/// 同一个做法）。按**字符**而不是字节截断，中文才不会切碎。
fn bound_chars(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut bounded: String = compact.chars().take(max_chars.saturating_sub(1)).collect();
    bounded.push('…');
    bounded
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RelationNoteStoreError {
    /// 底层错误要带进消息里：调用方只记一行日志（fail-soft），消息里没有数据库
    /// 那边的说法就没法排查。
    #[error("相处结论存储操作失败：{0}")]
    Storage(#[source] Box<dyn StdError + Send + Sync>),
}

impl RelationNoteStoreError {
    /// 与核心的 `RelationStoreError::storage` 同一套写法：包住底层错误，保留 source 链。
    pub(crate) fn storage(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Storage(Box::new(source))
    }
}

pub(crate) type RelationNoteStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RelationNoteStoreError>> + Send + 'a>>;

/// 相处结论的落库出口。
///
/// 抽成 trait 只有一个目的：**让"落库失败"这条分支可测**。生产实现是
/// [`PostgresRelationNoteStore`]；测试塞一个必失败的替身，验"落库炸了也不能影响
/// 同一批立场候选"。除了写入没有别的语义——这些结论不参与任何决策。
///
/// 要求 `Debug` 只是因为 `MindRuntime` 派生了 `Debug`（替身也得跟着能打出来）。
pub(crate) trait RelationNoteSink: Send + Sync + std::fmt::Debug {
    fn upsert_notes<'a>(
        &'a self,
        notes: &'a [RelationNoteDraft],
    ) -> RelationNoteStoreFuture<'a, usize>;
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresRelationNoteStore {
    pool: PgPool,
}

impl PostgresRelationNoteStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_relation_notes (
                scope_kind TEXT NOT NULL
                    CHECK (scope_kind IN ('global', 'person', 'conversation')),
                scope_id UUID,
                scope_key TEXT NOT NULL
                    CHECK (octet_length(scope_key) BETWEEN 1 AND 256
                       AND btrim(scope_key) <> ''),
                target_key TEXT NOT NULL
                    CHECK (octet_length(target_key) BETWEEN 1 AND 512
                       AND btrim(target_key) <> ''),
                target_label TEXT NOT NULL
                    CHECK (octet_length(target_label) BETWEEN 1 AND 512
                       AND btrim(target_label) <> ''),
                note TEXT NOT NULL
                    CHECK (octet_length(note) BETWEEN 1 AND 1024
                       AND btrim(note) <> ''),
                confidence_milli INTEGER NOT NULL
                    CHECK (confidence_milli BETWEEN 0 AND 200),
                source_event_id UUID,
                observed_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (scope_key, target_key),
                CHECK (
                    (scope_kind = 'global' AND scope_id IS NULL)
                    OR (scope_kind <> 'global' AND scope_id IS NOT NULL)
                )
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        // 裁剪与"最近几条"都要按作用域取时间序，主键的 (scope_key, target_key)
        // 帮不上这个忙。
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_relation_notes_scope_observed_idx
             ON yunxi_relation_notes (scope_key, observed_at DESC, target_key)",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// 写入一批相处结论，返回写入（含覆盖）的行数。
    ///
    /// 三条语义都落在 SQL 里，因为它们是**并发安全**的写法：
    /// - 同一（作用域, 对象）走 `ON CONFLICT ... DO UPDATE` **覆盖**——先读后写会在
    ///   并发下攒出重复的行；
    /// - 覆盖后按作用域裁剪到 [`MAX_RELATION_NOTES_PER_SCOPE`]（丢观察时间最早的），
    ///   条数因此有硬上限；
    /// - 写入与裁剪在**同一个事务**里，读到的永远是裁剪过的状态。
    ///
    /// 不做重试、不做退避：调用方是后台反思，失败只记日志（见
    /// `MindRuntime::persist_relation_notes`）。
    pub(crate) async fn upsert_notes(
        &self,
        notes: &[RelationNoteDraft],
    ) -> Result<usize, RelationNoteStoreError> {
        if notes.is_empty() {
            return Ok(0);
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(RelationNoteStoreError::storage)?;
        let mut written = 0_usize;
        let mut scopes: Vec<&str> = Vec::new();
        for note in notes {
            let result = query(
                "INSERT INTO yunxi_relation_notes
                    (scope_kind, scope_id, scope_key, target_key, target_label, note,
                     confidence_milli, source_event_id, observed_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())
                 ON CONFLICT (scope_key, target_key) DO UPDATE SET
                    scope_kind = EXCLUDED.scope_kind,
                    scope_id = EXCLUDED.scope_id,
                    target_label = EXCLUDED.target_label,
                    note = EXCLUDED.note,
                    confidence_milli = EXCLUDED.confidence_milli,
                    source_event_id = EXCLUDED.source_event_id,
                    observed_at = EXCLUDED.observed_at,
                    updated_at = NOW()",
            )
            .bind(note.scope_kind)
            .bind(note.scope_id)
            .bind(&note.scope_key)
            .bind(&note.target_key)
            .bind(&note.target_label)
            .bind(&note.note)
            .bind(note.confidence_milli)
            .bind(note.source_event_id)
            .bind(note.observed_at)
            .execute(&mut *transaction)
            .await
            .map_err(RelationNoteStoreError::storage)?;
            written = written
                .saturating_add(usize::try_from(result.rows_affected()).unwrap_or(usize::MAX));
            if !scopes.contains(&note.scope_key.as_str()) {
                scopes.push(note.scope_key.as_str());
            }
        }
        for scope_key in scopes {
            query(
                "DELETE FROM yunxi_relation_notes
                 WHERE scope_key = $1
                   AND target_key NOT IN (
                       SELECT target_key FROM yunxi_relation_notes
                       WHERE scope_key = $1
                       ORDER BY observed_at DESC, target_key ASC
                       LIMIT $2
                   )",
            )
            .bind(scope_key)
            .bind(i64::try_from(MAX_RELATION_NOTES_PER_SCOPE).unwrap_or(i64::MAX))
            .execute(&mut *transaction)
            .await
            .map_err(RelationNoteStoreError::storage)?;
        }
        transaction
            .commit()
            .await
            .map_err(RelationNoteStoreError::storage)?;
        Ok(written)
    }

    /// 读一个作用域下的相处结论，按观察时间从新到旧。
    ///
    /// 现在只给集成测试与将来的可读出口用——**没有任何调用点会拿它做判断**，
    /// 这是设计约束（见本模块文档）。
    #[allow(dead_code)]
    /// 按"这个人的标识"删除相处结论：QQ 号原文，以及已知的显示名。
    ///
    /// 为什么需要它：`#删除我的数据` 必须真的删干净。这张表按显示名/QQ 的
    /// **文本**存（刻意不解析 PersonId），所以它不在 `delete_person_domain_data`
    /// 的外键级联范围里，必须显式删。
    ///
    /// 匹配用的是归一化键，与写入时同一个函数（`yunxi_core::normalized_key`），
    /// 因此"改名后写的新行"要用新名字才删得掉——调用方负责把所有已知别名都传
    /// 进来。做不到穷尽（模型可能写出我们没见过的称呼），这是不对齐身份的既定
    /// 代价，已在模块文档与残余风险里写明。
    pub(crate) async fn delete_targets(
        &self,
        target_labels: &[String],
    ) -> Result<u64, RelationNoteStoreError> {
        let mut keys: Vec<String> = target_labels
            .iter()
            .map(|label| normalized_key(label))
            .filter(|key| !key.is_empty())
            .collect();
        keys.sort();
        keys.dedup();
        if keys.is_empty() {
            return Ok(0);
        }
        let deleted = query("DELETE FROM yunxi_relation_notes WHERE target_key = ANY($1)")
            .bind(&keys)
            .execute(&self.pool)
            .await
            .map_err(RelationNoteStoreError::storage)?;
        Ok(deleted.rows_affected())
    }

    /// 删除这些会话作用域下的**全部**相处结论（`#删除本群数据` 用）。
    ///
    /// 与 [`Self::delete_targets`] 的分工：按人擦除要留着他人的结论，只删这个人；
    /// 按群擦除则是整个会话作用域都不要了，所以这里删全部对象。
    pub(crate) async fn delete_conversations(
        &self,
        conversation_ids: &[uuid::Uuid],
    ) -> Result<u64, RelationNoteStoreError> {
        let mut keys: Vec<String> = conversation_ids
            .iter()
            .map(|id| format!("conversation:{id}"))
            .collect();
        keys.sort();
        keys.dedup();
        if keys.is_empty() {
            return Ok(0);
        }
        let deleted = query("DELETE FROM yunxi_relation_notes WHERE scope_key = ANY($1)")
            .bind(&keys)
            .execute(&self.pool)
            .await
            .map_err(RelationNoteStoreError::storage)?;
        Ok(deleted.rows_affected())
    }

    /// 读接口：后台展示用，目前没有调用点。
    ///
    /// 留着的理由不是"以后可能有用"，而是它与门控的边界必须一眼可见：相处结论
    /// 是**给人看的可读记录**，不参与"回不回"的判定（判定只在
    /// `core_model::silence_gate_plan` 里，用关系张力）。真要接进门控，先改那段
    /// 注释与设计文档，别绕过。
    #[allow(dead_code)]
    pub(crate) async fn notes_for_scope(
        &self,
        scope_key: &str,
        limit: usize,
    ) -> Result<Vec<RelationNoteRecord>, RelationNoteStoreError> {
        let rows = query(
            "SELECT scope_key, target_label, note, confidence_milli, observed_at, updated_at
             FROM yunxi_relation_notes
             WHERE scope_key = $1
             ORDER BY observed_at DESC, target_key ASC
             LIMIT $2",
        )
        .bind(scope_key)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(RelationNoteStoreError::storage)?;
        rows.into_iter()
            .map(|row| {
                Ok(RelationNoteRecord {
                    scope_key: row
                        .try_get("scope_key")
                        .map_err(RelationNoteStoreError::storage)?,
                    target_label: row
                        .try_get("target_label")
                        .map_err(RelationNoteStoreError::storage)?,
                    note: row
                        .try_get("note")
                        .map_err(RelationNoteStoreError::storage)?,
                    confidence_milli: row
                        .try_get("confidence_milli")
                        .map_err(RelationNoteStoreError::storage)?,
                    observed_at: row
                        .try_get("observed_at")
                        .map_err(RelationNoteStoreError::storage)?,
                    updated_at: row
                        .try_get("updated_at")
                        .map_err(RelationNoteStoreError::storage)?,
                })
            })
            .collect()
    }
}

impl RelationNoteSink for PostgresRelationNoteStore {
    fn upsert_notes<'a>(
        &'a self,
        notes: &'a [RelationNoteDraft],
    ) -> RelationNoteStoreFuture<'a, usize> {
        // 显式走固有方法：trait 方法与固有方法同名，写清楚免得看的人以为在递归。
        Box::pin(PostgresRelationNoteStore::upsert_notes(self, notes))
    }
}

/// 内存版"覆盖 + 有界"语义，供**测试**在没有 PostgreSQL 时钉住去重/覆盖/条数上限。
///
/// 生产库那侧由 [`PostgresRelationNoteStore::upsert_notes`] 的
/// `ON CONFLICT ... DO UPDATE` 与裁剪 `DELETE` 做同一件事（另有一条 `#[ignore]`
/// 的真库集成测试守着它们的等价性）。
#[cfg(test)]
pub(crate) fn merge_relation_notes(
    existing: &mut Vec<RelationNoteDraft>,
    incoming: &[RelationNoteDraft],
) {
    for note in incoming {
        match existing.iter_mut().find(|current| {
            current.scope_key == note.scope_key && current.target_key == note.target_key
        }) {
            Some(current) => *current = note.clone(),
            None => existing.push(note.clone()),
        }
    }
    let mut scopes: Vec<String> = existing.iter().map(|note| note.scope_key.clone()).collect();
    scopes.sort();
    scopes.dedup();
    for scope_key in scopes {
        let mut in_scope: Vec<RelationNoteDraft> = existing
            .iter()
            .filter(|note| note.scope_key == scope_key)
            .cloned()
            .collect();
        if in_scope.len() <= MAX_RELATION_NOTES_PER_SCOPE {
            continue;
        }
        in_scope.sort_by(|left, right| {
            right
                .observed_at
                .cmp(&left.observed_at)
                .then_with(|| left.target_key.cmp(&right.target_key))
        });
        let keep: std::collections::HashSet<String> = in_scope
            .iter()
            .take(MAX_RELATION_NOTES_PER_SCOPE)
            .map(|note| note.target_key.clone())
            .collect();
        existing.retain(|note| note.scope_key != scope_key || keep.contains(&note.target_key));
    }
}

/// 内存替身：给接缝测试用，语义与生产库一致。
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct InMemoryRelationNoteSink {
    notes: std::sync::Mutex<Vec<RelationNoteDraft>>,
}

#[cfg(test)]
impl InMemoryRelationNoteSink {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn notes(&self) -> Vec<RelationNoteDraft> {
        self.notes
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
    }
}

#[cfg(test)]
impl RelationNoteSink for InMemoryRelationNoteSink {
    fn upsert_notes<'a>(
        &'a self,
        notes: &'a [RelationNoteDraft],
    ) -> RelationNoteStoreFuture<'a, usize> {
        Box::pin(async move {
            let mut stored = self.notes.lock().unwrap_or_else(|lock| lock.into_inner());
            merge_relation_notes(&mut stored, notes);
            Ok(notes.len())
        })
    }
}

/// 必失败的替身：验"落库炸了也不能影响同一批的其它候选"。
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FailingRelationNoteSink;

#[cfg(test)]
impl RelationNoteSink for FailingRelationNoteSink {
    fn upsert_notes<'a>(
        &'a self,
        _notes: &'a [RelationNoteDraft],
    ) -> RelationNoteStoreFuture<'a, usize> {
        Box::pin(async {
            Err(RelationNoteStoreError::storage(std::io::Error::other(
                "测试替身故意让落库失败",
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InMemoryRelationNoteSink, MAX_RELATION_NOTE_CHARS, MAX_RELATION_NOTES_PER_SCOPE,
        MAX_RELATION_TARGET_CHARS, PostgresRelationNoteStore, RelationNoteDraft, RelationNoteSink,
        merge_relation_notes,
    };
    use chrono::{Duration as ChronoDuration, Utc};
    use sqlx_core::query::query;
    use sqlx_postgres::PgPoolOptions;
    use yunxi_core::{EventId, MindScope, PersonId};

    fn conversation_scope() -> MindScope {
        MindScope::Conversation {
            conversation_id: yunxi_core::ConversationId::new(),
        }
    }

    fn draft(
        scope: MindScope,
        target: &str,
        note: &str,
        observed_at: chrono::DateTime<Utc>,
    ) -> RelationNoteDraft {
        RelationNoteDraft::new(scope, target, note, 100, Some(EventId::new()), observed_at)
            .expect("测试用的相处结论应当有效")
    }

    #[test]
    fn draft_bounds_text_and_clamps_confidence() {
        let now = Utc::now();
        let long_note = "她".repeat(MAX_RELATION_NOTE_CHARS * 2);
        let long_target = "张".repeat(MAX_RELATION_TARGET_CHARS * 2);
        let bounded = RelationNoteDraft::new(
            conversation_scope(),
            &long_target,
            &long_note,
            9_999,
            None,
            now,
        )
        .expect("超长但非空的结论应当被截断而不是丢弃");
        assert_eq!(bounded.note().chars().count(), MAX_RELATION_NOTE_CHARS);
        assert!(bounded.note().ends_with('…'), "截断要看得出来");
        assert_eq!(
            bounded.target_label().chars().count(),
            MAX_RELATION_TARGET_CHARS
        );
        assert_eq!(bounded.confidence_milli(), 200, "置信度要夹在上限内");
        assert!(
            bounded.scope_key().starts_with("conversation:"),
            "群会话作用域的键要带上会话 id（去重是按作用域分开的）"
        );
        assert_eq!(
            bounded.observed_at(),
            now,
            "观察时间是这次反思的时间，不是入库时间"
        );

        // 换行与多余空白压成一行：结论是单行记录，日志与库里都不该被换行撑开。
        let compact = RelationNoteDraft::new(
            conversation_scope(),
            "  张 三 ",
            "她说话\n很冲\t但讲道理",
            0,
            None,
            now,
        )
        .expect("有效结论");
        assert_eq!(compact.target_label(), "张 三");
        assert_eq!(compact.note(), "她说话 很冲 但讲道理");

        // 空对象或空正文一律丢弃，不猜。
        assert!(
            RelationNoteDraft::new(conversation_scope(), "  ", "有内容", 100, None, now).is_none()
        );
        assert!(
            RelationNoteDraft::new(conversation_scope(), "张三", " \n ", 100, None, now).is_none()
        );
    }

    #[test]
    fn scope_and_target_keys_are_stable_and_case_insensitive() {
        let now = Utc::now();
        let global = draft(MindScope::Global, "Alice", "她回消息很慢", now);
        assert_eq!(global.scope_key(), "global");

        let person_id = PersonId::new();
        let person = draft(
            MindScope::Person { person_id },
            "Alice",
            "她回消息很慢",
            now,
        );
        assert_eq!(person.scope_key(), format!("person:{person_id}"));

        // 归一化键：大小写与空白不同还是同一个人，覆盖才不会变成两条。
        let noisy = draft(MindScope::Global, "  alice ", "她回消息很慢", now);
        assert_eq!(
            noisy.target_key, global.target_key,
            "对象键必须与 core 的 normalized_key 同源"
        );
    }

    #[test]
    fn merging_overwrites_the_same_target_instead_of_appending() {
        let now = Utc::now();
        let scope = conversation_scope();
        let mut stored = vec![draft(scope, "张三", "他说话很冲", now)];
        merge_relation_notes(
            &mut stored,
            &[draft(scope, "张三", "他只是着急，其实讲道理", now)],
        );
        assert_eq!(stored.len(), 1, "同一个人只留一条，绝不追加");
        assert_eq!(
            stored[0].note(),
            "他只是着急，其实讲道理",
            "新的观察覆盖旧的"
        );

        merge_relation_notes(&mut stored, &[draft(scope, "李四", "她很安静", now)]);
        assert_eq!(stored.len(), 2, "不同的人是不同的结论");
    }

    #[test]
    fn each_scope_keeps_a_bounded_number_of_notes() {
        let start = Utc::now();
        let scope = conversation_scope();
        let other = conversation_scope();
        let mut stored = Vec::new();
        let overflow = MAX_RELATION_NOTES_PER_SCOPE + 8;
        for index in 0..overflow {
            merge_relation_notes(
                &mut stored,
                &[draft(
                    scope,
                    &format!("对象{index}"),
                    "他今天又这样",
                    start + ChronoDuration::seconds(index as i64),
                )],
            );
        }
        merge_relation_notes(
            &mut stored,
            &[draft(other, "另一个人", "另一个地方的他", start)],
        );
        let scope_key = stored.first().expect("有记录").scope_key.clone();
        assert_eq!(
            stored
                .iter()
                .filter(|note| note.scope_key == scope_key)
                .count(),
            MAX_RELATION_NOTES_PER_SCOPE,
            "单个作用域的条数必须被裁到上限"
        );
        assert!(
            stored.iter().any(|note| note.scope_key != scope_key),
            "别的作用域不受影响——裁剪是按作用域各算各的"
        );
        assert!(
            stored
                .iter()
                .any(|note| note.target_label() == format!("对象{}", overflow - 1)),
            "最新的观察要留下"
        );
        assert!(
            !stored.iter().any(|note| note.target_label() == "对象0"),
            "最早的观察在下一条同作用域结论进来时被丢掉"
        );
    }

    /// 内存替身（测试用）真的走同一套覆盖语义。
    #[test]
    fn in_memory_sink_applies_the_same_overwrite_semantics() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            let sink = InMemoryRelationNoteSink::new();
            let now = Utc::now();
            let scope = conversation_scope();
            sink.upsert_notes(&[draft(scope, "张三", "他说话很冲", now)])
                .await
                .expect("内存替身不该失败");
            sink.upsert_notes(&[draft(scope, "张三", "他其实只是着急", now)])
                .await
                .expect("内存替身不该失败");
            let notes = sink.notes();
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].note(), "他其实只是着急");
        });
    }

    /// 真库集成测试：覆盖、条数上限与读取顺序。默认忽略，需要 `DATABASE_URL`。
    #[test]
    #[ignore = "requires PostgreSQL via DATABASE_URL"]
    fn postgres_relation_notes_overwrite_and_stay_bounded() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let database_url = std::env::var("DATABASE_URL").expect("requires DATABASE_URL");
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .connect(&database_url)
                    .await
                    .expect("should connect to PostgreSQL");
                let store = PostgresRelationNoteStore::new(pool.clone());
                store
                    .initialize_schema()
                    .await
                    .expect("should initialize relation note schema");

                let scope = conversation_scope();
                let scope_key = draft(scope, "占位", "占位", Utc::now())
                    .scope_key()
                    .to_string();
                query("DELETE FROM yunxi_relation_notes WHERE scope_key = $1")
                    .bind(&scope_key)
                    .execute(&pool)
                    .await
                    .expect("should clean up isolated scope");

                let start = Utc::now();
                assert_eq!(
                    store
                        .upsert_notes(&[draft(scope, "张三", "他说话很冲", start)])
                        .await
                        .expect("首次写入"),
                    1
                );
                store
                    .upsert_notes(&[draft(scope, "张三", "他其实只是着急", start)])
                    .await
                    .expect("覆盖写入");
                let notes = store
                    .notes_for_scope(&scope_key, 64)
                    .await
                    .expect("应当读回相处结论");
                assert_eq!(notes.len(), 1, "同一个人只留一条");
                assert_eq!(notes[0].note, "他其实只是着急");

                let overflow = MAX_RELATION_NOTES_PER_SCOPE + 5;
                for index in 0..overflow {
                    store
                        .upsert_notes(&[draft(
                            scope,
                            &format!("对象{index}"),
                            "他今天又这样",
                            start + ChronoDuration::seconds(index as i64),
                        )])
                        .await
                        .expect("批量写入");
                }
                let notes = store
                    .notes_for_scope(&scope_key, 512)
                    .await
                    .expect("应当读回相处结论");
                assert_eq!(
                    notes.len(),
                    MAX_RELATION_NOTES_PER_SCOPE,
                    "每个作用域的条数必须被裁到上限"
                );
                assert_eq!(
                    notes[0].target_label,
                    format!("对象{}", overflow - 1),
                    "按观察时间从新到旧"
                );

                query("DELETE FROM yunxi_relation_notes WHERE scope_key = $1")
                    .bind(&scope_key)
                    .execute(&pool)
                    .await
                    .expect("should clean up isolated scope");
            });
    }
}
