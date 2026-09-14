use crate::config;
use crate::model::traffic::truncate_chars;
use crate::vision::{ImageAttachment, merge_image_attachments};
use kovi::tokio::sync::{Mutex, Notify};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};
use yunxi_core::{InputCompletion, TurnGateInput, TurnPolicyOverride, TurnScope};

/// TurnGateInput 的非文本上下文 (Phase 2, doc §8.2:private/group 在进入
/// coalescer 时构造同一份输入;pending 片段由 coalescer 自己补全)。
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnGateBatchContext {
    pub(crate) scope: TurnScope,
    pub(crate) conversation_active: bool,
    pub(crate) addressed_to_agent: bool,
    pub(crate) replies_to_agent: bool,
    pub(crate) pending_task: bool,
    pub(crate) pending_outgoing: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TextBatch {
    pub(crate) text: String,
    pub(crate) intent_text: String,
    pub(crate) addressed: bool,
    pub(crate) plain_text: bool,
    pub(crate) vision_requested: bool,
    pub(crate) sticker_reaction: bool,
    pub(crate) images: Vec<ImageAttachment>,
    pub(crate) message_ids: Vec<i32>,
    /// Phase 4 门控:bundle 加载且 response_mode=active 时,response head
    /// 对该批次的最终决策(仅高置信度,Abstain 不落入此字段)。
    pub(crate) turn_gate_response: Option<yunxi_core::TurnResponseDecision>,
}

pub(crate) struct MessagePart {
    pub(crate) text: String,
    pub(crate) intent_text: String,
    pub(crate) addressed: bool,
    pub(crate) plain_text: bool,
    pub(crate) vision_requested: bool,
    pub(crate) sticker_reaction: bool,
    pub(crate) images: Vec<ImageAttachment>,
    pub(crate) message_ids: Vec<i32>,
}

struct PendingBatch {
    identity: Arc<BatchIdentity>,
    parts: Vec<String>,
    intent_parts: Vec<String>,
    char_count: usize,
    addressed: bool,
    all_plain_text: bool,
    vision_requested: bool,
    sticker_reaction: bool,
    images: Vec<ImageAttachment>,
    message_ids: Vec<i32>,
    started_at: Instant,
    updated_at: Instant,
    completion: Option<InputCompletion>,
}

#[derive(Debug)]
struct BatchIdentity;

impl Default for PendingBatch {
    fn default() -> Self {
        Self {
            identity: Arc::new(BatchIdentity),
            parts: Vec::new(),
            intent_parts: Vec::new(),
            char_count: 0,
            addressed: false,
            all_plain_text: true,
            vision_requested: false,
            sticker_reaction: false,
            images: Vec::new(),
            message_ids: Vec::new(),
            started_at: Instant::now(),
            updated_at: Instant::now(),
            completion: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct BatchPolicy {
    enabled: bool,
    complete_delay: Duration,
    normal_delay: Duration,
    incomplete_delay: Duration,
    max_wait: Duration,
    max_parts: usize,
    max_chars: usize,
    max_input_chars: usize,
    /// 批次成轮前的**最短停留**：即使语义判定"说完了"，也至少等这么久，
    /// 让连发的下一条有机会并进来。0 = 立刻成轮（Host 链路的默认行为）。
    ///
    /// 为什么需要它：完成度判的是"这句话说完了没"，不是"这条请求说完了没"。
    /// 用户分三条发一个请求时，前两条往往每条都是完整句子——没有停留，
    /// 第一条一到就成轮，三条就变成三轮（2026-09-14 的接续链路实测）。
    min_dwell: Duration,
}

impl BatchPolicy {
    pub(crate) fn from_config() -> Self {
        let batching = config::get().message_batch().clone();
        Self {
            enabled: batching.enabled(),
            complete_delay: Duration::from_millis(batching.complete_delay_ms()),
            normal_delay: Duration::from_millis(batching.normal_delay_ms()),
            incomplete_delay: Duration::from_millis(batching.incomplete_delay_ms()),
            max_wait: Duration::from_millis(batching.max_wait_ms()),
            max_parts: batching.max_parts(),
            max_chars: batching.max_chars(),
            max_input_chars: config::get().traffic().max_input_chars(),
            min_dwell: Duration::ZERO,
        }
    }

    /// 给批次加一个最短停留（接续链路用，见 [`BatchPolicy::min_dwell`]）。
    pub(crate) fn with_min_dwell(mut self, min_dwell: Duration) -> Self {
        self.min_dwell = min_dwell;
        self
    }

    #[cfg(test)]
    fn testing() -> Self {
        Self {
            enabled: true,
            complete_delay: Duration::from_millis(25),
            normal_delay: Duration::from_millis(45),
            incomplete_delay: Duration::from_millis(65),
            max_wait: Duration::from_millis(100),
            max_parts: 6,
            max_chars: 500,
            max_input_chars: 6_000,
            min_dwell: Duration::ZERO,
        }
    }
}

/// 为每个聊天键提供轻量防抖队列；只有最后到达的任务会取走完整批次。
pub(crate) struct MessageCoalescer<K> {
    pending: Mutex<HashMap<K, PendingBatch>>,
}

impl<K> Default for MessageCoalescer<K> {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl<K> MessageCoalescer<K>
where
    K: Copy + Eq + Hash,
{
    /// Phase 2 completion gate (doc §8.2):TurnGate 优先决定 flush/hold;
    /// abstain 或引擎不可用时调用 `legacy` (现有 lexical + MiniMind 路径)。
    /// `legacy` 是惰性的——TurnGate 高置信度决策时不会被调用。
    pub(crate) async fn push_with_turn_gate<F, Fut>(
        &self,
        key: K,
        part: MessagePart,
        context: TurnGateBatchContext,
        legacy: F,
    ) -> Option<TextBatch>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = InputCompletion>,
    {
        self.push_with_turn_gate_and_policy(key, part, context, BatchPolicy::from_config(), legacy)
            .await
    }

    /// 同 [`Self::push_with_turn_gate`]，但由调用方给定批次策略。
    ///
    /// 接续链路要在这里加最短停留（`min_dwell`），Host 链路保持"判完就成轮"。
    pub(crate) async fn push_with_turn_gate_and_policy<F, Fut>(
        &self,
        key: K,
        part: MessagePart,
        context: TurnGateBatchContext,
        policy: BatchPolicy,
        legacy: F,
    ) -> Option<TextBatch>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = InputCompletion>,
    {
        let completion = if let Some(runtime) = crate::yunxi::turn_gate_runtime::get() {
            let pending_fragments = {
                let pending = self.pending.lock().await;
                pending
                    .get(&key)
                    .map(|batch| {
                        batch
                            .intent_parts
                            .iter()
                            .take(yunxi_core::TURN_GATE_MAX_PENDING_FRAGMENTS)
                            .map(|fragment| {
                                fragment
                                    .chars()
                                    .take(yunxi_core::TURN_GATE_MAX_FRAGMENT_CHARS)
                                    .collect()
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let input = turn_gate_input(&part, context, pending_fragments);
            runtime.classify_completion(&input, legacy).await
        } else {
            legacy().await
        };
        let mut batch = self
            .push_with_completion_policy(key, part, completion, policy)
            .await?;
        // Phase 3/4:批次成型后用同一份(空白 pending)输入跑 response
        // head——shadow 只记账;active 时连同决策写入 TextBatch 供门控。
        if let Some(runtime) = crate::yunxi::turn_gate_runtime::get() {
            let response_input = TurnGateInput {
                current_text: batch.intent_text.clone(),
                pending_user_fragments: Vec::new(),
                recent_turns: Vec::new(),
                scope: context.scope,
                conversation_active: context.conversation_active,
                bot_last_asked_question: None,
                pending_outgoing: context.pending_outgoing,
                pending_task: context.pending_task,
                addressed_to_agent: batch.addressed,
                replies_to_agent: context.replies_to_agent,
                has_image: !batch.images.is_empty(),
                has_sticker: batch.sticker_reaction,
                policy_override: if batch.addressed || context.replies_to_agent {
                    TurnPolicyOverride::MustReply
                } else {
                    TurnPolicyOverride::None
                },
            };
            if let Some(output) = runtime.classify_response_shadow(&response_input) {
                crate::yunxi::turn_gate_shadow::record_batch(context.scope, &output);
                if runtime.response_gate_active() {
                    batch.turn_gate_response = Some(output.decision);
                }
            }
        }
        Some(batch)
    }

    #[cfg(test)]
    async fn push_with_policy_paused(
        &self,
        key: K,
        part: MessagePart,
        policy: BatchPolicy,
        hook: Arc<BatchPushHook>,
    ) -> Option<TextBatch> {
        self.push_with_policy_internal(key, part, policy, Some(hook), None)
            .await
    }

    pub(crate) async fn cancel(&self, key: K) {
        self.pending.lock().await.remove(&key);
    }

    pub(crate) async fn cancel_where(&self, mut predicate: impl FnMut(&K) -> bool) {
        self.pending.lock().await.retain(|key, _| !predicate(key));
    }

    #[cfg(test)]
    async fn push_with_policy(
        &self,
        key: K,
        part: MessagePart,
        policy: BatchPolicy,
    ) -> Option<TextBatch> {
        self.push_with_policy_internal(key, part, policy, None, None)
            .await
    }

    async fn push_with_completion_policy(
        &self,
        key: K,
        part: MessagePart,
        completion: InputCompletion,
        policy: BatchPolicy,
    ) -> Option<TextBatch> {
        self.push_with_policy_internal(key, part, policy, None, Some(completion))
            .await
    }

    async fn push_with_policy_internal(
        &self,
        key: K,
        mut part: MessagePart,
        policy: BatchPolicy,
        hook: Option<Arc<BatchPushHook>>,
        semantic_completion: Option<InputCompletion>,
    ) -> Option<TextBatch> {
        part.text = truncate_chars(&part.text, policy.max_input_chars);
        part.intent_text = truncate_chars(&part.intent_text, policy.max_input_chars);
        let image_only_part = !part.images.is_empty() && part.intent_text.trim().is_empty();
        if !policy.enabled {
            self.cancel(key).await;
            return Some(TextBatch {
                text: part.text,
                intent_text: part.intent_text,
                addressed: part.addressed,
                plain_text: part.plain_text,
                vision_requested: part.vision_requested,
                sticker_reaction: part.sticker_reaction,
                images: part.images,
                message_ids: part.message_ids,
                turn_gate_response: None,
            });
        }

        let identity = Arc::new(BatchIdentity);
        let delay = {
            let now = Instant::now();
            let mut pending = self.pending.lock().await;
            if pending.len() > 2_048 {
                let retention = policy.max_wait.saturating_mul(2);
                pending.retain(|_, batch| batch.updated_at.elapsed() < retention);
            }
            let batch = pending.entry(key).or_default();
            if batch.parts.is_empty() {
                batch.started_at = now;
            }
            batch.identity = Arc::clone(&identity);
            batch.completion = semantic_completion;
            let remaining = policy.max_input_chars.saturating_sub(batch.char_count);
            let bounded_text = truncate_chars(&part.text, remaining.max(1));
            let bounded_intent = truncate_chars(&part.intent_text, policy.max_input_chars);
            batch.char_count = batch
                .char_count
                .saturating_add(bounded_text.chars().count())
                .min(policy.max_input_chars);
            batch.parts.push(bounded_text);
            batch.intent_parts.push(bounded_intent);
            batch.addressed |= part.addressed;
            batch.all_plain_text &= part.plain_text;
            batch.vision_requested |= part.vision_requested;
            batch.sticker_reaction |= part.sticker_reaction;
            batch.images = merge_image_attachments(&batch.images, &part.images);
            for message_id in part.message_ids {
                if !batch.message_ids.contains(&message_id) {
                    batch.message_ids.push(message_id);
                }
            }
            batch.updated_at = now;

            batch_delay(&policy, batch, image_only_part)
        };
        if !delay.is_zero() {
            kovi::tokio::time::sleep(delay).await;
        }
        if let Some(hook) = hook {
            hook.inserted.notify_one();
            hook.release.notified().await;
        }

        let mut pending = self.pending.lock().await;
        if !pending
            .get(&key)
            .is_some_and(|batch| Arc::ptr_eq(&batch.identity, &identity))
        {
            return None;
        }
        pending.remove(&key).map(|batch| TextBatch {
            text: truncate_chars(&batch.parts.join("\n"), policy.max_input_chars),
            intent_text: truncate_chars(&batch.intent_parts.join("\n"), policy.max_input_chars),
            addressed: batch.addressed,
            plain_text: batch.all_plain_text,
            vision_requested: batch.vision_requested,
            sticker_reaction: batch.sticker_reaction,
            images: batch.images,
            message_ids: batch.message_ids,
            turn_gate_response: None,
        })
    }
}

/// 一个已经吸收了本次 part 的批次该等多久再成轮。
///
/// 优先级：容量到顶 → 不等；首条纯图片 → 给满窗口（图片常先发、文字随后补）；
/// 语义"说完了" → 只等 `min_dwell`；语义"还没说完" → 等到 `max_wait`（它只是
/// "对方一直没把话说完"的看门狗）；门控弃权（没有语义结论）→ 词法自适应。
/// 最后统一受 `min_dwell` 抬底、受剩余窗口封顶。
fn batch_delay(policy: &BatchPolicy, batch: &PendingBatch, image_only_part: bool) -> Duration {
    let reached_capacity =
        batch.parts.len() >= policy.max_parts || batch.char_count >= policy.max_chars;
    if reached_capacity {
        return Duration::ZERO;
    }
    let semantic_delay = if batch.parts.len() == 1 && image_only_part {
        policy.max_wait
    } else if matches!(batch.completion, Some(InputCompletion::Complete)) {
        policy.min_dwell
    } else if matches!(batch.completion, Some(InputCompletion::Incomplete)) {
        policy.max_wait
    } else {
        adaptive_delay(
            batch.parts.last().map(String::as_str).unwrap_or_default(),
            *policy,
        )
    };
    let remaining = policy.max_wait.saturating_sub(batch.started_at.elapsed());
    semantic_delay.max(policy.min_dwell).min(remaining)
}

struct BatchPushHook {
    inserted: Notify,
    release: Notify,
}

/// 用 pending 片段 + 新 part 构造 (有界) TurnGateInput。pending 片段即
/// "尚未提交的用户片段" (doc §7.1);recent_turns 留给 Phase 3 接入会话
/// 历史;文本截断由特征层完成。
fn turn_gate_input(
    part: &MessagePart,
    context: TurnGateBatchContext,
    pending_user_fragments: Vec<String>,
) -> TurnGateInput {
    TurnGateInput {
        current_text: part.intent_text.clone(),
        pending_user_fragments,
        recent_turns: Vec::new(),
        scope: context.scope,
        conversation_active: context.conversation_active,
        bot_last_asked_question: None,
        pending_outgoing: context.pending_outgoing,
        pending_task: context.pending_task,
        addressed_to_agent: context.addressed_to_agent,
        replies_to_agent: context.replies_to_agent,
        has_image: !part.images.is_empty(),
        has_sticker: part.sticker_reaction,
        policy_override: if context.addressed_to_agent || context.replies_to_agent {
            TurnPolicyOverride::MustReply
        } else {
            TurnPolicyOverride::None
        },
    }
}

fn adaptive_delay(message: &str, policy: BatchPolicy) -> Duration {
    let text = message.trim();
    if looks_incomplete(text) {
        policy.incomplete_delay
    } else if ends_complete_sentence(text) {
        policy.complete_delay
    } else {
        policy.normal_delay
    }
}

fn looks_incomplete(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    let meaningful_chars = text
        .chars()
        .filter(|character| !character.is_whitespace() && !character.is_ascii_punctuation())
        .count();
    if meaningful_chars <= 4 && !ends_complete_sentence(text) {
        return true;
    }
    if text.ends_with(['，', ',', '、', '：', ':', '；', ';', '…', '-', '—']) {
        return true;
    }
    false
}

fn ends_complete_sentence(text: &str) -> bool {
    text.ends_with([
        '。', '！', '？', '!', '?', '～', '~', '”', '"', '）', ')', '】', ']',
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        BatchPolicy, BatchPushHook, MessageCoalescer, MessagePart, PendingBatch, TextBatch,
        TurnGateBatchContext, adaptive_delay, batch_delay,
    };
    use crate::vision::ImageAttachment;
    use kovi::tokio::sync::Notify;
    use std::sync::Arc;
    use std::time::Duration;
    use yunxi_core::InputCompletion;

    #[test]
    fn turn_gate_path_without_runtime_falls_back_to_legacy_completion() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let coalescer = MessageCoalescer::default();
                let context = TurnGateBatchContext {
                    scope: yunxi_core::TurnScope::Group,
                    conversation_active: false,
                    addressed_to_agent: false,
                    replies_to_agent: false,
                    pending_task: false,
                    pending_outgoing: false,
                };
                // 环境没有 TurnGate runtime:push_with_turn_gate 必须调用
                // legacy 闭包并按完整度语义返回批次(行为与 Phase 1 一致)。
                let legacy_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let legacy_called_2 = Arc::clone(&legacy_called);
                let batch = coalescer
                    .push_with_turn_gate(
                        42,
                        MessagePart {
                            text: "完整的一句话。".to_owned(),
                            intent_text: "完整的一句话。".to_owned(),
                            addressed: false,
                            plain_text: true,
                            vision_requested: false,
                            sticker_reaction: false,
                            images: Vec::new(),
                            message_ids: vec![1],
                        },
                        context,
                        || async {
                            legacy_called_2.store(true, std::sync::atomic::Ordering::Relaxed);
                            yunxi_core::InputCompletion::Complete
                        },
                    )
                    .await;
                assert!(batch.is_some());
                assert!(legacy_called.load(std::sync::atomic::Ordering::Relaxed));
            })
    }

    #[test]
    fn rapid_messages_are_returned_as_one_batch() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let coalescer = Arc::new(MessageCoalescer::default());
                let first = {
                    let coalescer = Arc::clone(&coalescer);
                    kovi::tokio::spawn(async move {
                        coalescer
                            .push_with_policy(
                                7_i64,
                                MessagePart {
                                    text: "第一句".to_string(),
                                    intent_text: "第一句".to_string(),
                                    addressed: true,
                                    plain_text: true,
                                    vision_requested: true,
                                    sticker_reaction: false,
                                    images: Vec::new(),
                                    message_ids: vec![101],
                                },
                                BatchPolicy::testing(),
                            )
                            .await
                    })
                };
                kovi::tokio::time::sleep(Duration::from_millis(10)).await;
                let second = coalescer
                    .push_with_policy(
                        7_i64,
                        MessagePart {
                            text: "第二句。".to_string(),
                            intent_text: "第二句。".to_string(),
                            addressed: false,
                            plain_text: true,
                            vision_requested: false,
                            sticker_reaction: true,
                            images: Vec::new(),
                            message_ids: vec![102],
                        },
                        BatchPolicy::testing(),
                    )
                    .await;

                assert!(first.await.expect("任务应正常结束").is_none());
                assert_eq!(
                    second,
                    Some(TextBatch {
                        text: "第一句\n第二句。".to_string(),
                        intent_text: "第一句\n第二句。".to_string(),
                        addressed: true,
                        plain_text: true,
                        vision_requested: true,
                        sticker_reaction: true,
                        images: Vec::new(),
                        message_ids: vec![101, 102],
                        turn_gate_response: None,
                    })
                );
            });
    }

    #[test]
    fn incomplete_fragments_wait_longer_than_complete_sentences() {
        let policy = BatchPolicy::testing();
        assert_eq!(adaptive_delay("还有，", policy), policy.incomplete_delay);
        assert_eq!(adaptive_delay("我今天", policy), policy.incomplete_delay);
        assert_eq!(adaptive_delay("因为这个原因", policy), policy.normal_delay);
        assert_eq!(adaptive_delay("我知道了。", policy), policy.complete_delay);
        assert_eq!(adaptive_delay("我们晚点再聊", policy), policy.normal_delay);
    }

    #[test]
    fn image_only_messages_wait_for_a_follow_up_text() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let mut policy = BatchPolicy::testing();
                policy.max_wait = Duration::from_millis(300);
                let coalescer = Arc::new(MessageCoalescer::default());
                let mut first = {
                    let coalescer = Arc::clone(&coalescer);
                    kovi::tokio::spawn(async move {
                        coalescer
                            .push_with_policy(
                                8_i64,
                                MessagePart {
                                    text: "先看看这张图。".to_string(),
                                    intent_text: String::new(),
                                    addressed: false,
                                    plain_text: false,
                                    vision_requested: true,
                                    sticker_reaction: false,
                                    images: vec![ImageAttachment {
                                        key: "avatar".to_string(),
                                        file: Some("avatar.png".to_string()),
                                        url: None,
                                    }],
                                    message_ids: vec![601],
                                },
                                policy,
                            )
                            .await
                    })
                };

                assert!(
                    kovi::tokio::time::timeout(Duration::from_millis(60), &mut first)
                        .await
                        .is_err(),
                    "纯图片批次不应在普通文本窗口内提前结束"
                );
                let combined = coalescer
                    .push_with_policy(
                        8_i64,
                        MessagePart {
                            text: "这个角色是什么？".to_string(),
                            intent_text: "这个角色是什么？".to_string(),
                            addressed: false,
                            plain_text: true,
                            vision_requested: false,
                            sticker_reaction: false,
                            images: Vec::new(),
                            message_ids: vec![602],
                        },
                        policy,
                    )
                    .await
                    .expect("补充文字应与图片合并");

                assert_eq!(combined.text, "先看看这张图。\n这个角色是什么？");
                assert_eq!(combined.intent_text, "\n这个角色是什么？");
                assert_eq!(combined.message_ids, vec![601, 602]);
                assert_eq!(combined.images.len(), 1);
                assert!(first.await.expect("首个任务应正常结束").is_none());
            });
    }

    /// 最短停留：判"说完了"也别立刻成轮（接续链路），Host 链路保持立刻成轮。
    ///
    /// 这是"分三条发一个请求"能不能合成一轮的关键——没有停留时，第一条本身
    /// 是完整句子就已经成轮了，后两条只能各自成轮。
    #[test]
    fn min_dwell_holds_a_complete_batch_open() {
        let policy = BatchPolicy::testing();
        let batch = |completion| PendingBatch {
            parts: vec!["你去看看德国现在几点了".to_string()],
            completion: Some(completion),
            ..PendingBatch::default()
        };

        // Host 链路：判完即成轮。
        assert_eq!(
            batch_delay(&policy, &batch(InputCompletion::Complete), false),
            Duration::ZERO
        );

        // 接续链路：判为"说完了"也停留一个窗口，让下一条并进来。
        let dwell = Duration::from_millis(40);
        let patient = policy.with_min_dwell(dwell);
        assert_eq!(
            batch_delay(&patient, &batch(InputCompletion::Complete), false),
            dwell
        );
        // 剩余窗口按"批次已经存活了多久"扣减，所以贴顶断言留一点余量。
        let near_max_wait = |delay: Duration| {
            assert!(
                delay <= policy.max_wait && delay > policy.max_wait - Duration::from_millis(5),
                "{delay:?} 应贴住 max_wait={:?}",
                policy.max_wait
            );
        };
        // "还没说完"本来就等满窗口，停留不会把它拖过 max_wait。
        near_max_wait(batch_delay(
            &patient,
            &batch(InputCompletion::Incomplete),
            false,
        ));
        // 停留本身也受剩余窗口封顶，不会把批次拖到 max_wait 之外。
        let greedy = policy.with_min_dwell(policy.max_wait * 4);
        near_max_wait(batch_delay(
            &greedy,
            &batch(InputCompletion::Complete),
            false,
        ));
    }

    #[test]
    fn reaching_the_part_limit_flushes_immediately() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let mut policy = BatchPolicy::testing();
                policy.max_parts = 2;
                let coalescer = Arc::new(MessageCoalescer::default());
                let first = {
                    let coalescer = Arc::clone(&coalescer);
                    kovi::tokio::spawn(async move {
                        coalescer
                            .push_with_policy(
                                9_i64,
                                MessagePart {
                                    text: "第一段".to_string(),
                                    intent_text: "第一段".to_string(),
                                    addressed: false,
                                    plain_text: true,
                                    vision_requested: false,
                                    sticker_reaction: false,
                                    images: Vec::new(),
                                    message_ids: vec![201],
                                },
                                policy,
                            )
                            .await
                    })
                };
                kovi::tokio::time::sleep(Duration::from_millis(5)).await;
                let second = coalescer
                    .push_with_policy(
                        9_i64,
                        MessagePart {
                            text: "第二段".to_string(),
                            intent_text: "第二段".to_string(),
                            addressed: false,
                            plain_text: true,
                            vision_requested: false,
                            sticker_reaction: false,
                            images: Vec::new(),
                            message_ids: vec![202],
                        },
                        policy,
                    )
                    .await;
                assert!(first.await.expect("任务应正常结束").is_none());
                assert_eq!(second.expect("第二段应立即取出").text, "第一段\n第二段");
            });
    }

    #[test]
    fn cancellation_discards_a_pending_batch() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let coalescer = Arc::new(MessageCoalescer::default());
                let pending = {
                    let coalescer = Arc::clone(&coalescer);
                    kovi::tokio::spawn(async move {
                        coalescer
                            .push_with_policy(
                                12_i64,
                                MessagePart {
                                    text: "先别急".to_string(),
                                    intent_text: "先别急".to_string(),
                                    addressed: false,
                                    plain_text: true,
                                    vision_requested: false,
                                    sticker_reaction: false,
                                    images: Vec::new(),
                                    message_ids: vec![301],
                                },
                                BatchPolicy::testing(),
                            )
                            .await
                    })
                };
                kovi::tokio::time::sleep(Duration::from_millis(5)).await;
                coalescer.cancel(12_i64).await;
                assert!(pending.await.expect("任务应正常结束").is_none());
            });
    }

    #[test]
    fn stale_sleeper_cannot_take_a_recreated_batch_after_cancel() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let coalescer = Arc::new(MessageCoalescer::default());
                let old_hook = Arc::new(BatchPushHook {
                    inserted: Notify::new(),
                    release: Notify::new(),
                });
                let old_task = {
                    let coalescer = Arc::clone(&coalescer);
                    let hook = Arc::clone(&old_hook);
                    kovi::tokio::spawn(async move {
                        coalescer
                            .push_with_policy_paused(
                                14_i64,
                                MessagePart {
                                    text: "旧批次".to_string(),
                                    intent_text: "旧批次".to_string(),
                                    addressed: false,
                                    plain_text: true,
                                    vision_requested: false,
                                    sticker_reaction: false,
                                    images: Vec::new(),
                                    message_ids: vec![501],
                                },
                                BatchPolicy::testing(),
                                hook,
                            )
                            .await
                    })
                };
                old_hook.inserted.notified().await;
                coalescer.cancel(14_i64).await;

                let new_hook = Arc::new(BatchPushHook {
                    inserted: Notify::new(),
                    release: Notify::new(),
                });
                let new_task = {
                    let coalescer = Arc::clone(&coalescer);
                    let hook = Arc::clone(&new_hook);
                    kovi::tokio::spawn(async move {
                        coalescer
                            .push_with_policy_paused(
                                14_i64,
                                MessagePart {
                                    text: "新批次。".to_string(),
                                    intent_text: "新批次。".to_string(),
                                    addressed: true,
                                    plain_text: true,
                                    vision_requested: false,
                                    sticker_reaction: false,
                                    images: Vec::new(),
                                    message_ids: vec![502],
                                },
                                BatchPolicy::testing(),
                                hook,
                            )
                            .await
                    })
                };
                new_hook.inserted.notified().await;

                old_hook.release.notify_one();
                assert!(old_task.await.expect("旧任务应正常结束").is_none());

                new_hook.release.notify_one();
                assert_eq!(
                    new_task
                        .await
                        .expect("新任务应正常结束")
                        .expect("新批次应被取出")
                        .text,
                    "新批次。"
                );
            });
    }

    #[test]
    fn one_oversized_part_is_hard_capped_before_batching() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let mut policy = BatchPolicy::testing();
                policy.max_chars = 1;
                policy.max_input_chars = 5;
                let coalescer = MessageCoalescer::default();
                let batch = coalescer
                    .push_with_policy(
                        13_i64,
                        MessagePart {
                            text: "这是一个明显过长的输入".to_string(),
                            intent_text: "这是一个明显过长的输入".to_string(),
                            addressed: true,
                            plain_text: true,
                            vision_requested: false,
                            sticker_reaction: false,
                            images: Vec::new(),
                            message_ids: vec![401],
                        },
                        policy,
                    )
                    .await
                    .expect("达到容量后应立即返回");
                assert!(batch.text.chars().count() <= 5);
                assert!(batch.intent_text.chars().count() <= 5);
                assert!(batch.text.ends_with('…'));
            });
    }
}
