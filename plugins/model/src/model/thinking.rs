use crate::model::interrupt::{ReplyTicket, is_current};
use crate::model::message_actions::{MessageDestination, send_tracked_reply_text};
use kovi::RuntimeBot;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

pub(crate) const THINKING_NOTICE_START: &str = "[[THINKING_NOTICE]]";
pub(crate) const THINKING_NOTICE_END: &str = "[[/THINKING_NOTICE]]";

const MAX_NOTICE_CHARS: usize = 80;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ThinkingDestination {
    Group(i64),
    Private(i64),
}

#[derive(Debug, Clone, Copy)]
struct ThinkingEstimate {
    should_notify: bool,
}

pub(crate) struct ThinkingReporter {
    bot: Arc<RuntimeBot>,
    destination: ThinkingDestination,
    ticket: ReplyTicket,
    estimate: ThinkingEstimate,
    notice_sent: AtomicBool,
}

static THINKING_PROTOCOL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "如果这次任务确实需要较长时间，先输出一句像聊天里随口说的短状态，再输出最终回答。状态提示必须放在 {start} 和 {end} 之间；简单问题不要输出状态提示。不要承诺精确秒数，不要解释技术机制，不要像客服或系统进度提示，也不要把状态提示写进最终回答。",
        start = THINKING_NOTICE_START,
        end = THINKING_NOTICE_END,
    )
});

impl ThinkingReporter {
    pub(crate) fn new(
        bot: Arc<RuntimeBot>,
        destination: ThinkingDestination,
        ticket: ReplyTicket,
        message: &str,
        image_count: usize,
        supports_vision: bool,
        history_len: usize,
    ) -> Arc<Self> {
        let estimate = estimate_thinking(message, image_count, supports_vision, history_len);
        Arc::new(Self {
            bot,
            destination,
            ticket,
            estimate,
            notice_sent: AtomicBool::new(false),
        })
    }

    pub(crate) fn protocol() -> &'static str {
        THINKING_PROTOCOL.as_str()
    }

    pub(crate) async fn observe_model_output(&self, output: &str) {
        if let Some(notice) = extract_first_thinking_notice(output) {
            self.send_notice(notice).await;
        }
    }

    async fn send_notice(&self, notice: impl AsRef<str>) {
        if !self.estimate.should_notify || self.notice_sent.swap(true, Ordering::AcqRel) {
            return;
        }
        if !is_current(self.ticket).await {
            self.notice_sent.store(false, Ordering::Release);
            return;
        }
        let notice = sanitize_notice(notice.as_ref());
        if notice.is_empty() {
            self.notice_sent.store(false, Ordering::Release);
            return;
        }
        let destination = match self.destination {
            ThinkingDestination::Group(group_id) => MessageDestination::Group(group_id),
            ThinkingDestination::Private(user_id) => MessageDestination::Private(user_id),
        };
        if !send_tracked_reply_text(&self.bot, destination, &notice, self.ticket).await {
            self.notice_sent.store(false, Ordering::Release);
        }
    }
}

fn estimate_thinking(
    message: &str,
    image_count: usize,
    supports_vision: bool,
    history_len: usize,
) -> ThinkingEstimate {
    let text = message.trim();
    let mut score = 0_u8;
    if image_count > 0 {
        score = score.saturating_add(3 + image_count.min(3) as u8);
        if !supports_vision {
            score = score.saturating_add(2);
        }
    }
    if text.chars().count() > 180 {
        score = score.saturating_add(2);
    } else if text.chars().count() > 90 {
        score = score.saturating_add(1);
    }
    if history_len > 36 {
        score = score.saturating_add(2);
    } else if history_len > 22 {
        score = score.saturating_add(1);
    }
    ThinkingEstimate {
        should_notify: score >= 3,
    }
}

pub(crate) fn extract_first_thinking_notice(content: &str) -> Option<String> {
    let start = content.find(THINKING_NOTICE_START)? + THINKING_NOTICE_START.len();
    let end = content[start..].find(THINKING_NOTICE_END)? + start;
    Some(content[start..end].trim().to_string())
}

pub(crate) fn strip_thinking_notices(content: &str) -> String {
    let mut clean = content.to_string();
    let mut cursor = 0;
    while let Some(relative_start) = clean[cursor..].find(THINKING_NOTICE_START) {
        let start = cursor + relative_start;
        let body_start = start + THINKING_NOTICE_START.len();
        let Some(relative_end) = clean[body_start..].find(THINKING_NOTICE_END) else {
            clean.replace_range(start.., "");
            break;
        };
        let end = body_start + relative_end;
        clean.replace_range(start..end + THINKING_NOTICE_END.len(), "");
        cursor = start;
    }
    clean.trim().to_string()
}

fn sanitize_notice(notice: &str) -> String {
    let notice = notice
        .replace(THINKING_NOTICE_START, "")
        .replace(THINKING_NOTICE_END, "")
        .replace('\n', " ");
    let notice = notice.trim();
    if notice.is_empty() {
        return String::new();
    }
    truncate_chars(notice, MAX_NOTICE_CHARS)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::{extract_first_thinking_notice, strip_thinking_notices};

    #[test]
    fn extracts_and_removes_thinking_notice_markers() {
        let content = "[[THINKING_NOTICE]]我先理一下。[[/THINKING_NOTICE]]最终答案";
        assert_eq!(
            extract_first_thinking_notice(content).as_deref(),
            Some("我先理一下。")
        );
        assert_eq!(strip_thinking_notices(content), "最终答案");
    }
}
