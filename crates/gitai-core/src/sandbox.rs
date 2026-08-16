//! The execution boundary. Model-written code runs behind this trait and
//! nowhere else, which is what makes the isolation claim checkable.

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{AttemptId, TaskId};

#[derive(Debug, Clone)]
pub struct WorkspaceSpec {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    /// Authenticated clone URL. Secret.
    pub repo_url: String,
    /// Short name used for the on-disk cache directory, e.g. `acme-widgets`.
    pub repo_slug: String,
    pub base_branch: String,
    /// Branch the attempt commits to.
    pub branch: String,
}

#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// Run through a shell, so config can hold ordinary command lines.
    pub cmd: String,
    /// Overrides the sandbox default when set.
    pub timeout_secs: Option<u64>,
    pub env: BTreeMap<String, String>,
    /// Relative to the workspace root.
    pub cwd: Option<String>,
}

impl ExecRequest {
    pub fn new(cmd: impl Into<String>) -> Self {
        Self {
            cmd: cmd.into(),
            timeout_secs: None,
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub timed_out: bool,
}

impl ExecOutput {
    pub fn ok(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }

    /// Last `limit` bytes of combined output, on a char boundary. Model context
    /// is expensive and the interesting part of a failure is at the end.
    pub fn tail(&self, limit: usize) -> String {
        let mut combined = String::new();
        if !self.stdout.is_empty() {
            combined.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&self.stderr);
        }
        if self.timed_out {
            combined.push_str(&format!("\n[timed out after {}ms]", self.duration_ms));
        }
        if combined.len() <= limit {
            return combined;
        }
        let mut start = combined.len() - limit;
        while start < combined.len() && !combined.is_char_boundary(start) {
            start += 1;
        }
        format!("[...truncated...]\n{}", &combined[start..])
    }
}

/// One isolated checkout. Dropped without `cleanup` it leaks a container or a
/// worktree, so the pipeline always cleans up on every exit path.
#[async_trait]
pub trait Workspace: Send + Sync {
    /// Host-side path of the checkout.
    fn root(&self) -> &Path;

    async fn exec(&self, req: &ExecRequest) -> Result<ExecOutput>;

    async fn read_file(&self, rel: &str) -> Result<String>;

    async fn write_file(&self, rel: &str, contents: &str) -> Result<()>;

    async fn delete_file(&self, rel: &str) -> Result<()>;

    /// Tracked paths, relative to the root.
    async fn list_files(&self) -> Result<Vec<String>>;

    /// Unified diff of the working tree against the base branch.
    async fn diff(&self) -> Result<String>;

    /// Paths changed against the base branch.
    async fn changed_files(&self) -> Result<Vec<String>>;

    /// `(insertions, deletions)` against the base branch.
    async fn line_stats(&self) -> Result<(u64, u64)>;

    /// Stages everything and commits. `Ok(None)` when there was nothing to commit.
    async fn commit_all(&self, message: &str) -> Result<Option<String>>;

    /// Pushes the attempt branch to the forge.
    async fn push(&self) -> Result<()>;

    /// Throws away uncommitted work, back to the base branch state.
    async fn reset(&self) -> Result<()>;

    async fn cleanup(&self) -> Result<()>;
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Fails fast with an actionable message when the backend is unusable,
    /// for instance Docker not installed.
    async fn preflight(&self) -> Result<()>;

    async fn create(&self, spec: &WorkspaceSpec) -> Result<Box<dyn Workspace>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end_and_respects_char_boundaries() {
        let out = ExecOutput {
            exit_code: 1,
            stdout: "ошибка ".repeat(50),
            stderr: String::new(),
            duration_ms: 5,
            timed_out: false,
        };
        let tail = out.tail(40);
        assert!(tail.starts_with("[...truncated...]"));
        assert!(tail.len() <= 40 + "[...truncated...]\n".len() + 4);
        assert!(
            out.stdout
                .ends_with(tail.trim_start_matches("[...truncated...]\n"))
        );
    }

    #[test]
    fn short_output_is_returned_whole() {
        let out = ExecOutput {
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
            duration_ms: 1,
            timed_out: false,
        };
        assert_eq!(out.tail(100), "ok");
    }
}
