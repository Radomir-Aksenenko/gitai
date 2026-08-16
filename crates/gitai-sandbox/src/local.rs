//! Host sandbox. Commands run directly on the machine gitai runs on.
//!
//! This exists so the pipeline can be developed and smoke-tested without
//! Docker installed. It is not an isolation boundary: code written by a model
//! executes with the daemon's own privileges. `Sandbox::preflight` says so out
//! loud, and production should use the Docker backend.
//!
//! Second known limit: a command that hits its timeout is killed at the shell,
//! and on Windows that does not take the grandchild process with it, so a hung
//! test runner can outlive the attempt. The Docker backend has no such gap,
//! since removing the container removes everything inside it.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use gitai_core::config::SandboxConfig;
use gitai_core::error::Result;
use gitai_core::sandbox::{ExecOutput, ExecRequest, Sandbox, Workspace, WorkspaceSpec};

use crate::git::{Checkout, safe_join};
use crate::proc::run_shell;

pub struct LocalSandbox {
    cfg: SandboxConfig,
}

impl LocalSandbox {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Sandbox for LocalSandbox {
    fn kind(&self) -> &'static str {
        "local"
    }

    async fn preflight(&self) -> Result<()> {
        crate::require_git().await?;
        tracing::warn!(
            "sandbox.kind = local: model-written code runs on the host with no isolation. \
             Use sandbox.kind = docker outside development."
        );
        Ok(())
    }

    async fn create(&self, spec: &WorkspaceSpec) -> Result<Box<dyn Workspace>> {
        let checkout = Checkout::prepare(&self.cfg.work_root, spec).await?;
        Ok(Box::new(LocalWorkspace {
            checkout,
            cfg: self.cfg.clone(),
        }))
    }
}

pub struct LocalWorkspace {
    checkout: Checkout,
    cfg: SandboxConfig,
}

#[async_trait]
impl Workspace for LocalWorkspace {
    fn root(&self) -> &Path {
        &self.checkout.root
    }

    async fn exec(&self, req: &ExecRequest) -> Result<ExecOutput> {
        let mut env: BTreeMap<String, String> = self.cfg.env.clone();
        env.extend(req.env.clone());

        let cwd = match &req.cwd {
            Some(rel) => safe_join(&self.checkout.root, rel)?,
            None => self.checkout.root.clone(),
        };

        let timeout = Duration::from_secs(req.timeout_secs.unwrap_or(self.cfg.timeout_secs));
        run_shell(&req.cmd, Some(&cwd), &env, timeout).await
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
        self.checkout.remove().await
    }
}
