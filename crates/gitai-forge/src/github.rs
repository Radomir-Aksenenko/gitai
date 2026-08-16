//! GitHub, including GitHub Enterprise Server via a `/api/v3` base URL.

use async_trait::async_trait;
use gitai_core::config::{ForgeConfig, ForgeKind};
use gitai_core::error::{Error, Result};
use gitai_core::forge::{Forge, ForgeEvent, RepoInfo, WebhookDelivery};
use gitai_core::model::{Issue, PullRequest, PullRequestReq, RepoRef};
use serde_json::json;

use crate::client::{ApiClient, esc, esc_ref};
use crate::map::map_issue_event;
use crate::payload::{ApiIssue, ApiPullRequest, ApiRepo, IssueEvent};
use crate::sig::verify_hmac_sha256;
use crate::url;

/// Username GitHub expects for token-over-HTTPS git access.
const GIT_USER: &str = "x-access-token";

pub struct GithubForge {
    name: String,
    cfg: ForgeConfig,
    api: ApiClient,
}

impl GithubForge {
    pub fn new(name: impl Into<String>, mut cfg: ForgeConfig) -> Result<Self> {
        let name = name.into();
        if cfg.base_url.is_empty() {
            cfg.base_url = "https://api.github.com".into();
        }
        let auth = if cfg.token.is_empty() {
            None
        } else {
            Some(("Authorization".to_string(), format!("Bearer {}", cfg.token)))
        };
        let extra = vec![
            (
                "Accept".to_string(),
                "application/vnd.github+json".to_string(),
            ),
            ("X-GitHub-Api-Version".to_string(), "2022-11-28".to_string()),
        ];
        let api = ApiClient::new(&name, &cfg.base_url, auth, extra, cfg.timeout_secs)?;
        Ok(Self { name, cfg, api })
    }

    fn repo_path(repo: &RepoRef) -> String {
        format!("/repos/{}/{}", esc(&repo.owner), esc(&repo.name))
    }
}

#[async_trait]
impl Forge for GithubForge {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> ForgeKind {
        ForgeKind::Github
    }

    async fn repo_info(&self, repo: &RepoRef) -> Result<RepoInfo> {
        let r: ApiRepo = self.api.get(&Self::repo_path(repo), "repo_info").await?;
        Ok(RepoInfo {
            default_branch: if r.default_branch.is_empty() {
                "main".into()
            } else {
                r.default_branch
            },
            private: r.private,
            clone_url: r.clone_url,
        })
    }

    async fn get_issue(&self, repo: &RepoRef, number: u64) -> Result<Issue> {
        let path = format!("{}/issues/{number}", Self::repo_path(repo));
        let issue: ApiIssue = self.api.get(&path, "get_issue").await?;
        if issue.is_pull_request() {
            return Err(Error::forge(
                &self.name,
                format!("#{number} is a pull request, not an issue"),
            ));
        }
        Ok(issue.into_domain())
    }

    async fn comment_issue(&self, repo: &RepoRef, number: u64, body: &str) -> Result<()> {
        let path = format!("{}/issues/{number}/comments", Self::repo_path(repo));
        self.api
            .post_ignore(&path, &json!({ "body": body }), "comment_issue")
            .await
    }

    async fn add_labels(&self, repo: &RepoRef, number: u64, labels: &[String]) -> Result<()> {
        if labels.is_empty() {
            return Ok(());
        }
        let path = format!("{}/issues/{number}/labels", Self::repo_path(repo));
        self.api
            .post_ignore(&path, &json!({ "labels": labels }), "add_labels")
            .await
    }

    async fn remove_label(&self, repo: &RepoRef, number: u64, label: &str) -> Result<()> {
        let path = format!(
            "{}/issues/{number}/labels/{}",
            Self::repo_path(repo),
            esc(label)
        );
        // Removing a label that is not there is not a failure worth raising.
        self.api.delete_idempotent(&path, "remove_label").await
    }

    async fn open_pull_request(&self, repo: &RepoRef, req: &PullRequestReq) -> Result<PullRequest> {
        let path = format!("{}/pulls", Self::repo_path(repo));
        let body = json!({
            "title": req.title,
            "body": req.body,
            "head": req.head,
            "base": req.base,
            "draft": req.draft,
        });
        let pr: ApiPullRequest = self.api.post(&path, &body, "open_pull_request").await?;

        if !req.labels.is_empty()
            && let Err(e) = self.add_labels(repo, pr.number, &req.labels).await
        {
            tracing::warn!(forge = %self.name, error = %e, "could not label pull request");
        }

        Ok(PullRequest {
            number: pr.number,
            url: if pr.html_url.is_empty() {
                pr.url
            } else {
                pr.html_url
            },
            head: req.head.clone(),
            base: req.base.clone(),
        })
    }

    async fn delete_branch(&self, repo: &RepoRef, branch: &str) -> Result<()> {
        let path = format!(
            "{}/git/refs/heads/{}",
            Self::repo_path(repo),
            esc_ref(branch)
        );
        self.api.delete_idempotent(&path, "delete_branch").await
    }

    fn prunes_branches(&self) -> bool {
        self.cfg.delete_rejected_branches
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        url::clone_url(
            self.api.base_url(),
            GIT_USER,
            &self.cfg.token,
            &repo.owner,
            &repo.name,
        )
    }

    fn verify_webhook(&self, delivery: &WebhookDelivery) -> Result<()> {
        let signature = delivery
            .header("x-hub-signature-256")
            .ok_or_else(|| Error::forge(&self.name, "delivery carries no signature header"))?;
        verify_hmac_sha256(
            &self.cfg.webhook_secret,
            &delivery.body,
            signature,
            &self.name,
        )
    }

    fn parse_webhook(&self, delivery: &WebhookDelivery) -> Result<ForgeEvent> {
        let kind = delivery.header("x-github-event").unwrap_or("").to_string();
        let event: IssueEvent = serde_json::from_slice(&delivery.body)
            .map_err(|e| Error::forge(&self.name, format!("unparseable webhook body: {e}")))?;
        Ok(map_issue_event(
            &self.name,
            &self.cfg.trigger_label,
            &self.cfg.bot_login,
            &kind,
            event,
        ))
    }

    fn trigger_label(&self) -> &str {
        &self.cfg.trigger_label
    }

    fn bot_login(&self) -> &str {
        &self.cfg.bot_login
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn forge() -> GithubForge {
        GithubForge::new(
            "github",
            ForgeConfig {
                kind: ForgeKind::Github,
                base_url: String::new(),
                token: "ghp_secret".into(),
                webhook_secret: "shh".into(),
                trigger_label: "gitai".into(),
                bot_login: "gitai[bot]".into(),
                draft_prs: true,
                delete_rejected_branches: true,
                timeout_secs: 30,
            },
        )
        .unwrap()
    }

    #[test]
    fn an_empty_base_url_defaults_to_github_dot_com() {
        let repo = RepoRef::parse("acme/widgets", "github").unwrap();
        assert_eq!(
            forge().clone_url(&repo),
            "https://x-access-token:ghp_secret@github.com/acme/widgets.git"
        );
    }

    #[test]
    fn enterprise_base_url_is_respected() {
        let f = GithubForge::new(
            "ghe",
            ForgeConfig {
                base_url: "https://ghe.corp/api/v3".into(),
                token: "tok".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let repo = RepoRef::parse("acme/widgets", "ghe").unwrap();
        assert_eq!(
            f.clone_url(&repo),
            "https://x-access-token:tok@ghe.corp/acme/widgets.git"
        );
    }

    #[test]
    fn github_signature_header_is_required() {
        let d = WebhookDelivery {
            headers: BTreeMap::new(),
            body: b"{}".to_vec(),
        };
        assert!(forge().verify_webhook(&d).is_err());
    }

    #[test]
    fn a_correctly_signed_labeled_event_parses() {
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        let body = r#"{"action":"labeled","label":{"name":"gitai"},
                       "issue":{"number":11,"title":"t","labels":[{"name":"gitai"}]},
                       "repository":{"full_name":"acme/widgets","default_branch":"main"}}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"shh").unwrap();
        mac.update(body.as_bytes());
        let sig: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let mut headers = BTreeMap::new();
        headers.insert("x-github-event".to_string(), "issues".to_string());
        headers.insert("x-hub-signature-256".to_string(), format!("sha256={sig}"));
        let d = WebhookDelivery {
            headers,
            body: body.as_bytes().to_vec(),
        };

        let f = forge();
        f.verify_webhook(&d).unwrap();
        match f.parse_webhook(&d).unwrap() {
            ForgeEvent::IssueLabeled { issue, label, .. } => {
                assert_eq!(issue.number, 11);
                assert_eq!(label, "gitai");
            }
            other => panic!("{other:?}"),
        }
    }
}
