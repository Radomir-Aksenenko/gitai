//! Read API. Enough to watch a run without opening the database.

use std::convert::Infallible;
use std::str::FromStr;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::{Json, response::IntoResponse};
use futures::stream::Stream;
use gitai_core::model::{TaskId, TaskState};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::error::ApiError;

/// How often the event stream looks for new rows. The store is the source of
/// truth rather than an in-process channel, so a restarted daemon and a second
/// replica both stream the same history.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const PAGE: i64 = 200;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/tasks`
pub async fn list_tasks(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let filter = match q.state.as_deref() {
        Some(s) => Some(
            TaskState::from_str(s)
                .map_err(|_| ApiError::bad_request(format!("unknown task state `{s}`")))?,
        ),
        None => None,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let tasks = state.store.list_tasks(filter, limit).await?;
    Ok(Json(json!({ "tasks": tasks })))
}

/// `GET /api/tasks/{id}`
pub async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = TaskId::from_str(&id).map_err(|_| ApiError::bad_request("task id is not a uuid"))?;
    let task = state.store.get_task(id).await?;
    let attempts = state.store.list_attempts(id).await?;
    Ok(Json(json!({ "task": task, "attempts": attempts })))
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// Resume point. Clients pass the last `seq` they saw.
    #[serde(default)]
    pub after: Option<i64>,
}

/// `GET /api/tasks/{id}/events` as Server-Sent Events.
pub async fn task_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let id = TaskId::from_str(&id).map_err(|_| ApiError::bad_request("task id is not a uuid"))?;
    // Fail here rather than opening a stream that will never produce anything.
    state.store.get_task(id).await?;

    let stream = futures::stream::unfold(
        (state, id, q.after.unwrap_or(0), false),
        |(state, id, cursor, finished)| async move {
            if finished {
                return None;
            }

            let events = state
                .store
                .list_events(id, cursor, PAGE)
                .await
                .unwrap_or_default();

            if let Some(last) = events.last() {
                let next = last.seq;
                let payloads: Vec<Result<SseEvent, Infallible>> = events
                    .iter()
                    .map(|e| {
                        Ok(SseEvent::default()
                            .id(e.seq.to_string())
                            .event(e.kind.as_str())
                            .data(serde_json::to_string(e).unwrap_or_default()))
                    })
                    .collect();
                return Some((futures::stream::iter(payloads), (state, id, next, false)));
            }

            // Caught up. Close the stream once the task can produce no more.
            let done = state
                .store
                .get_task(id)
                .await
                .map(|t| t.state.is_terminal())
                .unwrap_or(true);

            tokio::time::sleep(POLL_INTERVAL).await;
            Some((futures::stream::iter(Vec::new()), (state, id, cursor, done)))
        },
    );

    Ok(Sse::new(futures::StreamExt::flatten(stream)).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_is_clamped_into_a_sane_range() {
        let clamp = |n: i64| n.clamp(1, 500);
        assert_eq!(clamp(0), 1);
        assert_eq!(clamp(-5), 1);
        assert_eq!(clamp(10_000), 500);
        assert_eq!(clamp(50), 50);
    }

    #[test]
    fn task_states_round_trip_through_the_query_string() {
        assert_eq!(
            TaskState::from_str("awaiting_human").unwrap(),
            TaskState::AwaitingHuman
        );
        assert!(TaskState::from_str("nonsense").is_err());
    }
}
