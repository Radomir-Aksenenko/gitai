//! Turning a checkout on disk plus a description into a [`Task`].
//!
//! `gitai run` is how the pipeline gets exercised without a forge, a webhook
//! or a tunnel. It is the loop most development happens in, so it stays a
//! first-class path rather than a test fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use gitai_core::error::{Error, Result};
use gitai_core::model::{Budget, Issue, RepoRef, Task};

/// Prepares a task against a local git repository.
pub fn local_task(
    repo_path: &Path,
    title: String,
    body: String,
    base: Option<String>,
    budget: Budget,
) -> Result<Task> {
    let path = normalise(repo_path)?;

    if !path.join(".git").exists() {
        return Err(Error::config(format!(
            "{} is not a git repository",
            path.display()
        )));
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());

    let base_branch = match base {
        Some(b) => b,
        None => current_branch(&path)?,
    };

    let repo = RepoRef {
        forge: "local".into(),
        owner: "local".into(),
        name,
        default_branch: Some(base_branch.clone()),
    };

    let issue = Issue {
        // Local runs have no issue numbering, so 0 marks "not from a forge".
        number: 0,
        title,
        body,
        url: path.display().to_string(),
        labels: vec![],
        author: "local".into(),
    };

    let mut task = Task::new(repo, issue, budget);
    task.local_repo = Some(path.display().to_string());
    task.base_branch = Some(base_branch);
    Ok(task)
}

/// Absolute path, without the Windows extended-length prefix that `git` on
/// Windows does not accept as a clone source.
fn normalise(path: &Path) -> Result<PathBuf> {
    let abs = std::fs::canonicalize(path)
        .map_err(|e| Error::config(format!("cannot resolve {}: {e}", path.display())))?;
    let text = abs.to_string_lossy();
    Ok(match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => abs,
    })
}

fn current_branch(path: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(path)
        .output()
        .map_err(|e| Error::config(format!("cannot run git in {}: {e}", path.display())))?;

    if !out.status.success() {
        return Err(Error::config(format!(
            "cannot read the current branch of {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        return Err(Error::config(format!(
            "{} is on a detached HEAD; pass --base with a branch name",
            path.display()
        )));
    }
    Ok(branch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {:?}", out);
        };
        run(&["init", "--initial-branch=trunk"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "first"]);
    }

    #[test]
    fn a_local_task_picks_up_the_current_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let task = local_task(
            dir.path(),
            "Add a greeting".into(),
            "It should say hello.".into(),
            None,
            Budget::default(),
        )
        .unwrap();

        assert_eq!(task.repo.forge, "local");
        assert_eq!(task.base_branch.as_deref(), Some("trunk"));
        assert_eq!(task.issue.number, 0);
        assert!(task.local_repo.is_some());
    }

    #[test]
    fn an_explicit_base_overrides_detection() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let task = local_task(
            dir.path(),
            "t".into(),
            "b".into(),
            Some("release".into()),
            Budget::default(),
        )
        .unwrap();
        assert_eq!(task.base_branch.as_deref(), Some("release"));
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = local_task(dir.path(), "t".into(), "b".into(), None, Budget::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a git repository"), "{err}");
    }

    #[test]
    #[cfg(windows)]
    fn the_windows_long_path_prefix_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let path = normalise(dir.path()).unwrap();
        assert!(!path.to_string_lossy().starts_with(r"\\?\"), "{path:?}");
    }
}
