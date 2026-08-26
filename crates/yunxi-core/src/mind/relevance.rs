use std::collections::HashSet;

pub const MAX_LEXICAL_TERMS: usize = 64;

const ASCII_STOP_WORDS: &[&str] = &[
    "about", "also", "and", "are", "bad", "best", "but", "can", "could", "did", "do", "does",
    "for", "from", "good", "have", "how", "into", "is", "it", "like", "not", "of", "on", "or",
    "should", "that", "the", "this", "to", "value", "was", "were", "what", "when", "which", "who",
    "why", "with", "would", "you",
];

const CJK_STOP_TERMS: &[&str] = &[
    "一个",
    "不是",
    "什么",
    "但是",
    "你们",
    "他们",
    "价值",
    "可以",
    "因为",
    "垃圾",
    "如果",
    "已经",
    "怎么",
    "我们",
    "所以",
    "时候",
    "是不是",
    "最好",
    "有没有",
    "现在",
    "用户",
    "系统",
    "觉得",
    "这个",
    "还是",
    "那个",
    "问题",
];

/// Returns a deterministic, bounded set of meaningful lexical terms suitable
/// for both in-memory matching and persistence-adapter prefiltering.
#[must_use]
pub fn lexical_terms(value: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut ascii = String::new();
    let mut cjk = Vec::new();

    let flush_ascii = |buffer: &mut String, terms: &mut Vec<String>| {
        if buffer.len() >= 2 && !ASCII_STOP_WORDS.contains(&buffer.as_str()) {
            push_unique_bounded(terms, std::mem::take(buffer));
        } else {
            buffer.clear();
        }
    };
    let flush_cjk = |buffer: &mut Vec<char>, terms: &mut Vec<String>| {
        for pair in buffer.windows(2) {
            let term = pair.iter().collect::<String>();
            if !CJK_STOP_TERMS.contains(&term.as_str()) {
                push_unique_bounded(terms, term);
            }
        }
        buffer.clear();
    };

    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            flush_cjk(&mut cjk, &mut terms);
            ascii.push(character);
        } else if is_cjk(character) {
            flush_ascii(&mut ascii, &mut terms);
            cjk.push(character);
        } else {
            flush_ascii(&mut ascii, &mut terms);
            flush_cjk(&mut cjk, &mut terms);
        }
        if terms.len() == MAX_LEXICAL_TERMS {
            return terms;
        }
    }
    flush_ascii(&mut ascii, &mut terms);
    flush_cjk(&mut cjk, &mut terms);
    terms
}

/// Lexical overlap coefficient in `0.0..=1.0`. A non-empty query with no
/// meaningful shared topic returns zero, which represents `NoOpinion` for
/// belief/preference retrieval.
#[must_use]
pub fn lexical_relevance(value: &str, query: &str) -> f32 {
    if query.trim().is_empty() {
        return 0.0;
    }
    let value_terms = lexical_terms(value);
    let query_terms = lexical_terms(query);
    if value_terms.is_empty() || query_terms.is_empty() {
        return 0.0;
    }
    let value_terms = value_terms.into_iter().collect::<HashSet<_>>();
    let overlap = query_terms
        .iter()
        .filter(|term| value_terms.contains(*term))
        .count();
    overlap as f32 / value_terms.len().min(query_terms.len()) as f32
}

pub(crate) fn explicitly_opposes(left: &str, right: &str) -> bool {
    lexical_relevance(left, right) > 0.0
        && matches!(
            (explicit_stance(left), explicit_stance(right)),
            (Some(Stance::Positive), Some(Stance::Negative))
                | (Some(Stance::Negative), Some(Stance::Positive))
        )
}

fn push_unique_bounded(terms: &mut Vec<String>, term: String) {
    if terms.len() < MAX_LEXICAL_TERMS && !terms.contains(&term) {
        terms.push(term);
    }
}

fn is_cjk(character: char) -> bool {
    matches!(
        character,
        '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{2fa1f}'
    )
}

#[derive(Clone, Copy)]
enum Stance {
    Positive,
    Negative,
}

fn explicit_stance(value: &str) -> Option<Stance> {
    let normalized = value.to_lowercase();
    if contains_any(
        &normalized,
        &[
            "不是没有价值",
            "不是垃圾",
            "并非垃圾",
            "不糟糕",
            "不算差",
            "没那么差",
            "isn't bad",
            "is not bad",
            "not bad",
            "not garbage",
            "not useless",
        ],
    ) {
        return Some(Stance::Positive);
    }
    if contains_any(
        &normalized,
        &[
            "没有价值",
            "没价值",
            "并无价值",
            "不是很好",
            "并不好",
            "不可靠",
            "不值得",
            "不喜欢",
            "不赞成",
            "不支持",
            "不正确",
            "不合理",
            "不安全",
            "不稳定",
            "没有用",
            "没用",
            "不重要",
            "isn't valuable",
            "is not valuable",
            "not valuable",
            "not good",
            "not reliable",
            "not worth",
            "do not like",
            "don't like",
            "not useful",
            "not correct",
            "not safe",
            "not stable",
        ],
    ) {
        return Some(Stance::Negative);
    }

    let positive = contains_any(
        &normalized,
        &[
            "有价值",
            "很好",
            "优秀",
            "可靠",
            "值得",
            "喜欢",
            "赞成",
            "支持",
            "正确",
            "有用",
            "重要",
            "更好",
            "满意",
            "认可",
            "合理",
            "安全",
            "稳定",
            "推荐",
        ],
    ) || contains_ascii_word(
        &normalized,
        &[
            "excellent",
            "good",
            "helpful",
            "like",
            "reliable",
            "safe",
            "stable",
            "useful",
            "valuable",
        ],
    );
    let negative = contains_any(
        &normalized,
        &[
            "垃圾", "糟糕", "差劲", "讨厌", "有害", "错误", "危险", "不行", "很烂", "太烂", "很坏",
            "更差",
        ],
    ) || contains_ascii_word(
        &normalized,
        &[
            "awful", "bad", "garbage", "harmful", "hate", "trash", "unsafe", "useless", "worse",
            "wrong",
        ],
    );
    match (positive, negative) {
        (true, false) => Some(Stance::Positive),
        (false, true) => Some(Stance::Negative),
        (false, false) | (true, true) => None,
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn contains_ascii_word(value: &str, words: &[&str]) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| words.contains(&token))
}

#[cfg(test)]
mod tests {
    use super::{explicitly_opposes, lexical_relevance, lexical_terms};

    #[test]
    fn multilingual_terms_are_bounded_and_ignore_stance_only_overlap() {
        let terms = lexical_terms("你觉得 Rust 的类型系统有价值吗？");
        assert!(terms.iter().any(|term| term == "rust"));
        assert!(terms.iter().any(|term| term == "类型"));
        assert!(!terms.iter().any(|term| term == "价值"));
        assert!(terms.len() <= super::MAX_LEXICAL_TERMS);
    }

    #[test]
    fn relevance_requires_a_shared_meaningful_topic() {
        assert!(lexical_relevance("Rust 类型系统有价值", "Rust 是垃圾吗") > 0.0);
        assert_eq!(
            lexical_relevance("PostgreSQL 事务很可靠", "Rust 是垃圾吗"),
            0.0
        );
    }

    #[test]
    fn opposition_requires_related_and_explicitly_opposite_stances() {
        assert!(explicitly_opposes(
            "Rust 的严格类型系统总体有价值",
            "Rust 就是一坨垃圾"
        ));
        assert!(!explicitly_opposes(
            "Rust 的严格类型系统总体有价值",
            "Rust 的严格类型系统确实很有价值"
        ));
        assert!(!explicitly_opposes(
            "Rust 的严格类型系统总体有价值",
            "PostgreSQL 的错误信息很糟糕"
        ));
    }
}
