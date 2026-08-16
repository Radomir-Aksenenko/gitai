//! Gitea and Forgejo. Forgejo is a Gitea fork and kept the API, so one adapter
//! serves both; Forgejo just sends an extra `X-Forgejo-Event` header alongside
//! the Gitea one.

use async_trait::async_trait;
use gitai_core::config::{ForgeConfig, ForgeKind};
use gitai_core::error::{Error, Result};
use gitai_core::forge::{Forge, ForgeEvent, RepoInfo, WebhookDelivery};
use gitai_core::model::{Issue, PullRequest, PullRequestReq, RepoRef};
use serde_json::json;

use crate::client::{ApiClient, esc, esc_ref};
use crate::map::map_issue_event;
use crate::payload::{ApiIssue, ApiLabel, ApiPullRequest, ApiRepo, IssueEvent};
use crate::sig::verify_hmac_sha256;
use crate::url;

pub struct GiteaForge {
    name: String,
    cfg: ForgeConfig,
    api: ApiClient,
}

impl GiteaForge {
    pub fn new(name: impl Into<String>, cfg: ForgeConfig) -> Result<Self> {
        let name = name.into();
        let auth = if cfg.token.is_empty() {
            None
        } else {
            Some(("Authorization".to_string(), format!("token {}", cfg.token)))
        };
        let api = ApiClient::new(&name, &cfg.base_url, auth, vec![], cfg.timeout_secs)?;
        Ok(Self { name, cfg, api })
    }

    fn repo_path(repo: &RepoRef) -> String {
        format!("/repos/{}/{}", esc(&repo.owner), esc(&repo.name))
    }
}

#[async_trait]
impl Forge for GiteaForge {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> ForgeKind {
        ForgeKind::Gitea
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
        // Gitea deletes by label id, so the name has to be resolved first.
        let list_path = format!("{}/issues/{number}/labels", Self::repo_path(repo));
        let current: Vec<ApiLabel> = self.api.get(&list_path, "list_issue_labels").await?;
        let Some(found) = current.iter().find(|l| l.name == label) else {
            return Ok(());
        };
        let path = format!("{list_path}/{}", found.id);
        self.api.delete(&path, "remove_label").await
    }

    async fn open_pull_request(&self, repo: &RepoRef, req: &PullRequestReq) -> Result<PullRequest> {
        // Gitea has no draft flag on the API; a `WIP:` title prefix is how it
        // marks a pull request as not ready.
        let title = if req.draft && !req.title.starts_with("WIP:") {
            format!("WIP: {}", req.title)
        } else {
            req.title.clone()
        };
        let path = format!("{}/pulls", Self::repo_path(repo));
        let body = json!({
            "title": title,
            "body": req.body,
            "head": req.head,
            "base": req.base,
        });
        let pr: ApiPullRequest = self.api.post(&path, &body, "open_pull_request").await?;

        if !req.labels.is_empty() {
            // Labels are attached through the issue endpoint, and a failure
            // here is cosmetic: the pull request already exists.
            if let Err(e) = self.add_labels(repo, pr.number, &req.labels).await {
                tracing::warn!(forge = %self.name, error = %e, "could not label pull request");
            }
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
        // The git refs endpoint rather than /branches/{name}: branch names
        // contain slashes, and this one takes them as a real path.
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
            &self.cfg.bot_login,
            &self.cfg.token,
            &repo.owner,
            &repo.name,
        )
    }

    fn verify_webhook(&self, delivery: &WebhookDelivery) -> Result<()> {
        let signature = delivery
            .header("x-gitea-signature")
            .or_else(|| delivery.header("x-forgejo-signature"))
            .or_else(|| delivery.header("x-hub-signature-256"))
            .ok_or_else(|| Error::forge(&self.name, "delivery carries no signature header"))?;
        verify_hmac_sha256(
            &self.cfg.webhook_secret,
            &delivery.body,
            signature,
            &self.name,
        )
    }

    fn parse_webhook(&self, delivery: &WebhookDelivery) -> Result<ForgeEvent> {
        let kind = delivery
            .header("x-gitea-event")
            .or_else(|| delivery.header("x-forgejo-event"))
            .unwrap_or("")
            .to_string();
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

    fn forge() -> GiteaForge {
        GiteaForge::new(
            "gitea",
            ForgeConfig {
                kind: ForgeKind::Gitea,
                base_url: "https://git.example.com/api/v1".into(),
                token: "t0ken".into(),
                webhook_secret: "shh".into(),
                trigger_label: "gitai".into(),
                bot_login: "gitai".into(),
                draft_prs: true,
                delete_rejected_branches: true,
                timeout_secs: 30,
            },
        )
        .unwrap()
    }

    fn delivery(event: &str, body: &str, secret: &str) -> WebhookDelivery {
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        let sig: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let mut headers = BTreeMap::new();
        headers.insert("x-gitea-event".to_string(), event.to_string());
        headers.insert("x-gitea-signature".to_string(), sig);
        WebhookDelivery {
            headers,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn repo_paths_are_escaped() {
        let repo = RepoRef::parse("acme/my widgets", "gitea").unwrap();
        assert_eq!(GiteaForge::repo_path(&repo), "/repos/acme/my%20widgets");
    }

    #[test]
    fn clone_url_carries_credentials_and_is_redactable() {
        let repo = RepoRef::parse("acme/widgets", "gitea").unwrap();
        let u = forge().clone_url(&repo);
        assert_eq!(u, "https://gitai:t0ken@git.example.com/acme/widgets.git");
        assert!(!url::redact(&u).contains("t0ken"));
    }

    #[test]
    fn a_correctly_signed_delivery_verifies_and_parses() {
        let body = r#"{"action":"opened","issue":{"number":3,"title":"t"},
                       "repository":{"full_name":"acme/widgets","default_branch":"main"}}"#;
        let d = delivery("issues", body, "shh");
        let f = forge();
        f.verify_webhook(&d).unwrap();
        assert!(matches!(
            f.parse_webhook(&d).unwrap(),
            ForgeEvent::IssueOpened { .. }
        ));
    }

    #[test]
    fn a_delivery_signed_with_the_wrong_secret_is_refused() {
        let body = r#"{"action":"opened"}"#;
        let d = delivery("issues", body, "wrong");
        assert!(forge().verify_webhook(&d).is_err());
    }

    #[test]
    fn a_delivery_with_no_signature_header_is_refused() {
        let d = WebhookDelivery {
            headers: BTreeMap::new(),
            body: b"{}".to_vec(),
        };
        let err = forge().verify_webhook(&d).unwrap_err().to_string();
        assert!(err.contains("signature"), "{err}");
    }

    #[test]
    fn forgejo_headers_are_accepted_too() {
        let mut d = delivery("issues", r#"{"action":"opened"}"#, "shh");
        let sig = d.headers.remove("x-gitea-signature").unwrap();
        d.headers.remove("x-gitea-event");
        d.headers.insert("x-forgejo-signature".into(), sig);
        d.headers.insert("x-forgejo-event".into(), "issues".into());
        forge().verify_webhook(&d).unwrap();
    }
}
