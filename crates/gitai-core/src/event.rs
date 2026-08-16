//! Append-only event log. Every meaningful step writes one entry, which is
//! what makes a run replayable: when an attempt goes sideways on iteration 6
//! you can read exactly what the model saw and answered.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::model::{AttemptId, EventId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    TaskCreated,
    StateChanged,
    PlanReady,
    AttemptStarted,
    PatchProduced,
    GateRan,
    EditorFeedback,
    ReviewVerdict,
    ArbiterVerdict,
    RoundFinished,
    PullRequestOpened,
    BudgetWarning,
    Log,
    Failed,
    Cancelled,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TaskCreated => "task_created",
            Self::StateChanged => "state_changed",
            Self::PlanReady => "plan_ready",
            Self::AttemptStarted => "attempt_started",
            Self::PatchProduced => "patch_produced",
            Self::GateRan => "gate_ran",
            Self::EditorFeedback => "editor_feedback",
            Self::ReviewVerdict => "review_verdict",
            Self::ArbiterVerdict => "arbiter_verdict",
            Self::RoundFinished => "round_finished",
            Self::PullRequestOpened => "pull_request_opened",
            Self::BudgetWarning => "budget_warning",
            Self::Log => "log",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "task_created" => Self::TaskCreated,
            "state_changed" => Self::StateChanged,
            "plan_ready" => Self::PlanReady,
            "attempt_started" => Self::AttemptStarted,
            "patch_produced" => Self::PatchProduced,
            "gate_ran" => Self::GateRan,
            "editor_feedback" => Self::EditorFeedback,
            "review_verdict" => Self::ReviewVerdict,
            "arbiter_verdict" => Self::ArbiterVerdict,
            "round_finished" => Self::RoundFinished,
            "pull_request_opened" => Self::PullRequestOpened,
            "budget_warning" => Self::BudgetWarning,
            "log" => Self::Log,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            other => return Err(Error::store(format!("unknown event kind `{other}`"))),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    /// Assigned by the store. Clients resume an SSE stream from the last seq.
    #[serde(default)]
    pub seq: i64,
    pub task_id: TaskId,
    #[serde(default)]
    pub attempt_id: Option<AttemptId>,
    pub kind: EventKind,
    /// Human-readable one-liner for the log view.
    pub message: String,
    /// Structured payload. Shape depends on `kind`.
    #[serde(default)]
    pub data: Value,
    pub at: DateTime<Utc>,
}

impl Event {
    pub fn new(task_id: TaskId, kind: EventKind, message: impl Into<String>) -> Self {
        Self {
            id: EventId::new(),
            seq: 0,
            task_id,
            attempt_id: None,
            kind,
            message: message.into(),
            data: Value::Null,
            at: Utc::now(),
        }
    }

    pub fn with_attempt(mut self, attempt_id: AttemptId) -> Self {
        self.attempt_id = Some(attempt_id);
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }
}
