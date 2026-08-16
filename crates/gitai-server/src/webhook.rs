//! Webhook intake.
//!
//! Two things happen here and they are kept apart on purpose. Deciding whether
//! a delivery should start work is pure logic with no I/O, so it is tested
//! directly; the handler around it does verification, deduplication and
//! enqueueing.

use std::collections::BTreeMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use gitai_core::forge::{ForgeEvent, WebhookDelivery};
use gitai_core::model::{Issue, RepoRef, Task, TaskState};
use serde_json::json;

use crate::AppState;
use crate::error::ApiError;

/// What a delivery means for the queue.
#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    Start { repo: RepoRef, issue: Issue },
    Stop { repo: RepoRef, issue: Issue },
    Help { repo: RepoRef, issue: Issue },
    Ignore(String),
}

/// Decides whether to act, given the forge's trigger label.
///
/// An empty `trigger_label` means every issue qualifies. That is the setting
/// people reach for first and regret on a busy repository, so the default
/// config ships with a label.
pub fn decide(event: ForgeEvent, trigger_label: &str) -> Trigger {
    let armed_by_label =
        |labels: &[String]| trigger_label.is_empty() || labels.iter().any(|l| l == trigger_label);

    match event {
        ForgeEvent::IssueOpened { repo, issue } => {
            if armed_by_label(&issue.labels) {
                Trigger::Start { repo, issue }
            } else {
                Trigger::Ignore(format!("issue carries no `{trigger_label}` label"))
            }
        }

        ForgeEvent::IssueLabeled { repo, issue, label } => {
            if trigger_label.is_empty() || label == trigger_label {
                Trigger::Start { repo, issue }
            } else {
                Trigger::Ignore(format!("label `{label}` is not the trigger"))
            }
        }

        // A comment restarts/stops/guides work on explicit commands.
        ForgeEvent::IssueComment {
            repo, issue, body, ..
        } => {
            let prefix = if trigger_label.is_empty() {
                "gitai"
            } else {
                trigger_label
            };
            let help_cmd = format!("/{prefix} help");
            let stop_cmd = format!("/{prefix} stop");
            let cancel_cmd = format!("/{prefix} cancel");
            let start_cmd = format!("/{prefix}");

            if body.lines().any(|l| l.trim().starts_with(&help_cmd)) {
                Trigger::Help { repo, issue }
            } else if body.lines().any(|l| {
                let t = l.trim();
                t.starts_with(&stop_cmd) || t.starts_with(&cancel_cmd)
            }) {
                Trigger::Stop { repo, issue }
            } else if body.lines().any(|l| l.trim().starts_with(&start_cmd)) {
                Trigger::Start { repo, issue }
            } else {
                Trigger::Ignore(format!("comment does not start with `/{prefix}`"))
            }
        }

        ForgeEvent::Ignored { reason } => Trigger::Ignore(reason),
    }
}

/// `POST /webhooks/{forge}`
pub async fn handle(
    State(state): State<AppState>,
    Path(forge_name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let forge = state
        .forges
        .get(&forge_name)
        .map_err(|_| ApiError::not_found(format!("no forge named `{forge_name}`")))?;

    let delivery = WebhookDelivery {
        headers: lowercase_headers(&headers),
        body: body.to_vec(),
    };

    // Verification comes before parsing: an unsigned body is not worth reading.
    forge
        .verify_webhook(&delivery)
        .map_err(ApiError::unauthorized)?;

    let event = forge
        .parse_webhook(&delivery)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let trigger = decide(event, forge.trigger_label());

    match trigger {
        Trigger::Ignore(reason) => {
            tracing::debug!(forge = %forge_name, %reason, "delivery ignored");
            Ok((
                StatusCode::OK,
                Json(json!({ "status": "ignored", "reason": reason })),
            ))
        }

        Trigger::Help { repo, issue } => {
            let prefix = if forge.trigger_label().is_empty() {
                "gitai"
            } else {
                forge.trigger_label()
            };
            let help_text = format!(
                "👋 **Справка по командам GitAI:**\n\n\
                 - `/{prefix}` (или `/{prefix} start`) — Запустить автоматическое решение этой задачи (анализ, план, воркеры, тесты, код-ревью и Pull Request).\n\
                 - `/{prefix} stop` (или `/{prefix} cancel`) — Остановить выполнение текущей задачи и прервать работу воркеров.\n\
                 - `/{prefix} help` — Показать эту справку.\n\n\
                 💡 *Также запуск происходит автоматически при добавлении метки `{prefix}` к задаче.*"
            );
            if let Err(e) = forge.comment_issue(&repo, issue.number, &help_text).await {
                tracing::warn!(error = %e, "could not post help comment on issue");
            }
            Ok((
                StatusCode::OK,
                Json(json!({ "status": "help_posted", "issue": issue.number })),
            ))
        }

        Trigger::Stop { repo, issue } => {
            if let Some(mut existing) = state
                .store
                .find_open_task_for_issue(&repo, issue.number)
                .await?
            {
                existing.state = TaskState::Cancelled;
                state.store.update_task(&existing).await?;
                state
                    .store
                    .append_event(&gitai_core::event::Event::new(
                        existing.id,
                        gitai_core::event::EventKind::Cancelled,
                        format!("task stopped by user command on issue #{}", issue.number),
                    ))
                    .await?;

                let comment = "🛑 **Выполнение задачи остановлено по команде `/gitai stop`**.\n\nВоркеры прервали работу.";
                if let Err(e) = forge.comment_issue(&repo, issue.number, comment).await {
                    tracing::warn!(error = %e, "could not comment cancellation on the issue");
                }

                tracing::info!(task = %existing.id, repo = %repo, issue = issue.number, "task stopped by user");
                Ok((
                    StatusCode::OK,
                    Json(json!({
                        "status": "stopped",
                        "task_id": existing.id.to_string(),
                    })),
                ))
            } else {
                let comment = "ℹ️ В данный момент нет активных задач GitAI для остановки по этой проблеме.";
                let _ = forge.comment_issue(&repo, issue.number, comment).await;
                Ok((
                    StatusCode::OK,
                    Json(json!({ "status": "no active task to stop" })),
                ))
            }
        }

        Trigger::Start { repo, issue } => {
            // One task per issue at a time. A second label event while a run is in
            // flight is a duplicate, not a reason to start over.
            if let Some(existing) = state
                .store
                .find_open_task_for_issue(&repo, issue.number)
                .await?
            {
                return Ok((
                    StatusCode::OK,
                    Json(json!({
                        "status": "already running",
                        "task_id": existing.id.to_string(),
                        "state": existing.state.as_str(),
                    })),
                ));
            }

            let task = Task::new(repo, issue, state.cfg.budget);
            state.store.create_task(&task).await?;
            state
                .store
                .append_event(&gitai_core::event::Event::new(
                    task.id,
                    gitai_core::event::EventKind::TaskCreated,
                    format!("issue #{} picked up", task.issue.number),
                ))
                .await?;
            state.store.enqueue(task.id, chrono::Utc::now()).await?;

            tracing::info!(task = %task.id, repo = %task.repo, issue = task.issue.number, "queued");

            Ok((
                StatusCode::ACCEPTED,
                Json(json!({ "status": "queued", "task_id": task.id.to_string() })),
            ))
        }
    }
}

/// Forge adapters look headers up by lowercase name, and HTTP header names are
/// case insensitive, so normalising once here keeps that out of every adapter.
fn lowercase_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|value| (k.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}

/// `GET /healthz`
pub async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoRef {
        RepoRef::parse("acme/widgets", "gitea").unwrap()
    }

    fn issue(labels: &[&str]) -> Issue {
        Issue {
            number: 7,
            title: "Cache is never invalidated".into(),
            body: "b".into(),
            url: "u".into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            author: "radomir".into(),
        }
    }

    #[test]
    fn an_opened_issue_needs_the_trigger_label() {
        let with = ForgeEvent::IssueOpened {
            repo: repo(),
            issue: issue(&["gitai"]),
        };
        assert!(matches!(decide(with, "gitai"), Trigger::Start { .. }));

        let without = ForgeEvent::IssueOpened {
            repo: repo(),
            issue: issue(&["bug"]),
        };
        assert!(matches!(decide(without, "gitai"), Trigger::Ignore(_)));
    }

    #[test]
    fn an_empty_trigger_label_accepts_every_issue() {
        let ev = ForgeEvent::IssueOpened {
            repo: repo(),
            issue: issue(&[]),
        };
        assert!(matches!(decide(ev, ""), Trigger::Start { .. }));
    }

    #[test]
    fn only_the_trigger_label_starts_work() {
        let right = ForgeEvent::IssueLabeled {
            repo: repo(),
            issue: issue(&["gitai"]),
            label: "gitai".into(),
        };
        assert!(matches!(decide(right, "gitai"), Trigger::Start { .. }));

        let wrong = ForgeEvent::IssueLabeled {
            repo: repo(),
            issue: issue(&["wontfix"]),
            label: "wontfix".into(),
        };
        match decide(wrong, "gitai") {
            Trigger::Ignore(reason) => assert!(reason.contains("wontfix"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_comment_only_counts_when_it_is_a_command() {
        let command = ForgeEvent::IssueComment {
            repo: repo(),
            issue: issue(&[]),
            author: "radomir".into(),
            body: "/gitai try again with the null case".into(),
        };
        assert!(matches!(decide(command, "gitai"), Trigger::Start { .. }));

        let chatter = ForgeEvent::IssueComment {
            repo: repo(),
            issue: issue(&[]),
            author: "radomir".into(),
            body: "I think gitai should handle this".into(),
        };
        assert!(matches!(decide(chatter, "gitai"), Trigger::Ignore(_)));
    }

    #[test]
    fn a_command_further_down_a_comment_still_counts() {
        let ev = ForgeEvent::IssueComment {
            repo: repo(),
            issue: issue(&[]),
            author: "radomir".into(),
            body: "Some context first.\n\n/gitai go".into(),
        };
        assert!(matches!(decide(ev, "gitai"), Trigger::Start { .. }));
    }

    #[test]
    fn an_ignored_event_keeps_its_reason() {
        let ev = ForgeEvent::Ignored {
            reason: "event `push` is not acted on".into(),
        };
        match decide(ev, "gitai") {
            Trigger::Ignore(r) => assert_eq!(r, "event `push` is not acted on"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stop_and_help_commands_are_recognized() {
        let stop = ForgeEvent::IssueComment {
            repo: repo(),
            issue: issue(&[]),
            author: "radomir".into(),
            body: "/gitai stop".into(),
        };
        assert!(matches!(decide(stop, "gitai"), Trigger::Stop { .. }));

        let cancel = ForgeEvent::IssueComment {
            repo: repo(),
            issue: issue(&[]),
            author: "radomir".into(),
            body: "/gitai cancel please".into(),
        };
        assert!(matches!(decide(cancel, "gitai"), Trigger::Stop { .. }));

        let help = ForgeEvent::IssueComment {
            repo: repo(),
            issue: issue(&[]),
            author: "radomir".into(),
            body: "/gitai help".into(),
        };
        assert!(matches!(decide(help, "gitai"), Trigger::Help { .. }));
    }

    #[test]
    fn header_names_are_normalised_to_lowercase() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Gitea-Event", "issues".parse().unwrap());
        headers.insert("X-GITEA-SIGNATURE", "abcd".parse().unwrap());
        let map = lowercase_headers(&headers);
        assert_eq!(map.get("x-gitea-event").map(String::as_str), Some("issues"));
        assert_eq!(
            map.get("x-gitea-signature").map(String::as_str),
            Some("abcd")
        );
    }
}
