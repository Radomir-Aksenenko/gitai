//! Schema.
//!
//! Domain objects are stored as JSON in a `data` column, with only the fields
//! that are actually queried lifted out into real columns. The pipeline's shape
//! is still moving, and this keeps a change to `Spec` or `GateReport` from
//! being a migration. The moment a query needs something new, it gets its own
//! column and a backfill.

/// Applied in order, every time the process starts. Each statement is written
/// to be safe to re-run.
pub const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS tasks (
        id            TEXT PRIMARY KEY,
        forge         TEXT    NOT NULL,
        owner         TEXT    NOT NULL,
        repo          TEXT    NOT NULL,
        issue_number  INTEGER NOT NULL,
        state         TEXT    NOT NULL,
        data          TEXT    NOT NULL,
        created_at    TEXT    NOT NULL,
        updated_at    TEXT    NOT NULL
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_tasks_issue
        ON tasks (forge, owner, repo, issue_number)
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_tasks_state
        ON tasks (state, updated_at DESC)
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS attempts (
        id         TEXT PRIMARY KEY,
        task_id    TEXT    NOT NULL,
        round      INTEGER NOT NULL,
        idx        INTEGER NOT NULL,
        state      TEXT    NOT NULL,
        data       TEXT    NOT NULL,
        updated_at TEXT    NOT NULL
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_attempts_task
        ON attempts (task_id, round, idx)
    "#,
    // seq is the cursor an SSE client resumes from, so it must be monotonic
    // across the whole table rather than per task.
    r#"
    CREATE TABLE IF NOT EXISTS events (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        id         TEXT NOT NULL,
        task_id    TEXT NOT NULL,
        attempt_id TEXT,
        kind       TEXT NOT NULL,
        message    TEXT NOT NULL,
        data       TEXT NOT NULL,
        at         TEXT NOT NULL
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_events_task
        ON events (task_id, seq)
    "#,
    // One row per task: a task sent back for another round re-uses its job
    // rather than piling up duplicates.
    r#"
    CREATE TABLE IF NOT EXISTS jobs (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id     TEXT    NOT NULL UNIQUE,
        attempts    INTEGER NOT NULL DEFAULT 0,
        run_after   TEXT    NOT NULL,
        claimed_by  TEXT,
        lease_until TEXT
    )
    "#,
    r#"
    CREATE INDEX IF NOT EXISTS idx_jobs_due
        ON jobs (run_after)
    "#,
];
