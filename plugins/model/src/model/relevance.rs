//! 轻量词法相关性(软注意力信号)。
//!
//! 未点名群消息是否值得花一次语义评估(then 一次模型生成),旧实现完全由
//! 均匀随机采样决定。这里提供一个零依赖、零外部 API 的相关性分数:
//! 消息与"本群近况"(最近若干条消息文本)的语义近似度。相关性越高,
//! 采样概率越高,让注意力偏向仍在延续的话题;完全无关的消息仍保留低
//! 基数概率,避免"话题突变但值得接"的发言被系统性漏掉。
//!
//! 特征引擎只使用 CJK 双字组与 ASCII 词的带权词袋 + 余弦相似度,简单、
//! 确定、可完全离线测试。未来可替换为 embedding provider 而不改调用方。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// 单条消息纳入近因窗口的最大字符数(防长文占满缓冲)。
const TRACKED_MESSAGE_MAX_CHARS: usize = 160;
/// 每条消息的上下文权重:窗口内位置越靠后(越新)权重越高。
const RECENCY_WEIGHT_BASE: f32 = 0.6;

fn stable_token_hash(token: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

/// 把文本转为带权词袋特征(CJK 双字组 + ASCII 词)。
pub(crate) fn text_features(text: &str) -> HashMap<u64, f32> {
    let mut features: HashMap<u64, f32> = HashMap::new();
    let lower = text.to_lowercase();
    let mut ascii_word = String::new();
    let chars: Vec<char> = lower.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character.is_ascii_alphanumeric() || character == '_' {
            ascii_word.push(character);
            index += 1;
            continue;
        }
        if !ascii_word.is_empty() {
            if ascii_word.len() >= 2 {
                *features
                    .entry(stable_token_hash(&ascii_word))
                    .or_insert(0.0) += 1.0;
            }
            ascii_word.clear();
        }
        if is_cjk(character)
            && let Some(next) = chars.get(index + 1)
            && is_cjk(*next)
        {
            let token = format!("{character}{next}");
            *features.entry(stable_token_hash(&token)).or_insert(0.0) += 1.0;
            // 单字特征(权重 0.5)提升同主题变体间的重叠,泛化不同措辞。
            let unigram = character.to_string();
            *features.entry(stable_token_hash(&unigram)).or_insert(0.0) += 0.5;
            index += 1;
            continue;
        }
        index += 1;
    }
    if !ascii_word.is_empty() && ascii_word.len() >= 2 {
        *features
            .entry(stable_token_hash(&ascii_word))
            .or_insert(0.0) += 1.0;
    }
    features
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x4E00..=0x9FFF
            | 0x3400..=0x4DBF
            | 0xF900..=0xFAFF
            | 0x3040..=0x30FF
            | 0xAC00..=0xD7AF
    )
}

/// 两个词袋特征的余弦相似度(0..1)。
pub(crate) fn cosine(a: &HashMap<u64, f32>, b: &HashMap<u64, f32>) -> f32 {
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    for (token, weight) in a {
        norm_a += weight * weight;
        if let Some(other) = b.get(token) {
            dot += weight * other;
        }
    }
    let mut norm_b = 0.0_f32;
    for weight in b.values() {
        norm_b += weight * weight;
    }
    if norm_a <= f32::EPSILON || norm_b <= f32::EPSILON {
        return 0.0;
    }
    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(0.0, 1.0)
}

/// 消息与近况窗口的相关性:对窗口内每条消息算 cosine,按新鲜度加权取
/// 对数-logsum 聚合(避免单条高度相似就封顶)。
pub(crate) fn message_context_relevance<'a>(
    text: &str,
    recent: impl Iterator<Item = &'a str>,
) -> f32 {
    let message_features = text_features(text);
    if message_features.is_empty() {
        return 0.0;
    }
    let samples: Vec<f32> = recent
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| cosine(&message_features, &text_features(entry)))
        .collect();
    if samples.is_empty() {
        return 0.0;
    }
    // 越新权重越高;log-sum 按热力聚合,稳定地向"存在高相关片段"靠拢。
    let total = samples.len() as f32;
    let weights: Vec<f32> = (0..samples.len())
        .map(|i| RECENCY_WEIGHT_BASE + (1.0 - RECENCY_WEIGHT_BASE) * (i as f32 / total.max(1.0)))
        .collect();
    let weight_sum: f32 = weights.iter().sum();
    let mut weighted_log = 0.0_f32;
    for (score, weight) in samples.iter().zip(weights.iter()) {
        weighted_log += weight * (1.0 + score).ln();
    }
    let log_mean = weighted_log / weight_sum.max(f32::EPSILON);
    (log_mean.exp() - 1.0).clamp(0.0, 1.0)
}

/// 截断一条消息供近因窗口存储。
pub(crate) fn tracked_message(text: &str) -> String {
    let mut trimmed = text.trim();
    if trimmed.chars().count() > TRACKED_MESSAGE_MAX_CHARS {
        let end = trimmed
            .char_indices()
            .nth(TRACKED_MESSAGE_MAX_CHARS)
            .map(|(byte, _)| byte)
            .unwrap_or(trimmed.len());
        trimmed = &trimmed[..end];
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_cjk_messages_score_higher_than_unrelated() {
        let recent = [
            "明天去动物园看熊猫吗".to_string(),
            "熊猫馆新来的那只特别能睡".to_string(),
        ];
        let related = message_context_relevance(
            "那明天去看熊猫吧,顺便问问几点开馆",
            recent.iter().map(String::as_str),
        );
        let unrelated =
            message_context_relevance("哈哈哈今晚吃什么", recent.iter().map(String::as_str));
        assert!(
            related > unrelated + 0.15,
            "related={related} unrelated={unrelated}"
        );
    }

    #[test]
    fn empty_or_short_text_has_zero_relevance() {
        assert_eq!(message_context_relevance("", std::iter::empty()), 0.0);
        assert_eq!(
            message_context_relevance("哈", vec!["哈哈哈"].into_iter()),
            0.0
        );
    }

    #[test]
    fn recency_weights_prefer_newer_entries() {
        let related_old = message_context_relevance(
            "熊猫好可爱",
            ["熊猫好可爱".to_string(), "今晚吃什么".to_string()]
                .iter()
                .map(String::as_str),
        );
        let related_new = message_context_relevance(
            "熊猫好可爱",
            ["今晚吃什么".to_string(), "熊猫好可爱".to_string()]
                .iter()
                .map(String::as_str),
        );
        assert!(related_new > related_old);
    }

    #[test]
    fn tracked_message_is_bounded_and_trimmed() {
        let long = "长".repeat(400);
        assert_eq!(tracked_message(&long).chars().count(), 160);
        assert_eq!(tracked_message("  你好  "), "你好");
    }

    #[test]
    fn ascii_word_features_are_case_insensitive() {
        let a = text_features("Rust borrow checker");
        let b = text_features("rust borrow checker");
        let related = text_features("rust compiler borrow rules");
        assert!(cosine(&a, &b) > 0.99);
        assert!(cosine(&a, &related) > 0.5);
        assert!(cosine(&a, &text_features("晚饭吃饺子")) < 0.05);
    }
}
