//! Getting JSON back out of a model that was asked for JSON.
//!
//! Small models wrap answers in prose, in ``` fences, or in both. Rather than
//! insisting they behave, we dig the object out. This is the single most
//! load-bearing piece of glue when the workers are cheap models.

use gitai_core::error::{Error, Result};
use serde::de::DeserializeOwned;

/// Extracts the first balanced JSON object or array from `text`.
///
/// Handles fenced blocks, leading commentary and trailing chatter. Braces
/// inside string literals and escaped quotes do not confuse the scan.
pub fn extract_json(text: &str) -> Option<&str> {
    let candidate = strip_fence(text);

    // If the stripped text is already a clean JSON object or array, take it directly
    let trimmed = candidate.trim();
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        return Some(trimmed);
    }

    let bytes = candidate.as_bytes();
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut opener = b'{';

    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                if depth == 0 {
                    start = Some(i);
                    opener = b;
                }
                depth += 1;
            }
            b'}' | b']' => {
                let matches_opener = (opener == b'{' && b == b'}') || (opener == b'[' && b == b']');
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 && matches_opener {
                        return start.map(|s| &candidate[s..=i]);
                    }
                }
            }
            _ => {}
        }
    }

    // Fallback: slice from first '{' to last '}'
    if let (Some(first), Some(last)) = (candidate.find('{'), candidate.rfind('}')) {
        if first < last {
            return Some(&candidate[first..=last]);
        }
    }
    if let (Some(first), Some(last)) = (candidate.find('['), candidate.rfind(']')) {
        if first < last {
            return Some(&candidate[first..=last]);
        }
    }

    None
}

/// Returns the contents of the first ``` fence, or the whole input when there
/// is no fence.
fn strip_fence(text: &str) -> &str {
    let Some(open) = text.find("```") else {
        return text;
    };
    let after = &text[open + 3..];
    // Skip an optional language tag on the opening line.
    let body_start = after.find('\n').map(|n| n + 1).unwrap_or(0);
    let body = &after[body_start..];
    match body.find("```") {
        Some(close) => &body[..close],
        None => body,
    }
}

/// Parses `text` into `T`, digging the JSON out first.
pub fn parse_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    let raw = extract_json(text)
        .ok_or_else(|| Error::bad_output(format!("no JSON found in:\n{}", head(text, 400))))?;
    serde_json::from_str(raw)
        .map_err(|e| Error::bad_output(format!("{e}\nwhile parsing:\n{}", head(raw, 400))))
}

fn head(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Sample {
        goal: String,
        n: u32,
    }

    #[test]
    fn plain_object() {
        let v: Sample = parse_json(r#"{"goal":"x","n":1}"#).unwrap();
        assert_eq!(v.goal, "x");
        assert_eq!(v.n, 1);
    }

    #[test]
    fn fenced_with_language_tag_and_prose() {
        let text =
            "Sure, here you go:\n```json\n{\"goal\":\"fix it\",\"n\":2}\n```\nHope that helps.";
        let v: Sample = parse_json(text).unwrap();
        assert_eq!(v.goal, "fix it");
    }

    #[test]
    fn braces_inside_strings_do_not_break_the_scan() {
        let text = r#"prefix {"goal":"use {} carefully \" ok","n":3} suffix"#;
        let v: Sample = parse_json(text).unwrap();
        assert_eq!(v.goal, r#"use {} carefully " ok"#);
        assert_eq!(v.n, 3);
    }

    #[test]
    fn nested_objects_return_the_outer_one() {
        let raw = extract_json(r#"{"a":{"b":[1,2]},"c":1} trailing"#).unwrap();
        assert_eq!(raw, r#"{"a":{"b":[1,2]},"c":1}"#);
    }

    #[test]
    fn arrays_are_supported() {
        let raw = extract_json("here: [1, 2, 3] done").unwrap();
        assert_eq!(raw, "[1, 2, 3]");
    }

    #[test]
    fn missing_json_is_an_error_naming_the_output() {
        let err = parse_json::<Sample>("I would rather not")
            .unwrap_err()
            .to_string();
        assert!(err.contains("rather not"), "{err}");
    }
}
