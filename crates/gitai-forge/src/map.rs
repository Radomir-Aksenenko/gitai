//! Turning a parsed webhook body into a [`ForgeEvent`].
//!
//! Gitea and GitHub use different verbs for the same thing (`label_updated`
//! against `labeled`) but the vocabularies do not overlap, so one mapper
//! serves both.

use gitai_core::forge::ForgeEvent;

use crate::payload::IssueEvent;

pub fn map_issue_event(
    forge: &str,
    trigger_label: &str,
    bot_login: &str,
    event_kind: &str,
    ev: IssueEvent,
) -> ForgeEvent {
    let ignored = |reason: &str| ForgeEvent::Ignored {
        reason: reason.to_string(),
    };

    if event_kind != "issues" && event_kind != "issue_comment" {
        return ignored(&format!("event `{event_kind}` is not acted on"));
    }

    let Some(repo) = ev.repository.as_ref().and_then(|r| r.to_repo_ref(forge)) else {
        return ignored("payload has no identifiable repository");
    };
    let Some(api_issue) = ev.issue else {
        return ignored("payload has no issue");
    };
    if api_issue.is_pull_request() {
        return ignored("issue is a pull request");
    }
    let issue = api_issue.into_domain();

    // gitai's own comments and labels must not restart the loop it just finished.
    if !bot_login.is_empty() && issue.author == bot_login && event_kind == "issues" {
        return ignored("issue was opened by the bot itself");
    }

    if event_kind == "issue_comment" {
        let Some(comment) = ev.comment else {
            return ignored("comment event without a comment");
        };
        let author = comment.user.map(|u| u.login).unwrap_or_default();
        if !bot_login.is_empty() && author == bot_login {
            return ignored("comment written by the bot itself");
        }
        if ev.action != "created" && !ev.action.is_empty() {
            return ignored(&format!("comment action `{}` is not acted on", ev.action));
        }
        return ForgeEvent::IssueComment {
            repo,
            issue,
            author,
            body: comment.body,
        };
    }

    match ev.action.as_str() {
        "opened" => ForgeEvent::IssueOpened { repo, issue },

        // GitHub says which label was added.
        "labeled" => {
            let label = ev.label.map(|l| l.name).unwrap_or_default();
            ForgeEvent::IssueLabeled { repo, issue, label }
        }

        // Gitea only says the set changed, so we look for the trigger ourselves.
        "label_updated" => {
            if trigger_label.is_empty() {
                return ignored("label change with no trigger_label configured");
            }
            if issue.labels.iter().any(|l| l == trigger_label) {
                let label = trigger_label.to_string();
                ForgeEvent::IssueLabeled { repo, issue, label }
            } else {
                ignored("label change did not add the trigger label")
            }
        }

        other => ignored(&format!("issue action `{other}` is not acted on")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::IssueEvent;

    fn parse(raw: &str) -> IssueEvent {
        serde_json::from_str(raw).unwrap()
    }

    const REPO: &str = r#""repository": {"full_name": "acme/widgets", "default_branch": "main"}"#;

    #[test]
    fn github_labeled_event_is_recognised() {
        let ev = parse(&format!(
            r#"{{"action":"labeled","label":{{"name":"gitai"}},
                 "issue":{{"number":4,"title":"t","labels":[{{"name":"gitai"}}]}},{REPO}}}"#
        ));
        match map_issue_event("github", "gitai", "gitai-bot", "issues", ev) {
            ForgeEvent::IssueLabeled { label, issue, repo } => {
                assert_eq!(label, "gitai");
                assert_eq!(issue.number, 4);
                assert_eq!(repo.forge, "github");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn gitea_label_update_triggers_only_when_the_label_is_present() {
        let with = parse(&format!(
            r#"{{"action":"label_updated",
                 "issue":{{"number":4,"labels":[{{"name":"gitai"}}]}},{REPO}}}"#
        ));
        assert!(matches!(
            map_issue_event("gitea", "gitai", "bot", "issues", with),
            ForgeEvent::IssueLabeled { .. }
        ));

        let without = parse(&format!(
            r#"{{"action":"label_updated",
                 "issue":{{"number":4,"labels":[{{"name":"bug"}}]}},{REPO}}}"#
        ));
        assert!(matches!(
            map_issue_event("gitea", "gitai", "bot", "issues", without),
            ForgeEvent::Ignored { .. }
        ));
    }

    #[test]
    fn opened_issue_is_picked_up() {
        let ev = parse(&format!(
            r#"{{"action":"opened","issue":{{"number":9,"title":"x"}},{REPO}}}"#
        ));
        assert!(matches!(
            map_issue_event("gitea", "gitai", "bot", "issues", ev),
            ForgeEvent::IssueOpened { .. }
        ));
    }

    #[test]
    fn the_bots_own_activity_is_ignored() {
        let own_issue = parse(&format!(
            r#"{{"action":"opened","issue":{{"number":9,"user":{{"login":"gitai"}}}},{REPO}}}"#
        ));
        assert!(matches!(
            map_issue_event("gitea", "gitai", "gitai", "issues", own_issue),
            ForgeEvent::Ignored { .. }
        ));

        let own_comment = parse(&format!(
            r#"{{"action":"created","issue":{{"number":9}},
                 "comment":{{"body":"done","user":{{"login":"gitai"}}}},{REPO}}}"#
        ));
        assert!(matches!(
            map_issue_event("gitea", "gitai", "gitai", "issue_comment", own_comment),
            ForgeEvent::Ignored { .. }
        ));
    }

    #[test]
    fn human_comments_come_through() {
        let ev = parse(&format!(
            r#"{{"action":"created","issue":{{"number":9}},
                 "comment":{{"body":"also handle nulls","user":{{"login":"radomir"}}}},{REPO}}}"#
        ));
        match map_issue_event("gitea", "gitai", "gitai", "issue_comment", ev) {
            ForgeEvent::IssueComment { author, body, .. } => {
                assert_eq!(author, "radomir");
                assert_eq!(body, "also handle nulls");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pull_requests_and_unknown_events_are_ignored() {
        let pr = parse(&format!(
            r#"{{"action":"opened","issue":{{"number":9,"pull_request":{{"url":"u"}}}},{REPO}}}"#
        ));
        assert!(matches!(
            map_issue_event("github", "gitai", "bot", "issues", pr),
            ForgeEvent::Ignored { .. }
        ));

        let ping = parse(r#"{"action":"ping"}"#);
        assert!(matches!(
            map_issue_event("github", "gitai", "bot", "ping", ping),
            ForgeEvent::Ignored { .. }
        ));
    }

    #[test]
    fn closed_and_edited_issues_do_not_start_work() {
        for action in ["closed", "edited", "assigned"] {
            let ev = parse(&format!(
                r#"{{"action":"{action}","issue":{{"number":9}},{REPO}}}"#
            ));
            assert!(
                matches!(
                    map_issue_event("gitea", "gitai", "bot", "issues", ev),
                    ForgeEvent::Ignored { .. }
                ),
                "action {action} should be ignored"
            );
        }
    }
}
