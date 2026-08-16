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
    // 1. If text is already a trimmed JSON object or array, check if balanced
    let trimmed = text.trim();
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        if is_balanced(trimmed) {
            return Some(trimmed);
        }
    }

    // 2. If there are code fences, look inside them
    for block in extract_code_blocks(text) {
        let b_trimmed = block.trim();
        if (b_trimmed.starts_with('{') && b_trimmed.ends_with('}'))
            || (b_trimmed.starts_with('[') && b_trimmed.ends_with(']'))
        {
            if is_balanced(b_trimmed) {
                return Some(b_trimmed);
            }
        }
        if let Some(inner) = scan_balanced(block) {
            return Some(inner);
        }
    }

    // 3. Scan the whole text for the first balanced JSON object or array
    if let Some(extracted) = scan_balanced(text) {
        return Some(extracted);
    }

    // 4. Fallback: slice from first '{' to last '}' or '[' to ']'
    if let (Some(first), Some(last)) = (text.find('{'), text.rfind('}')) {
        if first < last {
            return Some(&text[first..=last]);
        }
    }
    if let (Some(first), Some(last)) = (text.find('['), text.rfind(']')) {
        if first < last {
            return Some(&text[first..=last]);
        }
    }

    None
}

/// Checks whether `text` contains a balanced JSON structure without unclosed strings or unbalanced braces.
fn is_balanced(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut opener = None;

    for &b in bytes {
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
                    opener = Some(b);
                }
                depth += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    return false;
                }
                let expected = match opener {
                    Some(b'{') => b'}',
                    Some(b'[') => b']',
                    _ => return false,
                };
                depth -= 1;
                if depth == 0 && b != expected {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_string && opener.is_some()
}

/// Scans `text` looking for balanced JSON objects/arrays. Prioritizes spans that parse as JSON.
fn scan_balanced(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut first_fallback = None;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'{' || b == b'[' {
            let opener = b;
            let start = i;
            let mut depth = 1usize;
            let mut in_string = false;
            let mut escaped = false;
            let mut j = i + 1;

            while j < bytes.len() {
                let curr = bytes[j];
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if curr == b'\\' {
                        escaped = true;
                    } else if curr == b'"' {
                        in_string = false;
                    }
                } else {
                    match curr {
                        b'"' => in_string = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            let matches_opener =
                                (opener == b'{' && curr == b'}') || (opener == b'[' && curr == b']');
                            depth -= 1;
                            if depth == 0 && matches_opener {
                                let candidate = &text[start..=j];
                                if serde_json::from_str::<serde_json::Value>(candidate).is_ok()
                                    || serde_json::from_str::<serde_json::Value>(&repair_json(candidate)).is_ok()
                                {
                                    return Some(candidate);
                                }
                                if first_fallback.is_none() {
                                    first_fallback = Some(candidate);
                                }
                                i = j;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
        }
        i += 1;
    }

    first_fallback
}

/// Scans all balanced JSON objects/arrays in `text`.
fn scan_all_balanced(text: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'{' || b == b'[' {
            let opener = b;
            let start = i;
            let mut depth = 1usize;
            let mut in_string = false;
            let mut escaped = false;
            let mut j = i + 1;

            while j < bytes.len() {
                let curr = bytes[j];
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if curr == b'\\' {
                        escaped = true;
                    } else if curr == b'"' {
                        in_string = false;
                    }
                } else {
                    match curr {
                        b'"' => in_string = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            let matches_opener =
                                (opener == b'{' && curr == b'}') || (opener == b'[' && curr == b']');
                            depth -= 1;
                            if depth == 0 && matches_opener {
                                results.push(&text[start..=j]);
                                i = j;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
        }
        i += 1;
    }

    results
}

/// Returns the contents of markdown ``` fences in `text`.
fn extract_code_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;

    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let body_start = after.find('\n').map(|n| n + 1).unwrap_or(after.len());
        let body = &after[body_start..];

        // Find closing fence at the end of this block
        if let Some(close) = body.rfind("```") {
            let block = &body[..close];
            blocks.push(block);
            let after_close = &body[close + 3..];
            let next_start = after_close.find('\n').map(|n| n + 1).unwrap_or(after_close.len());
            rest = &after_close[next_start..];
        } else {
            blocks.push(body);
            break;
        }
    }

    blocks
}

/// Repairs common LLM JSON generation quirks:
/// 1. Unescaped control characters in string literals (literal newlines, carriage returns, tabs).
/// 2. Trailing commas before `}` or `]`.
pub fn repair_json(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    let mut last_non_ws_comma = None;

    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
                out.push(b as char);
            } else if b == b'\\' {
                escaped = true;
                out.push('\\');
            } else if b == b'"' {
                in_string = false;
                out.push('"');
            } else if b == b'\n' {
                out.push_str("\\n");
            } else if b == b'\r' {
                out.push_str("\\r");
            } else if b == b'\t' {
                out.push_str("\\t");
            } else if b < 0x20 {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", b);
            } else {
                let ch = raw[i..].chars().next().unwrap_or(b as char);
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => {
                in_string = true;
                last_non_ws_comma = None;
                out.push('"');
            }
            b',' => {
                last_non_ws_comma = Some(out.len());
                out.push(',');
            }
            b'}' | b']' => {
                if let Some(pos) = last_non_ws_comma {
                    out.remove(pos);
                    last_non_ws_comma = None;
                }
                out.push(b as char);
            }
            b' ' | b'\t' | b'\n' | b'\r' => {
                out.push(b as char);
            }
            _ => {
                last_non_ws_comma = None;
                let ch = raw[i..].chars().next().unwrap_or(b as char);
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
        i += 1;
    }

    out
}

/// Parses `text` into `T`, digging the JSON out first and repairing minor model quirks.
pub fn parse_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    // 1. Try directly parsing the raw trimmed text
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<T>(trimmed) {
        return Ok(v);
    }
    if let Ok(v) = serde_json::from_str::<T>(&repair_json(trimmed)) {
        return Ok(v);
    }

    // 2. Try code blocks if present
    for block in extract_code_blocks(text) {
        let b_trimmed = block.trim();
        if let Ok(v) = serde_json::from_str::<T>(b_trimmed) {
            return Ok(v);
        }
        if let Ok(v) = serde_json::from_str::<T>(&repair_json(b_trimmed)) {
            return Ok(v);
        }
        if let Some(candidate) = extract_json(block) {
            if let Ok(v) = serde_json::from_str::<T>(candidate) {
                return Ok(v);
            }
            if let Ok(v) = serde_json::from_str::<T>(&repair_json(candidate)) {
                return Ok(v);
            }
        }
    }

    // 3. Try extract_json on the full text
    if let Some(raw) = extract_json(text) {
        if let Ok(v) = serde_json::from_str::<T>(raw) {
            return Ok(v);
        }
        if let Ok(v) = serde_json::from_str::<T>(&repair_json(raw)) {
            return Ok(v);
        }
    }

    // 4. Scan all balanced JSON spans in text
    for span in scan_all_balanced(text) {
        if let Ok(v) = serde_json::from_str::<T>(span) {
            return Ok(v);
        }
        if let Ok(v) = serde_json::from_str::<T>(&repair_json(span)) {
            return Ok(v);
        }
    }

    // 5. If everything failed, produce a helpful error
    let raw = extract_json(text).unwrap_or(text);
    match serde_json::from_str::<T>(raw) {
        Ok(v) => Ok(v),
        Err(e) => {
            let err_msg = e.to_string();
            let repaired = repair_json(raw);
            match serde_json::from_str::<T>(&repaired) {
                Ok(v) => Ok(v),
                Err(_) => {
                    if raw.trim().is_empty() || (!raw.contains('{') && !raw.contains('[')) {
                        Err(Error::bad_output(format!("no JSON found in:\n{}", head(text, 400))))
                    } else {
                        Err(Error::bad_output(format!("{err_msg}\nwhile parsing:\n{}", head(raw, 400))))
                    }
                }
            }
        }
    }
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

    #[derive(Debug, Deserialize, PartialEq)]
    struct Sample {
        goal: String,
        n: u32,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct WorkerSample {
        reasoning: String,
        #[serde(default)]
        read: Vec<String>,
        #[serde(default)]
        edits: Vec<EditSample>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct EditSample {
        op: String,
        path: String,
        #[serde(default)]
        content: String,
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

    #[test]
    fn fenced_with_inner_code_blocks_in_content() {
        let text = "```json\n{\n  \"reasoning\": \"Реализую терминальный калькулятор\",\n  \"read\": [],\n  \"edits\": [\n    {\n      \"op\": \"write\",\n      \"path\": \"README.md\",\n      \"content\": \"# Calculator\\n\\n```bash\\npython main.py\\n```\\n\"\n    }\n  ]\n}\n```";
        let v: WorkerSample = parse_json(text).unwrap();
        assert_eq!(v.reasoning, "Реализую терминальный калькулятор");
        assert_eq!(v.edits.len(), 1);
        assert_eq!(v.edits[0].path, "README.md");
        assert!(v.edits[0].content.contains("```bash\npython main.py\n```"));
    }

    #[test]
    fn unfenced_with_inner_code_blocks_and_cyrillic() {
        let text = "{\n  \"reasoning\": \"Реализую терминальный калькулятор: вынесу логику вычислений в calculator.py, а интерактивный цикл с обработкой ошибок (деление на ноль, нечисловой ввод, EOFError/KeyboardInterrupt) - в main.py. Обновлю README.md с кратким описанием запуска\",\n  \"read\": [],\n  \"edits\": [\n    {\n      \"op\": \"write\",\n      \"path\": \"README.md\",\n      \"content\": \"```bash\\npython main.py\\n```\"\n    }\n  ]\n}";
        let v: WorkerSample = parse_json(text).unwrap();
        assert!(v.reasoning.contains("калькулятор"));
        assert_eq!(v.edits.len(), 1);
        assert_eq!(v.edits[0].content, "```bash\npython main.py\n```");
    }

    #[test]
    fn raw_unescaped_newlines_in_strings_are_repaired() {
        let text = "{\n  \"goal\": \"Line 1\nLine 2\",\n  \"n\": 42\n}";
        let v: Sample = parse_json(text).unwrap();
        assert_eq!(v.goal, "Line 1\nLine 2");
        assert_eq!(v.n, 42);
    }

    #[test]
    fn trailing_commas_are_repaired() {
        let text = "{\n  \"goal\": \"ok\",\n  \"n\": 5,\n}";
        let v: Sample = parse_json(text).unwrap();
        assert_eq!(v.goal, "ok");
        assert_eq!(v.n, 5);
    }

    #[test]
    fn braces_in_leading_prose_do_not_distract_parser() {
        let text = "Note: formatting string like {param} should work.\n```json\n{\"goal\":\"fixed\",\"n\":10}\n```";
        let v: Sample = parse_json(text).unwrap();
        assert_eq!(v.goal, "fixed");
        assert_eq!(v.n, 10);
    }
}

