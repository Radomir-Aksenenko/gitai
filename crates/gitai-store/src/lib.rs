//! SQLite-backed [`Store`].
//!
//! SQLite rather than Postgres for the self-hosted build: it keeps the install
//! to a binary plus a file. The trait is the seam, so the hosted tier can grow
//! a Postgres implementation without the pipeline noticing.
//!
//! SQLite has no `SKIP LOCKED`, so the queue leans on `BEGIN IMMEDIATE` plus a
//! lease column. Under WAL that is correct for the concurrency a single gitai
//! process needs, and a claim that races simply finds the row already taken.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gitai_core::error::{Error, Result};
use gitai_core::event::Event;
use gitai_core::model::{Attempt, RepoRef, Task, TaskId, TaskState};
use gitai_core::store::{Job, Store};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

pub mod schema;

/// How long a claimed job stays claimed before it is considered abandoned.
const DEFAULT_LEASE_SECS: u64 = 1_800;

pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// `url` is a `sqlite://` DSN. The file and its parent directory are
    /// created when missing.
    pub async fn connect(url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(|e| Error::store(format!("bad storage.url `{url}`: {e}")))?
            .create_if_missing(true)
            // WAL is what lets a reader (the API) and the writer (the pipeline)
            // work at the same time instead of blocking each other.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(30));

        if let Some(path) = opts.get_filename().parent()
            && !path.as_os_str().is_empty()
        {
            std::fs::create_dir_all(path)
                .map_err(|e| Error::store(format!("cannot create {}: {e}", path.display())))?;
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(opts)
            .await
            .map_err(|e| Error::store(format!("cannot open database: {e}")))?;

        Ok(Self { pool })
    }

    pub async fn in_memory() -> Result<Self> {
        let store = Self::connect("sqlite::memory:").await?;
        store.migrate().await?;
        Ok(store)
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339()
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| Error::store(format!("bad timestamp `{s}`: {e}")))
}

fn db(e: sqlx::Error) -> Error {
    Error::store(e.to_string())
}

#[async_trait]
impl Store for SqliteStore {
    async fn migrate(&self) -> Result<()> {
        // Deref to `&'static str`: sqlx 0.9 refuses non-static SQL so that
        // dynamically built statements have to be audited deliberately. These
        // are compile-time literals.
        for stmt in schema::MIGRATIONS {
            sqlx::query(*stmt).execute(&self.pool).await.map_err(db)?;
        }
        Ok(())
    }

    async fn create_task(&self, task: &Task) -> Result<()> {
        sqlx::query(
            "INSERT INTO tasks (id, forge, owner, repo, issue_number, state, data, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task.id.to_string())
        .bind(&task.repo.forge)
        .bind(&task.repo.owner)
        .bind(&task.repo.name)
        .bind(task.issue.number as i64)
        .bind(task.state.as_str())
        .bind(serde_json::to_string(task)?)
        .bind(ts(task.created_at))
        .bind(ts(task.updated_at))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn get_task(&self, id: TaskId) -> Result<Task> {
        let row = sqlx::query("SELECT data FROM tasks WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| Error::NotFound(format!("task {id}")))?;
        Ok(serde_json::from_str(
            row.try_get::<String, _>("data").map_err(db)?.as_str(),
        )?)
    }

    async fn find_open_task_for_issue(
        &self,
        repo: &RepoRef,
        issue_number: u64,
    ) -> Result<Option<Task>> {
        let row = sqlx::query(
            "SELECT data FROM tasks
             WHERE forge = ? AND owner = ? AND repo = ? AND issue_number = ?
               AND state NOT IN ('done', 'failed', 'cancelled')
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&repo.forge)
        .bind(&repo.owner)
        .bind(&repo.name)
        .bind(issue_number as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;

        match row {
            Some(r) => Ok(Some(serde_json::from_str(
                r.try_get::<String, _>("data").map_err(db)?.as_str(),
            )?)),
            None => Ok(None),
        }
    }

    async fn update_task(&self, task: &Task) -> Result<()> {
        let affected =
            sqlx::query("UPDATE tasks SET state = ?, data = ?, updated_at = ? WHERE id = ?")
                .bind(task.state.as_str())
                .bind(serde_json::to_string(task)?)
                .bind(ts(Utc::now()))
                .bind(task.id.to_string())
                .execute(&self.pool)
                .await
                .map_err(db)?
                .rows_affected();

        if affected == 0 {
            return Err(Error::NotFound(format!("task {}", task.id)));
        }
        Ok(())
    }

    async fn list_tasks(&self, state: Option<TaskState>, limit: i64) -> Result<Vec<Task>> {
        let rows = match state {
            Some(s) => {
                sqlx::query(
                    "SELECT data FROM tasks WHERE state = ? ORDER BY updated_at DESC LIMIT ?",
                )
                .bind(s.as_str())
                .bind(limit)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query("SELECT data FROM tasks ORDER BY updated_at DESC LIMIT ?")
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(db)?;

        rows.into_iter()
            .map(|r| {
                let data: String = r.try_get("data").map_err(db)?;
                Ok(serde_json::from_str(&data)?)
            })
            .collect()
    }

    async fn save_attempt(&self, attempt: &Attempt) -> Result<()> {
        sqlx::query(
            "INSERT INTO attempts (id, task_id, round, idx, state, data, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 state = excluded.state,
                 data = excluded.data,
                 updated_at = excluded.updated_at",
        )
        .bind(attempt.id.to_string())
        .bind(attempt.task_id.to_string())
        .bind(attempt.round as i64)
        .bind(attempt.index as i64)
        .bind(attempt.state.as_str())
        .bind(serde_json::to_string(attempt)?)
        .bind(ts(Utc::now()))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn list_attempts(&self, task_id: TaskId) -> Result<Vec<Attempt>> {
        let rows =
            sqlx::query("SELECT data FROM attempts WHERE task_id = ? ORDER BY round ASC, idx ASC")
                .bind(task_id.to_string())
                .fetch_all(&self.pool)
                .await
                .map_err(db)?;

        rows.into_iter()
            .map(|r| {
                let data: String = r.try_get("data").map_err(db)?;
                Ok(serde_json::from_str(&data)?)
            })
            .collect()
    }

    async fn append_event(&self, event: &Event) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO events (id, task_id, attempt_id, kind, message, data, at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event.id.to_string())
        .bind(event.task_id.to_string())
        .bind(event.attempt_id.map(|a| a.to_string()))
        .bind(event.kind.as_str())
        .bind(&event.message)
        .bind(serde_json::to_string(&event.data)?)
        .bind(ts(event.at))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(res.last_insert_rowid())
    }

    async fn list_events(&self, task_id: TaskId, after: i64, limit: i64) -> Result<Vec<Event>> {
        let rows = sqlx::query(
            "SELECT seq, id, task_id, attempt_id, kind, message, data, at
             FROM events WHERE task_id = ? AND seq > ? ORDER BY seq ASC LIMIT ?",
        )
        .bind(task_id.to_string())
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let attempt_id: Option<String> = r.try_get("attempt_id").map_err(db)?;
            let data: String = r.try_get("data").map_err(db)?;
            out.push(Event {
                id: r.try_get::<String, _>("id").map_err(db)?.parse()?,
                seq: r.try_get("seq").map_err(db)?,
                task_id: r.try_get::<String, _>("task_id").map_err(db)?.parse()?,
                attempt_id: attempt_id.map(|a| a.parse()).transpose()?,
                kind: r.try_get::<String, _>("kind").map_err(db)?.parse()?,
                message: r.try_get("message").map_err(db)?,
                data: serde_json::from_str(&data).unwrap_or(serde_json::Value::Null),
                at: parse_ts(&r.try_get::<String, _>("at").map_err(db)?)?,
            });
        }
        Ok(out)
    }

    async fn enqueue(&self, task_id: TaskId, run_after: DateTime<Utc>) -> Result<()> {
        // A task already queued has its schedule moved, not a second row added.
        sqlx::query(
            "INSERT INTO jobs (task_id, attempts, run_after, claimed_by, lease_until)
             VALUES (?, 0, ?, NULL, NULL)
             ON CONFLICT(task_id) DO UPDATE SET
                 run_after = excluded.run_after,
                 claimed_by = NULL,
                 lease_until = NULL",
        )
        .bind(task_id.to_string())
        .bind(ts(run_after))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn claim_job(&self, worker: &str, lease_secs: u64) -> Result<Option<Job>> {
        let now = Utc::now();
        let lease_secs = if lease_secs == 0 {
            DEFAULT_LEASE_SECS
        } else {
            lease_secs
        };
        let lease_until = now + chrono::Duration::seconds(lease_secs as i64);

        // BEGIN IMMEDIATE, not the plain BEGIN sqlx defaults to.
        //
        // A deferred transaction takes a read lock for the SELECT and then has
        // to upgrade it for the UPDATE. SQLite will not wait on a lock upgrade
        // even with busy_timeout set, because waiting there can deadlock, so it
        // returns SQLITE_BUSY at once. Taking the write lock up front both
        // fixes that and is what makes the claim atomic between runners.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;

        let row = sqlx::query(
            "SELECT id, task_id, attempts FROM jobs
             WHERE run_after <= ?
               AND (claimed_by IS NULL OR lease_until IS NULL OR lease_until < ?)
             ORDER BY run_after ASC LIMIT 1",
        )
        .bind(ts(now))
        .bind(ts(now))
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;

        let Some(row) = row else {
            tx.commit().await.map_err(db)?;
            return Ok(None);
        };

        let id: i64 = row.try_get("id").map_err(db)?;
        let task_id: String = row.try_get("task_id").map_err(db)?;
        let attempts: i64 = row.try_get("attempts").map_err(db)?;

        sqlx::query(
            "UPDATE jobs SET claimed_by = ?, lease_until = ?, attempts = attempts + 1
             WHERE id = ?",
        )
        .bind(worker)
        .bind(ts(lease_until))
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;

        tx.commit().await.map_err(db)?;

        Ok(Some(Job {
            id,
            task_id: task_id.parse()?,
            attempts: (attempts + 1) as u32,
            run_after: now,
            claimed_by: Some(worker.to_string()),
        }))
    }

    async fn finish_job(&self, job_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM jobs WHERE id = ?")
            .bind(job_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn release_job(&self, job_id: i64, run_after: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET claimed_by = NULL, lease_until = NULL, run_after = ?
             WHERE id = ?",
        )
        .bind(ts(run_after))
        .bind(job_id)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn reap_expired_leases(&self) -> Result<u64> {
        let now = ts(Utc::now());
        let affected = sqlx::query(
            "UPDATE jobs SET claimed_by = NULL, lease_until = NULL
             WHERE claimed_by IS NOT NULL AND lease_until IS NOT NULL AND lease_until < ?",
        )
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db)?
        .rows_affected();

        if affected > 0 {
            tracing::warn!(count = affected, "released jobs whose lease had expired");
        }
        Ok(affected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitai_core::event::{Event, EventKind};
    use gitai_core::model::{AttemptState, Budget, Issue};

    fn task() -> Task {
        Task::new(
            RepoRef::parse("acme/widgets", "gitea").unwrap(),
            Issue {
                number: 42,
                title: "Cache is never invalidated".into(),
                body: "steps...".into(),
                url: "https://git.example.com/acme/widgets/issues/42".into(),
                labels: vec!["gitai".into()],
                author: "radomir".into(),
            },
            Budget::default(),
        )
    }

    #[tokio::test]
    async fn a_task_round_trips_with_its_nested_state() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut t = task();
        t.spec = Some(gitai_core::model::Spec {
            goal: "invalidate on write".into(),
            acceptance: vec!["stale reads stop".into()],
            ..Default::default()
        });
        store.create_task(&t).await.unwrap();

        let back = store.get_task(t.id).await.unwrap();
        assert_eq!(back.issue.number, 42);
        assert_eq!(back.spec.unwrap().acceptance, vec!["stale reads stop"]);
    }

    #[tokio::test]
    async fn a_missing_task_is_not_found_rather_than_a_panic() {
        let store = SqliteStore::in_memory().await.unwrap();
        let err = store.get_task(TaskId::new()).await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn open_task_lookup_ignores_finished_ones() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut t = task();
        store.create_task(&t).await.unwrap();
        assert!(
            store
                .find_open_task_for_issue(&t.repo, 42)
                .await
                .unwrap()
                .is_some()
        );

        t.state = TaskState::Done;
        store.update_task(&t).await.unwrap();
        assert!(
            store
                .find_open_task_for_issue(&t.repo, 42)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn attempts_are_upserted_not_duplicated() {
        let store = SqliteStore::in_memory().await.unwrap();
        let t = task();
        store.create_task(&t).await.unwrap();

        let mut a = Attempt::new(t.id, 0, 0, "small".into(), "gitai/x".into());
        store.save_attempt(&a).await.unwrap();
        a.state = AttemptState::GatePassed;
        a.patch = Some("diff --git a b".into());
        store.save_attempt(&a).await.unwrap();

        let list = store.list_attempts(t.id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].state, AttemptState::GatePassed);
        assert!(list[0].patch.is_some());
    }

    #[tokio::test]
    async fn events_carry_a_monotonic_cursor() {
        let store = SqliteStore::in_memory().await.unwrap();
        let t = task();
        store.create_task(&t).await.unwrap();

        for i in 0..5 {
            let ev = Event::new(t.id, EventKind::Log, format!("step {i}"));
            store.append_event(&ev).await.unwrap();
        }

        let all = store.list_events(t.id, 0, 100).await.unwrap();
        assert_eq!(all.len(), 5);
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));

        let resumed = store.list_events(t.id, all[2].seq, 100).await.unwrap();
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed[0].message, "step 3");
    }

    #[tokio::test]
    async fn a_job_is_handed_out_once() {
        let store = SqliteStore::in_memory().await.unwrap();
        let t = task();
        store.create_task(&t).await.unwrap();
        store.enqueue(t.id, Utc::now()).await.unwrap();

        let first = store.claim_job("runner-a", 60).await.unwrap();
        assert!(first.is_some());
        let second = store.claim_job("runner-b", 60).await.unwrap();
        assert!(
            second.is_none(),
            "a claimed job must not be handed out again"
        );

        store.finish_job(first.unwrap().id).await.unwrap();
        assert!(store.claim_job("runner-c", 60).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_job_scheduled_for_later_is_not_due_yet() {
        let store = SqliteStore::in_memory().await.unwrap();
        let t = task();
        store.create_task(&t).await.unwrap();
        store
            .enqueue(t.id, Utc::now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        assert!(store.claim_job("runner", 60).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn enqueueing_twice_does_not_create_a_second_job() {
        let store = SqliteStore::in_memory().await.unwrap();
        let t = task();
        store.create_task(&t).await.unwrap();
        store.enqueue(t.id, Utc::now()).await.unwrap();
        store.enqueue(t.id, Utc::now()).await.unwrap();

        assert!(store.claim_job("a", 60).await.unwrap().is_some());
        assert!(store.claim_job("b", 60).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_expired_lease_is_reaped_and_the_job_runs_again() {
        let store = SqliteStore::in_memory().await.unwrap();
        let t = task();
        store.create_task(&t).await.unwrap();
        store.enqueue(t.id, Utc::now()).await.unwrap();

        // A runner takes the job, then dies.
        let claimed = store.claim_job("doomed-runner", 1).await.unwrap().unwrap();
        assert_eq!(claimed.attempts, 1);

        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(store.reap_expired_leases().await.unwrap(), 1);

        let again = store
            .claim_job("healthy-runner", 60)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.id, claimed.id);
        assert_eq!(again.attempts, 2, "the retry count must survive the reap");
    }

    #[tokio::test]
    async fn a_released_job_comes_back_when_its_backoff_elapses() {
        let store = SqliteStore::in_memory().await.unwrap();
        let t = task();
        store.create_task(&t).await.unwrap();
        store.enqueue(t.id, Utc::now()).await.unwrap();

        let job = store.claim_job("a", 600).await.unwrap().unwrap();
        store
            .release_job(job.id, Utc::now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        assert!(store.claim_job("b", 600).await.unwrap().is_none());

        store.release_job(job.id, Utc::now()).await.unwrap();
        assert!(store.claim_job("c", 600).await.unwrap().is_some());
    }

    /// Two independent pools on one database file stand in for two gitai
    /// processes. SQLite locks at the file level, so this exercises the same
    /// path a second replica would take. `BEGIN IMMEDIATE` plus the lease
    /// column is what keeps one job from being handed out twice.
    #[tokio::test]
    async fn two_connections_to_one_file_cannot_claim_the_same_job() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let url = format!(
            "sqlite://{}?mode=rwc",
            path.display().to_string().replace('\\', "/")
        );

        let first = SqliteStore::connect(&url).await.unwrap();
        first.migrate().await.unwrap();
        let second = SqliteStore::connect(&url).await.unwrap();

        let t = task();
        first.create_task(&t).await.unwrap();
        first.enqueue(t.id, Utc::now()).await.unwrap();

        let (a, b) = tokio::join!(
            first.claim_job("process-a", 600),
            second.claim_job("process-b", 600)
        );
        let claimed = [a.unwrap(), b.unwrap()];
        let winners = claimed.iter().filter(|c| c.is_some()).count();
        assert_eq!(winners, 1, "exactly one process may hold the job");

        // The loser must also see nothing on a later poll.
        assert!(second.claim_job("process-b", 600).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_second_process_picks_up_work_the_first_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let url = format!(
            "sqlite://{}?mode=rwc",
            path.display().to_string().replace('\\', "/")
        );

        let first = SqliteStore::connect(&url).await.unwrap();
        first.migrate().await.unwrap();
        let second = SqliteStore::connect(&url).await.unwrap();

        let t = task();
        first.create_task(&t).await.unwrap();
        first.enqueue(t.id, Utc::now()).await.unwrap();

        let held = first.claim_job("dying-process", 1).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;

        assert_eq!(second.reap_expired_leases().await.unwrap(), 1);
        let taken = second
            .claim_job("healthy-process", 600)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(taken.id, held.id);
        assert_eq!(taken.task_id, t.id);
    }

    #[tokio::test]
    async fn listing_filters_by_state() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut a = task();
        let mut b = task();
        b.state = TaskState::AwaitingHuman;
        store.create_task(&a).await.unwrap();
        store.create_task(&b).await.unwrap();
        a.state = TaskState::Working;
        store.update_task(&a).await.unwrap();

        assert_eq!(store.list_tasks(None, 10).await.unwrap().len(), 2);
        assert_eq!(
            store
                .list_tasks(Some(TaskState::Working), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
