//! Semantic input-completion classification used by the host message queue.
//!
//! The lexical pass is intentionally conservative. It only decides cases that
//! are unambiguous; the grey area is sent to the embedded model. A timeout is
//! still allowed as a liveness watchdog, but it is never the semantic answer.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputCompletion {
    Complete,
    Incomplete,
}

/// Return a result only when punctuation or syntax makes the answer clear.
#[must_use]
pub fn lexical_completion(text: &str) -> Option<InputCompletion> {
    let text = text.trim();
    if text.is_empty() {
        return Some(InputCompletion::Incomplete);
    }
    if has_unclosed_structure(text) {
        return Some(InputCompletion::Incomplete);
    }

    if text.ends_with([
        '。', '！', '？', '!', '?', '.', '。', '～', '~', '”', '"', '）', ')', '】', ']',
    ]) {
        return Some(InputCompletion::Complete);
    }
    if text.ends_with([
        '，', ',', '、', '：', ':', '；', ';', '…', '—', '-', '/', '\\', '和', '与', '但',
    ]) || ["因为", "所以", "如果", "然后", "以及"]
        .iter()
        .any(|ending| text.ends_with(ending))
    {
        return Some(InputCompletion::Incomplete);
    }

    if ["我想问", "我有个问题", "请问一下", "我想知道"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
        && !text.ends_with(['吗', '呢', '?', '？'])
    {
        return Some(InputCompletion::Incomplete);
    }

    // These short utterances are complete despite having no terminal mark.
    if [
        "你好",
        "嗨",
        "哈喽",
        "谢谢",
        "感谢",
        "多谢",
        "好的",
        "好呀",
        "收到",
        "知道了",
        "明白了",
        "晚安",
        "早安",
        "再见",
        "嗯",
        "哦",
        "哈哈",
        "哈哈哈",
    ]
    .contains(&text)
    {
        return Some(InputCompletion::Complete);
    }

    // A Chinese question frequently omits '?', but these endings are still
    // strong enough to flush immediately.
    if text.ends_with(['吗', '呢'])
        || text.ends_with("怎么样")
        || text.ends_with("怎么了")
        || text.ends_with("好不好")
        || text.ends_with("是什么")
        || text.ends_with("可以吗")
        || text.ends_with("行不行")
    {
        return Some(InputCompletion::Complete);
    }

    None
}

/// Prompt for the small model's binary classifier. The output contract is
/// deliberately tiny so a malformed or verbose answer can fail closed.
#[must_use]
pub fn completion_prompt(text: &str) -> String {
    format!(
        "<|im_start|>system\n你是消息完成度分类器。判断用户这条输入是否已经说完。\n示例：\n用户：你好\n分类：完\n用户：我想问你一件事\n分类：未\n用户：你今天过得怎么样\n分类：完\n请只在心里判断，最后一个字应该是‘完’或‘未’。<|im_end|>\n<|im_start|>user\n用户：{}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n分类：",
        text.trim()
    )
}

/// Parse the strict classifier result, accepting a few common model variants
/// while refusing to infer a result from unrelated prose.
#[must_use]
pub fn parse_input_completion(output: &str) -> Option<InputCompletion> {
    let output = output.trim();
    if output.is_empty() {
        return None;
    }

    for (start, character) in output
        .char_indices()
        .filter(|(_, character)| *character == '{')
    {
        let Some(end) = output[start..].find('}') else {
            continue;
        };
        let candidate = &output[start..start + end + 1];
        let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) else {
            continue;
        };
        if let Some(complete) = value.get("complete").and_then(serde_json::Value::as_bool) {
            return Some(if complete {
                InputCompletion::Complete
            } else {
                InputCompletion::Incomplete
            });
        }
        let _ = character;
    }

    let normalized = output.to_ascii_lowercase();
    let compact = normalized.split_whitespace().collect::<String>();
    if output == "完"
        || output.starts_with("完\n")
        || output.starts_with("完：")
        || compact == "true"
        || compact == "complete"
        || compact == "completed"
        || output == "完整"
        || output.starts_with("完整\n")
        || output.starts_with("完整：")
    {
        return Some(InputCompletion::Complete);
    }
    if output == "未"
        || output.starts_with("未\n")
        || output.starts_with("未：")
        || compact == "false"
        || compact == "incomplete"
        || compact == "unfinished"
        || output == "未完成"
        || output == "还没说完"
        || output.starts_with("未完成\n")
        || output.starts_with("未完成：")
    {
        return Some(InputCompletion::Incomplete);
    }
    None
}

fn has_unclosed_structure(text: &str) -> bool {
    let mut stack = Vec::new();
    let mut in_ascii_quote = false;
    let mut in_code = false;
    let mut backtick_run = 0_usize;

    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '`' {
            backtick_run += 1;
            if chars.peek() != Some(&'`') {
                if backtick_run >= 3 {
                    in_code = !in_code;
                }
                backtick_run = 0;
            }
            continue;
        }
        backtick_run = 0;
        if in_code {
            continue;
        }
        match character {
            '"' => in_ascii_quote = !in_ascii_quote,
            '(' | '[' | '{' | '（' | '【' | '「' | '『' => stack.push(character),
            ')' => close_structure(&mut stack, '('),
            ']' => close_structure(&mut stack, '['),
            '}' => close_structure(&mut stack, '{'),
            '）' => close_structure(&mut stack, '（'),
            '】' => close_structure(&mut stack, '【'),
            '」' => close_structure(&mut stack, '「'),
            '』' => close_structure(&mut stack, '『'),
            _ => {}
        }
    }
    in_code || in_ascii_quote || !stack.is_empty()
}

fn close_structure(stack: &mut Vec<char>, expected: char) {
    if stack.last() == Some(&expected) {
        stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::{InputCompletion, completion_prompt, lexical_completion, parse_input_completion};

    #[test]
    fn lexical_pass_only_flushes_unambiguous_inputs() {
        assert_eq!(lexical_completion("你好"), Some(InputCompletion::Complete));
        assert_eq!(
            lexical_completion("你先听我说，"),
            Some(InputCompletion::Incomplete)
        );
        assert_eq!(lexical_completion("你先听我说"), None);
        assert_eq!(
            lexical_completion("你好吗？"),
            Some(InputCompletion::Complete)
        );
        assert_eq!(
            lexical_completion("请看一下（"),
            Some(InputCompletion::Incomplete)
        );
    }

    #[test]
    fn classifier_parser_requires_a_known_shape() {
        assert_eq!(
            parse_input_completion(r#"{"complete":true}"#),
            Some(InputCompletion::Complete)
        );
        assert_eq!(
            parse_input_completion("未\n用户："),
            Some(InputCompletion::Incomplete)
        );
        assert_eq!(
            parse_input_completion("<think>done</think>\n{\"complete\":false}"),
            Some(InputCompletion::Incomplete)
        );
        assert_eq!(parse_input_completion("我觉得应该完整"), None);
    }

    #[test]
    fn prompt_contains_the_untrusted_input_and_label_contract() {
        let prompt = completion_prompt("你好，芸汐");
        assert!(prompt.contains("你好，芸汐"));
        assert!(prompt.contains("‘完’或‘未’"));
    }
}
