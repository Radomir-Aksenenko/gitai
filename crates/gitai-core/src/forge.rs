//! The forge boundary. Gitea, Forgejo, GitHub and later GitLab all reduce to
//! this: read an issue, talk back on it, hand out a clone URL, open a PR, and
//! turn an incoming webhook into something the queue understands.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::ForgeKind;
use crate::error::Result;
use crate::model::{Issue, PullRequest, PullRequestReq, RepoRef};

/// A raw inbound webhook, before any forge-specific interpretation.
#[derive(Debug, Clone)]
pub struct WebhookDelivery {
    /// Header names are lowercased by the server before they get here.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl WebhookDelivery {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// What a delivery means, once the forge adapter has read it.
#[derive(Debug, Clone)]
pub enum ForgeEvent {
    IssueOpened {
        repo: RepoRef,
        issue: Issue,
    },
    IssueLabeled {
        repo: RepoRef,
        issue: Issue,
        label: String,
    },
    /// Lets a human steer a running task from the issue thread.
    IssueComment {
        repo: RepoRef,
        issue: Issue,
        author: String,
        body: String,
    },
    /// Everything gitai does not act on. Ping events, pushes, its own noise.
    Ignored {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub default_branch: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub clone_url: String,
}

#[async_trait]
pub trait Forge: Send + Sync {
    /// Key in `[forges.*]`.
    fn name(&self) -> &str;

    fn kind(&self) -> ForgeKind;

    async fn repo_info(&self, repo: &RepoRef) -> Result<RepoInfo>;

    async fn get_issue(&self, repo: &RepoRef, number: u64) -> Result<Issue>;

    async fn comment_issue(&self, repo: &RepoRef, number: u64, body: &str) -> Result<()>;

    async fn add_labels(&self, repo: &RepoRef, number: u64, labels: &[String]) -> Result<()>;

    async fn remove_label(&self, repo: &RepoRef, number: u64, label: &str) -> Result<()>;

    async fn open_pull_request(&self, repo: &RepoRef, req: &PullRequestReq) -> Result<PullRequest>;

    /// Removes a branch gitai pushed. A branch that is already gone is not an
    /// error: cleanup runs on paths that may have partially completed before.
    async fn delete_branch(&self, repo: &RepoRef, branch: &str) -> Result<()>;

    /// Whether rejected attempt branches should be swept up after a task ends.
    fn prunes_branches(&self) -> bool;

    /// Clone URL with credentials embedded. Treat the return value as a secret:
    /// never log it, never write it into an event payload.
    fn clone_url(&self, repo: &RepoRef) -> String;

    /// Constant-time signature check. Errors when the delivery is not authentic.
    fn verify_webhook(&self, delivery: &WebhookDelivery) -> Result<()>;

    fn parse_webhook(&self, delivery: &WebhookDelivery) -> Result<ForgeEvent>;

    /// Label that arms gitai on an issue. Empty means every issue qualifies.
    fn trigger_label(&self) -> &str;

    /// Login gitai pushes as, so its own activity does not retrigger a task.
    fn bot_login(&self) -> &str;
}
