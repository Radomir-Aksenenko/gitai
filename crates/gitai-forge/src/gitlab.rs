//! GitLab, API v4.
//!
//! The third forge, and the one that justifies the trait. GitLab differs from
//! the other two in every place they agreed:
//!
//! - projects are addressed by a URL-encoded path, and that path can contain
//!   subgroups, so `owner` here is a whole namespace
//! - webhooks are not signed. GitLab echoes a shared token in a header, so
//!   authentication is a constant-time string comparison, not an HMAC
//! - the webhook body is shaped nothing like the GitHub one: `object_kind`,
//!   `object_attributes`, and label changes reported as a diff under `changes`
//! - merge requests instead of pull requests, and drafts are a title prefix
//! - the REST API returns labels as strings, the webhook as objects

use async_trait::async_trait;
use gitai_core::config::{ForgeConfig, ForgeKind};
use gitai_core::error::{Error, Result};
use gitai_core::forge::{Forge, ForgeEvent, RepoInfo, WebhookDelivery};
use gitai_core::model::{Issue, PullRequest, PullRequestReq, RepoRef};
use serde::Deserialize;
use serde_json::json;

use crate::client::{ApiClient, esc};
use crate::sig::verify_shared_secret;
use crate::url;

/// Username GitLab expects for token-over-HTTPS git access.
const GIT_USER: &str = "oauth2";

pub struct GitlabForge {
    name: String,
    cfg: ForgeConfig,
    api: ApiClient,
}

impl GitlabForge {
    pub fn new(name: impl Into<String>, mut cfg: ForgeConfig) -> Result<Self> {
        let name = name.into();
        if cfg.base_url.is_empty() {
            cfg.base_url = "https://gitlab.com/api/v4".into();
        }
        let auth = if cfg.token.is_empty() {
            None
        } else {
            Some(("PRIVATE-TOKEN".to_string(), cfg.token.clone()))
        };
        let api = ApiClient::new(&name, &cfg.base_url, auth, vec![], cfg.timeout_secs)?;
        Ok(Self { name, cfg, api })
    }

    /// `/projects/{url-encoded path}`. The whole namespace goes through `esc`,
    /// slashes included, because GitLab wants one opaque identifier here.
    fn project_path(repo: &RepoRef) -> String {
        format!("/projects/{}", esc(&repo.full_name()))
    }
}

// ---------------------------------------------------------------------------
// REST shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApiProject {
    #[serde(default)]
    default_branch: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    http_url_to_repo: String,
}

#[derive(Debug, Deserialize)]
struct ApiIssue {
    #[serde(default)]
    iid: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    web_url: String,
    /// Strings over REST, objects over the webhook. Two shapes, two types.
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    author: Option<ApiAuthor>,
}

#[derive(Debug, Deserialize)]
struct ApiAuthor {
    #[serde(default)]
    username: String,
}

#[derive(Debug, Deserialize)]
struct ApiMergeRequest {
    #[serde(default)]
    iid: u64,
    #[serde(default)]
    web_url: String,
}

impl ApiIssue {
    fn into_domain(self) -> Issue {
        Issue {
            number: self.iid,
            title: self.title,
            body: self.description.unwrap_or_default(),
            url: self.web_url,
            labels: self.labels,
            author: self.author.map(|a| a.username).unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Webhook shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct HookLabel {
    #[serde(default)]
    title: String,
}

#[derive(Debug, Deserialize)]
struct HookProject {
    #[serde(default)]
    path_with_namespace: String,
    #[serde(default)]
    default_branch: String,
}

#[derive(Debug, Deserialize)]
struct HookIssueAttrs {
    #[serde(default)]
    iid: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    url: String,
    /// "open", "update", "close", "reopen". Labels arrive as an "update".
    #[serde(default)]
    action: String,
    #[serde(default)]
    labels: Vec<HookLabel>,
    /// Present on note hooks.
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    noteable_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HookLabelChange {
    #[serde(default)]
    previous: Vec<HookLabel>,
    #[serde(default)]
    current: Vec<HookLabel>,
}

#[derive(Debug, Default, Deserialize)]
struct HookChanges {
    #[serde(default)]
    labels: Option<HookLabelChange>,
}

#[derive(Debug, Deserialize)]
struct HookUser {
    #[serde(default)]
    username: String,
}

#[derive(Debug, Deserialize)]
struct HookBody {
    #[serde(default)]
    object_kind: String,
    #[serde(default)]
    user: Option<HookUser>,
    #[serde(default)]
    project: Option<HookProject>,
    #[serde(default)]
    object_attributes: Option<HookIssueAttrs>,
    #[serde(default)]
    labels: Vec<HookLabel>,
    #[serde(default)]
    changes: HookChanges,
    /// Present on note hooks: the issue the comment is attached to.
    #[serde(default)]
    issue: Option<HookIssueAttrs>,
}

/// Reads a GitLab delivery. Kept as a free function so the payload handling is
/// testable without constructing a client.
fn interpret(body: &HookBody, forge: &str, trigger_label: &str, bot_login: &str) -> ForgeEvent {
    let ignored = |reason: String| ForgeEvent::Ignored { reason };

    let Some(project) = body.project.as_ref() else {
        return ignored("payload has no project".into());
    };
    if project.path_with_namespace.is_empty() {
        return ignored("project has no path".into());
    }
    let Ok(mut repo) = RepoRef::parse(&project.path_with_namespace, forge) else {
        return ignored(format!(
            "cannot read project path `{}`",
            project.path_with_namespace
        ));
    };
    if !project.default_branch.is_empty() {
        repo.default_branch = Some(project.default_branch.clone());
    }

    let actor = body
        .user
        .as_ref()
        .map(|u| u.username.clone())
        .unwrap_or_default();
    if !bot_login.is_empty() && actor == bot_login {
        return ignored("event was caused by the bot itself".into());
    }

    match body.object_kind.as_str() {
        "issue" => {
            let Some(attrs) = body.object_attributes.as_ref() else {
                return ignored("issue hook without object_attributes".into());
            };

            // Labels live in two places depending on the GitLab version.
            let labels: Vec<String> = if attrs.labels.is_empty() {
                body.labels.iter().map(|l| l.title.clone()).collect()
            } else {
                attrs.labels.iter().map(|l| l.title.clone()).collect()
            };

            let issue = Issue {
                number: attrs.iid,
                title: attrs.title.clone(),
                body: attrs.description.clone().unwrap_or_default(),
                url: attrs.url.clone(),
                labels,
                author: actor,
            };

            match attrs.action.as_str() {
                "open" => ForgeEvent::IssueOpened { repo, issue },

                // GitLab reports a label change as a generic update carrying a
                // before and after list, so the added labels are a set difference.
                "update" => {
                    let Some(change) = body.changes.labels.as_ref() else {
                        return ignored("issue update did not change labels".into());
                    };
                    let before: Vec<&str> =
                        change.previous.iter().map(|l| l.title.as_str()).collect();
                    let added: Vec<String> = change
                        .current
                        .iter()
                        .map(|l| l.title.clone())
                        .filter(|t| !before.contains(&t.as_str()))
                        .collect();

                    if added.is_empty() {
                        return ignored("labels were removed, not added".into());
                    }
                    match added
                        .iter()
                        .find(|l| trigger_label.is_empty() || *l == trigger_label)
                    {
                        Some(label) => ForgeEvent::IssueLabeled {
                            repo,
                            issue,
                            label: label.clone(),
                        },
                        None => ignored(format!("added labels {added:?} are not the trigger")),
                    }
                }

                other => ignored(format!("issue action `{other}` is not acted on")),
            }
        }

        "note" => {
            let Some(attrs) = body.object_attributes.as_ref() else {
                return ignored("note hook without object_attributes".into());
            };
            if attrs.noteable_type.as_deref() != Some("Issue") {
                return ignored(format!(
                    "comment is on a {}, not an issue",
                    attrs.noteable_type.as_deref().unwrap_or("unknown object")
                ));
            }
            let Some(issue_attrs) = body.issue.as_ref() else {
                return ignored("note hook without an issue".into());
            };

            let issue = Issue {
                number: issue_attrs.iid,
                title: issue_attrs.title.clone(),
                body: issue_attrs.description.clone().unwrap_or_default(),
                url: issue_attrs.url.clone(),
                labels: issue_attrs.labels.iter().map(|l| l.title.clone()).collect(),
                author: actor.clone(),
            };

            ForgeEvent::IssueComment {
                repo,
                issue,
                author: actor,
                body: attrs.note.clone().unwrap_or_default(),
            }
        }

        other => ignored(format!("object_kind `{other}` is not acted on")),
    }
}

#[async_trait]
impl Forge for GitlabForge {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> ForgeKind {
        ForgeKind::Gitlab
    }

    async fn repo_info(&self, repo: &RepoRef) -> Result<RepoInfo> {
        let p: ApiProject = self.api.get(&Self::project_path(repo), "repo_info").await?;
        Ok(RepoInfo {
            default_branch: if p.default_branch.is_empty() {
                "main".into()
            } else {
                p.default_branch
            },
            private: p.visibility != "public",
            clone_url: p.http_url_to_repo,
        })
    }

    async fn get_issue(&self, repo: &RepoRef, number: u64) -> Result<Issue> {
        let path = format!("{}/issues/{number}", Self::project_path(repo));
        let issue: ApiIssue = self.api.get(&path, "get_issue").await?;
        Ok(issue.into_domain())
    }

    async fn comment_issue(&self, repo: &RepoRef, number: u64, body: &str) -> Result<()> {
        let path = format!("{}/issues/{number}/notes", Self::project_path(repo));
        self.api
            .post_ignore(&path, &json!({ "body": body }), "comment_issue")
            .await
    }

    async fn add_labels(&self, repo: &RepoRef, number: u64, labels: &[String]) -> Result<()> {
        if labels.is_empty() {
            return Ok(());
        }
        // GitLab takes a comma-separated list on the issue itself, not a
        // dedicated labels endpoint.
        let path = format!("{}/issues/{number}", Self::project_path(repo));
        self.api
            .put(
                &path,
                &json!({ "add_labels": labels.join(",") }),
                "add_labels",
            )
            .await
    }

    async fn remove_label(&self, repo: &RepoRef, number: u64, label: &str) -> Result<()> {
        let path = format!("{}/issues/{number}", Self::project_path(repo));
        self.api
            .put(&path, &json!({ "remove_labels": label }), "remove_label")
            .await
    }

    async fn open_pull_request(&self, repo: &RepoRef, req: &PullRequestReq) -> Result<PullRequest> {
        // GitLab has no draft flag either; a `Draft:` title prefix is the marker.
        let title = if req.draft && !req.title.starts_with("Draft:") {
            format!("Draft: {}", req.title)
        } else {
            req.title.clone()
        };

        let path = format!("{}/merge_requests", Self::project_path(repo));
        let mut body = json!({
            "source_branch": req.head,
            "target_branch": req.base,
            "title": title,
            "description": req.body,
            "remove_source_branch": true,
        });
        if !req.labels.is_empty() {
            body["labels"] = json!(req.labels.join(","));
        }

        let mr: ApiMergeRequest = self.api.post(&path, &body, "open_merge_request").await?;
        Ok(PullRequest {
            number: mr.iid,
            url: mr.web_url,
            head: req.head.clone(),
            base: req.base.clone(),
        })
    }

    async fn delete_branch(&self, repo: &RepoRef, branch: &str) -> Result<()> {
        // Unlike the git refs API on the other two, this takes the branch as a
        // single encoded identifier.
        let path = format!(
            "{}/repository/branches/{}",
            Self::project_path(repo),
            esc(branch)
        );
        self.api.delete_idempotent(&path, "delete_branch").await
    }

    fn prunes_branches(&self) -> bool {
        self.cfg.delete_rejected_branches
    }

    fn clone_url(&self, repo: &RepoRef) -> String {
        // full_name may carry subgroups, so it is split back apart here rather
        // than passed as an owner and a name.
        let (owner, name) = repo
            .full_name()
            .rsplit_once('/')
            .map(|(o, n)| (o.to_string(), n.to_string()))
            .unwrap_or_else(|| (repo.owner.clone(), repo.name.clone()));

        url::clone_url(
            self.api.base_url(),
            GIT_USER,
            &self.cfg.token,
            &owner,
            &name,
        )
    }

    fn verify_webhook(&self, delivery: &WebhookDelivery) -> Result<()> {
        let token = delivery
            .header("x-gitlab-token")
            .ok_or_else(|| Error::forge(&self.name, "delivery carries no X-Gitlab-Token header"))?;
        verify_shared_secret(&self.cfg.webhook_secret, token, &self.name)
    }

    fn parse_webhook(&self, delivery: &WebhookDelivery) -> Result<ForgeEvent> {
        let body: HookBody = serde_json::from_slice(&delivery.body)
            .map_err(|e| Error::forge(&self.name, format!("unparseable webhook body: {e}")))?;
        Ok(interpret(
            &body,
            &self.name,
            &self.cfg.trigger_label,
            &self.cfg.bot_login,
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

    fn forge() -> GitlabForge {
        GitlabForge::new(
            "gitlab",
            ForgeConfig {
                kind: ForgeKind::Gitlab,
                base_url: String::new(),
                token: "glpat-secret".into(),
                webhook_secret: "hooktoken".into(),
                trigger_label: "gitai".into(),
                bot_login: "gitai-bot".into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn parse(raw: &str) -> HookBody {
        serde_json::from_str(raw).unwrap()
    }

    const PROJECT: &str =
        r#""project": {"path_with_namespace": "acme/platform/widgets", "default_branch": "main"}"#;

    #[test]
    fn project_paths_are_one_encoded_identifier() {
        let repo = RepoRef::parse("acme/platform/widgets", "gitlab").unwrap();
        assert_eq!(
            GitlabForge::project_path(&repo),
            "/projects/acme%2Fplatform%2Fwidgets"
        );
    }

    #[test]
    fn clone_urls_survive_subgroups() {
        let repo = RepoRef::parse("acme/platform/widgets", "gitlab").unwrap();
        assert_eq!(
            forge().clone_url(&repo),
            "https://oauth2:glpat-secret@gitlab.com/acme/platform/widgets.git"
        );
    }

    #[test]
    fn an_opened_issue_is_recognised() {
        let body = parse(&format!(
            r#"{{"object_kind":"issue",{PROJECT},
                 "user":{{"username":"radomir"}},
                 "object_attributes":{{"iid":23,"title":"Cache is never invalidated",
                   "description":"stale reads","url":"https://gl/x/-/issues/23",
                   "action":"open","labels":[{{"title":"gitai"}}]}}}}"#
        ));
        match interpret(&body, "gitlab", "gitai", "gitai-bot") {
            ForgeEvent::IssueOpened { repo, issue } => {
                assert_eq!(repo.full_name(), "acme/platform/widgets");
                assert_eq!(repo.default_branch.as_deref(), Some("main"));
                assert_eq!(issue.number, 23);
                assert_eq!(issue.labels, vec!["gitai"]);
                assert_eq!(issue.author, "radomir");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_label_addition_is_read_out_of_the_changes_diff() {
        let body = parse(&format!(
            r#"{{"object_kind":"issue",{PROJECT},
                 "user":{{"username":"radomir"}},
                 "object_attributes":{{"iid":23,"title":"t","action":"update",
                   "labels":[{{"title":"bug"}},{{"title":"gitai"}}]}},
                 "changes":{{"labels":{{
                    "previous":[{{"title":"bug"}}],
                    "current":[{{"title":"bug"}},{{"title":"gitai"}}]}}}}}}"#
        ));
        match interpret(&body, "gitlab", "gitai", "gitai-bot") {
            ForgeEvent::IssueLabeled { label, issue, .. } => {
                assert_eq!(label, "gitai");
                assert_eq!(issue.number, 23);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_label_that_was_already_there_does_not_retrigger() {
        let body = parse(&format!(
            r#"{{"object_kind":"issue",{PROJECT},
                 "object_attributes":{{"iid":23,"action":"update"}},
                 "changes":{{"labels":{{
                    "previous":[{{"title":"gitai"}},{{"title":"bug"}}],
                    "current":[{{"title":"gitai"}}]}}}}}}"#
        ));
        assert!(matches!(
            interpret(&body, "gitlab", "gitai", "bot"),
            ForgeEvent::Ignored { .. }
        ));
    }

    #[test]
    fn adding_an_unrelated_label_is_ignored() {
        let body = parse(&format!(
            r#"{{"object_kind":"issue",{PROJECT},
                 "object_attributes":{{"iid":23,"action":"update"}},
                 "changes":{{"labels":{{
                    "previous":[],
                    "current":[{{"title":"wontfix"}}]}}}}}}"#
        ));
        match interpret(&body, "gitlab", "gitai", "bot") {
            ForgeEvent::Ignored { reason } => assert!(reason.contains("wontfix"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_note_on_an_issue_becomes_a_comment_event() {
        let body = parse(&format!(
            r#"{{"object_kind":"note",{PROJECT},
                 "user":{{"username":"radomir"}},
                 "object_attributes":{{"note":"/gitai try again","noteable_type":"Issue"}},
                 "issue":{{"iid":23,"title":"t","description":"d"}}}}"#
        ));
        match interpret(&body, "gitlab", "gitai", "gitai-bot") {
            ForgeEvent::IssueComment {
                author,
                body,
                issue,
                ..
            } => {
                assert_eq!(author, "radomir");
                assert_eq!(body, "/gitai try again");
                assert_eq!(issue.number, 23);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_note_on_a_merge_request_is_ignored() {
        let body = parse(&format!(
            r#"{{"object_kind":"note",{PROJECT},
                 "object_attributes":{{"note":"x","noteable_type":"MergeRequest"}}}}"#
        ));
        match interpret(&body, "gitlab", "gitai", "bot") {
            ForgeEvent::Ignored { reason } => assert!(reason.contains("MergeRequest"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_bots_own_events_are_ignored() {
        let body = parse(&format!(
            r#"{{"object_kind":"issue",{PROJECT},
                 "user":{{"username":"gitai-bot"}},
                 "object_attributes":{{"iid":1,"action":"open"}}}}"#
        ));
        assert!(matches!(
            interpret(&body, "gitlab", "gitai", "gitai-bot"),
            ForgeEvent::Ignored { .. }
        ));
    }

    #[test]
    fn pipeline_and_push_hooks_are_ignored() {
        let body = parse(&format!(r#"{{"object_kind":"push",{PROJECT}}}"#));
        match interpret(&body, "gitlab", "gitai", "bot") {
            ForgeEvent::Ignored { reason } => assert!(reason.contains("push"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_shared_token_is_what_authenticates_a_delivery() {
        let f = forge();
        let mut headers = BTreeMap::new();
        headers.insert("x-gitlab-token".to_string(), "hooktoken".to_string());
        let good = WebhookDelivery {
            headers: headers.clone(),
            body: b"{}".to_vec(),
        };
        f.verify_webhook(&good).unwrap();

        headers.insert("x-gitlab-token".to_string(), "wrong".to_string());
        let bad = WebhookDelivery {
            headers,
            body: b"{}".to_vec(),
        };
        assert!(f.verify_webhook(&bad).is_err());

        let missing = WebhookDelivery {
            headers: BTreeMap::new(),
            body: b"{}".to_vec(),
        };
        assert!(f.verify_webhook(&missing).is_err());
    }
}
