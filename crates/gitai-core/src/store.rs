//! Persistence and the job queue behind one trait. SQLite backs it today;
//! the shape deliberately avoids anything SQLite-only so Postgres can slot in
//! for the hosted tier without touching the pipeline.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::event::Event;
use crate::model::{Attempt, RepoRef, Task, TaskId, TaskState};

/// A unit of work waiting for a runner. One job per task; a task that is sent
/// back for another round re-enqueues itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub task_id: TaskId,
    /// How many times this job has been handed out, including now.
    pub attempts: u32,
    pub run_after: DateTime<Utc>,
    #[serde(default)]
    pub claimed_by: Option<String>,
}

#[async_trait]
pub trait Store: Send + Sync {
    async fn migrate(&self) -> Result<()>;

    // -- tasks --------------------------------------------------------------

    async fn create_task(&self, task: &Task) -> Result<()>;

    async fn get_task(&self, id: TaskId) -> Result<Task>;

    /// Used to avoid starting a second task for an issue already in flight.
    async fn find_open_task_for_issue(
        &self,
        repo: &RepoRef,
        issue_number: u64,
    ) -> Result<Option<Task>>;

    async fn update_task(&self, task: &Task) -> Result<()>;

    async fn list_tasks(&self, state: Option<TaskState>, limit: i64) -> Result<Vec<Task>>;

    // -- attempts -----------------------------------------------------------

    async fn save_attempt(&self, attempt: &Attempt) -> Result<()>;

    async fn list_attempts(&self, task_id: TaskId) -> Result<Vec<Attempt>>;

    // -- events -------------------------------------------------------------

    /// Assigns `seq` and returns it.
    async fn append_event(&self, event: &Event) -> Result<i64>;

    /// Events for a task with `seq > after`, oldest first.
    async fn list_events(&self, task_id: TaskId, after: i64, limit: i64) -> Result<Vec<Event>>;

    // -- queue --------------------------------------------------------------

    async fn enqueue(&self, task_id: TaskId, run_after: DateTime<Utc>) -> Result<()>;

    /// Atomically hands one due job to `worker`. `Ok(None)` when nothing is due.
    async fn claim_job(&self, worker: &str, lease_secs: u64) -> Result<Option<Job>>;

    async fn finish_job(&self, job_id: i64) -> Result<()>;

    /// Puts a job back for a later attempt, typically with backoff.
    async fn release_job(&self, job_id: i64, run_after: DateTime<Utc>) -> Result<()>;

    /// Frees jobs whose lease expired because a runner died mid-flight.
    async fn reap_expired_leases(&self) -> Result<u64>;
}
