//! 回复生命周期决策。
//!
//! 这里仅描述本轮是否发送可见正文；引用、@、撤回和分段由各自的消息动作处理。

pub(crate) const SILENT_REPLY_OUTPUT: &str =
    r#"[[REPLY_ACTION]]{"disposition":"silent"}[[/REPLY_ACTION]]"#;

const LEGACY_SILENCE_MARKER: &str = "[sp]";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ReplyDisposition {
    #[default]
    Reply,
    Silent,
}

impl ReplyDisposition {
    pub(crate) fn from_protocol(value: &str) -> Option<Self> {
        match value {
            "reply" => Some(Self::Reply),
            "silent" => Some(Self::Silent),
            _ => None,
        }
    }

    pub(crate) fn is_silent(self) -> bool {
        matches!(self, Self::Silent)
    }
}

/// 将旧版精确 `[sp]` 输出迁移为结构化静默决策，并清除静默轮次的可见正文。
pub(crate) fn normalize_reply_disposition(
    mut disposition: ReplyDisposition,
    content: String,
) -> (ReplyDisposition, String) {
    if content.trim() == LEGACY_SILENCE_MARKER {
        disposition = ReplyDisposition::Silent;
    }
    if disposition.is_silent() {
        (disposition, String::new())
    } else {
        (disposition, content)
    }
}

#[cfg(test)]
mod tests {
    use super::{ReplyDisposition, normalize_reply_disposition};

    #[test]
    fn only_the_exact_legacy_marker_becomes_silent() {
        assert_eq!(
            normalize_reply_disposition(ReplyDisposition::Reply, " [sp] \n".to_string()),
            (ReplyDisposition::Silent, String::new())
        );
        assert_eq!(
            normalize_reply_disposition(ReplyDisposition::Reply, "不要回复[sp]".to_string()),
            (ReplyDisposition::Reply, "不要回复[sp]".to_string())
        );
    }

    #[test]
    fn structured_silence_discards_conflicting_visible_content() {
        assert_eq!(
            normalize_reply_disposition(ReplyDisposition::Silent, "这段文字不应发送".to_string()),
            (ReplyDisposition::Silent, String::new())
        );
    }
}
