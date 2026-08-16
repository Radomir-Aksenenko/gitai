//! Checkout management, host-side.
//!
//! Every attempt gets its own working copy, cut from a bare mirror kept under
//! the work root. The mirror is fetched once per task instead of once per
//! attempt, which is the difference between a fan-out of eight being cheap and
//! being eight full clones.
//!
//! Git itself always runs on the host, never inside the sandbox. The sandbox
//! only ever executes build and test commands, so model-written code never
//! gets to hold a credentialed remote.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use gitai_core::error::{Error, Result};
use gitai_core::sandbox::{ExecOutput, WorkspaceSpec};

use crate::proc::run;

/// Identity on gitai's commits.
const COMMIT_NAME: &str = "gitai";
const COMMIT_EMAIL: &str = "gitai@localhost";

const GIT_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Checkout {
    pub root: PathBuf,
    pub base_branch: String,
    /// Commit the attempt started from. Every diff is taken against this.
    pub base_sha: String,
    pub branch: String,
    /// Credentialed remote. Secret: never logged, never sent to a model.
    repo_url: String,
}

impl Checkout {
    /// Fetches into the mirror, then cuts a fresh working copy for one attempt.
    pub async fn prepare(work_root: &Path, spec: &WorkspaceSpec) -> Result<Self> {
        let mirror = work_root
            .join("mirrors")
            .join(format!("{}.git", spec.repo_slug));
        let root = work_root.join("attempts").join(spec.attempt_id.to_string());

        tokio::fs::create_dir_all(mirror.parent().expect("mirrors has a parent"))
            .await
            .map_err(|e| Error::sandbox(format!("cannot create work root: {e}")))?;
        tokio::fs::create_dir_all(root.parent().expect("attempts has a parent"))
            .await
            .map_err(|e| Error::sandbox(format!("cannot create work root: {e}")))?;

        if root.exists() {
            tokio::fs::remove_dir_all(&root)
                .await
                .map_err(|e| Error::sandbox(format!("cannot clear {}: {e}", root.display())))?;
        }

        Self::sync_mirror(&mirror, &spec.repo_url).await?;

        // --local hardlinks the object store, so this costs almost nothing even
        // for a large repository.
        git_ok(
            None,
            &[
                "clone".into(),
                "--local".into(),
                "--no-checkout".into(),
                mirror.to_string_lossy().into_owned(),
                root.to_string_lossy().into_owned(),
            ],
            "clone attempt workspace",
        )
        .await?;

        git_ok(
            Some(&root),
            &[
                "checkout".into(),
                "-b".into(),
                spec.branch.clone(),
                format!("origin/{}", spec.base_branch),
            ],
            "create attempt branch",
        )
        .await?;

        for (key, value) in [("user.name", COMMIT_NAME), ("user.email", COMMIT_EMAIL)] {
            git_ok(
                Some(&root),
                &["config".into(), key.into(), value.into()],
                "set commit identity",
            )
            .await?;
        }

        let base_sha = git_ok(
            Some(&root),
            &["rev-parse".into(), "HEAD".into()],
            "read base sha",
        )
        .await?
        .trim()
        .to_string();

        Ok(Self {
            root,
            base_branch: spec.base_branch.clone(),
            base_sha,
            branch: spec.branch.clone(),
            repo_url: spec.repo_url.clone(),
        })
    }

    /// Creates the mirror on first use, refreshes it after.
    async fn sync_mirror(mirror: &Path, repo_url: &str) -> Result<()> {
        if !mirror.exists() {
            git_ok_quiet(
                None,
                &[
                    "clone".into(),
                    "--mirror".into(),
                    repo_url.to_string(),
                    mirror.to_string_lossy().into_owned(),
                ],
                "clone mirror",
            )
            .await?;
            // The credentialed URL must not be left sitting in .git/config.
            git_ok_quiet(
                Some(mirror),
                &[
                    "remote".into(),
                    "set-url".into(),
                    "origin".into(),
                    crate::redact_url(repo_url),
                ],
                "scrub mirror remote",
            )
            .await?;
            return Ok(());
        }

        git_ok_quiet(
            Some(mirror),
            &[
                "fetch".into(),
                "--prune".into(),
                "--force".into(),
                repo_url.to_string(),
                "+refs/heads/*:refs/heads/*".into(),
                "+refs/tags/*:refs/tags/*".into(),
            ],
            "refresh mirror",
        )
        .await?;
        Ok(())
    }

    pub async fn list_files(&self) -> Result<Vec<String>> {
        let out = git_ok(Some(&self.root), &["ls-files".into()], "list files").await?;
        Ok(out
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// Unified diff of the working tree against the base commit, new files
    /// included. Staging first is what makes untracked files show up.
    pub async fn diff(&self) -> Result<String> {
        self.stage_all().await?;
        git_ok(
            Some(&self.root),
            &[
                "diff".into(),
                "--cached".into(),
                "--no-color".into(),
                self.base_sha.clone(),
            ],
            "diff against base",
        )
        .await
    }

    pub async fn changed_files(&self) -> Result<Vec<String>> {
        self.stage_all().await?;
        let out = git_ok(
            Some(&self.root),
            &[
                "diff".into(),
                "--cached".into(),
                "--name-only".into(),
                self.base_sha.clone(),
            ],
            "list changed files",
        )
        .await?;
        Ok(out
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// `(insertions, deletions)` against the base commit.
    pub async fn line_stats(&self) -> Result<(u64, u64)> {
        self.stage_all().await?;
        let out = git_ok(
            Some(&self.root),
            &[
                "diff".into(),
                "--cached".into(),
                "--numstat".into(),
                self.base_sha.clone(),
            ],
            "diff stats",
        )
        .await?;
        let mut ins = 0;
        let mut del = 0;
        for line in out.lines() {
            let mut parts = line.split('\t');
            // Binary files report "-" instead of a count.
            if let (Some(a), Some(d)) = (parts.next(), parts.next()) {
                ins += a.parse::<u64>().unwrap_or(0);
                del += d.parse::<u64>().unwrap_or(0);
            }
        }
        Ok((ins, del))
    }

    async fn stage_all(&self) -> Result<()> {
        git_ok(
            Some(&self.root),
            &["add".into(), "-A".into()],
            "stage changes",
        )
        .await?;
        Ok(())
    }

    /// `Ok(None)` when the tree is clean, so an attempt that changed nothing is
    /// reported as such instead of failing.
    pub async fn commit_all(&self, message: &str) -> Result<Option<String>> {
        self.stage_all().await?;
        let status = git_ok(
            Some(&self.root),
            &["status".into(), "--porcelain".into()],
            "check for changes",
        )
        .await?;
        if status.trim().is_empty() {
            return Ok(None);
        }
        git_ok(
            Some(&self.root),
            &["commit".into(), "-m".into(), message.to_string()],
            "commit",
        )
        .await?;
        let sha = git_ok(
            Some(&self.root),
            &["rev-parse".into(), "HEAD".into()],
            "read head",
        )
        .await?
        .trim()
        .to_string();
        Ok(Some(sha))
    }

    /// Pushes the attempt branch. Forced, because a rejected round pushes the
    /// same branch name again.
    pub async fn push(&self) -> Result<()> {
        git_ok_quiet(
            Some(&self.root),
            &[
                "push".into(),
                "--force".into(),
                self.repo_url.clone(),
                format!("HEAD:refs/heads/{}", self.branch),
            ],
            "push attempt branch",
        )
        .await?;
        Ok(())
    }

    pub async fn reset(&self) -> Result<()> {
        git_ok(
            Some(&self.root),
            &["reset".into(), "--hard".into(), self.base_sha.clone()],
            "reset to base",
        )
        .await?;
        git_ok(
            Some(&self.root),
            &["clean".into(), "-fd".into()],
            "clean untracked files",
        )
        .await?;
        Ok(())
    }

    pub async fn read_file(&self, rel: &str) -> Result<String> {
        let path = safe_join(&self.root, rel)?;
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| Error::sandbox(format!("read {rel}: {e}")))
    }

    pub async fn write_file(&self, rel: &str, contents: &str) -> Result<()> {
        let path = safe_join(&self.root, rel)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::sandbox(format!("mkdir for {rel}: {e}")))?;
        }
        tokio::fs::write(&path, contents)
            .await
            .map_err(|e| Error::sandbox(format!("write {rel}: {e}")))
    }

    pub async fn delete_file(&self, rel: &str) -> Result<()> {
        let path = safe_join(&self.root, rel)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // Deleting something already gone is what was asked for.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::sandbox(format!("delete {rel}: {e}"))),
        }
    }

    pub async fn remove(&self) -> Result<()> {
        if self.root.exists() {
            tokio::fs::remove_dir_all(&self.root)
                .await
                .map_err(|e| Error::sandbox(format!("cannot remove workspace: {e}")))?;
        }
        Ok(())
    }
}

/// Joins a model-supplied relative path onto the workspace root, refusing
/// anything that would escape it.
///
/// The model decides these paths, so `../../.ssh/id_rsa` is a case that has to
/// be handled rather than assumed away.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return Err(Error::sandbox(format!("absolute path refused: {rel}")));
    }
    let mut out = root.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::sandbox(format!("path escapes the workspace: {rel}")));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::sandbox(format!("absolute path refused: {rel}")));
            }
        }
    }
    Ok(out)
}

pub async fn git_raw(cwd: Option<&Path>, args: &[String]) -> Result<ExecOutput> {
    run("git", args, cwd, &BTreeMap::new(), GIT_TIMEOUT).await
}

/// Runs git and fails on a non-zero exit, quoting stderr.
async fn git_ok(cwd: Option<&Path>, args: &[String], what: &str) -> Result<String> {
    let out = git_raw(cwd, args).await?;
    if !out.ok() {
        return Err(Error::sandbox(format!(
            "git {what} failed ({}): {}",
            out.exit_code,
            out.tail(2000)
        )));
    }
    Ok(out.stdout)
}

/// Same, for commands whose arguments contain a credentialed URL. The failure
/// message is scrubbed before it can reach a log or an event payload.
async fn git_ok_quiet(cwd: Option<&Path>, args: &[String], what: &str) -> Result<String> {
    let out = git_raw(cwd, args).await?;
    if !out.ok() {
        return Err(Error::sandbox(format!(
            "git {what} failed ({}): {}",
            out.exit_code,
            crate::redact_url(&out.tail(2000))
        )));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_relative_paths_join() {
        let root = Path::new("/work");
        assert_eq!(
            safe_join(root, "src/main.rs").unwrap(),
            Path::new("/work/src/main.rs")
        );
        assert_eq!(
            safe_join(root, "./a.txt").unwrap(),
            Path::new("/work/a.txt")
        );
    }

    #[test]
    fn traversal_is_refused() {
        let root = Path::new("/work");
        assert!(safe_join(root, "../secrets").is_err());
        assert!(safe_join(root, "src/../../etc/passwd").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
    }

    #[test]
    #[cfg(windows)]
    fn windows_absolute_paths_are_refused() {
        let root = Path::new(r"C:\work");
        assert!(safe_join(root, r"C:\Windows\System32\config").is_err());
        assert!(safe_join(root, r"..\..\secrets").is_err());
    }
}
