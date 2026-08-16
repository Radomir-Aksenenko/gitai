//! Token estimation.
//!
//! This is an estimate and the name says so. A real tokenizer would mean
//! shipping a vocabulary per model family, and gitai is meant to work against
//! whatever a local runtime happens to be serving, where the vocabulary is
//! often unknown. So instead: a calibrated character model that is deliberately
//! pessimistic, used to decide how much context to include rather than to bill
//! anyone. Actual spend is always taken from the provider's own `usage` field.
//!
//! Calibration, against BPE vocabularies of the GPT and Llama families:
//!
//! | Content              | Chars per token |
//! |----------------------|-----------------|
//! | English prose        | ~4.0            |
//! | Source code          | ~3.2            |
//! | Cyrillic, Greek      | ~1.6            |
//! | CJK                  | ~1.0            |
//!
//! Code is the common case here, so ASCII is priced at 3.2 and anything
//! outside Latin-1 is priced by script.

/// Estimated tokens in `text`. Never returns 0 for non-empty input.
pub fn estimate(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }

    let mut ascii = 0usize;
    let mut wide = 0usize;
    let mut cjk = 0usize;

    for ch in text.chars() {
        match ch as u32 {
            0..=0x7F => ascii += 1,
            // Latin-1 supplement and extended Latin still tokenize cheaply.
            0x80..=0x24F => ascii += 1,
            // CJK, Hiragana, Katakana, Hangul: roughly one token per character.
            0x3000..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF => cjk += 1,
            // Emoji and other astral characters cost more than they look.
            0x1F000..=0x1FAFF => cjk += 2,
            // Cyrillic, Greek, Hebrew, Arabic, Devanagari and friends.
            _ => wide += 1,
        }
    }

    let estimate = (ascii as f64 / 3.2) + (wide as f64 / 1.6) + (cjk as f64);

    (estimate.ceil() as usize).max(1)
}

/// Estimated tokens across a set of strings.
pub fn estimate_all<'a>(parts: impl IntoIterator<Item = &'a str>) -> usize {
    parts.into_iter().map(estimate).sum()
}

/// Characters that fit in `tokens`, for turning a token budget back into the
/// byte limits the context builder works in. Uses the code ratio, which is the
/// conservative direction: it under-fills rather than overflows.
pub fn chars_for(tokens: usize) -> usize {
    tokens.saturating_mul(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_zero_and_anything_else_is_at_least_one() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("a"), 1);
    }

    #[test]
    fn english_prose_lands_near_four_characters_per_token() {
        let text = "The quick brown fox jumps over the lazy dog and keeps on running.";
        let est = estimate(text);
        let per_token = text.len() as f64 / est as f64;
        assert!(
            (2.5..=4.0).contains(&per_token),
            "{est} tokens for {} chars is {per_token:.2} per token",
            text.len()
        );
    }

    #[test]
    fn cyrillic_costs_about_twice_ascii_per_character() {
        // The table prices ASCII at 3.2 chars per token and Cyrillic at 1.6,
        // so the same character count should come out near double.
        let ascii = estimate(&"a".repeat(320));
        let cyrillic = estimate(&"а".repeat(320));
        let ratio = cyrillic as f64 / ascii as f64;
        assert!(
            (1.8..=2.2).contains(&ratio),
            "ascii {ascii}, cyrillic {cyrillic}, ratio {ratio:.2}"
        );
    }

    #[test]
    fn cjk_is_about_one_token_per_character() {
        let est = estimate("日本語のテキスト");
        assert!((7..=10).contains(&est), "{est}");
    }

    #[test]
    fn the_estimate_is_never_optimistic_for_code() {
        // A real BPE tokenizer puts this at roughly 20 tokens. The estimate
        // must not come in under that by much, or context planning overflows.
        let code = "pub fn estimate(text: &str) -> usize {\n    text.len() / 4\n}\n";
        let est = estimate(code);
        assert!(
            est >= 15,
            "{est} is too optimistic for {} chars",
            code.len()
        );
    }

    #[test]
    fn totals_add_up_across_parts() {
        let a = "first part";
        let b = "second part";
        assert_eq!(estimate_all([a, b]), estimate(a) + estimate(b));
    }

    #[test]
    fn the_char_budget_round_trips_conservatively() {
        let chars = chars_for(1000);
        // Whatever we claim fits in 1000 tokens must actually estimate at or
        // under 1000, otherwise the budget is a lie.
        let sample: String = "x".repeat(chars);
        assert!(estimate(&sample) <= 1000, "{} > 1000", estimate(&sample));
    }
}
