//! Turning a worker model's answer into changes on disk.
//!
//! Three operations, chosen because a 7B model can produce them reliably:
//! rewrite a whole file, replace an exact string, delete a file. Unified diffs
//! were the obvious alternative and small models get the line numbers wrong
//! often enough that the loop never converges.
//!
//! A failed edit is data, not a crash. Failures come back as text and go
//! straight into the next prompt, which is the mechanism that lets a cheap
//! model correct itself.

use gitai_core::error::Result;
use gitai_core::sandbox::Workspace;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Edit {
    /// Replaces the entire file, creating it if needed.
    #[serde(alias = "create", alias = "add", alias = "new", alias = "overwrite", alias = "Write")]
    Write {
        #[serde(alias = "file", alias = "filename", alias = "filepath")]
        path: String,
        #[serde(alias = "code", alias = "text", alias = "body")]
        content: String,
    },
    /// Exact-string substitution. Refuses to guess when ambiguous.
    #[serde(alias = "update", alias = "modify", alias = "patch", alias = "Replace")]
    Replace {
        #[serde(alias = "file", alias = "filename", alias = "filepath")]
        path: String,
        #[serde(alias = "search", alias = "old", alias = "target")]
        find: String,
        #[serde(alias = "new", alias = "replacement", alias = "with")]
        replace: String,
        #[serde(default)]
        all: bool,
    },
    #[serde(alias = "remove", alias = "del", alias = "unlink", alias = "Delete")]
    Delete {
        #[serde(alias = "file", alias = "filename", alias = "filepath")]
        path: String,
    },
}

impl Edit {
    pub fn path(&self) -> &str {
        match self {
            Edit::Write { path, .. } | Edit::Replace { path, .. } | Edit::Delete { path } => path,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct EditOutcome {
    pub applied: Vec<String>,
    /// One line per rejected edit, phrased for the model that wrote it.
    pub failures: Vec<String>,
}

impl EditOutcome {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// Feedback text for the next worker turn.
    pub fn failure_report(&self) -> String {
        if self.failures.is_empty() {
            return String::new();
        }
        let mut out = String::from("Some of your edits could not be applied:\n");
        for f in &self.failures {
            out.push_str(&format!("- {f}\n"));
        }
        out.push_str(
            "\nThe other edits were applied. Re-read the files you got wrong before retrying them.",
        );
        out
    }
}

/// Applies edits in order, collecting failures rather than stopping at the first.
pub async fn apply(ws: &dyn Workspace, edits: &[Edit]) -> Result<EditOutcome> {
    let mut outcome = EditOutcome::default();

    for edit in edits {
        match edit {
            Edit::Write { path, content } => match ws.write_file(path, content).await {
                Ok(()) => outcome.applied.push(path.clone()),
                Err(e) => outcome.failures.push(format!("write `{path}`: {e}")),
            },

            Edit::Delete { path } => match ws.delete_file(path).await {
                Ok(()) => outcome.applied.push(path.clone()),
                Err(e) => outcome.failures.push(format!("delete `{path}`: {e}")),
            },

            Edit::Replace {
                path,
                find,
                replace,
                all,
            } => {
                let current = match ws.read_file(path).await {
                    Ok(c) => c,
                    Err(e) => {
                        outcome.failures.push(format!("replace in `{path}`: {e}"));
                        continue;
                    }
                };

                if find.is_empty() {
                    outcome
                        .failures
                        .push(format!("replace in `{path}`: `find` was empty"));
                    continue;
                }

                let count = current.matches(find.as_str()).count();
                match count {
                    0 => outcome.failures.push(format!(
                        "replace in `{path}`: the `find` text does not appear in the file. \
                         Read the file again and copy the text exactly, whitespace included."
                    )),
                    n if n > 1 && !all => outcome.failures.push(format!(
                        "replace in `{path}`: the `find` text appears {n} times, so it is \
                         ambiguous. Include more surrounding lines, or set \"all\": true if you \
                         really mean every occurrence."
                    )),
                    _ => {
                        let updated = if *all {
                            current.replace(find.as_str(), replace)
                        } else {
                            current.replacen(find.as_str(), replace, 1)
                        };
                        match ws.write_file(path, &updated).await {
                            Ok(()) => outcome.applied.push(path.clone()),
                            Err(e) => outcome.failures.push(format!("write `{path}`: {e}")),
                        }
                    }
                }
            }
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MemWorkspace;

    fn ws() -> MemWorkspace {
        MemWorkspace::with_files([
            ("src/a.rs", "fn main() {\n    println!(\"hi\");\n}\n"),
            ("src/dup.rs", "let x = 1;\nlet x = 1;\n"),
        ])
    }

    #[tokio::test]
    async fn write_creates_and_replaces_whole_files() {
        let ws = ws();
        let out = apply(
            &ws,
            &[Edit::Write {
                path: "src/new.rs".into(),
                content: "pub fn f() {}\n".into(),
            }],
        )
        .await
        .unwrap();

        assert!(out.ok(), "{:?}", out.failures);
        assert_eq!(ws.read("src/new.rs").unwrap(), "pub fn f() {}\n");
    }

    #[tokio::test]
    async fn replace_substitutes_a_unique_match() {
        let ws = ws();
        let out = apply(
            &ws,
            &[Edit::Replace {
                path: "src/a.rs".into(),
                find: "println!(\"hi\")".into(),
                replace: "println!(\"hello\")".into(),
                all: false,
            }],
        )
        .await
        .unwrap();

        assert!(out.ok(), "{:?}", out.failures);
        assert!(ws.read("src/a.rs").unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn replace_refuses_to_guess_when_the_match_is_ambiguous() {
        let ws = ws();
        let out = apply(
            &ws,
            &[Edit::Replace {
                path: "src/dup.rs".into(),
                find: "let x = 1;".into(),
                replace: "let x = 2;".into(),
                all: false,
            }],
        )
        .await
        .unwrap();

        assert!(!out.ok());
        assert!(
            out.failures[0].contains("appears 2 times"),
            "{:?}",
            out.failures
        );
        // Nothing was written, so the model can retry from a known state.
        assert_eq!(ws.read("src/dup.rs").unwrap(), "let x = 1;\nlet x = 1;\n");
    }

    #[tokio::test]
    async fn replace_all_is_honoured_when_asked_for() {
        let ws = ws();
        let out = apply(
            &ws,
            &[Edit::Replace {
                path: "src/dup.rs".into(),
                find: "let x = 1;".into(),
                replace: "let x = 2;".into(),
                all: true,
            }],
        )
        .await
        .unwrap();

        assert!(out.ok(), "{:?}", out.failures);
        assert_eq!(ws.read("src/dup.rs").unwrap(), "let x = 2;\nlet x = 2;\n");
    }

    #[tokio::test]
    async fn a_find_that_is_not_there_explains_itself() {
        let ws = ws();
        let out = apply(
            &ws,
            &[Edit::Replace {
                path: "src/a.rs".into(),
                find: "nonexistent".into(),
                replace: "x".into(),
                all: false,
            }],
        )
        .await
        .unwrap();

        assert!(!out.ok());
        assert!(
            out.failures[0].contains("does not appear"),
            "{:?}",
            out.failures
        );
        assert!(out.failure_report().contains("Re-read the files"));
    }

    #[tokio::test]
    async fn one_bad_edit_does_not_discard_the_good_ones() {
        let ws = ws();
        let out = apply(
            &ws,
            &[
                Edit::Write {
                    path: "src/good.rs".into(),
                    content: "ok\n".into(),
                },
                Edit::Replace {
                    path: "src/missing.rs".into(),
                    find: "a".into(),
                    replace: "b".into(),
                    all: false,
                },
                Edit::Write {
                    path: "src/also_good.rs".into(),
                    content: "ok\n".into(),
                },
            ],
        )
        .await
        .unwrap();

        assert_eq!(out.applied.len(), 2);
        assert_eq!(out.failures.len(), 1);
        assert!(ws.read("src/also_good.rs").is_some());
    }

    #[tokio::test]
    async fn delete_removes_a_file_and_tolerates_a_missing_one() {
        let ws = ws();
        let out = apply(
            &ws,
            &[
                Edit::Delete {
                    path: "src/a.rs".into(),
                },
                Edit::Delete {
                    path: "src/never_existed.rs".into(),
                },
            ],
        )
        .await
        .unwrap();

        assert!(out.ok(), "{:?}", out.failures);
        assert!(ws.read("src/a.rs").is_none());
    }

    #[tokio::test]
    async fn path_traversal_in_an_edit_is_rejected() {
        let ws = ws();
        let out = apply(
            &ws,
            &[Edit::Write {
                path: "../../.ssh/authorized_keys".into(),
                content: "ssh-rsa ...".into(),
            }],
        )
        .await
        .unwrap();

        assert!(!out.ok());
        assert!(out.failures[0].contains("escapes"), "{:?}", out.failures);
    }

    #[test]
    fn edits_deserialise_from_the_documented_shape() {
        let raw = r#"[
            {"op":"write","path":"a.rs","content":"x"},
            {"op":"replace","path":"b.rs","find":"a","replace":"b"},
            {"op":"replace","path":"c.rs","find":"a","replace":"b","all":true},
            {"op":"delete","path":"d.rs"}
        ]"#;
        let edits: Vec<Edit> = serde_json::from_str(raw).unwrap();
        assert_eq!(edits.len(), 4);
        assert_eq!(edits[0].path(), "a.rs");
        assert!(matches!(edits[2], Edit::Replace { all: true, .. }));
        assert!(matches!(edits[3], Edit::Delete { .. }));
    }

    #[test]
    fn edit_aliases_deserialise_cleanly() {
        let raw = r#"[
            {"op":"create","file":"main.py","code":"print(1)"},
            {"op":"modify","filename":"b.py","search":"1","with":"2"},
            {"op":"remove","filepath":"c.py"}
        ]"#;
        let edits: Vec<Edit> = serde_json::from_str(raw).unwrap();
        assert_eq!(edits.len(), 3);
        assert!(matches!(&edits[0], Edit::Write { path, content } if path == "main.py" && content == "print(1)"));
        assert!(matches!(&edits[1], Edit::Replace { path, find, replace, .. } if path == "b.py" && find == "1" && replace == "2"));
        assert!(matches!(&edits[2], Edit::Delete { path } if path == "c.py"));
    }
}
