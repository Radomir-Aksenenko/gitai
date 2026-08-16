//! HTTP surface and the worker pool.
//!
//! The two halves talk only through the store: the webhook handler writes a
//! task and a job row, the pool claims jobs and runs them. Nothing is held in
//! memory between them, so a restart mid-run loses nothing that was not
//! already lost, and a second replica is a configuration change rather than a
//! rewrite.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use chrono::Utc;
use gitai_core::config::Config;
use gitai_core::error::Result;
use gitai_core::store::Store;
use gitai_forge::ForgeRegistry;
use gitai_pipeline::Engine;
use tower_http::trace::TraceLayer;

pub mod api;
pub mod error;
pub mod webhook;

/// How long a runner may hold a job before it is treated as dead.
const LEASE_SECS: u64 = 3_600;
/// Idle poll interval when the queue is empty.
const IDLE_POLL: Duration = Duration::from_secs(2);
/// How often expired leases are swept back into the queue.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub forges: Arc<ForgeRegistry>,
    pub engine: Arc<Engine>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(webhook::health))
        .route("/webhooks/{forge}", post(webhook::handle))
        .route("/api/tasks", get(api::list_tasks))
        .route("/api/tasks/{id}", get(api::get_task))
        .route("/api/tasks/{id}/events", get(api::task_events))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Serves until `shutdown` resolves.
pub async fn serve(
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let bind = state.cfg.server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| gitai_core::Error::config(format!("cannot bind {bind}: {e}")))?;

    tracing::info!(%bind, "listening");
    for name in state.forges.names() {
        tracing::info!(forge = name, "webhook endpoint: /webhooks/{name}");
    }

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| gitai_core::Error::config(format!("server stopped: {e}")))?;
    Ok(())
}

/// Starts `concurrency` runners plus the lease reaper. Returns their handles so
/// the caller can await them on shutdown.
pub fn spawn_workers(state: AppState, concurrency: usize) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    for n in 0..concurrency.max(1) {
        let state = state.clone();
        let name = format!("runner-{n}");
        handles.push(tokio::spawn(async move { run_worker(state, name).await }));
    }

    let reaper = state.clone();
    handles.push(tokio::spawn(async move {
        loop {
            tokio::time::sleep(REAP_INTERVAL).await;
            if let Err(e) = reaper.store.reap_expired_leases().await {
                tracing::warn!(error = %e, "lease reaper failed");
            }
        }
    }));

    handles
}

async fn run_worker(state: AppState, name: String) {
    tracing::info!(worker = %name, "runner started");

    loop {
        let claimed = match state.store.claim_job(&name, LEASE_SECS).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(worker = %name, error = %e, "could not claim a job");
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }
        };

        let Some(job) = claimed else {
            tokio::time::sleep(IDLE_POLL).await;
            continue;
        };

        tracing::info!(worker = %name, task = %job.task_id, attempt = job.attempts, "running");

        match state.engine.clone().run_task(job.task_id).await {
            Ok(final_state) => {
                tracing::info!(worker = %name, task = %job.task_id, state = %final_state, "finished");
                if let Err(e) = state.store.finish_job(job.id).await {
                    tracing::warn!(error = %e, "could not clear the finished job");
                }
            }
            Err(e) => {
                // run_task already absorbs task-level failures, so reaching
                // here means something structural. Back off rather than spin.
                let delay = backoff(job.attempts);
                tracing::error!(
                    worker = %name, task = %job.task_id, error = %e,
                    retry_in_secs = delay, "run failed"
                );
                let when = Utc::now() + chrono::Duration::seconds(delay as i64);
                if let Err(e) = state.store.release_job(job.id, when).await {
                    tracing::warn!(error = %e, "could not reschedule the job");
                }
            }
        }
    }
}

/// Exponential, capped at ten minutes.
fn backoff(attempts: u32) -> u64 {
    let secs = 30u64.saturating_mul(2u64.saturating_pow(attempts.saturating_sub(1).min(10)));
    secs.min(600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_grows_then_stops() {
        assert_eq!(backoff(1), 30);
        assert_eq!(backoff(2), 60);
        assert_eq!(backoff(3), 120);
        assert_eq!(backoff(10), 600);
        assert_eq!(backoff(100), 600, "must not overflow into a shorter delay");
        assert_eq!(backoff(0), 30, "a zero attempt count is still a real delay");
    }
}
