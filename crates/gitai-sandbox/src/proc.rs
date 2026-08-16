//! Child-process plumbing with a hard timeout.
//!
//! A hung `npm install` must not hold a pipeline slot forever, so every
//! command here is killed at the deadline and reported as `timed_out` rather
//! than as a generic failure.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use gitai_core::error::{Error, Result};
use gitai_core::sandbox::ExecOutput;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Runs `program` with `args`, capturing both streams.
///
/// `Err` means the process could not be started at all. A command that ran and
/// failed comes back as `Ok` with a non-zero `exit_code`, because the gate
/// needs that distinction: a failing test suite is data, a missing binary is a
/// configuration problem.
pub async fn run(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<ExecOutput> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    run_prepared(cmd, program, cwd, env, timeout).await
}

/// Runs a command line through the platform shell.
///
/// Separate from [`run`] because of Windows. `cmd /C` re-parses its argument,
/// and Rust's own argument quoting escapes inner quotes in a way `cmd` does not
/// understand, so `cargo test --features "a b"` arrives mangled. Passing the
/// command verbatim after `/S` is the documented way through: `cmd` strips the
/// outer pair of quotes and takes everything between them literally.
pub async fn run_shell(
    command: &str,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<ExecOutput> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let mut cmd = Command::new("cmd");
        cmd.arg("/S").arg("/C");
        cmd.as_std_mut().raw_arg(format!("\"{command}\""));
        run_prepared(cmd, "cmd", cwd, env, timeout).await
    }

    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        run_prepared(cmd, "sh", cwd, env, timeout).await
    }
}

async fn run_prepared(
    mut cmd: Command,
    program: &str,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<ExecOutput> {
    let started = Instant::now();

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    // Armed before the spawn, because on Windows the job object has to exist
    // first and on Unix the process group is a spawn-time setting.
    let guard = crate::tree::TreeGuard::arm(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::sandbox(format!("cannot run `{program}`: {e}")))?;
    guard.adopt(&child);

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let mut stdout = String::new();
    let mut stderr = String::new();

    // Both pipes are drained while waiting, otherwise a chatty command fills
    // its buffer and deadlocks instead of finishing.
    let collect = async {
        let out = async {
            if let Some(p) = stdout_pipe.as_mut() {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf).await;
                String::from_utf8_lossy(&buf).into_owned()
            } else {
                String::new()
            }
        };
        let err = async {
            if let Some(p) = stderr_pipe.as_mut() {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf).await;
                String::from_utf8_lossy(&buf).into_owned()
            } else {
                String::new()
            }
        };
        let (o, e, status) = tokio::join!(out, err, child.wait());
        (o, e, status)
    };

    match tokio::time::timeout(timeout, collect).await {
        Ok((o, e, status)) => {
            stdout = o;
            stderr = e;
            let exit_code = status
                .map_err(|e| Error::sandbox(format!("waiting on `{program}`: {e}")))?
                .code()
                .unwrap_or(-1);
            Ok(ExecOutput {
                exit_code,
                stdout,
                stderr,
                duration_ms: started.elapsed().as_millis() as u64,
                timed_out: false,
            })
        }
        Err(_) => {
            // Not just the child: the shell it runs under spawns the actual
            // build, and killing the shell alone leaves that running.
            guard.kill_tree(&mut child);
            stdout.push_str("\n[killed: timeout]");
            Ok(ExecOutput {
                exit_code: -1,
                stdout,
                stderr,
                duration_ms: started.elapsed().as_millis() as u64,
                timed_out: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    async fn shell(cmd: &str) -> ExecOutput {
        run_shell(cmd, None, &BTreeMap::new(), LONG).await.unwrap()
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let out = shell("echo hello").await;
        assert!(out.ok(), "{out:?}");
        assert!(out.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn a_failing_command_is_ok_with_a_non_zero_code() {
        let out = shell("exit 3").await;
        assert_eq!(out.exit_code, 3);
        assert!(!out.ok());
    }

    /// The reason `run_shell` exists. A gate command carrying quotes has to
    /// survive the trip through the platform shell intact.
    #[tokio::test]
    async fn inner_quotes_reach_the_shell_unmangled() {
        let out = shell("echo \"one two\"").await;
        assert!(out.ok(), "{out:?}");
        assert!(
            out.stdout.contains("one two"),
            "quoted argument was mangled: {out:?}"
        );
    }

    #[tokio::test]
    async fn commands_run_in_the_requested_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();

        let cmd = if cfg!(windows) { "dir /b" } else { "ls" };
        let out = run_shell(cmd, Some(dir.path()), &BTreeMap::new(), LONG)
            .await
            .unwrap();
        assert!(out.stdout.contains("marker.txt"), "{out:?}");
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error_not_an_exit_code() {
        let out = run(
            "gitai-definitely-not-a-real-binary",
            &[],
            None,
            &BTreeMap::new(),
            Duration::from_secs(5),
        )
        .await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn a_hung_command_is_killed_and_flagged() {
        let cmd = if cfg!(windows) {
            "ping -n 20 127.0.0.1 > NUL"
        } else {
            "sleep 20"
        };
        let out = run_shell(cmd, None, &BTreeMap::new(), Duration::from_millis(400))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert!(
            out.duration_ms < 3_000,
            "should not wait out the full sleep"
        );
    }

    /// The one that matters. A gate command runs through a shell, and killing
    /// the shell alone used to leave the real build running: it kept writing,
    /// held the workspace open, and on Windows made the directory undeletable.
    #[tokio::test]
    async fn a_timed_out_command_does_not_leave_its_children_running() {
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("probe.txt");

        // ping is the portable "runs for a while and writes as it goes"
        // process. Redirected to a file, so its output survives the kill and
        // any growth afterwards is evidence it outlived us.
        // Run inside the directory and redirect to a bare name, so the probe
        // does not depend on how the shell handles a quoted path.
        let cmd = if cfg!(windows) {
            "ping -n 20 127.0.0.1 > probe.txt"
        } else {
            "ping -c 20 127.0.0.1 > probe.txt"
        };

        let out = run_shell(
            cmd,
            Some(dir.path()),
            &BTreeMap::new(),
            Duration::from_millis(1_500),
        )
        .await
        .unwrap();
        assert!(out.timed_out, "{out:?}");

        let size_at_kill = std::fs::metadata(&probe).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let size_later = std::fs::metadata(&probe).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the grandchild kept writing after its shell was killed"
        );

        // An open handle is what makes a workspace undeletable on Windows.
        std::fs::remove_dir_all(dir.path())
            .expect("the workspace must be removable once the tree is gone");
    }

    #[tokio::test]
    async fn environment_variables_reach_the_child() {
        let mut env = BTreeMap::new();
        env.insert("GITAI_PROBE".to_string(), "42".to_string());
        let cmd = if cfg!(windows) {
            "echo %GITAI_PROBE%"
        } else {
            "echo $GITAI_PROBE"
        };
        let out = run_shell(cmd, None, &env, LONG).await.unwrap();
        assert!(out.stdout.contains("42"), "{out:?}");
    }
}
