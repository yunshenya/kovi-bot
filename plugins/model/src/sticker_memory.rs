//! # 表情包记忆库
//!
//! 仅保存 OneBot 表情/图片的稳定标识与人工教会的含义；不下载或保存图片文件。

use crate::memory::MEMORY_MANAGER;
use anyhow::{Result, anyhow};
use kovi::Message;
use sqlx::Row;
use std::collections::HashSet;

const TEACH_COMMANDS: [&str; 2] = ["#教芸汐", "#教云汐"];
const MAX_LABEL_CHARS: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StickerImage {
    key: String,
}

/// 创建独立表情包表。该表不属于 JSON 记忆快照，因此可单独查询和更新。
pub(crate) async fn initialize_database() -> Result<()> {
    let pool = MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 记忆连接池尚未初始化"))?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kovi_bot_sticker_memory (
            sticker_key TEXT PRIMARY KEY,
            label TEXT NOT NULL,
            learned_by BIGINT NOT NULL,
            learned_in_group BIGINT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("创建表情包记忆表失败: {error}"))?;

    sqlx::query(
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
    learned_in_group: Option<i64>,
) -> Result<usize> {
    let pool = MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 表情包记忆库尚未初始化"))?;

    for sticker in stickers {
        sqlx::query(
            r#"
            INSERT INTO kovi_bot_sticker_memory
                (sticker_key, label, learned_by, learned_in_group, created_at, updated_at)
            VALUES ($1, $2, $3, $4, NOW(), NOW())
            ON CONFLICT (sticker_key) DO UPDATE
            SET label = EXCLUDED.label,
                learned_by = EXCLUDED.learned_by,
                learned_in_group = EXCLUDED.learned_in_group,
                updated_at = NOW()
            "#,
        )
        .bind(&sticker.key)
        .bind(label)
        .bind(learned_by)
        .bind(learned_in_group)
        .execute(pool)
        .await
        .map_err(|error| anyhow!("保存表情包记忆失败: {error}"))?;
    }
    Ok(stickers.len())
}

/// 返回消息中已学习表情的含义；没有标签的图片不会进入模型上下文。
pub(crate) async fn known_labels(stickers: &[StickerImage]) -> Result<Vec<String>> {
    let pool = MEMORY_MANAGER
        .database_pool()
        .ok_or_else(|| anyhow!("PostgreSQL 表情包记忆库尚未初始化"))?;
    let mut labels = Vec::new();
    let mut seen = HashSet::new();

    for sticker in stickers {
        let label = sqlx::query("SELECT label FROM kovi_bot_sticker_memory WHERE sticker_key = $1")
            .bind(&sticker.key)
            .fetch_optional(pool)
            .await
            .map_err(|error| anyhow!("读取表情包记忆失败: {error}"))?
            .map(|row| row.get::<String, _>("label"));
        if let Some(label) = label
            && seen.insert(label.clone())
        {
            labels.push(label);
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

#[cfg(test)]
mod tests {
    use super::{StickerImage, extract_stickers, teaching_label, with_sticker_context};
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
    fn adds_only_known_sticker_meanings_to_context() {
        assert_eq!(
            with_sticker_context("", &["无语又想笑".to_string()]),
            "对方发送了一个表情包。附带的已学习表情含义：无语又想笑。"
        );
        assert_eq!(with_sticker_context("你好", &[]), "你好");
    }
}
