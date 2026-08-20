use crate::model::interrupt::{ReplyTicket, is_current};
use crate::model::recall::record_bot_message;
use kovi::RuntimeBot;
use kovi::tokio::sync::Mutex;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

pub(crate) const THINKING_NOTICE_START: &str = "[[THINKING_NOTICE]]";
pub(crate) const THINKING_NOTICE_END: &str = "[[/THINKING_NOTICE]]";

const MAX_NOTICE_CHARS: usize = 80;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ThinkingDestination {
    Group(i64),
    Private(i64),
}

#[derive(Debug, Clone, Copy)]
enum ThinkingKind {
    Image,
    Complex,
    LongContext,
}

#[derive(Debug, Clone, Copy)]
struct ThinkingEstimate {
    should_notify: bool,
    fallback_delay: Duration,
    kind: ThinkingKind,
}

pub(crate) struct ThinkingReporter {
    bot: Arc<RuntimeBot>,
    destination: ThinkingDestination,
    ticket: ReplyTicket,
    estimate: ThinkingEstimate,
    fallback_seed: u64,
    notice_sent: AtomicBool,
    timer: Mutex<Option<kovi::tokio::task::JoinHandle<()>>>,
}

static THINKING_PROTOCOL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "如果这次任务确实需要较长时间，先输出一句自然、简短、符合当前语气的状态提示，再输出最终回答。状态提示必须放在 {start} 和 {end} 之间；简单问题不要输出状态提示。不要承诺精确秒数，不要解释技术机制，不要把状态提示写进最终回答。",
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
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        message.hash(&mut hasher);
        image_count.hash(&mut hasher);
        let fallback_seed = hasher.finish();
        Arc::new(Self {
            bot,
            destination,
            ticket,
            estimate,
            fallback_seed,
            notice_sent: AtomicBool::new(false),
            timer: Mutex::new(None),
        })
    }

    pub(crate) fn protocol() -> &'static str {
        THINKING_PROTOCOL.as_str()
    }

    pub(crate) async fn start(self: &Arc<Self>) {
        if !self.estimate.should_notify {
            return;
        }
        let reporter = Arc::clone(self);
        let delay = self.estimate.fallback_delay;
        let handle = kovi::tokio::spawn(async move {
            kovi::tokio::time::sleep(delay).await;
            reporter.send_notice(reporter.fallback_notice()).await;
        });
        *self.timer.lock().await = Some(handle);
    }

    pub(crate) async fn finish(&self) {
        if let Some(handle) = self.timer.lock().await.take() {
            handle.abort();
        }
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
        let message_id = match self.destination {
            ThinkingDestination::Group(group_id) => {
                self.bot.send_group_msg_return(group_id, notice).await
            }
            ThinkingDestination::Private(user_id) => {
                self.bot.send_private_msg_return(user_id, notice).await
            }
        };
        if let Ok(message_id) = message_id {
            let scope = match self.destination {
                ThinkingDestination::Group(group_id) => {
                    crate::model::interrupt::ReplyScope::Group(group_id)
                }
                ThinkingDestination::Private(user_id) => {
                    crate::model::interrupt::ReplyScope::Private(user_id)
                }
            };
            record_bot_message(scope, self.ticket, message_id, &self.bot).await;
        }
    }

    fn fallback_notice(&self) -> String {
        let choices = match self.estimate.kind {
            ThinkingKind::Image => [
                "我先把图里的细节看清楚一点。",
                "这张图我看仔细一点，别急。",
                "我先确认一下图片里的内容，马上回来。",
            ],
            ThinkingKind::LongContext => [
                "我先把前后的内容理一遍，免得漏掉什么。",
                "我捋一下上下文，再认真回你。",
                "这段信息有点多，我先理清楚。",
            ],
            ThinkingKind::Complex => [
                "这个我先认真想一下，直接说容易漏东西。",
                "这题有点绕，我先把思路理顺。",
                "我先想清楚一点，再好好回答你。",
            ],
        };
        choices[(self.fallback_seed as usize) % choices.len()].to_string()
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
    if [
        "为什么",
        "怎么解决",
        "分析",
        "解释",
        "区别",
        "步骤",
        "原因",
        "代码",
        "报错",
        "论文",
        "总结",
        "详细",
    ]
    .iter()
    .any(|keyword| text.contains(keyword))
    {
        score = score.saturating_add(2);
    }

    let kind = if image_count > 0 {
        ThinkingKind::Image
    } else if history_len > 36 {
        ThinkingKind::LongContext
    } else {
        ThinkingKind::Complex
    };
    ThinkingEstimate {
        should_notify: score >= 3,
        fallback_delay: if image_count > 0 && !supports_vision {
            Duration::from_millis(1_200)
        } else if score >= 6 {
            Duration::from_millis(2_000)
        } else {
            Duration::from_millis(2_800)
        },
        kind,
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
