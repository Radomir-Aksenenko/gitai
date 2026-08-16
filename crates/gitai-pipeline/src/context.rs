//! Assembling what a model is allowed to see.
//!
//! Every byte here is paid for on every call, and a small model drowns long
//! before it hits its context limit. So the tree is truncated, file contents
//! are capped, and the worker asks for what it actually needs instead of being
//! handed the repository.

use gitai_core::error::Result;
use gitai_core::sandbox::Workspace;
use serde::Serialize;

/// One file, as shown to a model.
#[derive(Debug, Clone, Serialize)]
pub struct OpenFile {
    pub path: String,
    pub content: String,
    /// True when the content shown is only the head of the file.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ContextLimits {
    pub max_tree_entries: usize,
    /// Cap on a single file, so one large file cannot crowd out the rest.
    pub max_file_bytes: usize,
    /// Cap on all opened files together. Without this the per-file cap and the
    /// file count multiply, and the budget is out by the number of files.
    pub max_files_total_bytes: usize,
    pub max_open_files: usize,
    pub max_readme_bytes: usize,
    pub max_diff_bytes: usize,
}

/// Share of a model's window reserved for its own answer, the prompt template
/// and the estimator being wrong. A worker replying with a whole rewritten file
/// needs real room, so this is not a token or two of slack.
const OUTPUT_RESERVE: f64 = 0.35;

/// Assumed cost of one line in the file tree, path plus newline.
const TREE_ENTRY_BYTES: usize = 60;

impl Default for ContextLimits {
    /// Sized for a 32k window, which is what a local coder model usually has.
    fn default() -> Self {
        Self::for_context(32_000)
    }
}

impl ContextLimits {
    /// Derives byte limits from a model's context window.
    ///
    /// Everything downstream measures in bytes, because that is what can be
    /// trimmed on a character boundary. The conversion is deliberately
    /// pessimistic: see [`gitai_llm::tokens`].
    pub fn for_context(context_tokens: u32) -> Self {
        let usable = (context_tokens as f64 * (1.0 - OUTPUT_RESERVE)) as usize;
        let chars = gitai_llm::tokens::chars_for(usable);

        // The four consumers split the budget between them and the shares add
        // up to one whole, so a context built to these limits fits.
        let files_total = chars * 45 / 100;
        let tree_bytes = chars * 10 / 100;

        Self {
            // The tree is a flat list of paths, so entries rather than bytes.
            max_tree_entries: (tree_bytes / TREE_ENTRY_BYTES).clamp(40, 600),
            max_file_bytes: (files_total / 3).clamp(2_000, 60_000),
            max_files_total_bytes: files_total.max(4_000),
            max_open_files: 12,
            max_readme_bytes: (chars * 5 / 100).clamp(500, 8_000),
            max_diff_bytes: (chars * 40 / 100).clamp(4_000, 120_000),
        }
    }

    /// Rough ceiling on what a context built to these limits can cost, in
    /// tokens. Used by the test that keeps the shares honest.
    pub fn estimated_ceiling_tokens(&self) -> usize {
        let bytes = self.max_tree_entries * TREE_ENTRY_BYTES
            + self.max_files_total_bytes
            + self.max_readme_bytes
            + self.max_diff_bytes;
        bytes / 3
    }
}

/// Newline-separated file list, truncated with a count of what was hidden.
pub async fn file_tree(ws: &dyn Workspace, limits: &ContextLimits) -> Result<String> {
    let mut files = ws.list_files().await?;
    files.sort();
    let total = files.len();

    if total <= limits.max_tree_entries {
        return Ok(files.join("\n"));
    }

    let shown = &files[..limits.max_tree_entries];
    Ok(format!(
        "{}\n... and {} more files not shown",
        shown.join("\n"),
        total - limits.max_tree_entries
    ))
}

/// Reads the requested paths, skipping what does not exist or is not text.
///
/// A missing file is silently dropped rather than raised: models routinely ask
/// for paths they inferred, and failing the whole turn over it wastes an
/// iteration.
pub async fn open_files(
    ws: &dyn Workspace,
    paths: &[String],
    limits: &ContextLimits,
) -> Vec<OpenFile> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut spent = 0usize;

    for path in paths.iter().take(limits.max_open_files) {
        if !seen.insert(path.clone()) {
            continue;
        }
        // Files are read in the order the model asked for them, and the ones
        // that no longer fit are dropped rather than shrunk to nothing.
        let remaining = limits.max_files_total_bytes.saturating_sub(spent);
        if remaining < MIN_USEFUL_FILE_BYTES {
            break;
        }
        let Ok(content) = ws.read_file(path).await else {
            continue;
        };
        if looks_binary(&content) {
            continue;
        }
        let (content, truncated) = truncate(&content, limits.max_file_bytes.min(remaining));
        spent += content.len();
        out.push(OpenFile {
            path: path.clone(),
            content,
            truncated,
        });
    }
    out
}

/// Below this a file shows nothing worth the tokens, so the budget stops
/// instead of handing over a few hundred bytes of a header.
const MIN_USEFUL_FILE_BYTES: usize = 400;

pub async fn read_readme(ws: &dyn Workspace, limits: &ContextLimits) -> String {
    for candidate in [
        "README.md",
        "README.rst",
        "README.txt",
        "README",
        "readme.md",
    ] {
        if let Ok(content) = ws.read_file(candidate).await {
            return truncate(&content, limits.max_readme_bytes).0;
        }
    }
    String::new()
}

/// Trims a diff from the end. The head of a diff carries the file list, which
/// is the part a reviewer needs most.
pub fn cap_diff(diff: &str, limits: &ContextLimits) -> String {
    let (text, truncated) = truncate(diff, limits.max_diff_bytes);
    if truncated {
        format!("{text}\n... diff truncated, it is too large to show in full")
    } else {
        text
    }
}

fn truncate(s: &str, limit: usize) -> (String, bool) {
    if s.len() <= limit {
        return (s.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// A NUL byte is the cheap and reliable tell. Anything that survives being read
/// as UTF-8 and has no NUL is close enough to text for a prompt.
fn looks_binary(s: &str) -> bool {
    s.as_bytes().iter().take(8_000).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MemWorkspace;

    #[tokio::test]
    async fn a_small_tree_is_shown_whole_and_sorted() {
        let ws = MemWorkspace::with_files([("b.rs", "x"), ("a.rs", "x")]);
        let tree = file_tree(&ws, &ContextLimits::default()).await.unwrap();
        assert_eq!(tree, "a.rs\nb.rs");
    }

    #[tokio::test]
    async fn a_large_tree_is_truncated_with_a_count() {
        let files: Vec<(String, String)> = (0..50)
            .map(|i| (format!("f{i:03}.rs"), "x".to_string()))
            .collect();
        let ws = MemWorkspace::with_files(files.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let limits = ContextLimits {
            max_tree_entries: 10,
            ..Default::default()
        };
        let tree = file_tree(&ws, &limits).await.unwrap();
        assert_eq!(tree.lines().count(), 11);
        assert!(tree.ends_with("and 40 more files not shown"), "{tree}");
    }

    #[tokio::test]
    async fn requested_files_come_back_and_missing_ones_are_skipped() {
        let ws = MemWorkspace::with_files([("a.rs", "content a")]);
        let files = open_files(
            &ws,
            &["a.rs".into(), "ghost.rs".into()],
            &ContextLimits::default(),
        )
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.rs");
        assert!(!files[0].truncated);
    }

    #[tokio::test]
    async fn duplicate_requests_are_collapsed() {
        let ws = MemWorkspace::with_files([("a.rs", "x")]);
        let files = open_files(
            &ws,
            &["a.rs".into(), "a.rs".into()],
            &ContextLimits::default(),
        )
        .await;
        assert_eq!(files.len(), 1);
    }

    #[tokio::test]
    async fn an_oversized_file_is_capped_and_says_so() {
        let big = "x".repeat(1000);
        let ws = MemWorkspace::with_files([("big.rs", big.as_str())]);
        let limits = ContextLimits {
            max_file_bytes: 100,
            ..Default::default()
        };
        let files = open_files(&ws, &["big.rs".into()], &limits).await;
        assert!(files[0].truncated);
        assert_eq!(files[0].content.len(), 100);
    }

    #[tokio::test]
    async fn the_number_of_open_files_is_bounded() {
        let ws = MemWorkspace::with_files([("a", "1"), ("b", "2"), ("c", "3")]);
        let limits = ContextLimits {
            max_open_files: 2,
            ..Default::default()
        };
        let files = open_files(&ws, &["a".into(), "b".into(), "c".into()], &limits).await;
        assert_eq!(files.len(), 2);
    }

    #[tokio::test]
    async fn a_readme_is_found_under_any_of_its_usual_names() {
        let ws = MemWorkspace::with_files([("README.md", "# widgets")]);
        assert_eq!(
            read_readme(&ws, &ContextLimits::default()).await,
            "# widgets"
        );

        let none = MemWorkspace::with_files([("src/a.rs", "x")]);
        assert!(
            read_readme(&none, &ContextLimits::default())
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_aggregate_file_budget_stops_the_reads() {
        let big = "x".repeat(5_000);
        let ws = MemWorkspace::with_files([
            ("a.rs", big.as_str()),
            ("b.rs", big.as_str()),
            ("c.rs", big.as_str()),
        ]);
        let limits = ContextLimits {
            max_file_bytes: 5_000,
            max_files_total_bytes: 6_000,
            ..Default::default()
        };
        let files = open_files(&ws, &["a.rs".into(), "b.rs".into(), "c.rs".into()], &limits).await;

        let total: usize = files.iter().map(|f| f.content.len()).sum();
        assert!(total <= 6_000, "{total} bytes exceeds the aggregate budget");
        assert!(files.len() < 3, "the third file should not have fitted");
    }

    #[test]
    fn limits_derived_from_a_window_actually_fit_inside_it() {
        // The whole point of the shares adding up. If this drifts, every
        // prompt silently overflows on small local models.
        for window in [8_192u32, 32_000, 128_000, 200_000] {
            let limits = ContextLimits::for_context(window);
            let ceiling = limits.estimated_ceiling_tokens();
            assert!(
                ceiling <= window as usize,
                "a {window} token window plans for {ceiling} tokens of context"
            );
        }
    }

    #[test]
    fn a_tiny_window_still_leaves_something_usable() {
        let limits = ContextLimits::for_context(4_000);
        assert!(limits.max_file_bytes >= 2_000);
        assert!(limits.max_diff_bytes >= 4_000);
        assert!(limits.max_tree_entries >= 40);
    }

    #[test]
    fn a_bigger_window_buys_more_context() {
        let small = ContextLimits::for_context(8_192);
        let large = ContextLimits::for_context(200_000);
        assert!(large.max_diff_bytes > small.max_diff_bytes);
        assert!(large.max_files_total_bytes > small.max_files_total_bytes);
    }

    #[test]
    fn a_capped_diff_announces_the_truncation() {
        let limits = ContextLimits {
            max_diff_bytes: 20,
            ..Default::default()
        };
        let out = cap_diff(&"d".repeat(100), &limits);
        assert!(out.contains("diff truncated"), "{out}");
        assert!(cap_diff("small", &limits).ends_with("small"));
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        let (out, truncated) = truncate("ошибка", 3);
        assert!(truncated);
        assert_eq!(out, "о");
    }

    #[test]
    fn binary_content_is_recognised() {
        assert!(looks_binary("abc\0def"));
        assert!(!looks_binary("fn main() {}"));
    }
}
