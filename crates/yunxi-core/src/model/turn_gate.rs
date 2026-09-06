//! TurnGate: 共享字符特征、双 head(completion/response) 的有界线性门。
//!
//! 设计文档 `docs/yunxi-turngate-design.md` (v0.2)。本模块实现 Phase 0:
//! 协议、稳定特征抽取、输出/abstain 语义、指标与 manifest 契约,不改变
//! 生产路由。权重加载(Phase 1)之前 `TurnGateEngine` 一律 `abstain`
//! (不可用降级由调用方按 scope fallback 处理)。
//!
//! 稳定约定(与离线训练器共享):
//! - hash: FNV-1a 32-bit 作用于 UTF-8 字节;
//! - 文本特征: 字符 2..=5-gram,字段边界标记见 [`field_marker`],桶数
//!   [`TURN_GATE_HASH_BUCKETS`],每桶计数上限 2,单次推理最多
//!   [`TURN_GATE_MAX_TEXT_FEATURES`] 个非零特征;
//! - 结构化特征: 固定位置布尔位 [`TURN_GATE_CONTEXT_FEATURES`]。

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

/// 当前文本最大 Unicode 字符数 (doc §4)。
pub const TURN_GATE_MAX_CURRENT_CHARS: usize = 512;
/// 当前文本最大字节数 (doc §4)。
pub const TURN_GATE_MAX_CURRENT_BYTES: usize = 2_048;
/// pending 用户片段最大条数 (doc §4)。
pub const TURN_GATE_MAX_PENDING_FRAGMENTS: usize = 4;
/// 单条 pending/最近 turn 的最大字符数 (doc §4)。
pub const TURN_GATE_MAX_FRAGMENT_CHARS: usize = 160;
/// 最近 turn 最大条数 (doc §4)。
pub const TURN_GATE_MAX_RECENT_TURNS: usize = 4;
/// bot 最近提问的最大字符数 (doc §4)。
pub const TURN_GATE_MAX_QUESTION_CHARS: usize = 160;
/// 字符 n-gram 桶数 (doc §5.1)。
pub const TURN_GATE_HASH_BUCKETS: u32 = 65_536;
/// n-gram 范围 (doc §5.1)。
pub const TURN_GATE_NGRAM_MIN: usize = 2;
pub const TURN_GATE_NGRAM_MAX: usize = 5;
/// 单次推理文本非零特征上限 (doc §5.1)。
pub const TURN_GATE_MAX_TEXT_FEATURES: usize = 512;
/// 单桶重复计数上限 (doc §5.1)。
pub const TURN_GATE_MAX_TEXT_FEATURE_COUNT: u32 = 2;
/// 结构化特征位置总数 (固定 ≤64,doc §5.2)。
pub const TURN_GATE_CONTEXT_FEATURES: usize = 24;
/// 特征协议版本 (manifest 校验 + 训练器共享)。
pub const TURN_GATE_FEATURE_VERSION: &str = "char-2-5-v2/ctx-v0";
/// manifest 协议版本 (doc §6)。
pub const TURN_GATE_MANIFEST_VERSION: u16 = 1;

/// FNV-1a 32-bit。禁用的 [`std::collections::hash_map::DefaultHasher`]
/// 跨版本不稳定,这里必须与离线训练器逐字节一致。
pub fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// 文本归一化 (doc §5.1): ASCII 小写、合并空白、截断到
/// [`TURN_GATE_MAX_CURRENT_CHARS`]/[`TURN_GATE_MAX_CURRENT_BYTES`]。
/// 中文标点与 emoji 原样保留。
pub fn normalize_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len().min(TURN_GATE_MAX_CURRENT_BYTES));
    let mut last_was_space = true;
    for character in text.chars() {
        if character.is_whitespace() {
            if !last_was_space {
                normalized.push(' ');
                last_was_space = true;
            }
            continue;
        }
        last_was_space = false;
        if character.is_ascii() {
            normalized.push(character.to_ascii_lowercase());
        } else {
            normalized.push(character);
        }
    }
    while normalized.ends_with(' ') {
        normalized.pop();
    }
    let mut byte_len = 0;
    let mut char_count = 0;
    for character in normalized.chars() {
        if byte_len + character.len_utf8() > TURN_GATE_MAX_CURRENT_BYTES {
            break;
        }
        byte_len += character.len_utf8();
        char_count += 1;
        if char_count >= TURN_GATE_MAX_CURRENT_CHARS {
            break;
        }
    }
    normalized.chars().take(char_count).collect()
}

/// 字段边界标记 (doc §5.1 第 5 条)。每个字段先拼标记,n-gram 只在其
/// 所属字段内部提取,模型看到"谁说了什么、在哪一段上下文"。
fn field_marker(field: FieldMarker) -> &'static str {
    match field {
        FieldMarker::Current => "\u{0001}",
        FieldMarker::Question => "\u{0002}",
        FieldMarker::Pending => "\u{0003}",
        FieldMarker::RecentUser => "\u{0004}u",
        FieldMarker::RecentOther => "\u{0004}o",
        FieldMarker::RecentAssistant => "\u{0004}a",
    }
}

#[derive(Clone, Copy)]
pub(crate) enum FieldMarker {
    Current,
    Question,
    Pending,
    RecentUser,
    RecentOther,
    RecentAssistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCompletion {
    FlushNow,
    HoldForMore,
    Abstain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnResponseDecision {
    Answer,
    Continue,
    Ack,
    Ignore,
    Wait,
    Abstain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TurnScope {
    #[default]
    Private,
    Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TurnPolicyOverride {
    #[default]
    None,
    MustReply,
    Command,
    Stop,
    Erase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecentTurnRole {
    User,
    OtherMember,
    Assistant,
}

/// 有限历史 turn (doc §4): 每条在结构化之前已截断到
/// [`TURN_GATE_MAX_FRAGMENT_CHARS`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentTurn {
    pub role: RecentTurnRole,
    pub text: String,
}

impl RecentTurn {
    pub fn new(role: RecentTurnRole, text: impl Into<String>) -> Self {
        let text = text.into();
        let truncated: String = text.chars().take(TURN_GATE_MAX_FRAGMENT_CHARS).collect();
        Self {
            role,
            text: truncated,
        }
    }
}

/// 共享输入 (doc §4)。字段全部有界:构造时不强制,特征抽取阶段防御性
/// 截断;调用方不应放入 QQ 号、URL、Token 或完整聊天记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TurnGateInput {
    pub current_text: String,
    pub pending_user_fragments: Vec<String>,
    pub recent_turns: Vec<RecentTurn>,
    pub scope: TurnScope,
    pub conversation_active: bool,
    pub bot_last_asked_question: Option<String>,
    pub pending_outgoing: bool,
    pub pending_task: bool,
    pub addressed_to_agent: bool,
    pub replies_to_agent: bool,
    pub has_image: bool,
    pub has_sticker: bool,
    pub policy_override: TurnPolicyOverride,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionOutput {
    pub decision: TurnCompletion,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseOutput {
    pub decision: TurnResponseDecision,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnGateOutput {
    pub completion: CompletionOutput,
    pub response: ResponseOutput,
    pub model_version: String,
}

/// 单个文本特征:稳定桶索引 + 截断后的出现次数 (≤2,doc §5.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextFeature {
    pub index: u32,
    pub count: u32,
}

/// 一次抽取的完整特征向量 (训练器与 Rust 必须逐项一致)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnGateFeatures {
    /// 非零文本特征,字段序 → n-gram 长度序 → 位置序,上限 512。
    pub text_features: Vec<TextFeature>,
    /// 激活的结构化特征位置 (升序),固定位置见表
    /// [`CONTEXT_FEATURE_INDEX`]。
    pub context_indices: Vec<u16>,
}

/// 结构化特征位置协议 (doc §5.2)。修改即提升
/// [`TURN_GATE_FEATURE_VERSION`]。
pub mod context_feature_index {
    pub const SCOPE_PRIVATE: u16 = 0;
    pub const SCOPE_GROUP: u16 = 1;
    pub const PENDING_NOT_EMPTY: u16 = 2;
    pub const RECENT_TURNS_NOT_EMPTY: u16 = 3;
    pub const RECENT_ROLE_USER: u16 = 4;
    pub const RECENT_ROLE_OTHER: u16 = 5;
    pub const RECENT_ROLE_ASSISTANT: u16 = 6;
    pub const ADDRESSED_TO_AGENT: u16 = 7;
    pub const REPLIES_TO_AGENT: u16 = 8;
    pub const CONVERSATION_ACTIVE: u16 = 9;
    pub const HAD_ASKED_QUESTION: u16 = 10;
    pub const PENDING_OUTGOING: u16 = 11;
    pub const PENDING_TASK: u16 = 12;
    pub const HAS_IMAGE: u16 = 13;
    pub const HAS_STICKER: u16 = 14;
    pub const POLICY_MUST_REPLY: u16 = 15;
    pub const POLICY_COMMAND: u16 = 16;
    pub const POLICY_STOP: u16 = 17;
    pub const POLICY_ERASE: u16 = 18;
}

/// 稳定的 FNV-1a 提取 (doc §5.1):对归一化文本逐字段加边界标记后
/// 提取字符 2..=5-gram,桶内重复计数上限 2,总量上限 512。
pub(crate) fn extract_text_features(text: &str, marker: FieldMarker) -> Vec<TextFeature> {
    let mut counts: Vec<TextFeature> = Vec::with_capacity(64);
    let normalized = normalize_text(text);
    let chars: Vec<char> = normalized.chars().collect();
    'outer: for width in TURN_GATE_NGRAM_MIN..=TURN_GATE_NGRAM_MAX {
        if chars.len() < width {
            break;
        }
        for window in chars.windows(width) {
            let mut gram = String::with_capacity(width * 3 + 2);
            gram.push_str(field_marker(marker));
            for character in window {
                gram.push(*character);
            }
            let bucket = fnv1a_32(gram.as_bytes()) % TURN_GATE_HASH_BUCKETS;
            if let Some(existing) = counts.iter_mut().find(|feature| feature.index == bucket) {
                existing.count = (existing.count + 1).min(TURN_GATE_MAX_TEXT_FEATURE_COUNT);
                continue;
            }
            if counts.len() >= TURN_GATE_MAX_TEXT_FEATURES {
                break 'outer;
            }
            counts.push(TextFeature {
                index: bucket,
                count: 1,
            });
        }
    }
    counts
}

/// 抽取完整特征向量 (doc §5)。字段顺序:current → question → pending →
/// recent;结构化特征按 [`context_feature_index`] 固定位置。
pub fn extract_features(input: &TurnGateInput) -> TurnGateFeatures {
    let mut text_features: Vec<TextFeature> = Vec::with_capacity(128);
    for feature in extract_text_features(&input.current_text, FieldMarker::Current) {
        merge_feature(&mut text_features, feature);
    }
    if let Some(question) = input.bot_last_asked_question.as_deref() {
        for feature in extract_text_features(question, FieldMarker::Question) {
            merge_feature(&mut text_features, feature);
        }
    }
    for fragment in input
        .pending_user_fragments
        .iter()
        .take(TURN_GATE_MAX_PENDING_FRAGMENTS)
    {
        for feature in extract_text_features(fragment, FieldMarker::Pending) {
            merge_feature(&mut text_features, feature);
        }
    }
    for turn in input.recent_turns.iter().take(TURN_GATE_MAX_RECENT_TURNS) {
        let marker = match turn.role {
            RecentTurnRole::User => FieldMarker::RecentUser,
            RecentTurnRole::OtherMember => FieldMarker::RecentOther,
            RecentTurnRole::Assistant => FieldMarker::RecentAssistant,
        };
        for feature in extract_text_features(&turn.text, marker) {
            merge_feature(&mut text_features, feature);
        }
    }
    text_features.truncate(TURN_GATE_MAX_TEXT_FEATURES);

    let mut context = Vec::with_capacity(TURN_GATE_CONTEXT_FEATURES / 2);
    let mut push = |position: u16, active: bool| {
        if active {
            context.push(position);
        }
    };
    push(
        context_feature_index::SCOPE_PRIVATE,
        input.scope == TurnScope::Private,
    );
    push(
        context_feature_index::SCOPE_GROUP,
        input.scope == TurnScope::Group,
    );
    push(
        context_feature_index::PENDING_NOT_EMPTY,
        !input.pending_user_fragments.is_empty(),
    );
    push(
        context_feature_index::RECENT_TURNS_NOT_EMPTY,
        !input.recent_turns.is_empty(),
    );
    push(
        context_feature_index::RECENT_ROLE_USER,
        input
            .recent_turns
            .iter()
            .any(|turn| turn.role == RecentTurnRole::User),
    );
    push(
        context_feature_index::RECENT_ROLE_OTHER,
        input
            .recent_turns
            .iter()
            .any(|turn| turn.role == RecentTurnRole::OtherMember),
    );
    push(
        context_feature_index::RECENT_ROLE_ASSISTANT,
        input
            .recent_turns
            .iter()
            .any(|turn| turn.role == RecentTurnRole::Assistant),
    );
    push(
        context_feature_index::ADDRESSED_TO_AGENT,
        input.addressed_to_agent,
    );
    push(
        context_feature_index::REPLIES_TO_AGENT,
        input.replies_to_agent,
    );
    push(
        context_feature_index::CONVERSATION_ACTIVE,
        input.conversation_active,
    );
    push(
        context_feature_index::HAD_ASKED_QUESTION,
        input.bot_last_asked_question.is_some(),
    );
    push(
        context_feature_index::PENDING_OUTGOING,
        input.pending_outgoing,
    );
    push(context_feature_index::PENDING_TASK, input.pending_task);
    push(context_feature_index::HAS_IMAGE, input.has_image);
    push(context_feature_index::HAS_STICKER, input.has_sticker);
    push(
        context_feature_index::POLICY_MUST_REPLY,
        input.policy_override == TurnPolicyOverride::MustReply,
    );
    push(
        context_feature_index::POLICY_COMMAND,
        input.policy_override == TurnPolicyOverride::Command,
    );
    push(
        context_feature_index::POLICY_STOP,
        input.policy_override == TurnPolicyOverride::Stop,
    );
    push(
        context_feature_index::POLICY_ERASE,
        input.policy_override == TurnPolicyOverride::Erase,
    );

    TurnGateFeatures {
        text_features,
        context_indices: context,
    }
}

fn merge_feature(target: &mut Vec<TextFeature>, feature: TextFeature) {
    if let Some(existing) = target
        .iter_mut()
        .find(|existing| existing.index == feature.index)
    {
        existing.count = (existing.count + feature.count).min(TURN_GATE_MAX_TEXT_FEATURE_COUNT);
        return;
    }
    target.push(feature);
}

/// 无权重时的引擎 (Phase 0):一律 abstain;Phase 1 加载校验后的权重后
/// 提供真实分类。可用性由调用方检查,缺失时必须走现有回退路径。
#[derive(Debug, Clone, Default)]
pub struct TurnGateEngine {
    model_version: &'static str,
}

impl TurnGateEngine {
    pub const fn unavailable() -> Self {
        Self {
            model_version: "none",
        }
    }

    /// 当前实现固定不可用(无 bundle);返回 false 时调用方必须走
    /// lexical/MinMind 或现有回复链的 fallback (doc §6 加载顺序)。
    pub fn available(&self) -> bool {
        false
    }

    pub fn feature_signature(&self) -> &'static str {
        TURN_GATE_FEATURE_VERSION
    }

    pub fn model_version(&self) -> &'static str {
        self.model_version
    }

    pub fn classify_completion(&self, _input: &TurnGateInput) -> CompletionOutput {
        CompletionOutput {
            decision: TurnCompletion::Abstain,
            confidence: 0.0,
        }
    }

    pub fn classify_response(&self, _input: &TurnGateInput) -> ResponseOutput {
        ResponseOutput {
            decision: TurnResponseDecision::Abstain,
            confidence: 0.0,
        }
    }
}

/// 训练器共享的 manifest 契约 (doc §6)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnGateManifest {
    pub manifest_version: u16,
    pub model_id: String,
    pub model_version: String,
    pub algorithm: String,
    pub feature_version: String,
    pub hash_buckets: u32,
    pub max_text_chars: usize,
    pub max_pending_fragments: usize,
    pub max_pending_fragment_chars: usize,
    pub max_recent_turns: usize,
    pub max_recent_turn_chars: usize,
    pub max_question_chars: usize,
    #[serde(default)]
    pub completion_labels: Vec<String>,
    #[serde(default)]
    pub response_labels: Vec<String>,
    pub training_data_version: String,
    #[serde(default)]
    pub assets: Vec<TurnGateManifestAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnGateManifestAsset {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TurnGateManifestError {
    #[error("turn gate manifest version {actual}, expected {expected}")]
    Version { actual: u16, expected: u16 },
    #[error("turn gate model id must be `yunxi-turngate`")]
    ModelId,
    #[error("turn gate algorithm must be `hashed-char-ngram-logistic`")]
    Algorithm,
    #[error("turn gate feature version mismatch: {actual}, expected {expected}")]
    FeatureVersion { actual: String, expected: String },
    #[error(
        "turn gate hash buckets {actual}, expected {expected} (feature version fixed at 65536)"
    )]
    HashBuckets { actual: u32, expected: u32 },
    #[error("turn gate bounds mismatch (max_text_chars={actual})")]
    MaxTextChars { actual: usize },
    #[error("turn gate bounds mismatch (max_pending_fragments={actual})")]
    MaxPendingFragments { actual: usize },
    #[error("turn gate bounds mismatch (max_pending_fragment_chars={actual})")]
    MaxPendingFragmentChars { actual: usize },
    #[error("turn gate bounds mismatch (max_recent_turns={actual})")]
    MaxRecentTurns { actual: usize },
    #[error("turn gate bounds mismatch (max_recent_turn_chars={actual})")]
    MaxRecentTurnChars { actual: usize },
    #[error("turn gate bounds mismatch (max_question_chars={actual})")]
    MaxQuestionChars { actual: usize },
    #[error("turn gate asset sha256 must be 64 lowercase hex chars")]
    AssetSha256,
}

impl TurnGateManifest {
    pub fn validate(&self) -> Result<(), TurnGateManifestError> {
        if self.manifest_version != TURN_GATE_MANIFEST_VERSION {
            return Err(TurnGateManifestError::Version {
                actual: self.manifest_version,
                expected: TURN_GATE_MANIFEST_VERSION,
            });
        }
        if self.model_id != "yunxi-turngate" {
            return Err(TurnGateManifestError::ModelId);
        }
        if self.algorithm != "hashed-char-ngram-logistic" {
            return Err(TurnGateManifestError::Algorithm);
        }
        if self.feature_version != TURN_GATE_FEATURE_VERSION {
            return Err(TurnGateManifestError::FeatureVersion {
                actual: self.feature_version.clone(),
                expected: TURN_GATE_FEATURE_VERSION.to_owned(),
            });
        }
        if self.hash_buckets != TURN_GATE_HASH_BUCKETS {
            return Err(TurnGateManifestError::HashBuckets {
                actual: self.hash_buckets,
                expected: TURN_GATE_HASH_BUCKETS,
            });
        }
        if self.max_text_chars != TURN_GATE_MAX_CURRENT_CHARS {
            return Err(TurnGateManifestError::MaxTextChars {
                actual: self.max_text_chars,
            });
        }
        if self.max_pending_fragments != TURN_GATE_MAX_PENDING_FRAGMENTS {
            return Err(TurnGateManifestError::MaxPendingFragments {
                actual: self.max_pending_fragments,
            });
        }
        if self.max_pending_fragment_chars != TURN_GATE_MAX_FRAGMENT_CHARS {
            return Err(TurnGateManifestError::MaxPendingFragmentChars {
                actual: self.max_pending_fragment_chars,
            });
        }
        if self.max_recent_turns != TURN_GATE_MAX_RECENT_TURNS {
            return Err(TurnGateManifestError::MaxRecentTurns {
                actual: self.max_recent_turns,
            });
        }
        if self.max_recent_turn_chars != TURN_GATE_MAX_FRAGMENT_CHARS {
            return Err(TurnGateManifestError::MaxRecentTurnChars {
                actual: self.max_recent_turn_chars,
            });
        }
        if self.max_question_chars != TURN_GATE_MAX_QUESTION_CHARS {
            return Err(TurnGateManifestError::MaxQuestionChars {
                actual: self.max_question_chars,
            });
        }
        if self.assets.iter().any(|asset| {
            asset.sha256.len() != 64 || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        }) {
            return Err(TurnGateManifestError::AssetSha256);
        }
        Ok(())
    }
}

/// 成本指标计数器 (Phase 0 先接入统计;行为接线在 Phase 2+)。
#[derive(Debug, Default)]
pub struct TurnGateMetrics {
    requests: AtomicU64,
    completion_flush: AtomicU64,
    completion_hold: AtomicU64,
    completion_abstain: AtomicU64,
    response_answer: AtomicU64,
    response_continue: AtomicU64,
    response_ack: AtomicU64,
    response_ignore: AtomicU64,
    response_wait: AtomicU64,
    response_abstain: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnGateMetricsSnapshot {
    pub requests: u64,
    pub completion_flush: u64,
    pub completion_hold: u64,
    pub completion_abstain: u64,
    pub response_answer: u64,
    pub response_continue: u64,
    pub response_ack: u64,
    pub response_ignore: u64,
    pub response_wait: u64,
    pub response_abstain: u64,
}

impl TurnGateMetrics {
    pub fn record_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_completion(&self, output: &CompletionOutput) {
        let counter = match output.decision {
            TurnCompletion::FlushNow => &self.completion_flush,
            TurnCompletion::HoldForMore => &self.completion_hold,
            TurnCompletion::Abstain => &self.completion_abstain,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_response(&self, output: &ResponseOutput) {
        let counter = match output.decision {
            TurnResponseDecision::Answer => &self.response_answer,
            TurnResponseDecision::Continue => &self.response_continue,
            TurnResponseDecision::Ack => &self.response_ack,
            TurnResponseDecision::Ignore => &self.response_ignore,
            TurnResponseDecision::Wait => &self.response_wait,
            TurnResponseDecision::Abstain => &self.response_abstain,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> TurnGateMetricsSnapshot {
        TurnGateMetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            completion_flush: self.completion_flush.load(Ordering::Relaxed),
            completion_hold: self.completion_hold.load(Ordering::Relaxed),
            completion_abstain: self.completion_abstain.load(Ordering::Relaxed),
            response_answer: self.response_answer.load(Ordering::Relaxed),
            response_continue: self.response_continue.load(Ordering::Relaxed),
            response_ack: self.response_ack.load(Ordering::Relaxed),
            response_ignore: self.response_ignore.load(Ordering::Relaxed),
            response_wait: self.response_wait.load(Ordering::Relaxed),
            response_abstain: self.response_abstain.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_golden_vectors() {
        assert_eq!(fnv1a_32(b""), 0x811C_9DC5);
        assert_eq!(fnv1a_32(b"a"), 0xE40C_292C);
        assert_eq!(fnv1a_32(b"foobar"), 0xBF9C_F968);
    }

    #[test]
    fn normalization_is_stable_and_bounded() {
        assert_eq!(normalize_text("  Hello  World\t\n"), "hello world");
        assert_eq!(normalize_text("【你好】😊 哟"), "【你好】😊 哟");
        assert_eq!(normalize_text("ABC abc 👋"), "abc abc 👋");
        let long = "长".repeat(TURN_GATE_MAX_CURRENT_CHARS + 100);
        assert_eq!(
            normalize_text(&long).chars().count(),
            TURN_GATE_MAX_CURRENT_CHARS
        );
    }

    #[test]
    fn duplicate_counts_are_capped_at_two() {
        // "哈哈哈":2-gram "哈哈" 出现两次,3-gram "哈哈哈" 一次。
        let input = TurnGateInput {
            current_text: "哈哈哈".to_owned(),
            ..TurnGateInput::default()
        };
        let features = extract_text_features(&input.current_text, FieldMarker::Current);
        let max_count = features
            .iter()
            .map(|feature| feature.count)
            .max()
            .unwrap_or(0);
        assert_eq!(max_count, TURN_GATE_MAX_TEXT_FEATURE_COUNT);
        assert!(features.iter().any(|feature| feature.count == 1));
    }

    #[test]
    fn field_boundaries_keep_identical_grams_apart() {
        // 相同文本出现在不同字段,桶索引应不同(边界标记参与哈希)。
        let input = TurnGateInput {
            current_text: "嗯嗯".to_owned(),
            recent_turns: vec![RecentTurn::new(RecentTurnRole::Assistant, "嗯嗯")],
            ..TurnGateInput::default()
        };
        let features = extract_features(&input);
        assert_eq!(features.text_features.len(), 2);
        assert_ne!(
            features.text_features[0].index,
            features.text_features[1].index
        );
    }

    #[test]
    fn context_features_occupy_fixed_positions() {
        let input = TurnGateInput {
            scope: TurnScope::Group,
            conversation_active: true,
            addressed_to_agent: true,
            has_image: true,
            policy_override: TurnPolicyOverride::MustReply,
            pending_user_fragments: vec!["等会".to_owned()],
            ..TurnGateInput::default()
        };
        let features = extract_features(&input);
        assert_eq!(
            features.context_indices,
            vec![
                context_feature_index::SCOPE_GROUP,
                context_feature_index::PENDING_NOT_EMPTY,
                context_feature_index::ADDRESSED_TO_AGENT,
                context_feature_index::CONVERSATION_ACTIVE,
                context_feature_index::HAS_IMAGE,
                context_feature_index::POLICY_MUST_REPLY,
            ]
        );
    }

    #[test]
    fn engine_abstains_without_bundle() {
        let engine = TurnGateEngine::unavailable();
        assert!(!engine.available());
        let input = TurnGateInput::default();
        assert_eq!(
            engine.classify_completion(&input).decision,
            TurnCompletion::Abstain
        );
        assert_eq!(
            engine.classify_response(&input).decision,
            TurnResponseDecision::Abstain
        );
    }

    #[test]
    fn manifest_validates_contract() {
        let manifest = sample_manifest();
        assert!(manifest.validate().is_ok());

        let mut wrong_version = sample_manifest();
        wrong_version.manifest_version = 2;
        assert_eq!(
            wrong_version.validate(),
            Err(TurnGateManifestError::Version {
                actual: 2,
                expected: TURN_GATE_MANIFEST_VERSION,
            })
        );

        let mut wrong_buckets = sample_manifest();
        wrong_buckets.hash_buckets = 32;
        assert_eq!(
            wrong_buckets.validate(),
            Err(TurnGateManifestError::HashBuckets {
                actual: 32,
                expected: TURN_GATE_HASH_BUCKETS,
            })
        );

        let mut wrong_feature = sample_manifest();
        wrong_feature.feature_version = "char-1-v1".to_owned();
        assert!(wrong_feature.validate().is_err());

        let mut bad_sha = sample_manifest();
        bad_sha.assets[0].sha256 = "zz".to_owned();
        assert_eq!(bad_sha.validate(), Err(TurnGateManifestError::AssetSha256));
    }

    #[test]
    fn golden_vector_matches_python_reference_extractor() {
        // 与 tools/turngate/features.py 的 SAMPLE 逐项一致(样本: 收微私聊
        // "我想问你一件事", 上下文含 user/assistant 两轮)。Phase 0 验收:
        // Rust 与离线训练器对相同样本得到完全一致的非零特征索引。
        let input = TurnGateInput {
            current_text: "我想问你一件事".to_owned(),
            recent_turns: vec![
                RecentTurn::new(RecentTurnRole::User, "最近准备去哪里玩"),
                RecentTurn::new(RecentTurnRole::Assistant, "还没有决定"),
            ],
            scope: TurnScope::Private,
            conversation_active: true,
            ..TurnGateInput::default()
        };
        let features = extract_features(&input);
        let expected_text = vec![
            TextFeature {
                index: 10123,
                count: 1,
            },
            TextFeature {
                index: 15570,
                count: 1,
            },
            TextFeature {
                index: 8327,
                count: 1,
            },
            TextFeature {
                index: 16843,
                count: 1,
            },
            TextFeature {
                index: 15609,
                count: 1,
            },
            TextFeature {
                index: 8840,
                count: 1,
            },
            TextFeature {
                index: 37463,
                count: 1,
            },
            TextFeature {
                index: 51575,
                count: 1,
            },
            TextFeature {
                index: 35657,
                count: 1,
            },
            TextFeature {
                index: 62924,
                count: 1,
            },
            TextFeature {
                index: 9540,
                count: 1,
            },
            TextFeature {
                index: 59252,
                count: 1,
            },
            TextFeature {
                index: 23353,
                count: 1,
            },
            TextFeature {
                index: 41142,
                count: 1,
            },
            TextFeature {
                index: 1431,
                count: 1,
            },
            TextFeature {
                index: 26824,
                count: 1,
            },
            TextFeature {
                index: 17990,
                count: 1,
            },
            TextFeature {
                index: 20317,
                count: 1,
            },
            TextFeature {
                index: 39100,
                count: 1,
            },
            TextFeature {
                index: 37700,
                count: 1,
            },
            TextFeature {
                index: 33154,
                count: 1,
            },
            TextFeature {
                index: 21516,
                count: 1,
            },
            TextFeature {
                index: 32324,
                count: 1,
            },
            TextFeature {
                index: 18416,
                count: 1,
            },
            TextFeature {
                index: 31046,
                count: 1,
            },
            TextFeature {
                index: 48982,
                count: 1,
            },
            TextFeature {
                index: 26870,
                count: 1,
            },
            TextFeature {
                index: 36158,
                count: 1,
            },
            TextFeature {
                index: 11654,
                count: 1,
            },
            TextFeature {
                index: 52148,
                count: 1,
            },
            TextFeature {
                index: 40408,
                count: 1,
            },
            TextFeature {
                index: 32816,
                count: 1,
            },
            TextFeature {
                index: 23482,
                count: 1,
            },
            TextFeature {
                index: 41460,
                count: 1,
            },
            TextFeature {
                index: 65382,
                count: 1,
            },
            TextFeature {
                index: 4900,
                count: 1,
            },
            TextFeature {
                index: 34744,
                count: 1,
            },
            TextFeature {
                index: 42416,
                count: 1,
            },
            TextFeature {
                index: 51588,
                count: 1,
            },
            TextFeature {
                index: 44170,
                count: 1,
            },
            TextFeature {
                index: 34160,
                count: 1,
            },
            TextFeature {
                index: 16226,
                count: 1,
            },
            TextFeature {
                index: 23417,
                count: 1,
            },
            TextFeature {
                index: 17865,
                count: 1,
            },
            TextFeature {
                index: 23401,
                count: 1,
            },
            TextFeature {
                index: 50366,
                count: 1,
            },
            TextFeature {
                index: 46268,
                count: 1,
            },
            TextFeature {
                index: 30255,
                count: 1,
            },
            TextFeature {
                index: 485,
                count: 1,
            },
            TextFeature {
                index: 1246,
                count: 1,
            },
        ];
        assert_eq!(features.text_features, expected_text);
        assert_eq!(features.context_indices, vec![0, 3, 4, 6, 9]);
    }

    #[test]
    fn metrics_count_redirects() {
        let metrics = TurnGateMetrics::default();
        metrics.record_request();
        metrics.record_completion(&CompletionOutput {
            decision: TurnCompletion::FlushNow,
            confidence: 0.9,
        });
        metrics.record_response(&ResponseOutput {
            decision: TurnResponseDecision::Wait,
            confidence: 0.8,
        });
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.completion_flush, 1);
        assert_eq!(snapshot.response_wait, 1);
        assert_eq!(snapshot.response_abstain, 0);
    }

    fn sample_manifest() -> TurnGateManifest {
        TurnGateManifest {
            manifest_version: TURN_GATE_MANIFEST_VERSION,
            model_id: "yunxi-turngate".to_owned(),
            model_version: "v0.2.0".to_owned(),
            algorithm: "hashed-char-ngram-logistic".to_owned(),
            feature_version: TURN_GATE_FEATURE_VERSION.to_owned(),
            hash_buckets: TURN_GATE_HASH_BUCKETS,
            max_text_chars: TURN_GATE_MAX_CURRENT_CHARS,
            max_pending_fragments: TURN_GATE_MAX_PENDING_FRAGMENTS,
            max_pending_fragment_chars: TURN_GATE_MAX_FRAGMENT_CHARS,
            max_recent_turns: TURN_GATE_MAX_RECENT_TURNS,
            max_recent_turn_chars: TURN_GATE_MAX_FRAGMENT_CHARS,
            max_question_chars: TURN_GATE_MAX_QUESTION_CHARS,
            completion_labels: vec!["flush_now".to_owned(), "hold_for_more".to_owned()],
            response_labels: vec![
                "answer".to_owned(),
                "continue".to_owned(),
                "ack".to_owned(),
                "ignore".to_owned(),
                "wait".to_owned(),
            ],
            training_data_version: "local-dataset-v0".to_owned(),
            assets: vec![TurnGateManifestAsset {
                path: "turn_gate.bin".to_owned(),
                sha256: "a".repeat(64),
                size_bytes: Some(0),
            }],
        }
    }
}
