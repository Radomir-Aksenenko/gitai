//! Webhook and REST payload shapes.
//!
//! Gitea, Forgejo and GitHub converged on nearly the same JSON for issues, so
//! one set of structs covers all three. Where they differ (the action verb for
//! a label change, mostly) the adapter handles it, not these types.

use gitai_core::model::{Issue, RepoRef};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ApiUser {
    #[serde(default)]
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiLabel {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiIssue {
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub labels: Vec<ApiLabel>,
    #[serde(default)]
    pub user: Option<ApiUser>,
    /// GitHub sets this on issues that are really pull requests. gitai skips them.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

impl ApiIssue {
    pub fn into_domain(self) -> Issue {
        let url = if self.html_url.is_empty() {
            self.url
        } else {
            self.html_url
        };
        Issue {
            number: self.number,
            title: self.title,
            body: self.body.unwrap_or_default(),
            url,
            labels: self.labels.into_iter().map(|l| l.name).collect(),
            author: self.user.map(|u| u.login).unwrap_or_default(),
        }
    }

    pub fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiRepo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub clone_url: String,
    #[serde(default)]
    pub owner: Option<ApiUser>,
}

impl ApiRepo {
    /// `full_name` is the reliable field: `owner` is absent from some Gitea
    /// webhook payloads.
    pub fn to_repo_ref(&self, forge: &str) -> Option<RepoRef> {
        let (owner, name) = match self.full_name.split_once('/') {
            Some((o, n)) => (o.to_string(), n.to_string()),
            None => {
                let owner = self.owner.as_ref()?.login.clone();
                if owner.is_empty() || self.name.is_empty() {
                    return None;
                }
                (owner, self.name.clone())
            }
        };
        Some(RepoRef {
            forge: forge.to_string(),
            owner,
            name,
            default_branch: if self.default_branch.is_empty() {
                None
            } else {
                Some(self.default_branch.clone())
            },
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiComment {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub user: Option<ApiUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiPullRequest {
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub url: String,
}

/// The `issues` and `issue_comment` webhook bodies, which share a shape.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueEvent {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub issue: Option<ApiIssue>,
    #[serde(default)]
    pub repository: Option<ApiRepo>,
    #[serde(default)]
    pub label: Option<ApiLabel>,
    #[serde(default)]
    pub comment: Option<ApiComment>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitea_issue_payload_maps_to_domain() {
        let raw = r#"{
          "action": "opened",
          "issue": {
            "number": 7,
            "title": "Cache is never invalidated",
            "body": "steps to reproduce...",
            "html_url": "https://git.example.com/acme/widgets/issues/7",
            "labels": [{"name": "gitai", "id": 3}],
            "user": {"login": "radomir"}
          },
          "repository": {
            "name": "widgets",
            "full_name": "acme/widgets",
            "default_branch": "main",
            "private": true
          }
        }"#;
        let ev: IssueEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(ev.action, "opened");
        let repo = ev.repository.unwrap().to_repo_ref("gitea").unwrap();
        assert_eq!(repo.full_name(), "acme/widgets");
        assert_eq!(repo.default_branch.as_deref(), Some("main"));
        let issue = ev.issue.unwrap().into_domain();
        assert_eq!(issue.number, 7);
        assert_eq!(issue.labels, vec!["gitai"]);
        assert_eq!(issue.author, "radomir");
    }

    #[test]
    fn repo_ref_falls_back_to_owner_when_full_name_is_missing() {
        let repo: ApiRepo =
            serde_json::from_str(r#"{"name":"widgets","owner":{"login":"acme"}}"#).unwrap();
        assert_eq!(
            repo.to_repo_ref("github").unwrap().full_name(),
            "acme/widgets"
        );
    }

    #[test]
    fn github_pull_requests_are_recognised_among_issues() {
        let issue: ApiIssue =
            serde_json::from_str(r#"{"number":1,"pull_request":{"url":"x"}}"#).unwrap();
        assert!(issue.is_pull_request());
    }

    #[test]
    fn missing_optional_fields_do_not_break_parsing() {
        let ev: IssueEvent = serde_json::from_str(r#"{"action":"ping"}"#).unwrap();
        assert!(ev.issue.is_none());
        assert!(ev.repository.is_none());
    }
}
