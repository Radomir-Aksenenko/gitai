//! In-memory test doubles.
//!
//! Shipped rather than hidden behind `#[cfg(test)]` so that anything building
//! on gitai can exercise the pipeline without Docker, a forge or a network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use gitai_core::error::{Error, Result};
use gitai_core::sandbox::{ExecOutput, ExecRequest, Sandbox, Workspace, WorkspaceSpec};

/// A workspace backed by a map. Applies the same path rules as the real one,
/// so traversal attempts fail here exactly as they would on disk.
pub struct MemWorkspace {
    root: PathBuf,
    files: Mutex<BTreeMap<String, String>>,
    /// Files as they were at creation, so `diff` and `changed_files` mean
    /// something.
    base: BTreeMap<String, String>,
    /// Canned results, matched by substring of the command.
    exec_rules: Vec<(String, ExecOutput)>,
    default_exec: ExecOutput,
}

impl MemWorkspace {
    pub fn with_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let map: BTreeMap<String, String> = files
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Self {
            root: PathBuf::from("/mem"),
            files: Mutex::new(map.clone()),
            base: map,
            exec_rules: Vec::new(),
            default_exec: ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: 1,
                timed_out: false,
            },
        }
    }

    /// Makes any command containing `needle` exit with `code`.
    pub fn on_exec(mut self, needle: &str, code: i32, output: &str) -> Self {
        self.exec_rules.push((
            needle.to_string(),
            ExecOutput {
                exit_code: code,
                stdout: output.to_string(),
                stderr: String::new(),
                duration_ms: 1,
                timed_out: false,
            },
        ));
        self
    }

    pub fn read(&self, path: &str) -> Option<String> {
        self.files.lock().expect("not poisoned").get(path).cloned()
    }

    pub fn file_count(&self) -> usize {
        self.files.lock().expect("not poisoned").len()
    }

    /// Same rules as the real workspace: no absolute paths, no escaping.
    fn check_path(rel: &str) -> Result<()> {
        gitai_sandbox::git::safe_join(Path::new("/mem"), rel).map(|_| ())
    }
}

#[async_trait]
impl Workspace for MemWorkspace {
    fn root(&self) -> &Path {
        &self.root
    }

    async fn exec(&self, req: &ExecRequest) -> Result<ExecOutput> {
        for (needle, out) in &self.exec_rules {
            if req.cmd.contains(needle) {
                return Ok(out.clone());
            }
        }
        Ok(self.default_exec.clone())
    }

    async fn read_file(&self, rel: &str) -> Result<String> {
        Self::check_path(rel)?;
        self.files
            .lock()
            .expect("not poisoned")
            .get(rel)
            .cloned()
            .ok_or_else(|| Error::sandbox(format!("no such file: {rel}")))
    }

    async fn write_file(&self, rel: &str, contents: &str) -> Result<()> {
        Self::check_path(rel)?;
        self.files
            .lock()
            .expect("not poisoned")
            .insert(rel.to_string(), contents.to_string());
        Ok(())
    }

    async fn delete_file(&self, rel: &str) -> Result<()> {
        Self::check_path(rel)?;
        self.files.lock().expect("not poisoned").remove(rel);
        Ok(())
    }

    async fn list_files(&self) -> Result<Vec<String>> {
        Ok(self
            .files
            .lock()
            .expect("not poisoned")
            .keys()
            .cloned()
            .collect())
    }

    async fn diff(&self) -> Result<String> {
        let files = self.files.lock().expect("not poisoned");
        let mut out = String::new();
        for path in self.changed_paths(&files) {
            out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
            match files.get(&path) {
                Some(content) => {
                    for line in content.lines() {
                        out.push_str(&format!("+{line}\n"));
                    }
                }
                None => out.push_str("(deleted)\n"),
            }
        }
        Ok(out)
    }

    async fn changed_files(&self) -> Result<Vec<String>> {
        let files = self.files.lock().expect("not poisoned");
        Ok(self.changed_paths(&files))
    }

    async fn line_stats(&self) -> Result<(u64, u64)> {
        let files = self.files.lock().expect("not poisoned");
        let mut ins = 0;
        let mut del = 0;
        for path in self.changed_paths(&files) {
            ins += files.get(&path).map_or(0, |c| c.lines().count() as u64);
            del += self.base.get(&path).map_or(0, |c| c.lines().count() as u64);
        }
        Ok((ins, del))
    }

    async fn commit_all(&self, _message: &str) -> Result<Option<String>> {
        let files = self.files.lock().expect("not poisoned");
        if self.changed_paths(&files).is_empty() {
            return Ok(None);
        }
        Ok(Some("0000000000000000000000000000000000000000".into()))
    }

    async fn push(&self) -> Result<()> {
        Ok(())
    }

    async fn reset(&self) -> Result<()> {
        *self.files.lock().expect("not poisoned") = self.base.clone();
        Ok(())
    }

    async fn cleanup(&self) -> Result<()> {
        Ok(())
    }
}

impl MemWorkspace {
    fn changed_paths(&self, files: &BTreeMap<String, String>) -> Vec<String> {
        let mut out: Vec<String> = files
            .iter()
            .filter(|(k, v)| self.base.get(*k) != Some(*v))
            .map(|(k, _)| k.clone())
            .collect();
        out.extend(
            self.base
                .keys()
                .filter(|k| !files.contains_key(*k))
                .cloned(),
        );
        out.sort();
        out.dedup();
        out
    }
}

/// A sandbox that hands out [`MemWorkspace`] clones of one file set.
pub struct MemSandbox {
    files: BTreeMap<String, String>,
    /// Applied to every workspace it creates, so a gate can be made to fail
    /// across a whole run.
    rules: Vec<(String, i32, String)>,
}

impl MemSandbox {
    pub fn new<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            rules: Vec::new(),
        }
    }

    /// Any command containing `needle` exits with `code` in every workspace.
    pub fn on_exec(mut self, needle: &str, code: i32, output: &str) -> Self {
        self.rules
            .push((needle.to_string(), code, output.to_string()));
        self
    }
}

#[async_trait]
impl Sandbox for MemSandbox {
    fn kind(&self) -> &'static str {
        "memory"
    }

    async fn preflight(&self) -> Result<()> {
        Ok(())
    }

    async fn create(&self, _spec: &WorkspaceSpec) -> Result<Box<dyn Workspace>> {
        let pairs: Vec<(&str, &str)> = self
            .files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut ws = MemWorkspace::with_files(pairs);
        for (needle, code, output) in &self.rules {
            ws = ws.on_exec(needle, *code, output);
        }
        Ok(Box::new(ws))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn changed_files_reflects_writes_and_deletes() {
        let ws = MemWorkspace::with_files([("a.rs", "one"), ("b.rs", "two")]);
        assert!(ws.changed_files().await.unwrap().is_empty());

        ws.write_file("a.rs", "changed").await.unwrap();
        ws.delete_file("b.rs").await.unwrap();
        ws.write_file("c.rs", "new").await.unwrap();

        let mut changed = ws.changed_files().await.unwrap();
        changed.sort();
        assert_eq!(changed, vec!["a.rs", "b.rs", "c.rs"]);
    }

    #[tokio::test]
    async fn reset_restores_the_starting_state() {
        let ws = MemWorkspace::with_files([("a.rs", "one")]);
        ws.write_file("a.rs", "changed").await.unwrap();
        ws.reset().await.unwrap();
        assert_eq!(ws.read("a.rs").unwrap(), "one");
        assert!(ws.changed_files().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn exec_rules_match_by_substring() {
        let ws = MemWorkspace::with_files([]).on_exec("cargo test", 101, "2 failed");
        let fail = ws
            .exec(&ExecRequest::new("cargo test --all"))
            .await
            .unwrap();
        assert_eq!(fail.exit_code, 101);
        let pass = ws.exec(&ExecRequest::new("cargo build")).await.unwrap();
        assert!(pass.ok());
    }

    #[tokio::test]
    async fn traversal_is_refused_here_too() {
        let ws = MemWorkspace::with_files([]);
        assert!(ws.write_file("../escape", "x").await.is_err());
    }
}
