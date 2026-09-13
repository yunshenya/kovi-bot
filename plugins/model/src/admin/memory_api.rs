//! 记忆浏览接口。
//!
//! 展示方式参考 Hindsight：**一个可搜索的列表 + 一个说清出处的详情**。芸汐的
//! 长期状态分散在若干张表里（长期记忆、Mind 记录、人物关系、目标、未完结线索、
//! 梗账本、Host 档案），这里把它们统一成同一种「卡片」形态：
//!
//! ```text
//! { kind, id, title, body, scope_kind, scope_id, scope_label, status, weight, occurred_at }
//! ```
//!
//! 之所以直接写 SQL 而不是复用领域层：领域层的读取接口是按"这次回复需要记住
//! 什么"设计的（作用域绑定、上限 32 条、无分页、无跨作用域检索），拿来做"把
//! 所有记忆摊开给人看"既不够也不合适。后台是只读视图，直接 SELECT 不会破坏
//! 任何领域不变量。

use super::ApiError;
use axum::Json;
use axum::extract::{Path as UrlPath, Query};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_postgres::{PgPool, PgRow};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uuid::Uuid;

/// 单次列表请求最多返回多少条。
const MAX_LIMIT: i64 = 200;
/// 默认每页条数。
const DEFAULT_LIMIT: i64 = 40;
/// 合并多种类型时，每种类型最多先取多少条参与归并排序。
const PER_KIND_FETCH: i64 = 200;
/// 旧版长期记忆在 `KINDS` 里的键。它和其它类型的作用域语义不同（QQ 作用域而非
/// person 作用域），凡是按人物筛记录的地方都要单独认它。
const LEGACY_MEMORY_KEY: &str = "legacy_memory";
/// 离线 Memory v2 backfill 的账本表（`legacy_id` → `target_id`）。它不是可浏览的
/// 记录类型，只在判定"这条旧记忆有没有 v2 副本"时用到。
const MIGRATION_LEDGER_TABLE: &str = "yunxi_memory_migration_items";

/// 一种可浏览的记录类型。
struct RecordKind {
    key: &'static str,
    label: &'static str,
    /// 表名（受控常量，不来自请求）。
    table: &'static str,
    /// 主键表达式（带 `t` 别名）。
    id_expr: &'static str,
    /// 标题表达式。
    title_expr: &'static str,
    /// 正文表达式。
    body_expr: &'static str,
    /// 搜索表达式；`$1` 为空串时不做过滤。
    search_expr: &'static str,
    /// 时间列表达式。
    time_expr: &'static str,
    /// 作用域类型表达式（空串表示没有作用域）。
    scope_kind_expr: &'static str,
    /// 作用域标识表达式。
    scope_id_expr: &'static str,
    /// 状态表达式。
    status_expr: &'static str,
    /// 权重/重要度表达式，`NULL` 表示没有。
    weight_expr: &'static str,
    /// 「标签」列：jsonb 数组。记忆用它自己的自由标签，其余类型至少带上自身类型。
    tags_expr: &'static str,
    /// 「实体」列：payload 里天然是名字的那个字段（偏好=对象、兴趣=话题），没有就是 NULL。
    entity_expr: &'static str,
    /// 「提及时间」列：记录进入记忆的时间（记忆是 created_at，其余是 updated_at）。
    mentioned_expr: &'static str,
    /// 记录内部引用到的其它记录 id（payload 里的 UUID），用于连「因果」边。
    refs_expr: &'static str,
    /// 概览页是否统计这张表。
    counted: bool,
}

/// 从 payload 里抠出被引用的记录 id（UUID）。Mind 的议程、未解问题等会直接指向
/// 另一条记录的 id，这是数据里真实存在的"因果/引用"关系，不是猜出来的。
const MIND_REFS: &str = "COALESCE((SELECT array_agg(DISTINCT m[1]) FROM \
     regexp_matches(t.payload::text, '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', 'g') AS m), ARRAY[]::text[])";

/// Mind 记录的正文藏在 `payload` 里，各类型字段名不同，按可能性依次取。
const MIND_TITLE: &str = "COALESCE(t.payload->>'summary', t.payload->>'proposition', \
     t.payload->>'statement', t.payload->>'subject', t.payload->>'topic', \
     t.payload->>'question', t.payload->>'text', t.payload->>'content', t.dedupe_key)";
const MIND_SEARCH: &str = "COALESCE(t.payload::text, '') || ' ' || t.dedupe_key || ' ' || t.status";

/// Mind v2 的七张记录表结构完全一致，只有语义不同。
macro_rules! mind_kind {
    ($key:literal, $label:literal, $table:literal, $tag:literal, $entity:expr) => {
        RecordKind {
            key: $key,
            label: $label,
            table: $table,
            id_expr: "t.id",
            title_expr: MIND_TITLE,
            body_expr: MIND_TITLE,
            search_expr: MIND_SEARCH,
            time_expr: "t.occurred_at",
            scope_kind_expr: "t.scope_kind",
            scope_id_expr: "t.scope_id::text",
            status_expr: "t.status",
            weight_expr: "t.primary_score",
            // Mind 记录本身没有自由标签，用它的类型当标签，筛选才有东西可筛。
            tags_expr: concat!("jsonb_build_array('", $tag, "')"),
            entity_expr: $entity,
            mentioned_expr: "t.updated_at",
            refs_expr: MIND_REFS,
            counted: true,
        }
    };
}

const KINDS: &[RecordKind] = &[
    RecordKind {
        key: "memory",
        label: "长期记忆",
        table: "yunxi_memories",
        id_expr: "t.id",
        title_expr: "t.content",
        body_expr: "t.content",
        search_expr: "t.content || ' ' || t.tags::text",
        time_expr: "t.occurred_at",
        scope_kind_expr: "t.scope_kind",
        scope_id_expr: "t.scope_id::text",
        status_expr: "''",
        weight_expr: "t.importance",
        // 自由标签之外补上记忆类型（fact/event/...），否则这一列在当前数据里几乎是空的。
        tags_expr: "t.tags || jsonb_build_array(t.kind)",
        entity_expr: "NULL",
        mentioned_expr: "t.created_at",
        refs_expr: "ARRAY[]::text[]",
        counted: true,
    },
    RecordKind {
        key: LEGACY_MEMORY_KEY,
        label: "旧版记忆",
        table: "kovi_bot_memories",
        id_expr: "t.id",
        // 这一列只是分类标签（如 private_chat），正文在 payload 里。
        title_expr: "COALESCE(NULLIF(t.payload->>'content', ''), t.context)",
        body_expr: "COALESCE(NULLIF(t.payload->>'content', ''), t.context)",
        search_expr: "t.context || ' ' || t.payload::text",
        time_expr: "t.occurred_at",
        scope_kind_expr: "CASE t.scope_type WHEN 'private' THEN 'qq' WHEN 'group' THEN 'qq_group' ELSE '' END",
        scope_id_expr: "COALESCE(t.subject_id::text, '')",
        status_expr: "COALESCE(t.payload->>'memory_type', '')",
        weight_expr: "t.importance",
        tags_expr: "jsonb_build_array(COALESCE(t.payload->>'memory_type', 'memory'))",
        entity_expr: "NULL",
        mentioned_expr: "t.occurred_at",
        refs_expr: "ARRAY[]::text[]",
        counted: true,
    },
    mind_kind!("episode", "情节", "yunxi_episodes", "情节", "NULL"),
    mind_kind!("belief", "信念", "yunxi_beliefs", "信念", "NULL"),
    mind_kind!(
        "preference",
        "偏好",
        "yunxi_preferences",
        "偏好",
        "t.payload->>'subject'"
    ),
    mind_kind!(
        "interest",
        "兴趣",
        "yunxi_interests",
        "兴趣",
        "t.payload->>'topic'"
    ),
    mind_kind!("curiosity", "好奇", "yunxi_curiosities", "好奇", "NULL"),
    mind_kind!(
        "open_question",
        "未解问题",
        "yunxi_open_questions",
        "未解问题",
        "NULL"
    ),
    mind_kind!("agenda", "议程", "yunxi_agenda_items", "议程", "NULL"),
    RecordKind {
        key: "goal",
        label: "目标",
        table: "yunxi_goals",
        id_expr: "t.id",
        title_expr: "t.title",
        body_expr: "COALESCE(t.details, '')",
        search_expr: "t.title || ' ' || COALESCE(t.details, '')",
        time_expr: "t.created_at",
        scope_kind_expr: "t.owner_kind",
        scope_id_expr: "COALESCE(t.owner_id::text, '')",
        status_expr: "t.state",
        weight_expr: "NULL",
        tags_expr: "jsonb_build_array(t.kind)",
        entity_expr: "NULL",
        mentioned_expr: "t.updated_at",
        refs_expr: "ARRAY[]::text[]",
        counted: true,
    },
    RecordKind {
        key: "open_loop",
        label: "未完结线索",
        table: "yunxi_open_loops",
        id_expr: "t.id",
        title_expr: "t.summary",
        body_expr: "t.summary",
        search_expr: "t.summary || ' ' || COALESCE(t.dedupe_key, '')",
        time_expr: "t.created_at",
        scope_kind_expr: "t.owner_kind",
        scope_id_expr: "COALESCE(t.owner_id::text, '')",
        status_expr: "t.status",
        weight_expr: "t.salience",
        tags_expr: "jsonb_build_array(t.kind)",
        entity_expr: "NULL",
        mentioned_expr: "t.updated_at",
        refs_expr: "ARRAY[]::text[]",
        counted: true,
    },
    RecordKind {
        key: "gag",
        label: "梗账本",
        table: "yunxi_gag_entries",
        id_expr: "t.id",
        title_expr: "t.text",
        body_expr: "t.text",
        search_expr: "t.text",
        time_expr: "t.created_at",
        scope_kind_expr: "t.scope_kind",
        scope_id_expr: "COALESCE(t.scope_id, '')",
        status_expr: "t.state",
        weight_expr: "t.importance",
        tags_expr: "jsonb_build_array(t.kind)",
        entity_expr: "NULL",
        mentioned_expr: "t.updated_at",
        refs_expr: "ARRAY[]::text[]",
        counted: true,
    },
    RecordKind {
        key: "user_profile",
        label: "用户档案",
        table: "kovi_bot_user_profiles",
        id_expr: "t.user_id",
        title_expr: "COALESCE(t.payload->>'name', t.payload->>'nickname', 'QQ ' || t.user_id)",
        body_expr: "t.payload::text",
        search_expr: "t.payload::text",
        time_expr: "t.updated_at",
        scope_kind_expr: "'qq'",
        scope_id_expr: "t.user_id::text",
        status_expr: "''",
        weight_expr: "NULL",
        tags_expr: "'[]'::jsonb",
        entity_expr: "COALESCE(t.payload->>'nickname', 'QQ ' || t.user_id)",
        mentioned_expr: "t.updated_at",
        refs_expr: "ARRAY[]::text[]",
        counted: true,
    },
    RecordKind {
        key: "group_profile",
        label: "群档案",
        table: "kovi_bot_group_profiles",
        id_expr: "t.group_id",
        title_expr: "COALESCE(t.payload->>'name', '群 ' || t.group_id)",
        body_expr: "t.payload::text",
        search_expr: "t.payload::text",
        time_expr: "t.updated_at",
        scope_kind_expr: "'qq_group'",
        scope_id_expr: "t.group_id::text",
        status_expr: "''",
        weight_expr: "NULL",
        tags_expr: "'[]'::jsonb",
        entity_expr: "COALESCE(t.payload->>'name', '群 ' || t.group_id)",
        mentioned_expr: "t.updated_at",
        refs_expr: "ARRAY[]::text[]",
        counted: true,
    },
    RecordKind {
        key: "summary",
        label: "会话摘要",
        table: "kovi_bot_conversation_summaries",
        id_expr: "t.summary_key",
        title_expr: "t.summary_key",
        body_expr: "t.summary",
        search_expr: "t.summary_key || ' ' || t.summary",
        time_expr: "t.updated_at",
        scope_kind_expr: "''",
        scope_id_expr: "t.summary_key",
        status_expr: "''",
        weight_expr: "NULL",
        tags_expr: "'[]'::jsonb",
        entity_expr: "NULL",
        mentioned_expr: "t.updated_at",
        refs_expr: "ARRAY[]::text[]",
        counted: true,
    },
];

fn kind_meta(key: &str) -> Result<&'static RecordKind, ApiError> {
    KINDS
        .iter()
        .find(|kind| kind.key == key)
        .ok_or_else(|| ApiError::not_found(format!("未知的记录类型: {key}")))
}

fn database_pool() -> Result<&'static PgPool, ApiError> {
    crate::memory::MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| ApiError::internal("PostgreSQL 记忆连接池尚未初始化，记忆功能不可用"))
}

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

// ---------------------------------------------------------------- 概览

/// `GET /api/memory/overview`
pub(crate) async fn overview() -> Result<Json<Value>, ApiError> {
    let pool = database_pool()?;
    let existing = existing_tables(pool).await?;

    let mut counts = Map::new();
    let mut total: i64 = 0;
    for kind in KINDS {
        if !kind.counted || !existing.contains(kind.table) {
            continue;
        }
        let sql = format!("SELECT count(*) AS n FROM {}", kind.table);
        let count: i64 = query(&sql)
            .fetch_one(pool)
            .await
            .map_err(|error| ApiError::internal(format!("统计 {} 失败: {error}", kind.table)))?
            .try_get("n")
            .unwrap_or_default();
        total += count;
        counts.insert(kind.key.to_string(), json!(count));
    }

    let people: i64 = if existing.contains("yunxi_persons") {
        count_rows(pool, "yunxi_persons").await?
    } else {
        0
    };
    let conversations: i64 = if existing.contains("yunxi_conversations") {
        count_rows(pool, "yunxi_conversations").await?
    } else {
        0
    };

    Ok(Json(json!({
        "counts": counts,
        "total_records": total,
        "people": people,
        "conversations": conversations,
        "kinds": KINDS.iter().map(|kind| json!({
            "key": kind.key,
            "label": kind.label,
            "count": counts.get(kind.key).cloned().unwrap_or(json!(0)),
            "available": existing.contains(kind.table),
        })).collect::<Vec<_>>(),
        "storage": {
            "size_bytes": crate::memory::MEMORY_MANAGER.storage_size_bytes().await,
        },
        "recent": recent_records(pool, &existing).await?,
    })))
}

async fn count_rows(pool: &PgPool, table: &str) -> Result<i64, ApiError> {
    let sql = format!("SELECT count(*) AS n FROM {table}");
    query(&sql)
        .fetch_one(pool)
        .await
        .map_err(|error| ApiError::internal(format!("统计 {table} 失败: {error}")))?
        .try_get("n")
        .map_err(|error| ApiError::internal(error.to_string()))
}

/// 数据库里当前存在哪些表。
///
/// 未启用的子系统（世界模型、Mind 的部分表）不会建表，概览页不该因此整个报错。
async fn existing_tables(pool: &PgPool) -> Result<BTreeSet<String>, ApiError> {
    let names: Vec<String> = KINDS
        .iter()
        .map(|kind| kind.table.to_string())
        .chain(
            [
                "yunxi_persons",
                "yunxi_conversations",
                "yunxi_relations",
                "yunxi_affect_states",
                MIGRATION_LEDGER_TABLE,
            ]
            .into_iter()
            .map(str::to_string),
        )
        .collect();
    let rows = query("SELECT to_regclass(name)::text AS resolved FROM unnest($1::text[]) AS name")
        .bind(&names)
        .fetch_all(pool)
        .await
        .map_err(|error| ApiError::internal(format!("读取表清单失败: {error}")))?;
    Ok(rows
        .iter()
        .filter_map(|row| row.try_get::<Option<String>, _>("resolved").ok().flatten())
        .collect())
}

/// 概览页的"最近变化"：各主要类型最新的几条。
async fn recent_records(
    pool: &PgPool,
    existing: &BTreeSet<String>,
) -> Result<Vec<Value>, ApiError> {
    let mut items = Vec::new();
    for kind in KINDS {
        if !existing.contains(kind.table) {
            continue;
        }
        let rows = fetch_kind(pool, kind, "", 3, 0).await?;
        items.extend(rows);
    }
    sort_and_slice(&mut items, 12);
    Ok(items)
}

// ---------------------------------------------------------------- 列表与详情

#[derive(Deserialize)]
pub(crate) struct RecordsQuery {
    /// 逗号分隔的类型；缺省表示全部。
    #[serde(default)]
    kinds: Option<String>,
    #[serde(default)]
    q: Option<String>,
    /// 作用域过滤：`person:<uuid>` / `conversation:<uuid>` / `qq:<号>` / `global`。
    #[serde(default)]
    scope: Option<String>,
    /// 标签过滤：逗号分隔，命中任意一个即保留。
    #[serde(default)]
    tags: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

/// `GET /api/memory/records`
pub(crate) async fn records(Query(params): Query<RecordsQuery>) -> Result<Json<Value>, ApiError> {
    let pool = database_pool()?;
    let existing = existing_tables(pool).await?;
    let query_text = params.q.unwrap_or_default().trim().to_string();
    let limit = clamp_limit(params.limit);
    let offset = params.offset.unwrap_or(0).max(0);

    let requested: Vec<&RecordKind> = match params.kinds.as_deref() {
        Some(list) if !list.trim().is_empty() => {
            let mut kinds = Vec::new();
            for key in list.split(',').map(str::trim).filter(|key| !key.is_empty()) {
                kinds.push(kind_meta(key)?);
            }
            kinds
        }
        _ => KINDS.iter().collect(),
    };

    let mut all = Vec::new();
    let mut totals = BTreeMap::new();
    for kind in requested {
        if !existing.contains(kind.table) {
            totals.insert(kind.key.to_string(), 0_i64);
            continue;
        }
        let total = count_kind(pool, kind, &query_text).await?;
        totals.insert(kind.key.to_string(), total);
        // 每种类型都从 0 开始取到 offset+limit，归并排序后再统一分页，
        // 这样跨类型的顺序是正确的（代价是有界的一次性排序）。
        let fetch = (offset + limit).min(PER_KIND_FETCH);
        let rows = fetch_kind(pool, kind, &query_text, fetch, 0).await?;
        all.extend(rows);
    }

    if let Some(filter) = params.tags.as_deref() {
        let wanted = parse_tags(filter);
        if !wanted.is_empty() {
            all.retain(|item| node_has_tag(item, &wanted));
        }
    }

    if let Some(scope) = params
        .scope
        .as_deref()
        .filter(|scope| !scope.trim().is_empty())
    {
        let (kind, id) = split_scope(scope);
        all.retain(|item| item["scope_kind"] == json!(kind) && item["scope_id"] == json!(id));
        // 作用域过滤后的总数不再等于全表统计，直接以过滤结果为准。
        let mut filtered = BTreeMap::new();
        for item in &all {
            let key = item["kind"].as_str().unwrap_or_default().to_string();
            *filtered.entry(key).or_insert(0_i64) += 1;
        }
        totals = filtered;
    }

    sort_items(&mut all);
    let total: i64 = totals.values().sum();
    let page: Vec<Value> = all
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();

    Ok(Json(json!({
        "items": page,
        "total": total,
        "limit": limit,
        "offset": offset,
        "counts": totals,
        "query": query_text,
    })))
}

/// 解析逗号分隔的标签过滤参数。
fn parse_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// 卡片的标签是否命中过滤集合。
fn node_has_tag(node: &Value, wanted: &[String]) -> bool {
    node["tags"]
        .as_array()
        .map(|tags| {
            tags.iter().any(|tag| {
                tag.as_str()
                    .is_some_and(|tag| wanted.iter().any(|want| want == tag))
            })
        })
        .unwrap_or(false)
}

fn split_scope(scope: &str) -> (String, String) {
    match scope.split_once(':') {
        Some((kind, id)) => (kind.to_string(), id.to_string()),
        None => (scope.to_string(), String::new()),
    }
}

/// `GET /api/memory/record/{kind}/{id}`
pub(crate) async fn record(
    UrlPath((kind, id)): UrlPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let meta = kind_meta(&kind)?;
    let pool = database_pool()?;
    let sql = format!(
        "SELECT row_to_json(t) AS row FROM {} t WHERE {}::text = $1",
        meta.table, meta.id_expr
    );
    let row = query(&sql)
        .bind(&id)
        .fetch_optional(pool)
        .await
        .map_err(|error| ApiError::internal(format!("读取记录失败: {error}")))?
        .ok_or_else(|| ApiError::not_found("记录不存在"))?;
    let raw: Value = row
        .try_get("row")
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let scope_label = resolve_scope_label(
        pool,
        raw.get("scope_kind")
            .or_else(|| raw.get("owner_kind"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        raw.get("scope_id")
            .or_else(|| raw.get("owner_id"))
            .and_then(Value::as_str),
    )
    .await;
    Ok(Json(json!({
        "kind": meta.key,
        "label": meta.label,
        "table": meta.table,
        "scope_label": scope_label,
        "record": raw,
    })))
}

// ---------------------------------------------------------------- 人物

#[derive(Deserialize)]
pub(crate) struct PeopleQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

/// `GET /api/memory/people`
pub(crate) async fn people(Query(params): Query<PeopleQuery>) -> Result<Json<Value>, ApiError> {
    let pool = database_pool()?;
    let existing = existing_tables(pool).await?;
    if !existing.contains("yunxi_persons") {
        return Ok(Json(json!({ "items": [], "total": 0 })));
    }
    let limit = clamp_limit(params.limit);
    let offset = params.offset.unwrap_or(0).max(0);
    let query_text = params.q.unwrap_or_default().trim().to_string();

    let rows = query(&people_sql(&existing))
        .bind(&query_text)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(|error| ApiError::internal(format!("读取人物失败: {error}")))?;

    let items: Vec<Value> = rows.iter().map(person_card).collect();
    let total: i64 = query(
        r#"
        SELECT count(*) AS n FROM yunxi_persons p
        LEFT JOIN (
            SELECT person_id,
                   string_agg(platform || ':' || external_id, ', ' ORDER BY platform, external_id) AS identities
            FROM yunxi_external_identities GROUP BY person_id
        ) i ON i.person_id = p.id
        WHERE ($1 = '' OR COALESCE(i.identities, '') ILIKE '%' || $1 || '%' OR p.id::text ILIKE '%' || $1 || '%')
        "#,
    )
    .bind(&query_text)
    .fetch_one(pool)
    .await
    .map_err(|error| ApiError::internal(format!("统计人物失败: {error}")))?
    .try_get("n")
    .unwrap_or_default();

    Ok(Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

/// 人物列表的 SQL。计数列不是"好看的数字"，而是**她真正记得这个人多少条**，
/// 因此必须跨两张表数：
///
/// - `yunxi_memories` 的 person 作用域（Memory v2）；
/// - `kovi_bot_memories` 里 `scope_type = 'private'` 的行，按这个人的 QQ 身份归属
///   （旧版记忆的 subject_id 对私聊就是对方 QQ 号）。
///
/// 只数前者会让卡片**结构性恒为 0**：聊天的记忆写入口至今仍在旧表，而 Core 的
/// person 作用域没有聊天写入口（`yunxi-memory-migrate` 是离线迁移，生产没跑之前
/// v2 里不会有 person 行）。群记忆不属于任何个人——它的 subject 是群号，会被
/// `scope_type = 'private'` 挡掉，这也是刻意的口径：卡片说的是"关于他"的记忆。
///
/// 两个来源是同一份内容的两个投影，所以还要排掉"旧表这一行在 v2 里已有副本"的
/// 情况（运行时双写按 Core UUID 对齐主键，离线 backfill 走 ledger），否则等
/// backfill 真跑起来，同一个人的数字会凭空翻倍。
fn people_sql(existing: &BTreeSet<String>) -> String {
    format!(
        r#"
        SELECT
            p.id::text AS id,
            p.created_at,
            COALESCE(i.identities, '') AS identities,
            i.qq AS qq,
            r.familiarity, r.affinity, r.trust, r.comfort, r.tension,
            a.valence, a.arousal, a.social_energy, a.curiosity,
            COALESCE(m.count, 0) + COALESCE(legacy.count, 0) AS memory_count
        FROM yunxi_persons p
        LEFT JOIN (
            SELECT person_id,
                   string_agg(platform || ':' || external_id, ', ' ORDER BY platform, external_id) AS identities,
                   min(external_id) FILTER (WHERE platform = 'qq') AS qq
            FROM yunxi_external_identities GROUP BY person_id
        ) i ON i.person_id = p.id
        LEFT JOIN {relations} r ON r.person_id = p.id
        LEFT JOIN {affect} a ON a.person_id = p.id
        LEFT JOIN (
            SELECT scope_id, count(*) AS count FROM yunxi_memories
            WHERE scope_kind = 'person' GROUP BY scope_id
        ) m ON m.scope_id = p.id
        LEFT JOIN {legacy} legacy ON legacy.person_id = p.id
        WHERE ($1 = '' OR COALESCE(i.identities, '') ILIKE '%' || $1 || '%'
               OR p.id::text ILIKE '%' || $1 || '%')
        ORDER BY memory_count DESC, p.created_at DESC
        LIMIT $2 OFFSET $3
        "#,
        relations = if existing.contains("yunxi_relations") {
            "yunxi_relations"
        } else {
            // 用一张同形状的空表替代，避免为了可选表写两套 SQL。
            "(SELECT NULL::uuid AS person_id, NULL::float8 AS familiarity, NULL::float8 AS affinity, NULL::float8 AS trust, NULL::float8 AS comfort, NULL::float8 AS tension WHERE FALSE) "
        },
        affect = if existing.contains("yunxi_affect_states") {
            "yunxi_affect_states"
        } else {
            "(SELECT NULL::uuid AS person_id, NULL::float8 AS valence, NULL::float8 AS arousal, NULL::float8 AS social_energy, NULL::float8 AS curiosity WHERE FALSE) "
        },
        legacy = if existing.contains("kovi_bot_memories") {
            // 按 person 聚合而不是按身份行 join：一个人挂多个 QQ 身份时，直接 join
            // 会让列表出现重复卡片、计数也会被乘开。
            //
            // 两处 NOT EXISTS 防的是"同一条记忆被数两次"。Core 与旧表是同一份内容的
            // 两个投影，来源有两条：运行时双写会沿用 Core 的 UUID 当旧表主键；
            // 离线 backfill 用 ledger 记录 legacy_id → target_id。任一条成立就说明
            // 这条旧记忆在 v2 里已经有对应行，不能再计一次。
            format!(
                "(SELECT identity.person_id, count(*) AS count \
                 FROM yunxi_external_identities identity \
                 JOIN kovi_bot_memories memory \
                   ON memory.subject_id::text = identity.external_id \
                  AND memory.scope_type = 'private' \
                 WHERE identity.platform = 'qq' \
                   AND NOT EXISTS (SELECT 1 FROM yunxi_memories core WHERE core.id::text = memory.id) \
                   {ledger} \
                 GROUP BY identity.person_id) ",
                ledger = if existing.contains(MIGRATION_LEDGER_TABLE) {
                    "AND NOT EXISTS (SELECT 1 FROM yunxi_memory_migration_items item \
                      JOIN yunxi_memories target ON target.id = item.target_id \
                      WHERE item.legacy_id = memory.id)"
                } else {
                    ""
                },
            )
        } else {
            "(SELECT NULL::uuid AS person_id, NULL::bigint AS count WHERE FALSE) ".to_string()
        },
    )
}

fn person_card(row: &PgRow) -> Value {
    let qq: Option<String> = row.try_get("qq").ok().flatten();
    json!({
        "id": row.try_get::<String, _>("id").unwrap_or_default(),
        "display": qq.clone().map(|qq| format!("QQ {qq}")).unwrap_or_else(|| "未知平台".to_string()),
        "identities": row.try_get::<String, _>("identities").unwrap_or_default(),
        "qq": qq,
        "memory_count": row.try_get::<i64, _>("memory_count").unwrap_or_default(),
        "relation": {
            "familiarity": row.try_get::<Option<f64>, _>("familiarity").ok().flatten(),
            "affinity": row.try_get::<Option<f64>, _>("affinity").ok().flatten(),
            "trust": row.try_get::<Option<f64>, _>("trust").ok().flatten(),
            "comfort": row.try_get::<Option<f64>, _>("comfort").ok().flatten(),
            "tension": row.try_get::<Option<f64>, _>("tension").ok().flatten(),
        },
        "affect": {
            "valence": row.try_get::<Option<f64>, _>("valence").ok().flatten(),
            "arousal": row.try_get::<Option<f64>, _>("arousal").ok().flatten(),
            "social_energy": row.try_get::<Option<f64>, _>("social_energy").ok().flatten(),
            "curiosity": row.try_get::<Option<f64>, _>("curiosity").ok().flatten(),
        },
        "created_at": row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("created_at")
            .ok()
            .flatten()
            .map(|time| time.to_rfc3339()),
    })
}

/// `GET /api/memory/person/{id}`
pub(crate) async fn person(UrlPath(id): UrlPath<String>) -> Result<Json<Value>, ApiError> {
    let pool = database_pool()?;
    let Ok(person_id) = Uuid::parse_str(&id) else {
        return Err(ApiError::bad_request("人物 id 必须是 UUID"));
    };

    let identity_rows = query(
        "SELECT platform, external_id, created_at FROM yunxi_external_identities \
         WHERE person_id = $1 ORDER BY platform, external_id",
    )
    .bind(person_id)
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::internal(format!("读取身份失败: {error}")))?;
    let mut identities: Vec<Value> = Vec::with_capacity(identity_rows.len());
    // 旧版记忆按 QQ 号归属（subject_id = 对方 QQ），所以筛旧记忆时要用这串身份，
    // 而不是 person 的 UUID。
    let mut qq_identities: Vec<String> = Vec::new();
    for row in &identity_rows {
        let platform: String = row.try_get("platform").unwrap_or_default();
        let external_id: String = row.try_get("external_id").unwrap_or_default();
        if platform == "qq" && !external_id.is_empty() {
            qq_identities.push(external_id.clone());
        }
        identities.push(json!({ "platform": platform, "external_id": external_id }));
    }

    // 注意：人物存在但没有外部身份也照样打开（此时 identities 为空数组），
    // 因为"解析不出 QQ 号"不该等于"这个人不存在"。

    let relation = fetch_optional_json(
        pool,
        "SELECT row_to_json(t) AS row FROM yunxi_relations t WHERE t.person_id = $1",
        person_id,
    )
    .await;
    let affect = fetch_optional_json(
        pool,
        "SELECT row_to_json(t) AS row FROM yunxi_affect_states t WHERE t.person_id = $1",
        person_id,
    )
    .await;
    let self_model = fetch_singleton_json(
        pool,
        "SELECT row_to_json(t) AS row FROM yunxi_self_model t WHERE t.singleton",
    )
    .await;

    let mut records = Vec::new();
    for kind in KINDS {
        let sql = format!(
            "SELECT * FROM ({}) t WHERE {} LIMIT 200",
            kind_select(kind),
            person_record_filter(kind)
        );
        let rows = if kind.key == LEGACY_MEMORY_KEY {
            // 旧版私有记忆在库里是 QQ 作用域（`scope_type='private'` 投影成 'qq'），
            // 只按 person 的 UUID 匹配会一条都取不到——人物弹窗因此长期是空的。
            query(&sql).bind(&qq_identities).fetch_all(pool).await
        } else {
            // scope_id 在各子查询里已是 text：按文本绑定，否则 Postgres 会因为
            // `text = uuid` 直接报operator不存在。
            query(&sql)
                .bind(person_id.to_string())
                .fetch_all(pool)
                .await
        };

        match rows {
            Ok(rows) => {
                let mut cards: Vec<Value> = rows.iter().map(|row| row_to_card(row, kind)).collect();
                records.append(&mut cards);
            }
            Err(_) => continue,
        }
    }
    sort_items(&mut records);
    records.truncate(120);

    let conversations = query(
        r#"
        SELECT c.id::text AS id, c.kind, COALESCE(e.external_id, '') AS external_id,
               m.role, COALESCE(e.platform, '') AS platform
        FROM yunxi_conversation_members m
        JOIN yunxi_conversations c ON c.id = m.conversation_id
        LEFT JOIN yunxi_external_conversations e ON e.conversation_id = c.id
        WHERE m.person_id = $1
        ORDER BY c.created_at DESC
        "#,
    )
    .bind(person_id)
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::internal(format!("读取会话成员关系失败: {error}")))?;
    let conversations: Vec<Value> = conversations
        .iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "kind": row.try_get::<String, _>("kind").unwrap_or_default(),
                "platform": row.try_get::<String, _>("platform").unwrap_or_default(),
                "external_id": row.try_get::<String, _>("external_id").unwrap_or_default(),
                "role": row.try_get::<Option<String>, _>("role").ok().flatten(),
            })
        })
        .collect();

    Ok(Json(json!({
        "id": person_id.to_string(),
        "identities": identities,
        "relation": relation,
        "affect": affect,
        "self_model": self_model,
        "records": records,
        "conversations": conversations,
    })))
}

/// 人物详情里"这条记录属于这个人"的判据。
///
/// 绝大多数类型是 Core 的 person 作用域；唯独旧版记忆是 QQ 作用域——它的
/// `scope_id` 是对方 QQ 号（可能多个身份），所以要绑一个文本数组进去。
fn person_record_filter(kind: &RecordKind) -> &'static str {
    if kind.key == LEGACY_MEMORY_KEY {
        "t.scope_kind = 'qq' AND t.scope_id = ANY($1::text[])"
    } else {
        "t.scope_kind = 'person' AND t.scope_id = $1"
    }
}

// ---------------------------------------------------------------- 图谱

/// 星座图（记忆地图）：节点是记忆记录，边是它们之间真实存在的关系。
///
/// 四类边的判定都落在数据上，不是画着好看的：
/// - **实体**：同一个人／群的记录（同一个作用域）；
/// - **时序**：时间上相邻的记录；
/// - **语义**：共享标签（Jaccard 最近的几条）；
/// - **因果**：payload 里直接引用了另一条记录的 id（例如议程指向某个未完结线索）。
#[derive(Deserialize)]
pub(crate) struct GraphQuery {
    #[serde(default)]
    kinds: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    /// 标签过滤：逗号分隔，命中任意一个即保留。
    #[serde(default)]
    tags: Option<String>,
}

/// 默认画多少节点。图是画给人看的，节点太多只会糊成一团。
const DEFAULT_GRAPH_NODES: i64 = 120;
/// 节点上限。
const MAX_GRAPH_NODES: i64 = 400;
/// 每个节点在每类关系里最多连几条，防止连成毛线球。
const LINKS_PER_NODE_PER_TYPE: usize = 4;

pub(crate) async fn graph(Query(params): Query<GraphQuery>) -> Result<Json<Value>, ApiError> {
    let pool = database_pool()?;
    let existing = existing_tables(pool).await?;
    let query_text = params.q.unwrap_or_default().trim().to_string();
    let limit = params
        .limit
        .unwrap_or(DEFAULT_GRAPH_NODES)
        .clamp(1, MAX_GRAPH_NODES);

    let requested: Vec<&RecordKind> = match params.kinds.as_deref() {
        Some(list) if !list.trim().is_empty() => {
            let mut kinds = Vec::new();
            for key in list.split(',').map(str::trim).filter(|key| !key.is_empty()) {
                kinds.push(kind_meta(key)?);
            }
            kinds
        }
        _ => KINDS.iter().collect(),
    };

    let mut nodes = Vec::new();
    for kind in requested {
        if !existing.contains(kind.table) || nodes.len() as i64 >= limit {
            continue;
        }
        let fetch = (limit - nodes.len() as i64).min(PER_KIND_FETCH);
        let mut cards = fetch_kind(pool, kind, &query_text, fetch, 0).await?;
        nodes.append(&mut cards);
    }
    if let Some(filter) = params.tags.as_deref() {
        let wanted = parse_tags(filter);
        if !wanted.is_empty() {
            nodes.retain(|node| node_has_tag(node, &wanted));
        }
    }
    sort_items(&mut nodes);
    nodes.truncate(limit as usize);
    resolve_scope_labels(pool, &mut nodes).await;

    let (links, counts) = build_links(&nodes);
    let mut link_degree = vec![0_usize; nodes.len()];
    let id_index: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node["id"].as_str().unwrap_or_default().to_string(), index))
        .collect();
    for link in &links {
        if let Some(&index) = id_index.get(link["source"].as_str().unwrap_or_default()) {
            link_degree[index] += 1;
        }
        if let Some(&index) = id_index.get(link["target"].as_str().unwrap_or_default()) {
            link_degree[index] += 1;
        }
    }
    for (index, node) in nodes.iter_mut().enumerate() {
        if let Some(object) = node.as_object_mut() {
            object.insert("links".to_string(), json!(link_degree[index]));
        }
    }

    let range = time_range(&nodes);
    Ok(Json(json!({
        "nodes": nodes,
        "links": links,
        "link_counts": counts,
        "range": range,
        "total": nodes.len(),
        "query": query_text,
    })))
}

/// 时间范围（画配色图例用）。
fn time_range(nodes: &[Value]) -> Value {
    let mut min: Option<&str> = None;
    let mut max: Option<&str> = None;
    for node in nodes {
        if let Some(time) = node["occurred_at"].as_str() {
            min = Some(min.map_or(time, |current| current.min(time)));
            max = Some(max.map_or(time, |current| current.max(time)));
        }
    }
    json!({ "min": min, "max": max })
}

/// 按四类关系连边。每类每个节点最多连 K 条，避免图变成毛线球。
fn build_links(nodes: &[Value]) -> (Vec<Value>, Value) {
    let mut links = Vec::new();
    let mut seen: BTreeSet<(usize, usize, &'static str)> = BTreeSet::new();
    let ids: Vec<String> = nodes
        .iter()
        .map(|node| node["id"].as_str().unwrap_or_default().to_string())
        .collect();
    let id_index: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();

    let mut push = |left: usize, right: usize, kind: &'static str, links: &mut Vec<Value>| {
        if left == right {
            return;
        }
        let key = (left.min(right), left.max(right), kind);
        if seen.insert(key) {
            links.push(json!({
                "source": ids[left],
                "target": ids[right],
                "type": kind,
            }));
        }
    };

    // 因果：payload 里直接引用到的另一条记录。
    for (index, node) in nodes.iter().enumerate() {
        for reference in node["refs"].as_array().into_iter().flatten() {
            if let Some(&other) = id_index.get(reference.as_str().unwrap_or_default()) {
                push(index, other, "causal", &mut links);
            }
        }
    }

    let tags_of = |node: &Value| -> Vec<String> {
        node["tags"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let scope_of = |node: &Value| {
        (
            node["scope_kind"].as_str().unwrap_or_default().to_string(),
            node["scope_id"].as_str().unwrap_or_default().to_string(),
        )
    };
    let time_of = |node: &Value| node["occurred_at"].as_str().unwrap_or_default().to_string();

    let mut temporal: Vec<Vec<(usize, i64)>> = vec![Vec::new(); nodes.len()];
    let mut entity: Vec<Vec<(usize, i64)>> = vec![Vec::new(); nodes.len()];
    let mut semantic: Vec<Vec<(usize, f64)>> = vec![Vec::new(); nodes.len()];

    for left in 0..nodes.len() {
        for right in (left + 1)..nodes.len() {
            // 时序：时间越近越靠前（全局记录也能连成一条时间轴）。
            temporal[left].push((
                right,
                time_distance(&time_of(&nodes[left]), &time_of(&nodes[right])),
            ));
            temporal[right].push((
                left,
                time_distance(&time_of(&nodes[left]), &time_of(&nodes[right])),
            ));

            // 实体：同一个非全局作用域。
            let (left_kind, left_id) = scope_of(&nodes[left]);
            let (right_kind, right_id) = scope_of(&nodes[right]);
            if !left_id.is_empty() && left_kind == right_kind && left_id == right_id {
                entity[left].push((right, 0));
                entity[right].push((left, 0));
            }

            // 语义：共享标签的 Jaccard 相似度。
            let left_tags = tags_of(&nodes[left]);
            let right_tags = tags_of(&nodes[right]);
            if !left_tags.is_empty() && !right_tags.is_empty() {
                let overlap = left_tags
                    .iter()
                    .filter(|tag| right_tags.contains(tag))
                    .count();
                if overlap > 0 {
                    let union = left_tags.len() + right_tags.len() - overlap;
                    let similarity = overlap as f64 / union.max(1) as f64;
                    semantic[left].push((right, similarity));
                    semantic[right].push((left, similarity));
                }
            }
        }
    }

    let mut connect =
        |candidates: &mut [Vec<(usize, i64)>], kind: &'static str, links: &mut Vec<Value>| {
            for (index, neighbours) in candidates.iter_mut().enumerate() {
                neighbours.sort_by_key(|(_, weight)| *weight);
                for &(other, _) in neighbours.iter().take(LINKS_PER_NODE_PER_TYPE) {
                    push(index, other, kind, links);
                }
            }
        };
    connect(&mut entity, "entity", &mut links);
    connect(&mut temporal, "temporal", &mut links);

    for (index, neighbours) in semantic.iter_mut().enumerate() {
        neighbours.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for &(other, _) in neighbours.iter().take(LINKS_PER_NODE_PER_TYPE) {
            push(index, other, "semantic", &mut links);
        }
    }

    let mut counts = Map::new();
    for kind in ["semantic", "temporal", "entity", "causal"] {
        let count = links
            .iter()
            .filter(|link| link["type"] == json!(kind))
            .count();
        counts.insert(kind.to_string(), json!(count));
    }
    (links, Value::Object(counts))
}

/// 两个时间戳相差多少毫秒（解析不了就当无穷远）。
fn time_distance(left: &str, right: &str) -> i64 {
    let parse = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|time| time.timestamp_millis())
    };
    match (parse(left), parse(right)) {
        (Some(left), Some(right)) => (left - right).abs(),
        _ => i64::MAX,
    }
}

/// `GET /api/memory/tags`：标签清单（按出现次数倒序），供标签筛选用。
pub(crate) async fn tags() -> Result<Json<Value>, ApiError> {
    let pool = database_pool()?;
    let existing = existing_tables(pool).await?;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for kind in KINDS {
        if !existing.contains(kind.table) {
            continue;
        }
        // 只统计标签本身，不为了筛选去全表扫正文。
        let sql = format!(
            "SELECT tag, count(*) AS n FROM {table} t, \
             LATERAL jsonb_array_elements_text({tags}) AS tag \
             WHERE tag <> '' GROUP BY tag",
            table = kind.table,
            tags = kind.tags_expr,
        );
        let Ok(rows) = query(&sql).fetch_all(pool).await else {
            continue;
        };
        for row in rows {
            let tag: String = row.try_get("tag").unwrap_or_default();
            let count: i64 = row.try_get("n").unwrap_or_default();
            *counts.entry(tag).or_default() += count;
        }
    }
    let mut items: Vec<(String, i64)> = counts.into_iter().collect();
    items.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    items.truncate(200);
    Ok(Json(json!({
        "tags": items
            .into_iter()
            .map(|(tag, count)| json!({ "tag": tag, "count": count }))
            .collect::<Vec<_>>(),
    })))
}

/// `GET /api/memory/stats`：顶部统计卡。
///
/// 「本周 / 上周」按记录进入记忆的时间（提及时间）统计，因此它回答的是
/// "芸汐最近记住了多少"，而不是"发生了多少事"。
pub(crate) async fn stats() -> Result<Json<Value>, ApiError> {
    let pool = database_pool()?;
    let existing = existing_tables(pool).await?;
    let mut by_kind = Map::new();
    let mut week_new = 0_i64;
    let mut prev_week = 0_i64;
    let mut week_by_kind: Vec<Value> = Vec::new();

    for kind in KINDS {
        if !existing.contains(kind.table) {
            continue;
        }
        let total_sql = format!("SELECT count(*) AS n FROM {}", kind.table);
        let Ok(row) = query(&total_sql).fetch_one(pool).await else {
            continue;
        };
        let total: i64 = row.try_get("n").unwrap_or_default();
        if total == 0 {
            continue;
        }
        by_kind.insert(kind.key.to_string(), json!(total));

        let window_sql = format!(
            "SELECT count(*) FILTER (WHERE {mentioned} >= now() - interval '7 days') AS this_week, \
                    count(*) FILTER (WHERE {mentioned} >= now() - interval '14 days' \
                                       AND {mentioned} < now() - interval '7 days') AS last_week \
             FROM {table} t",
            mentioned = kind.mentioned_expr,
            table = kind.table,
        );
        if let Ok(row) = query(&window_sql).fetch_one(pool).await {
            let this_week: i64 = row.try_get("this_week").unwrap_or_default();
            let last_week: i64 = row.try_get("last_week").unwrap_or_default();
            week_new += this_week;
            prev_week += last_week;
            if this_week > 0 {
                week_by_kind.push(json!({
                    "key": kind.key,
                    "label": kind.label,
                    "count": this_week,
                }));
            }
        }
    }

    let people = if existing.contains("yunxi_persons") {
        count_rows(pool, "yunxi_persons").await.unwrap_or_default()
    } else {
        0
    };
    let conversations = if existing.contains("yunxi_conversations") {
        count_rows(pool, "yunxi_conversations")
            .await
            .unwrap_or_default()
    } else {
        0
    };

    Ok(Json(json!({
        "total": by_kind.values().filter_map(Value::as_i64).sum::<i64>(),
        "by_kind": by_kind,
        "people": people,
        "conversations": conversations,
        // 本周 / 上周用量（保存、召回、反思、心智模型、模型调用）。
        "usage": crate::metrics::weekly_cards().await,
        "week": {
            "new": week_new,
            "previous": prev_week,
            "by_kind": week_by_kind,
        },
        "storage_bytes": crate::memory::MEMORY_MANAGER.storage_size_bytes().await,
    })))
}

// ---------------------------------------------------------------- 内部实现

/// 列表查询的完整 SQL。
///
/// 过滤条件必须写在**内层子查询**里：外层别名只暴露投影出来的那几列，
/// 在外层引用基表列（例如 `t.content`）会直接报 "column t.content does not exist"。
fn fetch_kind_sql(kind: &RecordKind) -> String {
    format!(
        "SELECT * FROM ({projection} FROM {table} t \
         WHERE ($1 = '' OR ({search}) ILIKE '%' || $1 || '%')) t \
         ORDER BY t.occurred_at DESC NULLS LAST, t.id DESC LIMIT $2 OFFSET $3",
        projection = kind_projection(kind),
        table = kind.table,
        search = kind.search_expr,
    )
}

/// 统一的"卡片"投影（供列表与人物详情复用）。
fn kind_projection(kind: &RecordKind) -> String {
    // id 一律转 text、weight 一律转 float8：上层统一按字符串与浮点解码，
    // 不必为每种表的主键/计分列类型写分支。
    format!(
        "SELECT ({id})::text AS id, {title} AS title, {body} AS body, \
         {scope_kind} AS scope_kind, {scope_id} AS scope_id, {status} AS status, \
         ({weight})::float8 AS weight, {time} AS occurred_at, \
         {tags} AS tags, {entity} AS entity, {mentioned} AS mentioned_at, {refs} AS refs",
        id = kind.id_expr,
        title = kind.title_expr,
        body = kind.body_expr,
        scope_kind = kind.scope_kind_expr,
        scope_id = kind.scope_id_expr,
        status = kind.status_expr,
        weight = kind.weight_expr,
        time = kind.time_expr,
        tags = kind.tags_expr,
        entity = kind.entity_expr,
        mentioned = kind.mentioned_expr,
        refs = kind.refs_expr,
    )
}

fn kind_select(kind: &RecordKind) -> String {
    // id 一律转 text、weight 一律转 float8：上层统一按字符串与浮点解码，
    // 不必为每种表的主键类型写分支。
    format!("{} FROM {} t", kind_projection(kind), kind.table,)
}

async fn fetch_kind(
    pool: &PgPool,
    kind: &RecordKind,
    query_text: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<Value>, ApiError> {
    let rows = query(&fetch_kind_sql(kind))
        .bind(query_text)
        .bind(limit.max(1))
        .bind(offset.max(0))
        .fetch_all(pool)
        .await
        .map_err(|error| ApiError::internal(format!("读取 {} 失败: {error}", kind.label)))?;
    Ok(rows.iter().map(|row| row_to_card(row, kind)).collect())
}

async fn count_kind(pool: &PgPool, kind: &RecordKind, query_text: &str) -> Result<i64, ApiError> {
    let sql = format!(
        "SELECT count(*) AS n FROM {table} t WHERE ($1 = '' OR ({search}) ILIKE '%' || $1 || '%')",
        table = kind.table,
        search = kind.search_expr,
    );
    query(&sql)
        .bind(query_text)
        .fetch_one(pool)
        .await
        .map_err(|error| ApiError::internal(format!("统计 {} 失败: {error}", kind.label)))?
        .try_get("n")
        .map_err(|error| ApiError::internal(error.to_string()))
}

/// 把可能是"枚举对象序列化结果"的字段渲染成人能读的短标题。
///
/// 例：议程条目的 `subject` 存的是 `{"type":"open_loop","value":"<uuid>"}`，
/// 直接显示就是一段 JSON。这里挑出最有信息量的字符串字段。
fn readable_title(raw: &str, fallback: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
    {
        let object = parsed.as_object();
        if let Some(object) = object {
            for key in [
                "label",
                "text",
                "title",
                "summary",
                "proposition",
                "question",
                "topic",
            ] {
                if let Some(text) = object.get(key).and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    return truncate(text, 160);
                }
            }
            let kind = object.get("type").and_then(Value::as_str);
            let value = object.get("value").and_then(Value::as_str);
            return match (kind, value) {
                (Some(kind), Some(value)) => format!("{kind} {}", short_id(value)),
                (Some(kind), None) => kind.to_string(),
                _ => truncate(trimmed, 160),
            };
        }
    }
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        truncate(trimmed, 160)
    }
}

fn row_to_card(row: &PgRow, kind: &RecordKind) -> Value {
    let body: String = row.try_get("body").unwrap_or_default();
    let title: Option<String> = row.try_get("title").ok().flatten();
    let scope_id: Option<String> = row.try_get("scope_id").ok().flatten();
    let scope_kind: Option<String> = row.try_get("scope_kind").ok().flatten();
    let status: Option<String> = row
        .try_get("status")
        .ok()
        .flatten()
        .filter(|s: &String| !s.is_empty());
    let mut tags: Vec<String> = row
        .try_get::<Value, _>("tags")
        .ok()
        .and_then(|value| value.as_array().cloned())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .filter(|tag| !tag.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    tags.sort();
    tags.dedup();

    // 「实体」= 记录指向的具体名字：payload 里天然是名字的字段，加上它所在作用域的
    // 可读标签（QQ 号 / 群号）。两者都没有时这一列就是空的，不编造。
    let mut entities: Vec<String> = Vec::new();
    if let Some(entity) = row.try_get::<Option<String>, _>("entity").ok().flatten()
        && !entity.trim().is_empty()
    {
        entities.push(entity);
    }
    let refs: Vec<String> = row
        .try_get::<Option<Vec<String>>, _>("refs")
        .ok()
        .flatten()
        .unwrap_or_default();
    let scope_kind_value = scope_kind.clone().unwrap_or_default();
    let scope_id_value = scope_id.clone().unwrap_or_default();
    if !scope_id_value.is_empty() && scope_kind_value != "global" {
        let label = scope_label_fallback(&scope_kind_value, Some(&scope_id_value));
        if !entities.contains(&label) {
            entities.push(label);
        }
    }

    json!({
        "kind": kind.key,
        "label": kind.label,
        "id": row.try_get::<String, _>("id").unwrap_or_default(),
        "title": readable_title(
            title.as_deref().unwrap_or_default(),
            &format!("（{} 无标题）", kind.label),
        ),
        "body": readable_title(&body, ""),
        "body_chars": body.chars().count(),
        "scope_kind": scope_kind.unwrap_or_default(),
        "scope_id": scope_id.unwrap_or_default(),
        "scope_label": scope_label_fallback(
            row.try_get::<Option<String>, _>("scope_kind").ok().flatten().as_deref().unwrap_or_default(),
            row.try_get::<Option<String>, _>("scope_id").ok().flatten().as_deref(),
        ),
        "status": status,
        "weight": row.try_get::<Option<f64>, _>("weight").ok().flatten(),
        "tags": tags,
        "entities": entities,
        "refs": refs,
        "occurred_at": row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("occurred_at")
            .ok()
            .flatten()
            .map(|time| time.to_rfc3339()),
        "mentioned_at": row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("mentioned_at")
            .ok()
            .flatten()
            .map(|time| time.to_rfc3339()),
    })
}

/// 没有查到 label 时的兜底显示，保证界面上不会出现空白的作用域。
fn scope_label_fallback(scope_kind: &str, scope_id: Option<&str>) -> String {
    match (scope_kind, scope_id) {
        ("", _) | ("global", _) => "全局".to_string(),
        ("qq", Some(id)) => format!("QQ {id}"),
        ("qq_group", Some(id)) => format!("群 {id}"),
        ("person", Some(id)) => format!("人物 {}", short_id(id)),
        ("conversation", Some(id)) => format!("会话 {}", short_id(id)),
        (kind, Some(id)) => format!("{kind} {}", short_id(id)),
        (kind, None) => kind.to_string(),
    }
}

/// 把 UUID／长 id 缩成前 8 位，列表里够用且不喧宾夺主。
fn short_id(id: &str) -> String {
    if id.len() > 8 {
        id[..8].to_string()
    } else {
        id.to_string()
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn sort_items(items: &mut [Value]) {
    items.sort_by(|left, right| {
        let left_time = left["occurred_at"].as_str().unwrap_or_default();
        let right_time = right["occurred_at"].as_str().unwrap_or_default();
        right_time.cmp(left_time)
    });
}

fn sort_and_slice(items: &mut Vec<Value>, take: usize) {
    sort_items(items);
    items.truncate(take);
}

/// 批量把人／会话 id 兑换成可读标签，直接写回卡片的 `scope_label`。
///
/// 列表可能有几百条，逐条查会变成几百次往返；这里按类型各查一次。
async fn resolve_scope_labels(pool: &PgPool, nodes: &mut [Value]) {
    let mut person_ids: Vec<Uuid> = Vec::new();
    let mut conversation_ids: Vec<Uuid> = Vec::new();
    for node in nodes.iter() {
        let Some(id) = node["scope_id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let Ok(uuid) = Uuid::parse_str(id) else {
            continue;
        };
        match node["scope_kind"].as_str().unwrap_or_default() {
            "person" => person_ids.push(uuid),
            "conversation" => conversation_ids.push(uuid),
            _ => {}
        }
    }
    person_ids.sort();
    person_ids.dedup();
    conversation_ids.sort();
    conversation_ids.dedup();

    let mut person_labels: HashMap<Uuid, String> = HashMap::new();
    if !person_ids.is_empty()
        && let Ok(rows) = query(
            "SELECT person_id, COALESCE(min(external_id) FILTER (WHERE platform = 'qq'), '') AS qq \
             FROM yunxi_external_identities WHERE person_id = ANY($1) GROUP BY person_id",
        )
        .bind(&person_ids)
        .fetch_all(pool)
        .await
    {
        for row in rows {
            if let (Ok(id), Ok(qq)) = (
                row.try_get::<Uuid, _>("person_id"),
                row.try_get::<String, _>("qq"),
            ) && !qq.is_empty()
            {
                person_labels.insert(id, format!("QQ {qq}"));
            }
        }
    }

    let mut conversation_labels: HashMap<Uuid, String> = HashMap::new();
    if !conversation_ids.is_empty()
        && let Ok(rows) = query(
            "SELECT e.conversation_id, e.external_id, c.kind \
             FROM yunxi_external_conversations e \
             JOIN yunxi_conversations c ON c.id = e.conversation_id \
             WHERE e.conversation_id = ANY($1)",
        )
        .bind(&conversation_ids)
        .fetch_all(pool)
        .await
    {
        for row in rows {
            if let (Ok(id), Ok(external), Ok(kind)) = (
                row.try_get::<Uuid, _>("conversation_id"),
                row.try_get::<String, _>("external_id"),
                row.try_get::<String, _>("kind"),
            ) {
                let label = if kind == "group" {
                    format!("群 {external}")
                } else {
                    format!("会话 {external}")
                };
                conversation_labels.insert(id, label);
            }
        }
    }

    for node in nodes.iter_mut() {
        let Some(id) = node["scope_id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let Ok(uuid) = Uuid::parse_str(id) else {
            continue;
        };
        let label = match node["scope_kind"].as_str().unwrap_or_default() {
            "person" => person_labels.get(&uuid).cloned(),
            "conversation" => conversation_labels.get(&uuid).cloned(),
            _ => None,
        };
        if let Some(label) = label
            && let Some(object) = node.as_object_mut()
        {
            object.insert("scope_label".to_string(), json!(label));
        }
    }
}

/// 批量把人／会话 id 兑换成可读标签，供详情页使用。
async fn resolve_scope_label(pool: &PgPool, scope_kind: &str, scope_id: Option<&str>) -> String {
    let Some(scope_id) = scope_id.filter(|id| !id.is_empty()) else {
        return scope_label_fallback(scope_kind, None);
    };
    let Ok(uuid) = Uuid::parse_str(scope_id) else {
        return scope_label_fallback(scope_kind, Some(scope_id));
    };
    let (sql, table) = match scope_kind {
        "person" => (
            "SELECT COALESCE(min(external_id) FILTER (WHERE platform = 'qq'), '') AS label, \
             COALESCE(string_agg(platform || ':' || external_id, ', '), '') AS detail \
             FROM yunxi_external_identities WHERE person_id = $1",
            "person",
        ),
        "conversation" => (
            "SELECT COALESCE(min(external_id), '') AS label, COALESCE(min(kind), '') AS detail \
             FROM yunxi_external_conversations e \
             JOIN yunxi_conversations c ON c.id = e.conversation_id WHERE e.conversation_id = $1",
            "conversation",
        ),
        _ => return scope_label_fallback(scope_kind, Some(scope_id)),
    };
    let row = query(sql).bind(uuid).fetch_optional(pool).await;
    let Ok(Some(row)) = row else {
        return scope_label_fallback(scope_kind, Some(scope_id));
    };
    let label: String = row.try_get("label").unwrap_or_default();
    let detail: String = row.try_get("detail").unwrap_or_default();
    if label.is_empty() {
        return scope_label_fallback(scope_kind, Some(scope_id));
    }
    if table == "person" {
        format!("QQ {label}")
    } else if detail == "group" {
        format!("群 {label}")
    } else {
        format!("会话 {label}")
    }
}

/// 取一行并转成 JSON；表不存在或没有记录都返回 null。
async fn fetch_optional_json(pool: &PgPool, sql: &str, id: Uuid) -> Option<Value> {
    let row = query(sql).bind(id).fetch_optional(pool).await.ok()??;
    row.try_get::<Value, _>("row").ok()
}

/// 同上，但语句不带参数（单例表）。
async fn fetch_singleton_json(pool: &PgPool, sql: &str) -> Option<Value> {
    let row = query(sql).fetch_optional(pool).await.ok()??;
    row.try_get::<Value, _>("row").ok()
}

/// 概览页顶部需要的少量统计（状态接口复用）。
pub(crate) async fn counts() -> Value {
    let Ok(pool) = database_pool() else {
        return json!({ "available": false });
    };
    let Ok(existing) = existing_tables(pool).await else {
        return json!({ "available": false });
    };
    let mut counts = Map::new();
    let mut total = 0_i64;
    let mut by_label: HashMap<&str, i64> = HashMap::new();
    for kind in KINDS {
        if !existing.contains(kind.table) {
            continue;
        }
        if let Ok(count) = count_rows(pool, kind.table).await {
            total += count;
            by_label.insert(kind.key, count);
            counts.insert(kind.key.to_string(), json!(count));
        }
    }
    let people = if existing.contains("yunxi_persons") {
        count_rows(pool, "yunxi_persons").await.unwrap_or_default()
    } else {
        0
    };
    json!({
        "available": true,
        "total": total,
        "people": people,
        "by_kind": counts,
        "memories": by_label.get("memory").copied().unwrap_or_default(),
        "episodes": by_label.get("episode").copied().unwrap_or_default(),
        "goals": by_label.get("goal").copied().unwrap_or_default(),
        "open_loops": by_label.get("open_loop").copied().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_declares_a_usable_select() {
        for kind in KINDS {
            let select = kind_select(kind);
            assert!(select.contains(kind.table), "{}: {select}", kind.key);
            assert!(!kind.label.is_empty());
            // 表名来自静态常量；这里防的是将来有人把请求参数拼进去。
            assert!(
                kind.table
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '_')
            );
        }
    }

    fn all_tables() -> BTreeSet<String> {
        KINDS
            .iter()
            .map(|kind| kind.table.to_string())
            .chain(
                [
                    "yunxi_persons",
                    "yunxi_relations",
                    "yunxi_affect_states",
                    MIGRATION_LEDGER_TABLE,
                ]
                .into_iter()
                .map(str::to_string),
            )
            .collect()
    }

    /// 人物卡片的数字必须把旧版私有记忆算进去。只数 `yunxi_memories` 的 person
    /// 作用域会让它结构性恒为 0——生产上线后就是这么表现的（156 人全是 0）。
    #[test]
    fn people_count_covers_both_memory_stores() {
        let sql = people_sql(&all_tables());
        assert!(sql.contains("FROM yunxi_persons"), "要以人物为基准: {sql}");
        assert!(
            sql.contains("FROM yunxi_memories") && sql.contains("scope_kind = 'person'"),
            "Memory v2 的 person 作用域仍要计入: {sql}"
        );
        assert!(
            sql.contains("kovi_bot_memories") && sql.contains("scope_type = 'private'"),
            "旧版私有记忆必须计入，否则这个数字恒为 0: {sql}"
        );
        assert!(
            sql.contains("COALESCE(m.count, 0) + COALESCE(legacy.count, 0) AS memory_count"),
            "两个来源必须是相加关系: {sql}"
        );
        assert!(
            sql.contains("ORDER BY memory_count DESC"),
            "排序要跟着计数走: {sql}"
        );
    }

    #[test]
    fn people_sql_falls_back_when_the_legacy_table_is_absent() {
        let mut tables = all_tables();
        tables.remove("kovi_bot_memories");
        let sql = people_sql(&tables);
        assert!(
            !sql.contains("kovi_bot_memories"),
            "表不存在时不能引用它: {sql}"
        );
        assert!(sql.contains("legacy ON legacy.person_id = p.id"), "{sql}");
    }

    /// 同一条记忆只该数一次：旧表这一行若在 v2 已有副本（双写同 id，或 backfill
    /// 走 ledger），就不能再计一遍，否则 backfill 一跑数字直接翻倍。
    #[test]
    fn people_count_drops_legacy_rows_that_already_have_a_core_twin() {
        let sql = people_sql(&all_tables());
        assert!(
            sql.contains(
                "NOT EXISTS (SELECT 1 FROM yunxi_memories core WHERE core.id::text = memory.id)"
            ),
            "双写副本要按 Core 主键排掉: {sql}"
        );
        assert!(
            sql.contains(MIGRATION_LEDGER_TABLE)
                && sql.contains("item.legacy_id = memory.id")
                && sql.contains("JOIN yunxi_memories target ON target.id = item.target_id"),
            "backfill 副本要按 ledger + 目标行存在性排掉: {sql}"
        );

        let mut tables = all_tables();
        tables.remove(MIGRATION_LEDGER_TABLE);
        let sql = people_sql(&tables);
        assert!(
            !sql.contains(MIGRATION_LEDGER_TABLE),
            "账本表不存在时不能引用它: {sql}"
        );
        assert!(
            sql.contains("core.id::text = memory.id"),
            "没有账本也仍要排双写副本: {sql}"
        );
    }

    /// 旧版记忆是 QQ 作用域：按 person UUID 匹配一条都取不到，人物弹窗会长期是空的。
    #[test]
    fn person_detail_scopes_legacy_memories_by_qq_identity() {
        let legacy = kind_meta(LEGACY_MEMORY_KEY).expect("旧版记忆类型应存在");
        assert_eq!(legacy.table, "kovi_bot_memories");
        let filter = person_record_filter(legacy);
        assert!(
            filter.contains("'qq'") && filter.contains("ANY($1::text[])"),
            "{filter}"
        );

        let memory = kind_meta("memory").expect("长期记忆类型应存在");
        let filter = person_record_filter(memory);
        assert!(
            filter.contains("'person'") && filter.contains("= $1"),
            "{filter}"
        );
    }

    #[test]
    fn mind_kinds_share_the_payload_title_fallback() {
        let episode = kind_meta("episode").expect("episode 应存在");
        assert!(episode.title_expr.contains("payload->>'summary'"));
        assert_eq!(episode.table, "yunxi_episodes");
        assert!(episode.counted);
    }

    #[test]
    fn list_filters_are_applied_inside_the_subquery() {
        // 这条断言防的是一个真出现过的 bug：过滤条件写在外层，引用了子查询
        // 投影里没有的基表列（t.content），Postgres 直接报 column does not exist。
        for kind in KINDS {
            let sql = fetch_kind_sql(kind);
            let (inner, outer) = sql
                .split_once(") t \n         ORDER BY")
                .or_else(|| sql.split_once(") t ORDER BY"))
                .unwrap_or_else(|| panic!("{}: 生成的 SQL 结构不对: {sql}", kind.key));
            assert!(
                inner.contains(kind.search_expr),
                "{}: 搜索条件必须写在子查询内部",
                kind.key
            );
            assert!(
                !outer.contains(kind.table),
                "{}: 外层不能再引用基表",
                kind.key
            );
        }
    }

    #[test]
    fn link_types_come_from_real_relations() {
        let nodes = vec![
            json!({"id":"a","scope_kind":"person","scope_id":"p1",
                   "occurred_at":"2026-01-01T00:00:00Z","tags":["x"],"refs":[]}),
            json!({"id":"b","scope_kind":"person","scope_id":"p1",
                   "occurred_at":"2026-01-01T01:00:00Z","tags":["x"],"refs":[]}),
            json!({"id":"c","scope_kind":"global","scope_id":"",
                   "occurred_at":"2026-02-01T00:00:00Z","tags":[],"refs":["a"]}),
        ];
        let (links, counts) = build_links(&nodes);
        let types: Vec<&str> = links
            .iter()
            .map(|link| link["type"].as_str().unwrap_or_default())
            .collect();
        assert!(types.contains(&"entity"), "同一作用域应连实体边: {links:?}");
        assert!(types.contains(&"temporal"), "时间相邻应连时序边");
        assert!(types.contains(&"semantic"), "共享标签应连语义边");
        assert!(types.contains(&"causal"), "payload 引用应连因果边");
        assert_eq!(counts["causal"], json!(1));
        // 自己不该连自己。
        assert!(links.iter().all(|link| link["source"] != link["target"]));
    }

    #[test]
    fn tag_filters_match_any_listed_tag() {
        let node = json!({"tags": ["fact", "情节"]});
        assert!(node_has_tag(&node, &["情节".to_string()]));
        assert!(node_has_tag(
            &node,
            &["nope".to_string(), "fact".to_string()]
        ));
        assert!(!node_has_tag(&node, &["nope".to_string()]));
        assert_eq!(
            parse_tags(" a , ,b "),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn unknown_kinds_are_rejected() {
        assert!(kind_meta("nope").is_err());
        assert!(kind_meta("memory").is_ok());
    }

    #[test]
    fn scope_labels_never_render_empty() {
        assert_eq!(scope_label_fallback("global", None), "全局");
        assert_eq!(scope_label_fallback("", None), "全局");
        assert_eq!(scope_label_fallback("qq", Some("10001")), "QQ 10001");
        assert_eq!(
            scope_label_fallback("person", Some("12345678-aaaa-bbbb-cccc-ddddeeeeffff")),
            "人物 12345678"
        );
    }

    #[test]
    fn enum_objects_render_as_short_readable_titles() {
        let agenda = r#"{"type":"open_loop","value":"72079197-a014-4538-89f4-000000000000"}"#;
        assert_eq!(readable_title(agenda, "（兜底）"), "open_loop 72079197");
        let labelled = r#"{"label":"周末计划","type":"x"}"#;
        assert_eq!(readable_title(labelled, "（兜底）"), "周末计划");
        assert_eq!(readable_title("  ", "（无标题）"), "（无标题）");
        assert_eq!(readable_title("普通正文", "（无标题）"), "普通正文");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        let text = "芸汐".repeat(10);
        let cut = truncate(&text, 3);
        assert_eq!(cut, "芸汐芸…");
        assert_eq!(truncate("短", 3), "短");
    }

    #[test]
    fn limits_are_clamped_to_a_bounded_page() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT);
    }

    #[test]
    fn kind_keys_are_unique() {
        let mut keys: Vec<&str> = KINDS.iter().map(|kind| kind.key).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "kind key 必须唯一");
    }
}
