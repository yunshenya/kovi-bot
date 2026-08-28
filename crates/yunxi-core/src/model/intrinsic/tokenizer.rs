//! Small, runtime-independent context accounting helpers.

pub const APPROXIMATE_BYTES_PER_TOKEN: usize = 4;

#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(APPROXIMATE_BYTES_PER_TOKEN).max(1)
}

#[must_use]
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens
        .saturating_mul(APPROXIMATE_BYTES_PER_TOKEN)
        .max(1);
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
