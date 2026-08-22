//! # 表情包记忆库
//!
//! 仅保存 OneBot 表情/图片的稳定标识与人工教会的含义；不下载或保存图片文件。

use crate::memory::MEMORY_MANAGER;
use crate::vision::{ImageAttachment, extract_image_attachments};
use anyhow::{Result, anyhow};
use kovi::bot::message::Segment;
use kovi::{Message, RuntimeBot};
use serde_json::{Map, Value};
use sqlx_core::query::query;
use sqlx_core::row::Row;
use std::collections::{HashMap, HashSet};

const TEACH_COMMANDS: [&str; 2] = ["#教芸汐", "#教云汐"];
const MAX_LABEL_CHARS: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StickerImage {
    key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StickerScope {
    Group(i64),
    Private(i64),
}

impl StickerScope {
    fn database_values(self) -> (&'static str, i64) {
        match self {
            Self::Group(group_id) => ("group", group_id),
            Self::Private(user_id) => ("private", user_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuotedMessageContext {
    pub(crate) content: String,
    pub(crate) message_id: Option<i32>,
    pub(crate) sender_id: Option<i64>,
    pub(crate) sender_label: Option<String>,
    pub(crate) images: Vec<ImageAttachment>,
}

#[derive(Debug)]
struct FetchedMessage {
    message: Message,
    message_id: i32,
    sender_id: Option<i64>,
    sender_label: Option<String>,
}

/// 创建独立表情包表。该表不属于 JSON 记忆快照，因此可单独查询和更新。
pub(crate) async fn initialize_database() -> Result<()> {
    let pool = MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 记忆连接池尚未初始化"))?;

    query(
        r#"
        CREATE TABLE IF NOT EXISTS kovi_bot_sticker_memory (
            sticker_key TEXT NOT NULL,
            scope_type TEXT NOT NULL DEFAULT 'global',
            scope_id BIGINT NOT NULL DEFAULT 0,
            label TEXT NOT NULL,
            learned_by BIGINT NOT NULL,
            learned_in_group BIGINT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (sticker_key, scope_type, scope_id)
        )
        "#,
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("创建表情包记忆表失败: {error}"))?;

    // 兼容第一版只有 sticker_key 主键的表：旧标签迁移为全局默认值。
    query(
        "ALTER TABLE kovi_bot_sticker_memory ADD COLUMN IF NOT EXISTS scope_type TEXT NOT NULL DEFAULT 'global'",
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("迁移表情包作用域失败: {error}"))?;
    query(
        "ALTER TABLE kovi_bot_sticker_memory ADD COLUMN IF NOT EXISTS scope_id BIGINT NOT NULL DEFAULT 0",
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("迁移表情包作用域失败: {error}"))?;

    let primary_key_columns = query(
        r#"
        SELECT a.attname AS column_name
        FROM pg_index i
        JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
        WHERE i.indrelid = 'kovi_bot_sticker_memory'::regclass AND i.indisprimary
        ORDER BY array_position(i.indkey, a.attnum)
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("读取表情包主键失败: {error}"))?
    .into_iter()
    .map(|row| row.get::<String, _>("column_name"))
    .collect::<Vec<_>>();
    if primary_key_columns == ["sticker_key"] {
        let mut transaction = pool.begin().await?;
        query("ALTER TABLE kovi_bot_sticker_memory DROP CONSTRAINT kovi_bot_sticker_memory_pkey")
            .execute(&mut *transaction)
            .await?;
        query(
            "ALTER TABLE kovi_bot_sticker_memory ADD CONSTRAINT kovi_bot_sticker_memory_pkey PRIMARY KEY (sticker_key, scope_type, scope_id)",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
    }

    query(
        "CREATE INDEX IF NOT EXISTS kovi_bot_sticker_memory_updated_at_idx ON kovi_bot_sticker_memory (updated_at DESC)",
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("创建表情包记忆索引失败: {error}"))?;

    println!("[INFO] PostgreSQL 表情包记忆库已就绪");
    Ok(())
}

/// 从 OneBot 消息段中提取可长期识别的图片、商城表情和普通表情标识。
pub(crate) fn extract_stickers(message: &Message) -> Vec<StickerImage> {
    let mut seen = HashSet::new();
    message
        .iter()
        .filter_map(|segment| {
            let kind = segment.type_.as_str();
            if !matches!(kind, "image" | "mface" | "face") {
                return None;
            }

            let identifier = match kind {
                "face" => value_as_identifier(&segment.data, &["id"]),
                "mface" => value_as_identifier(
                    &segment.data,
                    &["emoji_id", "emoji_package_id", "summary", "url"],
                ),
                _ => value_as_identifier(
                    &segment.data,
                    &["file_unique", "md5", "file_id", "file", "url"],
                ),
            }?;
            let key = format!("{}:{}", kind, identifier);
            seen.insert(key.clone()).then_some(StickerImage { key })
        })
        .collect()
}

/// 从引用消息段中读取原消息 ID。
fn reply_message_id(message: &Message) -> Option<i32> {
    message.iter().find_map(|segment| {
        if segment.type_ != "reply" {
            return None;
        }

        segment
            .data
            .get("id")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            })
            .and_then(|value| i32::try_from(value).ok())
    })
}

pub(crate) fn has_reply(message: &Message) -> bool {
    reply_message_id(message).is_some()
}

async fn fetch_replied_message(
    message: &Message,
    bot: &RuntimeBot,
    scope: StickerScope,
) -> Result<Option<FetchedMessage>> {
    let Some(message_id) = reply_message_id(message) else {
        return Ok(None);
    };
    let response = bot
        .get_msg(message_id)
        .await
        .map_err(|response| anyhow!("读取被引用消息失败: {}", response.retcode))?;
    validate_fetched_message_scope(&response.data, message_id, scope)?;
    let original_message = response
        .data
        .get("message")
        .cloned()
        .ok_or_else(|| anyhow!("被引用消息缺少消息内容"))?;
    let original_message = message_from_onebot_value(original_message)?;
    let sender_id = response
        .data
        .pointer("/sender/user_id")
        .and_then(value_as_i64)
        .or_else(|| response.data.get("user_id").and_then(value_as_i64));
    let sender_label = quoted_sender_label(&response.data);

    Ok(Some(FetchedMessage {
        message: original_message,
        message_id,
        sender_id,
        sender_label,
    }))
}

fn validate_fetched_message_scope(
    data: &Value,
    requested_message_id: i32,
    scope: StickerScope,
) -> Result<()> {
    let returned_message_id = data
        .get("message_id")
        .and_then(value_as_i64)
        .ok_or_else(|| anyhow!("被引用消息缺少可验证的消息 ID"))?;
    if returned_message_id != i64::from(requested_message_id) {
        return Err(anyhow!("被引用消息 ID 与请求不一致"));
    }

    let message_type = data
        .get("message_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or_else(|| anyhow!("被引用消息缺少可验证的会话类型"))?;
    match scope {
        StickerScope::Group(expected_group_id) => {
            if message_type != "group" {
                return Err(anyhow!("被引用消息不属于当前群聊"));
            }
            let group_id = data
                .get("group_id")
                .and_then(value_as_i64)
                .or_else(|| data.get("target_id").and_then(value_as_i64))
                .or_else(|| data.get("peer_id").and_then(value_as_i64))
                .ok_or_else(|| anyhow!("被引用群消息缺少可验证的群号"))?;
            if group_id != expected_group_id {
                return Err(anyhow!("被引用消息来自其他群聊"));
            }
        }
        StickerScope::Private(expected_user_id) => {
            if message_type != "private" {
                return Err(anyhow!("被引用消息不属于当前私聊"));
            }
            if data
                .get("group_id")
                .and_then(value_as_i64)
                .is_some_and(|group_id| group_id != 0)
            {
                return Err(anyhow!("被引用消息带有其他群聊作用域"));
            }
            let belongs_to_peer = [
                data.get("user_id").and_then(value_as_i64),
                data.get("target_id").and_then(value_as_i64),
                data.get("peer_id").and_then(value_as_i64),
                data.pointer("/sender/user_id").and_then(value_as_i64),
            ]
            .into_iter()
            .flatten()
            .any(|user_id| user_id == expected_user_id);
            if !belongs_to_peer {
                return Err(anyhow!("被引用消息来自其他私聊"));
            }
        }
    }
    Ok(())
}

fn message_from_onebot_value(value: Value) -> Result<Message> {
    match value {
        Value::Array(_) => {
            Message::from_value(value).map_err(|error| anyhow!("解析被引用消息失败: {error}"))
        }
        Value::String(cq_message) => Ok(parse_cq_message(&cq_message)),
        _ => Err(anyhow!("被引用消息格式不受支持")),
    }
}

/// 兼容 OneBot `get_msg` 可能返回的 CQ 字符串格式。
fn parse_cq_message(input: &str) -> Message {
    let mut segments = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find("[CQ:") {
        if start > 0 {
            segments.push(Segment::new(
                "text",
                serde_json::json!({"text": decode_cq(&rest[..start])}),
            ));
        }
        let after_start = &rest[start + 4..];
        let Some(end) = after_start.find(']') else {
            segments.push(Segment::new(
                "text",
                serde_json::json!({"text": decode_cq(&rest[start..])}),
            ));
            rest = "";
            break;
        };

        let mut fields = after_start[..end].split(',');
        let kind = fields.next().unwrap_or("text");
        let mut data = Map::new();
        for field in fields {
            if let Some((key, value)) = field.split_once('=') {
                data.insert(key.to_string(), Value::String(decode_cq(value)));
            }
        }
        segments.push(Segment::new(kind, Value::Object(data)));
        rest = &after_start[end + 1..];
    }

    if !rest.is_empty() {
        segments.push(Segment::new(
            "text",
            serde_json::json!({"text": decode_cq(rest)}),
        ));
    }
    Message::from(segments)
}

fn decode_cq(value: &str) -> String {
    value
        .replace("&#44;", ",")
        .replace("&#91;", "[")
        .replace("&#93;", "]")
        .replace("&amp;", "&")
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn quoted_sender_label(value: &Value) -> Option<String> {
    let sender = value.get("sender").and_then(Value::as_object)?;
    let user_id = sender.get("user_id").and_then(value_as_i64);
    let nickname = sender
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let card = sender
        .get("card")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if user_id.is_none() && nickname.is_none() && card.is_none() {
        return None;
    }
    Some(format!(
        "群名片={}；QQ昵称={}；QQ号={}",
        card.unwrap_or("未设置"),
        nickname.unwrap_or("未设置"),
        user_id
            .map(|value| value.to_string())
            .unwrap_or_else(|| "未知".to_string())
    ))
}

fn extract_text(message: &Message) -> String {
    message
        .iter()
        .filter(|segment| segment.type_ == "text")
        .filter_map(|segment| segment.data.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string()
}

/// 将被引用消息的文字和已学习表情含义整理成模型可理解的上下文。
pub(crate) async fn quoted_message_context(
    message: &Message,
    bot: &RuntimeBot,
    scope: StickerScope,
) -> Result<Option<QuotedMessageContext>> {
    let Some(quoted) = fetch_replied_message(message, bot, scope).await? else {
        return Ok(None);
    };
    let text = extract_text(&quoted.message);
    let stickers = extract_stickers(&quoted.message);
    let images = extract_image_attachments(&quoted.message);
    let labels = known_labels(&stickers, scope).await?;
    let content = if !labels.is_empty() {
        with_sticker_context(&text, &labels)
    } else if !text.is_empty() {
        text
    } else if !stickers.is_empty() {
        "对方发送了一张尚未学习含义的表情包。".to_string()
    } else {
        quoted.message.to_human_string()
    };

    Ok(Some(QuotedMessageContext {
        content,
        message_id: Some(quoted.message_id),
        sender_id: quoted.sender_id,
        sender_label: quoted.sender_label,
        images,
    }))
}

pub(crate) fn with_quoted_context(current: &str, quoted: &QuotedMessageContext) -> String {
    let current = current.trim();
    let quoted_label = quoted
        .message_id
        .map(|message_id| format!("消息ID：{}\n", message_id))
        .unwrap_or_default();
    let sender_label = quoted
        .sender_label
        .as_deref()
        .map(|label| format!("发送者身份：{}\n", label))
        .unwrap_or_default();
    if current.is_empty() {
        format!(
            "当前消息正在回复以下内容：\n{}{}{}",
            sender_label, quoted_label, quoted.content
        )
    } else {
        format!(
            "当前消息正在回复以下内容：\n{}{}{}\n当前消息：{}",
            sender_label, quoted_label, quoted.content, current
        )
    }
}

/// 教学时优先使用当前消息携带的表情；如果没有，则读取被引用消息中的表情。
pub(crate) async fn stickers_for_teaching(
    message: &Message,
    bot: &RuntimeBot,
    scope: StickerScope,
) -> Result<Vec<StickerImage>> {
    let stickers = extract_stickers(message);
    if !stickers.is_empty() {
        return Ok(stickers);
    }

    let original_message = fetch_replied_message(message, bot, scope)
        .await?
        .ok_or_else(|| anyhow!("教学消息没有携带或引用表情包"))?;
    Ok(extract_stickers(&original_message.message))
}

fn value_as_identifier(data: &serde_json::Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        let value = data.get(*field)?;
        value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .or_else(|| value.as_i64().map(|value| value.to_string()))
    })
}

/// 解析 `#教芸汐 这个表情是无语又想笑` 形式的人工标签。
pub(crate) fn teaching_label(message: &str) -> Option<String> {
    let text = message.trim();
    let remainder = TEACH_COMMANDS
        .iter()
        .find_map(|command| text.strip_prefix(command))?;
    let remainder = remainder.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, '：' | ':' | '，' | ',')
    });
    let label = remainder
        .strip_prefix("这个表情是")
        .or_else(|| remainder.strip_prefix("这个表情"))
        .unwrap_or(remainder)
        .trim()
        .trim_matches(|character: char| matches!(character, '。' | '！' | '!' | '，' | ','));

    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        None
    } else {
        Some(label.to_string())
    }
}

pub(crate) async fn teach(
    stickers: &[StickerImage],
    label: &str,
    learned_by: i64,
    scope: StickerScope,
) -> Result<usize> {
    let pool = MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 表情包记忆库尚未初始化"))?;

    let (scope_type, scope_id) = scope.database_values();
    let learned_in_group = match scope {
        StickerScope::Group(group_id) => Some(group_id),
        StickerScope::Private(_) => None,
    };
    let mut transaction = pool.begin().await?;
    for sticker in stickers {
        query(
            r#"
            INSERT INTO kovi_bot_sticker_memory
                (sticker_key, scope_type, scope_id, label, learned_by, learned_in_group, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())
            ON CONFLICT (sticker_key, scope_type, scope_id) DO UPDATE
            SET label = EXCLUDED.label,
                learned_by = EXCLUDED.learned_by,
                learned_in_group = EXCLUDED.learned_in_group,
                updated_at = NOW()
            "#,
        )
        .bind(&sticker.key)
        .bind(scope_type)
        .bind(scope_id)
        .bind(label)
        .bind(learned_by)
        .bind(learned_in_group)
        .execute(&mut *transaction)
        .await
        .map_err(|error| anyhow!("保存表情包记忆失败: {error}"))?;
    }
    transaction.commit().await?;
    Ok(stickers.len())
}

/// 删除与指定用户直接关联的表情教学数据，包括教学者身份和私聊作用域标签。
pub(crate) async fn delete_user_data(user_id: i64) -> Result<u64> {
    let pool = MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 表情包记忆库尚未初始化"))?;
    let result = query(
        r#"
        DELETE FROM kovi_bot_sticker_memory
        WHERE learned_by = $1
           OR (scope_type = 'private' AND scope_id = $1)
        "#,
    )
    .bind(user_id)
    .execute(pool)
    .await
    .map_err(|error| anyhow!("删除用户表情包记忆失败: {error}"))?;
    Ok(result.rows_affected())
}

/// 删除指定群聊作用域及来源于该群的表情教学数据。
pub(crate) async fn delete_group_data(group_id: i64) -> Result<u64> {
    let pool = MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 表情包记忆库尚未初始化"))?;
    let result = query(
        r#"
        DELETE FROM kovi_bot_sticker_memory
        WHERE (scope_type = 'group' AND scope_id = $1)
           OR learned_in_group = $1
        "#,
    )
    .bind(group_id)
    .execute(pool)
    .await
    .map_err(|error| anyhow!("删除群聊表情包记忆失败: {error}"))?;
    Ok(result.rows_affected())
}

/// 返回消息中已学习表情的含义；没有标签的图片不会进入模型上下文。
pub(crate) async fn known_labels(
    stickers: &[StickerImage],
    scope: StickerScope,
) -> Result<Vec<String>> {
    if stickers.is_empty() {
        return Ok(Vec::new());
    }
    let pool = MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 表情包记忆库尚未初始化"))?;
    let mut labels = Vec::new();
    let mut seen = HashSet::new();
    let keys = stickers
        .iter()
        .map(|sticker| sticker.key.clone())
        .collect::<Vec<_>>();
    let (scope_type, scope_id) = scope.database_values();
    let rows = query(
        r#"
        SELECT sticker_key, label,
               CASE WHEN scope_type = $2 AND scope_id = $3 THEN 0 ELSE 1 END AS priority
        FROM kovi_bot_sticker_memory
        WHERE sticker_key = ANY($1::TEXT[])
          AND ((scope_type = $2 AND scope_id = $3) OR (scope_type = 'global' AND scope_id = 0))
        ORDER BY priority ASC, updated_at DESC
        "#,
    )
    .bind(&keys)
    .bind(scope_type)
    .bind(scope_id)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("读取表情包记忆失败: {error}"))?;
    let mut label_by_key = HashMap::new();
    for row in rows {
        label_by_key
            .entry(row.get::<String, _>("sticker_key"))
            .or_insert_with(|| row.get::<String, _>("label"));
    }
    for sticker in stickers {
        if let Some(label) = label_by_key.get(&sticker.key)
            && seen.insert(label.clone())
        {
            labels.push(label.clone());
        }
    }
    Ok(labels)
}

/// 仅把已经人工教会的表情翻译为模型可理解的自然语言上下文。
pub(crate) fn with_sticker_context(text: &str, labels: &[String]) -> String {
    let message = text.trim();
    if labels.is_empty() {
        return message.to_string();
    }

    let description = format!("附带的已学习表情含义：{}。", labels.join("；"));
    if message.is_empty() {
        format!("对方发送了一个表情包。{description}")
    } else {
        format!("{message}\n{description}")
    }
}

/// 告知模型当前带有尚未学习的表情，但不臆造它的具体含义。
pub(crate) fn with_unknown_sticker_context(text: &str, count: usize) -> String {
    let message = text.trim();
    let description = if count == 1 {
        "附带了一个芸汐还没学会理解的表情包；不要擅自猜它的具体含义，可以自然地承认还没看懂。"
    } else {
        "附带了几个芸汐还没学会理解的表情包；不要擅自猜它们的具体含义，可以自然地承认还没看懂。"
    };
    if message.is_empty() {
        description.to_string()
    } else {
        format!("{message}\n{description}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QuotedMessageContext, StickerImage, StickerScope, extract_stickers, extract_text,
        message_from_onebot_value, reply_message_id, teaching_label,
        validate_fetched_message_scope, with_quoted_context, with_sticker_context,
        with_unknown_sticker_context,
    };
    use kovi::Message;
    use kovi::bot::message::Segment;
    use serde_json::json;

    #[test]
    fn extracts_stable_image_and_face_keys() {
        let message = Message::from(vec![
            Segment::new(
                "image",
                json!({"file_unique": "image-123", "url": "ignored"}),
            ),
            Segment::new("face", json!({"id": 14})),
        ]);
        assert_eq!(
            extract_stickers(&message),
            vec![
                StickerImage {
                    key: "image:image-123".to_string()
                },
                StickerImage {
                    key: "face:14".to_string()
                }
            ]
        );
    }

    #[test]
    fn parses_teaching_command_and_keeps_regular_chat_out() {
        assert_eq!(
            teaching_label("#教芸汐 这个表情是无语又想笑。"),
            Some("无语又想笑".to_string())
        );
        assert_eq!(teaching_label("#教云汐：委屈"), Some("委屈".to_string()));
        assert_eq!(teaching_label("这个表情是无语"), None);
    }

    #[test]
    fn extracts_quoted_message_id_from_string_or_number() {
        let string_id = Message::from(vec![Segment::new("reply", json!({"id": "12345"}))]);
        let number_id = Message::from(vec![Segment::new("reply", json!({"id": 67890}))]);
        assert_eq!(reply_message_id(&string_id), Some(12345));
        assert_eq!(reply_message_id(&number_id), Some(67890));
    }

    #[test]
    fn parses_cq_string_returned_by_get_msg() {
        let message = message_from_onebot_value(json!(
            "前文[CQ:image,file=sticker-123,url=https://example.com/a&#44;b]后文"
        ))
        .unwrap();
        assert_eq!(extract_text(&message), "前文后文");
        assert_eq!(
            extract_stickers(&message),
            vec![StickerImage {
                key: "image:sticker-123".to_string()
            }]
        );
    }

    #[test]
    fn accepts_only_quoted_messages_from_the_current_group() {
        let valid = json!({
            "message_id": 42,
            "message_type": "group",
            "group_id": 1001,
        });
        assert!(validate_fetched_message_scope(&valid, 42, StickerScope::Group(1001)).is_ok());

        let wrong_group = json!({
            "message_id": 42,
            "message_type": "group",
            "group_id": 2002,
        });
        assert!(
            validate_fetched_message_scope(&wrong_group, 42, StickerScope::Group(1001)).is_err()
        );
        assert!(validate_fetched_message_scope(&valid, 41, StickerScope::Group(1001)).is_err());
    }

    #[test]
    fn accepts_incoming_or_outgoing_messages_only_for_the_current_private_peer() {
        let incoming = json!({
            "message_id": "42",
            "message_type": "private",
            "user_id": "1001",
            "sender": {"user_id": "1001"},
        });
        let outgoing = json!({
            "message_id": 42,
            "message_type": "private",
            "user_id": 9999,
            "target_id": 1001,
            "sender": {"user_id": 9999},
        });
        assert!(validate_fetched_message_scope(&incoming, 42, StickerScope::Private(1001)).is_ok());
        assert!(validate_fetched_message_scope(&outgoing, 42, StickerScope::Private(1001)).is_ok());
        assert!(
            validate_fetched_message_scope(&incoming, 42, StickerScope::Private(2002)).is_err()
        );
    }

    #[test]
    fn rejects_quoted_messages_without_verifiable_scope() {
        let missing_scope = json!({
            "message_id": 42,
            "message_type": "group",
        });
        assert!(
            validate_fetched_message_scope(&missing_scope, 42, StickerScope::Group(1001)).is_err()
        );

        let group_message_disguised_as_private = json!({
            "message_id": 42,
            "message_type": "private",
            "user_id": 1001,
            "group_id": 2002,
        });
        assert!(
            validate_fetched_message_scope(
                &group_message_disguised_as_private,
                42,
                StickerScope::Private(1001),
            )
            .is_err()
        );
    }

    #[test]
    fn quoted_context_is_explicit_for_the_model() {
        let quoted = QuotedMessageContext {
            content: "上一句话".to_string(),
            message_id: Some(41),
            sender_id: Some(42),
            sender_label: None,
            images: Vec::new(),
        };
        assert_eq!(
            with_quoted_context("你说得对", &quoted),
            "当前消息正在回复以下内容：\n消息ID：41\n上一句话\n当前消息：你说得对"
        );
    }

    #[test]
    fn quoted_group_sender_keeps_card_and_qq_nickname() {
        assert_eq!(
            super::quoted_sender_label(&json!({
                "sender": {
                    "user_id": 42,
                    "nickname": "QQ用户名",
                    "card": "群内昵称"
                }
            }))
            .as_deref(),
            Some("群名片=群内昵称；QQ昵称=QQ用户名；QQ号=42")
        );
    }

    #[test]
    fn adds_only_known_sticker_meanings_to_context() {
        assert_eq!(
            with_sticker_context("", &["无语又想笑".to_string()]),
            "对方发送了一个表情包。附带的已学习表情含义：无语又想笑。"
        );
        assert_eq!(with_sticker_context("你好", &[]), "你好");
    }

    #[test]
    fn unknown_sticker_context_does_not_invent_a_meaning() {
        let context = with_unknown_sticker_context("这个怎么样", 1);
        assert!(context.contains("还没学会理解"));
        assert!(context.contains("不要擅自猜"));
        assert!(with_unknown_sticker_context("", 2).contains("几个"));
    }
}
