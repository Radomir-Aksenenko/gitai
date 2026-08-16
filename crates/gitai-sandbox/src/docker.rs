//! Docker sandbox. One container per attempt, held open for the life of the
//! attempt so that `setup` work (dependency installs, warm caches) survives
//! between gate steps.
//!
//! Driven through the `docker` CLI rather than the socket API: it is the same
//! binary an operator would debug with, it needs no extra crate, and it works
//! the same against Docker Desktop, Podman in Docker-compatible mode, and a
//! remote `DOCKER_HOST`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use gitai_core::config::SandboxConfig;
use gitai_core::error::{Error, Result};
use gitai_core::sandbox::{ExecOutput, ExecRequest, Sandbox, Workspace, WorkspaceSpec};

use crate::git::Checkout;
use crate::proc::run;

/// Keeps the container alive without needing coreutils in the image.
const KEEPALIVE: &str = "tail -f /dev/null";

pub struct DockerSandbox {
    cfg: SandboxConfig,
}

impl DockerSandbox {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self { cfg }
    }

    /// Mount ownership only matters where the host filesystem is shared
    /// directly. On Docker Desktop the mount is translated and `--user` is
    /// noise, so this is Unix-only.
    #[cfg(unix)]
    fn derive_user(&self, work_root: &Path) -> Option<String> {
        use std::os::unix::fs::MetadataExt;
        if !self.cfg.user.is_empty() {
            return Some(self.cfg.user.clone());
        }
        let meta = std::fs::metadata(work_root).ok()?;
        Some(format!("{}:{}", meta.uid(), meta.gid()))
    }

    #[cfg(not(unix))]
    fn derive_user(&self, _work_root: &Path) -> Option<String> {
        if self.cfg.user.is_empty() {
            None
        } else {
            Some(self.cfg.user.clone())
        }
    }

    fn create_args(
        &self,
        name: &str,
        host_dir: &Path,
        user: Option<String>,
        image: &str,
    ) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            name.into(),
            "--label".into(),
            "gitai=1".into(),
            "--workdir".into(),
            self.cfg.workdir.clone(),
            "--volume".into(),
            format!("{}:{}", host_dir.display(), self.cfg.workdir),
            "--network".into(),
            self.cfg.network.clone(),
            // Generated code gets no privileges it does not need.
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--pids-limit".into(),
            self.cfg.pids_limit.to_string(),
        ];

        if self.cfg.cpus > 0.0 {
            args.push("--cpus".into());
            args.push(format!("{:.2}", self.cfg.cpus));
        }
        if !self.cfg.memory.is_empty() {
            args.push("--memory".into());
            args.push(self.cfg.memory.clone());
        }
        if let Some(u) = user {
            args.push("--user".into());
            args.push(u);
        }
        for m in &self.cfg.mounts {
            args.push("--volume".into());
            args.push(m.clone());
        }
        for (k, v) in &self.cfg.env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        if !self.cfg.env.contains_key("PIP_BREAK_SYSTEM_PACKAGES") {
            args.push("--env".into());
            args.push("PIP_BREAK_SYSTEM_PACKAGES=1".into());
        }

        args.push(image.to_string());
        args.extend(["sh".to_string(), "-c".to_string(), KEEPALIVE.to_string()]);
        args
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    fn kind(&self) -> &'static str {
        "docker"
    }

    async fn preflight(&self) -> Result<()> {
        crate::require_git().await?;

        let out = run(
            "docker",
            &[
                "version".into(),
                "--format".into(),
                "{{.Server.Version}}".into(),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_secs(30),
        )
        .await
        .map_err(|_| {
            Error::sandbox(
                "docker was not found on PATH. Install Docker, or set sandbox.kind = \"local\" \
                 to run without isolation (development only).",
            )
        })?;

        if !out.ok() {
            return Err(Error::sandbox(format!(
                "docker is installed but the daemon did not answer: {}",
                out.tail(500)
            )));
        }
        tracing::info!(server = %out.stdout.trim(), "docker ready");
        Ok(())
    }

    async fn create(&self, spec: &WorkspaceSpec) -> Result<Box<dyn Workspace>> {
        let checkout = Checkout::prepare(&self.cfg.work_root, spec).await?;
        let name = format!("gitai-{}", spec.attempt_id);
        let user = self.derive_user(&self.cfg.work_root);
        let image = spec.image.as_deref().unwrap_or(&self.cfg.image);

        // A leftover container from a crashed run would block the name.
        let _ = docker(
            &["rm".into(), "--force".into(), name.clone()],
            Duration::from_secs(60),
        )
        .await;

        let create = docker(
            &self.create_args(&name, &checkout.root, user, image),
            Duration::from_secs(300),
        )
        .await?;
        if !create.ok() {
            let _ = checkout.remove().await;
            return Err(Error::sandbox(format!(
                "docker create failed: {}",
                create.tail(1000)
            )));
        }

        let start = docker(&["start".into(), name.clone()], Duration::from_secs(120)).await?;
        if !start.ok() {
            let _ = docker(
                &["rm".into(), "--force".into(), name.clone()],
                Duration::from_secs(60),
            )
            .await;
            let _ = checkout.remove().await;
            return Err(Error::sandbox(format!(
                "docker start failed: {}",
                start.tail(1000)
            )));
        }

        Ok(Box::new(DockerWorkspace {
            checkout,
            cfg: self.cfg.clone(),
            container: name,
        }))
    }
}

pub struct DockerWorkspace {
    checkout: Checkout,
    cfg: SandboxConfig,
    container: String,
}

#[async_trait]
impl Workspace for DockerWorkspace {
    fn root(&self) -> &Path {
        &self.checkout.root
    }

    async fn exec(&self, req: &ExecRequest) -> Result<ExecOutput> {
        let workdir = match &req.cwd {
            // Validated host-side so a `..` cannot walk out of the mount.
            Some(rel) => {
                crate::git::safe_join(&self.checkout.root, rel)?;
                format!("{}/{}", self.cfg.workdir.trim_end_matches('/'), rel)
            }
            None => self.cfg.workdir.clone(),
        };

        let mut args: Vec<String> = vec!["exec".into(), "--workdir".into(), workdir];
        for (k, v) in &req.env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        if !req.env.contains_key("PIP_BREAK_SYSTEM_PACKAGES")
            && !self.cfg.env.contains_key("PIP_BREAK_SYSTEM_PACKAGES")
        {
            args.push("--env".into());
            args.push("PIP_BREAK_SYSTEM_PACKAGES=1".into());
        }
        args.push(self.container.clone());
        args.extend(["sh".to_string(), "-c".to_string(), req.cmd.clone()]);

        let timeout = Duration::from_secs(req.timeout_secs.unwrap_or(self.cfg.timeout_secs));
        // A little slack over the inner timeout so docker itself can report
        // rather than being cut off mid-answer.
        docker(&args, timeout + Duration::from_secs(15)).await
    }

    async fn read_file(&self, rel: &str) -> Result<String> {
        self.checkout.read_file(rel).await
    }

    async fn write_file(&self, rel: &str, contents: &str) -> Result<()> {
        self.checkout.write_file(rel, contents).await
    }

    async fn delete_file(&self, rel: &str) -> Result<()> {
        self.checkout.delete_file(rel).await
    }

    async fn list_files(&self) -> Result<Vec<String>> {
        self.checkout.list_files().await
    }

    async fn diff(&self) -> Result<String> {
        self.checkout.diff().await
    }

    async fn changed_files(&self) -> Result<Vec<String>> {
        self.checkout.changed_files().await
    }

    async fn line_stats(&self) -> Result<(u64, u64)> {
        self.checkout.line_stats().await
    }

    async fn commit_all(&self, message: &str) -> Result<Option<String>> {
        self.checkout.commit_all(message).await
    }

    async fn push(&self) -> Result<()> {
        self.checkout.push().await
    }

    async fn reset(&self) -> Result<()> {
        self.checkout.reset().await
    }

    async fn cleanup(&self) -> Result<()> {
        // The container goes first: it holds the mount open on Windows.
        let removed = docker(
            &["rm".into(), "--force".into(), self.container.clone()],
            Duration::from_secs(120),
        )
        .await;
        if let Err(e) = &removed {
            tracing::warn!(container = %self.container, error = %e, "could not remove container");
        }
        self.checkout.remove().await
    }
}

async fn docker(args: &[String], timeout: Duration) -> Result<ExecOutput> {
    run("docker", args, None, &BTreeMap::new(), timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitai_core::config::SandboxKind;

    fn sandbox() -> DockerSandbox {
        DockerSandbox::new(SandboxConfig {
            kind: SandboxKind::Docker,
            image: "rust:1-slim".into(),
            workdir: "/work".into(),
            network: "none".into(),
            cpus: 2.0,
            memory: "4g".into(),
            pids_limit: 512,
            ..Default::default()
        })
    }

    #[test]
    fn create_args_carry_the_isolation_flags() {
        let args = sandbox().create_args("gitai-x", Path::new("/tmp/ws"), None, "rust:1-slim");
        let joined = args.join(" ");
        assert!(joined.contains("--network none"), "{joined}");
        assert!(joined.contains("--cap-drop ALL"), "{joined}");
        assert!(
            joined.contains("--security-opt no-new-privileges"),
            "{joined}"
        );
        assert!(joined.contains("--pids-limit 512"), "{joined}");
        assert!(joined.contains("--memory 4g"), "{joined}");
        assert!(joined.contains("--cpus 2.00"), "{joined}");
        assert!(joined.contains("rust:1-slim"), "{joined}");
        assert!(joined.ends_with(KEEPALIVE), "{joined}");
    }

    #[test]
    fn the_workspace_is_mounted_at_the_configured_workdir() {
        let args = sandbox().create_args("gitai-x", Path::new("/tmp/ws"), None, "rust:1-slim");
        let i = args.iter().position(|a| a == "--volume").unwrap();
        assert_eq!(args[i + 1], "/tmp/ws:/work");
        let w = args.iter().position(|a| a == "--workdir").unwrap();
        assert_eq!(args[w + 1], "/work");
    }

    #[test]
    fn an_explicit_user_is_passed_through() {
        let args = sandbox().create_args(
            "gitai-x",
            Path::new("/tmp/ws"),
            Some("1000:1000".into()),
            "rust:1-slim",
        );
        let i = args.iter().position(|a| a == "--user").unwrap();
        assert_eq!(args[i + 1], "1000:1000");
    }

    #[test]
    fn extra_mounts_and_env_are_appended() {
        let mut cfg = SandboxConfig {
            image: "img".into(),
            mounts: vec!["/host/cargo:/root/.cargo".into()],
            ..Default::default()
        };
        cfg.env.insert("CARGO_TERM_COLOR".into(), "never".into());
        let args = DockerSandbox::new(cfg).create_args("n", Path::new("/ws"), None, "img");
        let joined = args.join(" ");
        assert!(joined.contains("/host/cargo:/root/.cargo"), "{joined}");
        assert!(joined.contains("CARGO_TERM_COLOR=never"), "{joined}");
    }

    #[test]
    fn custom_image_is_respected() {
        let args = sandbox().create_args(
            "gitai-custom",
            Path::new("/tmp/ws"),
            None,
            "custom-repo-env:v2",
        );
        let joined = args.join(" ");
        assert!(joined.contains("custom-repo-env:v2"), "{joined}");
        assert!(!joined.contains("rust:1-slim"), "{joined}");
    }
}
