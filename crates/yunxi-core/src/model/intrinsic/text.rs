//! Text-specific helpers for the Intrinsic wrapper.

use super::tokenizer::truncate_to_tokens;

#[must_use]
pub fn bounded_text_prompt(prompt: &str, max_context_tokens: usize) -> String {
    truncate_to_tokens(prompt, max_context_tokens)
}
