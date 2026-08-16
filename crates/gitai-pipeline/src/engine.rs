//! The loop.
//!
//! ```text
//! plan  ->  fan out N attempts  ->  gate  ->  editor  ->  review  ->  arbiter
//!             ^                       |                                 |
//!             |                       +--- inner loop, until it passes --+
//!             +---------------- outer loop, on rejection ----------------+
//! ```
//!
//! Two loops, two different jobs. The inner one is a worker and an editor
//! pushing one attempt until the gate stops complaining. The outer one throws
//! the whole round away and starts again with the arbiter's reasons. Both are
//! bounded, because an unbounded self-correcting loop is just a way to spend
//! money slowly.

use std::sync::Arc;
use std::time::Instant;

use gitai_core::config::{Config, GateConfig, RepoConfig};
use gitai_core::error::{Error, Result};
use gitai_core::event::{Event, EventKind};
use gitai_core::forge::Forge;
use gitai_core::model::{
    Attempt, AttemptState, GateReport, PullRequestReq, Spec, Spend, Task, TaskId, TaskState,
    Verdict,
};
use gitai_core::sandbox::{Sandbox, Workspace, WorkspaceSpec};
use gitai_core::store::Store;
use gitai_forge::ForgeRegistry;
use gitai_llm::registry::ModelRegistry;
use serde_json::json;
use tokio::task::JoinSet;

use crate::context;
use crate::edits;
use crate::prompts::Prompts;
use crate::roles::{
    ArbiterCtx, EditorCtx, PlannerCtx, ReviewSummary, ReviewerCtx, Roles, WorkerCtx,
};
use gitai_core::model::Role;

pub struct Engine {
    cfg: Arc<Config>,
    store: Arc<dyn Store>,
    sandbox: Arc<dyn Sandbox>,
    forges: Arc<ForgeRegistry>,
    roles: Arc<Roles>,
}

impl Engine {
    pub fn new(
        cfg: Arc<Config>,
        store: Arc<dyn Store>,
        sandbox: Arc<dyn Sandbox>,
        forges: Arc<ForgeRegistry>,
    ) -> Result<Self> {
        let models = Arc::new(ModelRegistry::build(&cfg)?);
        let prompts = Arc::new(Prompts::load(&cfg.prompts.dir)?);
        let roles = Arc::new(Roles::new(cfg.clone(), models, prompts));
        Ok(Self {
            cfg,
            store,
            sandbox,
            forges,
            roles,
        })
    }

    /// Drives one task to a terminal state. Never panics on a model or forge
    /// failure: the task is marked failed and the reason is recorded.
    pub async fn run_task(self: Arc<Self>, task_id: TaskId) -> Result<TaskState> {
        let started = Instant::now();
        let mut task = self.store.get_task(task_id).await?;

        match self.clone().drive(&mut task, started).await {
            Ok(state) => Ok(state),
            Err(e) => {
                tracing::error!(task = %task_id, error = %e, "task failed");
                task.state = TaskState::Failed;
                task.last_error = Some(e.to_string());
                task.spend.wall_secs = started.elapsed().as_secs();
                let _ = self.store.update_task(&task).await;
                self.emit(Event::new(task_id, EventKind::Failed, e.to_string()))
                    .await;
                self.report_failure(&task, &e.to_string()).await;
                // Nothing is being merged, so no branch here has a future.
                let forge = self.delivery(&task).ok().flatten();
                self.prune_branches(&task, None, forge.as_deref()).await;
                Ok(TaskState::Failed)
            }
        }
    }

    async fn drive(self: Arc<Self>, task: &mut Task, started: Instant) -> Result<TaskState> {
        let forge = self.delivery(task)?;
        let repo_url = self.repo_url(task, forge.as_deref())?;

        // -- base branch ----------------------------------------------------
        if task.base_branch.is_none() {
            task.base_branch = Some(match forge.as_deref() {
                Some(f) => f.repo_info(&task.repo).await?.default_branch,
                // Local runs set this from the checkout before enqueueing;
                // this is only the fallback if that did not happen.
                None => "main".to_string(),
            });
        }
        let base_branch = task.base_branch.clone().expect("just set");
        self.announce_start(task, forge.as_deref()).await;

        // -- plan -----------------------------------------------------------
        if task.spec.is_none() {
            self.set_state(task, TaskState::Planning).await?;
            let (spec, spend) = self.plan(task, &repo_url, &base_branch).await?;
            // The planner runs on the strongest model in the pipeline and is
            // often the single most expensive call in a run, so its spend has
            // to land on the task before the round loop checks the budget.
            task.spend.add(&spend);
            self.emit(
                Event::new(task.id, EventKind::PlanReady, spec.goal.clone()).with_data(json!(spec)),
            )
            .await;
            task.spec = Some(spec);
            self.store.update_task(task).await?;
            self.announce_plan(task, forge.as_deref()).await;
        }
        let spec = task.spec.clone().expect("just set");

        // -- rounds ---------------------------------------------------------
        let budget = task.budget;
        let mut feedback = String::new();

        while task.round < budget.max_rounds {
            let current = self.store.get_task(task.id).await?;
            if current.state == TaskState::Cancelled {
                task.state = TaskState::Cancelled;
                let forge = self.delivery(task).ok().flatten();
                self.prune_branches(task, None, forge.as_deref()).await;
                return Ok(TaskState::Cancelled);
            }

            task.spend.wall_secs = started.elapsed().as_secs();
            task.spend.check(&budget)?;

            self.set_state(task, TaskState::Working).await?;
            let attempts = self
                .clone()
                .run_round(task, &spec, &repo_url, &base_branch, &feedback)
                .await;

            for a in &attempts {
                task.spend.add(&a.spend);
                let _ = self.store.save_attempt(a).await;
            }

            let passed: Vec<&Attempt> = attempts.iter().filter(|a| a.gate_passed()).collect();
            self.emit(
                Event::new(
                    task.id,
                    EventKind::RoundFinished,
                    format!(
                        "round {}: {}/{} attempts cleared the gate",
                        task.round,
                        passed.len(),
                        attempts.len()
                    ),
                )
                .with_data(json!({ "round": task.round, "passed": passed.len() })),
            )
            .await;
            self.announce_round(task, forge.as_deref(), &attempts, passed.len()).await;

            if passed.is_empty() {
                feedback = aggregate_failures(&attempts);
                task.round += 1;
                self.store.update_task(task).await?;
                continue;
            }

            // -- review ------------------------------------------------------
            // Fanned out like the attempts were: with five survivors and a
            // mid-tier reviewer, doing these in sequence adds minutes per round
            // for no reason. The provider semaphore still bounds the real
            // concurrency.
            self.set_state(task, TaskState::Reviewing).await?;
            let mut reviewed: Vec<Attempt> = Vec::new();

            if task.budget.parallel {
                let mut set = JoinSet::new();
                for attempt in passed {
                    let engine = self.clone();
                    let spec = spec.clone();
                    let task_view = task.clone();
                    let mut attempt = attempt.clone();

                    set.spawn(async move {
                        let spend = match engine.review(&spec, &attempt, &task_view).await {
                            Ok((verdict, spend)) => {
                                engine
                                    .emit(
                                        Event::new(
                                            task_view.id,
                                            EventKind::ReviewVerdict,
                                            format!("score {}: {}", verdict.score, verdict.summary),
                                        )
                                        .with_attempt(attempt.id)
                                        .with_data(json!(verdict)),
                                    )
                                    .await;
                                attempt.review = Some(verdict);
                                spend
                            }
                            Err(e) => {
                                tracing::warn!(attempt = %attempt.id, error = %e, "review failed");
                                attempt.review = Some(Verdict::rejected("reviewer", e.to_string()));
                                Spend::default()
                            }
                        };
                        let _ = engine.store.save_attempt(&attempt).await;
                        (attempt, spend)
                    });
                }

                while let Some(joined) = set.join_next().await {
                    match joined {
                        Ok((attempt, spend)) => {
                            task.spend.add(&spend);
                            reviewed.push(attempt);
                        }
                        Err(e) => tracing::error!(error = %e, "review task did not finish"),
                    }
                }
            } else {
                for attempt in passed {
                    let mut attempt = attempt.clone();
                    let spend = match self.review(&spec, &attempt, task).await {
                        Ok((verdict, spend)) => {
                            self.emit(
                                Event::new(
                                    task.id,
                                    EventKind::ReviewVerdict,
                                    format!("score {}: {}", verdict.score, verdict.summary),
                                )
                                .with_attempt(attempt.id)
                                .with_data(json!(verdict)),
                            )
                            .await;
                            attempt.review = Some(verdict);
                            spend
                        }
                        Err(e) => {
                            tracing::warn!(attempt = %attempt.id, error = %e, "review failed");
                            attempt.review = Some(Verdict::rejected("reviewer", e.to_string()));
                            Spend::default()
                        }
                    };
                    let _ = self.store.save_attempt(&attempt).await;
                    task.spend.add(&spend);
                    reviewed.push(attempt);
                }
            }

            if reviewed.is_empty() {
                feedback = "Every review failed to run. Try a different approach.".into();
                task.round += 1;
                self.store.update_task(task).await?;
                continue;
            }

            self.announce_reviews(task, forge.as_deref(), &reviewed).await;

            // Sort by index first so an equal rank resolves the same way on
            // every run, whatever order the reviews happened to finish in.
            reviewed.sort_by_key(|a| a.index);
            reviewed.sort_by_key(|a| std::cmp::Reverse(a.rank()));
            let best = reviewed.first().cloned().expect("passed was not empty");

            // -- arbiter -----------------------------------------------------
            let others: Vec<ReviewSummary> = reviewed
                .iter()
                .skip(1)
                .filter_map(|a| {
                    a.review.as_ref().map(|r| ReviewSummary {
                        score: r.score,
                        summary: r.summary.clone(),
                    })
                })
                .collect();

            let (verdict, spend) = self
                .arbitrate(task, &spec, &best, others, reviewed.len())
                .await?;
            task.spend.add(&spend);
            self.emit(
                Event::new(
                    task.id,
                    EventKind::ArbiterVerdict,
                    format!(
                        "{}: {}",
                        if verdict.approved {
                            "approved"
                        } else {
                            "sent back"
                        },
                        verdict.summary
                    ),
                )
                .with_attempt(best.id)
                .with_data(json!(verdict)),
            )
            .await;

            if verdict.approved {
                let mut winner = best.clone();
                winner.state = AttemptState::Selected;
                let _ = self.store.save_attempt(&winner).await;
                task.spend.wall_secs = started.elapsed().as_secs();
                return self
                    .deliver(task, &winner, &verdict, forge.as_deref())
                    .await;
            }

            for other in &reviewed {
                let mut other = other.clone();
                other.state = AttemptState::Rejected;
                let _ = self.store.save_attempt(&other).await;
            }

            self.announce_arbiter_feedback(task, forge.as_deref(), &verdict).await;
            feedback = verdict.feedback();
            task.round += 1;
            self.store.update_task(task).await?;
        }

        Err(Error::BudgetExhausted(format!(
            "no attempt was approved in {} rounds",
            budget.max_rounds
        )))
    }

    // -----------------------------------------------------------------------
    // Round
    // -----------------------------------------------------------------------

    async fn run_round(
        self: Arc<Self>,
        task: &Task,
        spec: &Spec,
        repo_url: &str,
        base_branch: &str,
        feedback: &str,
    ) -> Vec<Attempt> {
        if task.budget.parallel {
            let mut set = JoinSet::new();

            for index in 0..task.budget.attempts_per_round {
                let engine = self.clone();
                let task = task.clone();
                let spec = spec.clone();
                let repo_url = repo_url.to_string();
                let base_branch = base_branch.to_string();
                let feedback = feedback.to_string();

                set.spawn(async move {
                    engine
                        .run_attempt(&task, &spec, index, &repo_url, &base_branch, &feedback)
                        .await
                });
            }

            let mut attempts = Vec::new();
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(attempt) => attempts.push(attempt),
                    // A panicked attempt must not take the round down with it.
                    Err(e) => tracing::error!(error = %e, "attempt task did not finish"),
                }
            }
            attempts.sort_by_key(|a| a.index);
            attempts
        } else {
            let mut attempts = Vec::new();
            for index in 0..task.budget.attempts_per_round {
                let attempt = self
                    .clone()
                    .run_attempt(task, spec, index, repo_url, base_branch, feedback)
                    .await;
                attempts.push(attempt);
            }
            attempts
        }
    }

    /// One attempt, start to finish. Failures are recorded on the returned
    /// [`Attempt`] rather than raised, so one bad worker cannot fail the round.
    async fn run_attempt(
        self: Arc<Self>,
        task: &Task,
        spec: &Spec,
        index: u32,
        repo_url: &str,
        base_branch: &str,
        feedback: &str,
    ) -> Attempt {
        let model = self.roles.worker_model(index).to_string();
        let branch = task.branch_name(task.round, index);
        let mut attempt = Attempt::new(task.id, task.round, index, model.clone(), branch.clone());

        self.emit(
            Event::new(
                task.id,
                EventKind::AttemptStarted,
                format!("attempt {index} on model `{model}`"),
            )
            .with_attempt(attempt.id),
        )
        .await;

        let attempt_image = spec
            .image
            .clone()
            .or_else(|| self.resolve_configured_image(&task.repo.full_name()));

        let ws_spec = WorkspaceSpec {
            task_id: task.id,
            attempt_id: attempt.id,
            repo_url: repo_url.to_string(),
            repo_slug: format!("{}-{}", task.repo.owner, task.repo.name),
            base_branch: base_branch.to_string(),
            branch,
            image: attempt_image,
        };

        let workspace = match self.sandbox.create(&ws_spec).await {
            Ok(ws) => ws,
            Err(e) => {
                attempt.state = AttemptState::Errored;
                attempt.error = Some(e.to_string());
                return attempt;
            }
        };

        // From here on every exit has to clean up, so the body is factored out.
        let outcome = self
            .attempt_body(task, spec, &mut attempt, workspace.as_ref(), feedback)
            .await;

        if let Err(e) = outcome {
            tracing::warn!(attempt = %attempt.id, error = %e, "attempt failed");
            attempt.state = AttemptState::Errored;
            attempt.error = Some(e.to_string());
        }

        // The branch has to exist before the workspace is torn down, and at
        // this point we do not yet know which attempt the arbiter will pick.
        // So every attempt that cleared the gate gets pushed. Losing branches
        // are left behind on purpose in this version: they are the cheapest
        // way to see what the other models tried. Pruning them is a job for
        // whoever adds branch deletion to the Forge trait.
        if attempt.state == AttemptState::GatePassed {
            let message = commit_message(task, spec, &attempt);
            match workspace.commit_all(&message).await {
                Ok(Some(_sha)) => {
                    if let Err(e) = workspace.push().await {
                        tracing::warn!(attempt = %attempt.id, error = %e, "push failed");
                        attempt.state = AttemptState::Errored;
                        attempt.error = Some(format!("could not push the branch: {e}"));
                    }
                }
                Ok(None) => {
                    attempt.state = AttemptState::Errored;
                    attempt.error = Some("the gate passed but there was nothing to commit".into());
                }
                Err(e) => {
                    tracing::warn!(attempt = %attempt.id, error = %e, "commit failed");
                    attempt.state = AttemptState::Errored;
                    attempt.error = Some(format!("could not commit: {e}"));
                }
            }
        }

        if let Err(e) = workspace.cleanup().await {
            tracing::warn!(attempt = %attempt.id, error = %e, "workspace cleanup failed");
        }
        attempt.updated_at = chrono::Utc::now();
        attempt
    }

    async fn attempt_body(
        &self,
        task: &Task,
        spec: &Spec,
        attempt: &mut Attempt,
        ws: &dyn Workspace,
        round_feedback: &str,
    ) -> Result<()> {
        let forge = self.delivery(task).ok().flatten();
        let gate_cfg = self.effective_gate_config(spec);
        let setup = gitai_sandbox::run_setup(ws, &gate_cfg).await?;
        if !setup.ok {
            return Err(Error::sandbox(format!(
                "workspace setup failed: {}",
                setup.output
            )));
        }

        // Sized for the model doing the writing, which in a mixed fan-out is
        // not the same model the editor or the arbiter runs on.
        let limits = self.roles.limits(&attempt.model);
        let editor_limits = self.roles.limits_for_role(Role::Editor);

        let file_tree = context::file_tree(ws, &limits).await?;
        let mut open = context::open_files(ws, &spec.relevant_files, &limits).await;
        let mut feedback = round_feedback.to_string();

        for iteration in 0..task.budget.max_iterations {
            attempt.iterations = iteration + 1;

            let (out, spend) = self
                .roles
                .work(
                    &attempt.model,
                    WorkerCtx {
                        repo: task.repo.full_name(),
                        spec: spec.clone(),
                        file_tree: file_tree.clone(),
                        open_files: open.clone(),
                        feedback: feedback.clone(),
                        iteration,
                    },
                )
                .await?;
            attempt.spend.add(&spend);

            // The worker wants to look at something before committing to a change.
            if out.is_read_request() {
                let more = context::open_files(ws, &out.read, &limits).await;
                merge_open_files(&mut open, more);
                feedback = format!(
                    "You asked for {} file(s); they are included above now. \
                     Produce edits this turn.",
                    out.read.len()
                );
                continue;
            }

            let applied = edits::apply(ws, &out.edits).await?;
            self.emit(
                Event::new(
                    task.id,
                    EventKind::PatchProduced,
                    format!(
                        "iteration {iteration}: {} edit(s) applied, {} rejected",
                        applied.applied.len(),
                        applied.failures.len()
                    ),
                )
                .with_attempt(attempt.id)
                .with_data(json!({ "reasoning": out.reasoning })),
            )
            .await;

            let gate = gitai_sandbox::run_gate(ws, &gate_cfg, &spec.allowed_paths).await?;
            self.emit(
                Event::new(
                    task.id,
                    EventKind::GateRan,
                    format!(
                        "iteration {iteration}: gate {}",
                        if gate.passed { "passed" } else { "failed" }
                    ),
                )
                .with_attempt(attempt.id)
                .with_data(json!(gate)),
            )
            .await;

            // Stored at the worker's budget; each later stage re-caps to its
            // own before showing it to a model.
            let raw_diff = ws.diff().await?;
            attempt.patch = Some(context::cap_diff(&raw_diff, &limits));
            attempt.gate = Some(gate.clone());

            let (editor, spend) = self
                .roles
                .edit(EditorCtx {
                    repo: task.repo.full_name(),
                    spec: spec.clone(),
                    gate_summary: gate.summary(),
                    gate: gate.clone(),
                    diff: context::cap_diff(&raw_diff, &editor_limits),
                })
                .await?;
            attempt.spend.add(&spend);
            attempt.editor_notes.push(editor.notes.clone());

            self.emit(
                Event::new(
                    task.id,
                    EventKind::EditorFeedback,
                    if editor.done {
                        "editor is satisfied".to_string()
                    } else {
                        editor.notes.clone()
                    },
                )
                .with_attempt(attempt.id),
            )
            .await;

            // Both have to agree: the gate is objective, the editor catches the
            // patch that passes a weak test without doing the work.
            if gate.passed && editor.done {
                attempt.state = AttemptState::GatePassed;
                self.announce_iteration_result(
                    task,
                    forge.as_deref(),
                    attempt.index,
                    &attempt.model,
                    iteration + 1,
                    true,
                    "Gate пройден успешно и редактор подтвердил решение",
                )
                .await;
                return Ok(());
            }

            let reason = if !gate.passed {
                format!(
                    "Gate не прошёл проверку: `{}`\n> 🧠 **Анализ редактора:** {}",
                    gate.summary(),
                    editor.notes
                )
            } else {
                format!(
                    "Сборка и тесты пройдены, но редактор запросил доработку:\n> 🧠 **Замечания:** {}",
                    editor.notes
                )
            };

            self.announce_iteration_result(
                task,
                forge.as_deref(),
                attempt.index,
                &attempt.model,
                iteration + 1,
                false,
                &reason,
            )
            .await;

            feedback = merge_feedback(&applied.failure_report(), &editor.feedback());
        }

        attempt.state = AttemptState::GateFailed;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Stages
    // -----------------------------------------------------------------------

    async fn plan(&self, task: &Task, repo_url: &str, base_branch: &str) -> Result<(Spec, Spend)> {
        // The planner needs to see the repository, which means one throwaway
        // workspace before any attempt exists.
        let initial_image = self.resolve_configured_image(&task.repo.full_name());
        let spec_ws = WorkspaceSpec {
            task_id: task.id,
            attempt_id: gitai_core::model::AttemptId::new(),
            repo_url: repo_url.to_string(),
            repo_slug: format!("{}-{}", task.repo.owner, task.repo.name),
            base_branch: base_branch.to_string(),
            branch: format!("gitai/issue-{}/plan", task.issue.number),
            image: initial_image.clone(),
        };
        let ws = self.sandbox.create(&spec_ws).await?;
        let limits = self.roles.limits_for_role(Role::Planner);
        let repo_cfg = Self::read_repo_config(ws.as_ref()).await;

        let result = async {
            let file_tree = context::file_tree(ws.as_ref(), &limits).await?;
            let readme = context::read_readme(ws.as_ref(), &limits).await;
            let (mut spec, spend) = self
                .roles
                .plan(PlannerCtx {
                    repo: task.repo.full_name(),
                    issue: task.issue.clone(),
                    base_branch: base_branch.to_string(),
                    file_tree,
                    readme,
                    readme_limit: limits.max_readme_bytes,
                })
                .await?;

            // In-repo .gitai.toml takes precedence if provided:
            if let Some(rc) = &repo_cfg {
                if let Some(img) = &rc.image {
                    spec.image = Some(img.clone());
                }
                if !rc.setup.is_empty() {
                    spec.setup_commands = rc.setup.clone();
                }
                if !rc.build.is_empty() {
                    spec.build_commands = rc.build.clone();
                }
                if !rc.test.is_empty() {
                    spec.test_commands = rc.test.clone();
                }
                if !rc.lint.is_empty() {
                    spec.lint_commands = rc.lint.clone();
                }
            } else if spec.image.is_none() {
                spec.image = initial_image;
            }

            Ok((spec, spend))
        }
        .await;

        if let Err(e) = ws.cleanup().await {
            tracing::warn!(error = %e, "planning workspace cleanup failed");
        }

        result
    }

    async fn review(
        &self,
        spec: &Spec,
        attempt: &Attempt,
        task: &Task,
    ) -> Result<(Verdict, Spend)> {
        let gate = attempt
            .gate
            .clone()
            .ok_or_else(|| Error::bad_output("attempt reached review with no gate report"))?;
        let limits = self.roles.limits_for_role(Role::Reviewer);
        self.roles
            .review(ReviewerCtx {
                repo: task.repo.full_name(),
                spec: spec.clone(),
                gate_summary: gate.summary(),
                gate,
                diff: context::cap_diff(&attempt.patch.clone().unwrap_or_default(), &limits),
            })
            .await
    }

    async fn arbitrate(
        &self,
        task: &Task,
        spec: &Spec,
        best: &Attempt,
        others: Vec<ReviewSummary>,
        attempt_count: usize,
    ) -> Result<(Verdict, Spend)> {
        let gate = best
            .gate
            .clone()
            .unwrap_or_else(|| GateReport::from_checks(vec![]));
        let limits = self.roles.limits_for_role(Role::Arbiter);
        self.roles
            .arbitrate(ArbiterCtx {
                repo: task.repo.full_name(),
                issue: task.issue.clone(),
                spec: spec.clone(),
                round: task.round,
                max_rounds: task.budget.max_rounds,
                attempt_count,
                review: best
                    .review
                    .clone()
                    .unwrap_or_else(|| Verdict::rejected("reviewer", "no review was recorded")),
                other_reviews: others,
                gate_summary: gate.summary(),
                gate,
                diff: context::cap_diff(&best.patch.clone().unwrap_or_default(), &limits),
            })
            .await
    }

    /// Commits, pushes and opens the pull request. This is the only place the
    /// system writes anything a human will see outside the issue thread.
    async fn deliver(
        &self,
        task: &mut Task,
        winner: &Attempt,
        verdict: &Verdict,
        forge: Option<&dyn Forge>,
    ) -> Result<TaskState> {
        let Some(forge) = forge else {
            // Local runs stop here: the branch exists in the working copy and
            // there is nowhere to open a pull request.
            task.state = TaskState::AwaitingHuman;
            self.store.update_task(task).await?;
            self.emit(Event::new(
                task.id,
                EventKind::StateChanged,
                format!("local run finished on branch `{}`", winner.branch),
            ))
            .await;
            return Ok(TaskState::AwaitingHuman);
        };

        let title = format!("{} (#{})", task.issue.title, task.issue.number);
        let body = pull_request_body(task, winner, verdict);

        let pr = forge
            .open_pull_request(
                &task.repo,
                &PullRequestReq {
                    title,
                    body,
                    head: winner.branch.clone(),
                    base: task.base_branch.clone().unwrap_or_else(|| "main".into()),
                    draft: self
                        .cfg
                        .forge(&task.repo.forge)
                        .map(|f| f.draft_prs)
                        .unwrap_or(true),
                    labels: vec![],
                },
            )
            .await?;

        self.emit(
            Event::new(
                task.id,
                EventKind::PullRequestOpened,
                format!("opened #{} - {}", pr.number, pr.url),
            )
            .with_data(json!(pr)),
        )
        .await;

        let comment = format!(
            "Opened {} for this issue.\n\n{}\n\n---\n{}",
            pr.url,
            verdict.summary,
            spend_line(&task.spend)
        );
        if let Err(e) = forge
            .comment_issue(&task.repo, task.issue.number, &comment)
            .await
        {
            tracing::warn!(error = %e, "could not comment on the issue");
        }

        task.pull_request = Some(pr);
        task.state = TaskState::AwaitingHuman;
        self.store.update_task(task).await?;

        self.prune_branches(task, Some(&winner.branch), Some(forge))
            .await;
        Ok(TaskState::AwaitingHuman)
    }

    /// Deletes the branches of attempts that were pushed but not chosen.
    ///
    /// Every attempt that clears the gate has to be pushed, because the winner
    /// is not known until after the workspaces are gone. This is the other half
    /// of that trade. The diffs stay in the event log, so a post-mortem loses
    /// nothing; only the refs go.
    async fn prune_branches(&self, task: &Task, keep: Option<&str>, forge: Option<&dyn Forge>) {
        let Some(forge) = forge else {
            return;
        };
        if !forge.prunes_branches() {
            return;
        }

        let Ok(attempts) = self.store.list_attempts(task.id).await else {
            tracing::warn!(task = %task.id, "could not list attempts to prune branches");
            return;
        };

        let mut removed = 0;
        for attempt in attempts {
            // Only gate-passing attempts ever reached the remote.
            if !attempt.gate_passed() {
                continue;
            }
            if Some(attempt.branch.as_str()) == keep {
                continue;
            }
            match forge.delete_branch(&task.repo, &attempt.branch).await {
                Ok(()) => removed += 1,
                // Cosmetic: the task is already finished either way.
                Err(e) => tracing::warn!(
                    branch = %attempt.branch, error = %e, "could not delete branch"
                ),
            }
        }

        if removed > 0 {
            self.emit(Event::new(
                task.id,
                EventKind::Log,
                format!("removed {removed} branch(es) that were not selected"),
            ))
            .await;
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// `None` for a local run, which has no forge behind it.
    fn delivery(&self, task: &Task) -> Result<Option<Arc<dyn Forge>>> {
        if task.local_repo.is_some() {
            return Ok(None);
        }
        Ok(Some(self.forges.get(&task.repo.forge)?))
    }

    fn repo_url(&self, task: &Task, forge: Option<&dyn Forge>) -> Result<String> {
        match (&task.local_repo, forge) {
            (Some(path), _) => Ok(path.clone()),
            (None, Some(f)) => Ok(f.clone_url(&task.repo)),
            (None, None) => Err(Error::config(format!(
                "task {} has neither a local repository nor a configured forge",
                task.id
            ))),
        }
    }

    async fn set_state(&self, task: &mut Task, state: TaskState) -> Result<()> {
        if task.state == state {
            return Ok(());
        }
        task.state = state;
        self.store.update_task(task).await?;
        self.emit(Event::new(
            task.id,
            EventKind::StateChanged,
            state.to_string(),
        ))
        .await;
        Ok(())
    }

    /// Telemetry must never be the reason a run dies, so a store failure here
    /// is logged and swallowed.
    async fn emit(&self, event: Event) {
        tracing::info!(task = %event.task_id, kind = %event.kind, "{}", event.message);
        if let Err(e) = self.store.append_event(&event).await {
            tracing::warn!(error = %e, "could not record event");
        }
    }

    async fn announce_start(&self, task: &Task, forge: Option<&dyn Forge>) {
        let Some(forge) = forge else {
            return;
        };
        let body = format!(
            "🤖 **GitAI взял задачу в работу**\n\n\
             - **Репозиторий:** `{}` (ветка: `{}`)\n\
             - **Лимит:** до {} раундов по {} попыток воркеров\n\n\
             ⏳ *Анализирую репозиторий и составляю план реализации...*",
            task.repo.full_name(),
            task.base_branch.as_deref().unwrap_or("main"),
            task.budget.max_rounds,
            task.budget.attempts_per_round
        );
        if let Err(e) = forge
            .comment_issue(&task.repo, task.issue.number, &body)
            .await
        {
            tracing::warn!(error = %e, "could not announce start on the issue");
        }
    }

    async fn announce_plan(&self, task: &Task, forge: Option<&dyn Forge>) {
        let (Some(forge), Some(spec)) = (forge, task.spec.as_ref()) else {
            return;
        };
        let mut body = format!(
            "📋 **План решения составлен**\n\n\
             **Цель:**\n{}\n\n\
             **Критерии готовности:**\n",
            spec.goal
        );
        for a in &spec.acceptance {
            body.push_str(&format!("- {a}\n"));
        }
        if !spec.allowed_paths.is_empty() {
            body.push_str(&format!(
                "\n**Файлы в работе:** {}\n",
                spec.allowed_paths
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let gate_cfg = self.effective_gate_config(spec);
        let lang = spec.language.as_deref().unwrap_or("Не определён / Общий");
        let active_image = spec.image.as_deref().unwrap_or(&self.cfg.sandbox.image);
        body.push_str(&format!(
            "\n🔍 **Стек проекта и команды автопроверки (Adaptive Gate):**\n\
             - **Определённый стек:** `{lang}`\n\
             - **Сборочное окружение (Sandbox Image):** `{active_image}`\n\
             - **Установка зависимостей (Setup):** {}\n\
             - **Сборка (Build):** {}\n\
             - **Тестирование (Test):** {}\n\
             - **Линтинг (Lint):** {}\n",
            format_cmds(&gate_cfg.setup),
            format_cmds(&gate_cfg.build),
            format_cmds(&gate_cfg.test),
            format_cmds(&gate_cfg.lint),
        ));

        body.push_str(&format!(
            "\n🚀 *Запускаю раунд {} ({} параллельных попыток воркеров с проверкой через Gate)...*",
            task.round, task.budget.attempts_per_round
        ));
        if let Err(e) = forge
            .comment_issue(&task.repo, task.issue.number, &body)
            .await
        {
            tracing::warn!(error = %e, "could not announce the plan on the issue");
        }
    }

    async fn announce_round(
        &self,
        task: &Task,
        forge: Option<&dyn Forge>,
        attempts: &[Attempt],
        passed_count: usize,
    ) {
        let Some(forge) = forge else {
            return;
        };
        let mut body = format!(
            "🛠 **Раунд {} завершён**\n\n\
             - **Результаты проверки (Gate):** {} из {} попыток успешно прошли сборку и тесты.\n\n\
             **Попытки:**\n",
            task.round,
            passed_count,
            attempts.len()
        );
        for a in attempts {
            let status = if a.gate_passed() {
                format!("✅ Прошла Gate (итераций: {}, модель: `{}`)", a.iterations, a.model)
            } else {
                format!("❌ Не прошла Gate (итераций: {}, модель: `{}`)", a.iterations, a.model)
            };
            body.push_str(&format!("- Попытка #{}: {}\n", a.index, status));
        }

        if passed_count > 0 {
            body.push_str("\n🔍 *Перехожу к этапу независимого код-ревью и арбитража...*");
        } else {
            body.push_str("\n⚠️ *Ни одна попытка не прошла тесты. Передаю ошибки в следующий раунд для исправления...*");
        }

        if let Err(e) = forge
            .comment_issue(&task.repo, task.issue.number, &body)
            .await
        {
            tracing::warn!(error = %e, "could not announce round results on the issue");
        }
    }

    async fn announce_arbiter_feedback(
        &self,
        task: &Task,
        forge: Option<&dyn Forge>,
        verdict: &Verdict,
    ) {
        let Some(forge) = forge else {
            return;
        };
        let body = format!(
            "🔄 **Арбитр запросил доработку (Раунд {} → {})**\n\n\
             **Замечания арбитра:**\n{}\n\n\
             *Запускаю следующий раунд с учётом замечаний...*",
            task.round,
            task.round + 1,
            verdict.summary
        );
        if let Err(e) = forge
            .comment_issue(&task.repo, task.issue.number, &body)
            .await
        {
            tracing::warn!(error = %e, "could not announce arbiter feedback on the issue");
        }
    }

    async fn announce_iteration_result(
        &self,
        task: &Task,
        forge: Option<&dyn Forge>,
        index: u32,
        model: &str,
        iteration: u32,
        passed: bool,
        detail: &str,
    ) {
        let Some(forge) = forge else {
            return;
        };
        let body = if passed {
            format!(
                "✅ **Попытка #{index} (модель `{model}`, итерация {iteration}):** Сборка и тесты пройдены успешно! Редактор подтвердил корректность кода."
            )
        } else {
            format!(
                "🔄 **Попытка #{index} (модель `{model}`, итерация {iteration}):**\n{detail}\n\n*Передаю указания воркеру для исправления...*"
            )
        };
        if let Err(e) = forge
            .comment_issue(&task.repo, task.issue.number, &body)
            .await
        {
            tracing::warn!(error = %e, "could not announce iteration result on the issue");
        }
    }

    async fn announce_reviews(&self, task: &Task, forge: Option<&dyn Forge>, attempts: &[Attempt]) {
        let Some(forge) = forge else {
            return;
        };
        let mut body = "🧐 **Результаты независимого код-ревью (Reviewer):**\n\n".to_string();
        for a in attempts {
            if let Some(r) = &a.review {
                let icon = if r.approved { "🟢" } else { "🟡" };
                body.push_str(&format!(
                    "- {icon} **Попытка #{}** (модель `{}`): оценка **{}/100**\n  *{}*\n",
                    a.index, a.model, r.score, r.summary
                ));
            }
        }
        body.push_str("\n⚖️ *Передаю лучшее решение арбитру для финального утверждения...*");
        if let Err(e) = forge
            .comment_issue(&task.repo, task.issue.number, &body)
            .await
        {
            tracing::warn!(error = %e, "could not announce reviews on the issue");
        }
    }

    async fn report_failure(&self, task: &Task, reason: &str) {
        let Ok(Some(forge)) = self.delivery(task) else {
            return;
        };
        let body = format!(
            "Could not finish this one.\n\n```\n{reason}\n```\n\n{}",
            spend_line(&task.spend)
        );
        if let Err(e) = forge
            .comment_issue(&task.repo, task.issue.number, &body)
            .await
        {
            tracing::warn!(error = %e, "could not report the failure on the issue");
        }
    }

    /// Resolves configured sandbox image for a repository based on sandbox.images mapping.
    fn resolve_configured_image(&self, repo_full_name: &str) -> Option<String> {
        if let Some(img) = self.cfg.sandbox.images.get(repo_full_name) {
            return Some(img.clone());
        }
        if let Some((_owner, name)) = repo_full_name.split_once('/') {
            if let Some(img) = self.cfg.sandbox.images.get(name) {
                return Some(img.clone());
            }
        }
        None
    }

    /// Reads .gitai.toml in the repository root if present.
    async fn read_repo_config(ws: &dyn Workspace) -> Option<RepoConfig> {
        let content = ws.read_file(".gitai.toml").await.ok()?;
        RepoConfig::from_toml(&content)
            .map_err(|e| {
                tracing::warn!(error = %e, "failed to parse in-repo .gitai.toml");
                e
            })
            .ok()
    }

    /// Derives the effective gate config, combining static config with dynamically detected
    /// commands from the planner model if the static configuration is empty.
    fn effective_gate_config(&self, spec: &Spec) -> GateConfig {
        let mut cfg = self.cfg.gate.clone();
        if cfg.setup.is_empty() && !spec.setup_commands.is_empty() {
            cfg.setup = spec.setup_commands.clone();
        }
        if cfg.build.is_empty() && !spec.build_commands.is_empty() {
            cfg.build = spec.build_commands.clone();
        }
        if cfg.test.is_empty() && !spec.test_commands.is_empty() {
            cfg.test = spec.test_commands.clone();
        }
        if cfg.lint.is_empty() && !spec.lint_commands.is_empty() {
            cfg.lint = spec.lint_commands.clone();
        }
        cfg
    }
}

fn format_cmds(cmds: &[String]) -> String {
    if cmds.is_empty() {
        "`(нет)`".to_string()
    } else {
        cmds.iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Merges newly read files into the open set, replacing any earlier copy so
/// the model never sees the same path twice with different contents.
fn merge_open_files(open: &mut Vec<context::OpenFile>, more: Vec<context::OpenFile>) {
    for file in more {
        match open.iter_mut().find(|f| f.path == file.path) {
            Some(existing) => *existing = file,
            None => open.push(file),
        }
    }
}

fn merge_feedback(edit_failures: &str, editor: &str) -> String {
    match (edit_failures.is_empty(), editor.is_empty()) {
        (true, _) => editor.to_string(),
        (false, true) => edit_failures.to_string(),
        (false, false) => format!("{edit_failures}\n\n{editor}"),
    }
}

/// What the next round is told when every attempt in this one failed the gate.
/// Only the two most informative failures are carried, because a round of five
/// identical build errors is one piece of information, not five.
fn aggregate_failures(attempts: &[Attempt]) -> String {
    let mut out =
        String::from("No attempt in the previous round passed the gate. What went wrong:\n\n");
    let mut shown = 0;

    for a in attempts {
        if shown >= 2 {
            break;
        }
        let detail = match (&a.gate, &a.error) {
            (Some(gate), _) if !gate.passed => gate.summary(),
            (_, Some(err)) => err.clone(),
            _ => continue,
        };
        out.push_str(&format!(
            "Attempt {} (model `{}`):\n{detail}\n\n",
            a.index, a.model
        ));
        shown += 1;
    }

    if shown == 0 {
        out.push_str("No diagnostics were captured. Try a different approach entirely.\n");
    }
    out
}

/// Subject line plus provenance. A maintainer reading `git log` a year later
/// should be able to tell that a machine wrote this and find the issue.
fn commit_message(task: &Task, spec: &Spec, attempt: &Attempt) -> String {
    let subject = spec.goal.lines().next().unwrap_or(&task.issue.title).trim();
    let subject: String = subject.chars().take(68).collect();

    format!(
        "{subject}\n\n\
         Closes #{}\n\n\
         Written by gitai: round {}, model `{}`, {} iteration(s).\n",
        task.issue.number, attempt.round, attempt.model, attempt.iterations
    )
}

fn spend_line(spend: &Spend) -> String {
    format!(
        "_{} model calls, {} tokens, ${:.3}, {}s_",
        spend.llm_calls,
        spend.tokens(),
        spend.cost_usd,
        spend.wall_secs
    )
}

fn pull_request_body(task: &Task, winner: &Attempt, verdict: &Verdict) -> String {
    let mut body = format!("Closes #{}\n\n{}\n\n", task.issue.number, verdict.summary);

    if let Some(spec) = &task.spec {
        body.push_str("## Acceptance criteria\n\n");
        for a in &spec.acceptance {
            body.push_str(&format!("- {a}\n"));
        }
        body.push('\n');
    }

    if let Some(gate) = &winner.gate {
        body.push_str("## Gate\n\n```\n");
        body.push_str(&gate.summary());
        body.push_str("```\n\n");
        body.push_str(&format!(
            "{} file(s), +{} / -{}\n\n",
            gate.changed_files.len(),
            gate.insertions,
            gate.deletions
        ));
    }

    if !verdict.suggestions.is_empty() {
        body.push_str("## Not addressed\n\n");
        for s in &verdict.suggestions {
            body.push_str(&format!("- {s}\n"));
        }
        body.push('\n');
    }

    body.push_str(&format!(
        "---\nWritten by gitai: round {}, model `{}`, {} iteration(s). {}\n\n\
         Nothing here has been read by a human yet.\n",
        winner.round,
        winner.model,
        winner.iterations,
        spend_line(&task.spend)
    ));
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitai_core::model::{Budget, CheckResult, Issue, RepoRef};

    // -- the loop, end to end ------------------------------------------------
    //
    // Mock models, an in-memory workspace and an in-memory database, so this
    // exercises every stage with no network, no Docker and no forge.

    mod full_run {
        use super::*;
        use crate::testing::MemSandbox;
        use gitai_core::config::Config;
        use gitai_core::event::EventKind;
        use gitai_core::sandbox::Sandbox;
        use gitai_store::SqliteStore;

        fn config(gate_test: &[&str]) -> Arc<Config> {
            let tests = gate_test
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");

            Arc::new(
                Config::from_toml(&format!(
                    r#"
[providers.mock]
kind = "mock"
base_url = ""

[models.mock]
provider = "mock"
model = "mock"
context_tokens = 32000

[roles]
planner = "mock"
worker = ["mock"]
editor = "mock"
reviewer = "mock"
arbiter = "mock"

[gate]
test = [{tests}]
"#
                ))
                .unwrap(),
            )
        }

        async fn engine_for(
            cfg: Arc<Config>,
            sandbox: Arc<dyn Sandbox>,
        ) -> (Arc<Engine>, Arc<dyn Store>) {
            let store: Arc<dyn Store> = Arc::new(SqliteStore::in_memory().await.unwrap());
            let forges = Arc::new(gitai_forge::ForgeRegistry::build(&cfg).unwrap());
            let engine = Arc::new(Engine::new(cfg, store.clone(), sandbox, forges).unwrap());
            (engine, store)
        }

        fn local_task(budget: Budget) -> Task {
            let mut task = Task::new(
                RepoRef {
                    forge: "local".into(),
                    owner: "local".into(),
                    name: "demo".into(),
                    default_branch: Some("main".into()),
                },
                Issue {
                    number: 0,
                    title: "Cache is never invalidated".into(),
                    body: "get() returns stale values after a write.".into(),
                    url: "/mem".into(),
                    labels: vec![],
                    author: "local".into(),
                },
                budget,
            );
            task.local_repo = Some("/mem".into());
            task.base_branch = Some("main".into());
            task
        }

        fn sandbox() -> MemSandbox {
            MemSandbox::new([
                ("README.md", "# demo"),
                (
                    "src/cache.py",
                    "def get(key):\n    return _store.get(key)\n",
                ),
            ])
        }

        #[tokio::test]
        async fn a_task_runs_every_stage_and_ends_up_with_a_human() {
            let cfg = config(&[]);
            let (engine, store) = engine_for(cfg, Arc::new(sandbox())).await;

            let task = local_task(Budget {
                max_rounds: 1,
                attempts_per_round: 2,
                max_iterations: 2,
                ..Default::default()
            });
            store.create_task(&task).await.unwrap();

            let state = engine.run_task(task.id).await.unwrap();
            assert_eq!(state, TaskState::AwaitingHuman);

            let finished = store.get_task(task.id).await.unwrap();
            assert!(finished.spec.is_some(), "the planner must have run");
            assert!(finished.spend.llm_calls >= 5, "{:?}", finished.spend);
            assert!(finished.last_error.is_none());

            let attempts = store.list_attempts(task.id).await.unwrap();
            assert_eq!(attempts.len(), 2, "both attempts should be recorded");
            assert_eq!(
                attempts
                    .iter()
                    .filter(|a| a.state == AttemptState::Selected)
                    .count(),
                1,
                "exactly one attempt is selected"
            );
            assert!(attempts.iter().all(|a| a.review.is_some()), "all reviewed");

            // The event log has to be complete enough to replay the run.
            let events = store.list_events(task.id, 0, 500).await.unwrap();
            let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
            for expected in [
                EventKind::PlanReady,
                EventKind::AttemptStarted,
                EventKind::PatchProduced,
                EventKind::GateRan,
                EventKind::EditorFeedback,
                EventKind::ReviewVerdict,
                EventKind::ArbiterVerdict,
                EventKind::RoundFinished,
            ] {
                assert!(
                    kinds.contains(&expected),
                    "no {expected} event in {kinds:?}"
                );
            }
        }

        #[tokio::test]
        async fn a_gate_that_never_passes_exhausts_the_budget_instead_of_looping() {
            let cfg = config(&["run-tests"]);
            let sandbox = sandbox().on_exec("run-tests", 1, "2 tests failed");
            let (engine, store) = engine_for(cfg, Arc::new(sandbox)).await;

            let task = local_task(Budget {
                max_rounds: 2,
                attempts_per_round: 2,
                max_iterations: 2,
                ..Default::default()
            });
            store.create_task(&task).await.unwrap();

            let state = engine.run_task(task.id).await.unwrap();
            assert_eq!(state, TaskState::Failed);

            let finished = store.get_task(task.id).await.unwrap();
            let err = finished.last_error.unwrap();
            assert!(err.contains("no attempt was approved"), "{err}");

            let attempts = store.list_attempts(task.id).await.unwrap();
            assert_eq!(attempts.len(), 4, "2 rounds of 2 attempts");
            assert!(
                attempts.iter().all(|a| a.state == AttemptState::GateFailed),
                "a failing gate must not let anything through: {:?}",
                attempts.iter().map(|a| a.state).collect::<Vec<_>>()
            );
            assert!(
                attempts.iter().all(|a| a.iterations == 2),
                "each attempt should use its whole iteration budget trying"
            );
            assert!(
                attempts.iter().all(|a| a.review.is_none()),
                "nothing that failed the gate may reach a reviewer"
            );
        }

        #[tokio::test]
        async fn a_round_budget_of_zero_does_no_work_at_all() {
            let cfg = config(&[]);
            let (engine, store) = engine_for(cfg, Arc::new(sandbox())).await;

            let task = local_task(Budget {
                max_rounds: 0,
                ..Default::default()
            });
            store.create_task(&task).await.unwrap();

            assert_eq!(engine.run_task(task.id).await.unwrap(), TaskState::Failed);
            assert!(store.list_attempts(task.id).await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn a_cost_ceiling_of_zero_stops_before_the_first_round() {
            let cfg = config(&[]);
            let (engine, store) = engine_for(cfg, Arc::new(sandbox())).await;

            // The planner runs first and bills something, so the check at the
            // top of the round loop is what has to catch this.
            let task = local_task(Budget {
                max_cost_usd: 0.0,
                max_tokens: 1,
                ..Default::default()
            });
            store.create_task(&task).await.unwrap();

            assert_eq!(engine.run_task(task.id).await.unwrap(), TaskState::Failed);
            let finished = store.get_task(task.id).await.unwrap();
            let err = finished.last_error.unwrap();
            assert!(err.contains("budget exhausted"), "{err}");
            assert!(store.list_attempts(task.id).await.unwrap().is_empty());
        }
    }

    fn attempt_with_gate(index: u32, passed: bool, detail: &str) -> Attempt {
        let mut a = Attempt::new(TaskId::new(), 0, index, format!("m{index}"), "b".into());
        a.gate = Some(GateReport::from_checks(vec![CheckResult {
            name: "test".into(),
            ok: passed,
            skipped: false,
            exit_code: if passed { 0 } else { 1 },
            duration_ms: 1,
            output: detail.into(),
        }]));
        a
    }

    fn task() -> Task {
        let mut t = Task::new(
            RepoRef::parse("acme/widgets", "gitea").unwrap(),
            Issue {
                number: 7,
                title: "Cache is never invalidated".into(),
                body: "b".into(),
                url: "u".into(),
                labels: vec![],
                author: "radomir".into(),
            },
            Budget::default(),
        );
        t.spec = Some(Spec {
            goal: "invalidate on write".into(),
            acceptance: vec!["stale reads stop".into()],
            ..Default::default()
        });
        t
    }

    #[test]
    fn failure_aggregation_is_capped_at_two_attempts() {
        let attempts: Vec<Attempt> = (0..5)
            .map(|i| attempt_with_gate(i, false, &format!("error {i}")))
            .collect();
        let out = aggregate_failures(&attempts);
        assert!(out.contains("error 0"), "{out}");
        assert!(out.contains("error 1"), "{out}");
        assert!(!out.contains("error 3"), "{out}");
    }

    #[test]
    fn aggregation_falls_back_to_the_attempt_error() {
        let mut a = Attempt::new(TaskId::new(), 0, 0, "m".into(), "b".into());
        a.error = Some("the sandbox never came up".into());
        let out = aggregate_failures(&[a]);
        assert!(out.contains("sandbox never came up"), "{out}");
    }

    #[test]
    fn aggregation_says_so_when_nothing_was_captured() {
        let out = aggregate_failures(&[]);
        assert!(out.contains("No diagnostics"), "{out}");
    }

    #[test]
    fn feedback_merging_keeps_both_sources() {
        assert_eq!(merge_feedback("", "editor says"), "editor says");
        assert_eq!(merge_feedback("edits failed", ""), "edits failed");
        let both = merge_feedback("edits failed", "editor says");
        assert!(both.contains("edits failed") && both.contains("editor says"));
    }

    #[test]
    fn re_reading_a_file_replaces_the_stale_copy() {
        let mut open = vec![context::OpenFile {
            path: "a.rs".into(),
            content: "old".into(),
            truncated: false,
        }];
        merge_open_files(
            &mut open,
            vec![
                context::OpenFile {
                    path: "a.rs".into(),
                    content: "new".into(),
                    truncated: false,
                },
                context::OpenFile {
                    path: "b.rs".into(),
                    content: "b".into(),
                    truncated: false,
                },
            ],
        );
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].content, "new");
    }

    #[test]
    fn the_pull_request_body_carries_the_context_a_reviewer_needs() {
        let t = task();
        let winner = attempt_with_gate(0, true, "");
        let verdict = Verdict {
            approved: true,
            score: 85,
            summary: "Invalidates the cache on write.".into(),
            blocking: vec![],
            suggestions: vec!["the TTL is still hardcoded".into()],
            reviewer: "big".into(),
        };

        let body = pull_request_body(&t, &winner, &verdict);
        assert!(body.starts_with("Closes #7"), "{body}");
        assert!(body.contains("Invalidates the cache on write."));
        assert!(body.contains("stale reads stop"), "acceptance criteria");
        assert!(body.contains("the TTL is still hardcoded"), "suggestions");
        assert!(body.contains("Nothing here has been read by a human yet."));
    }

    #[test]
    fn attempts_are_ranked_so_the_best_review_wins() {
        let mut a = attempt_with_gate(0, true, "");
        let mut b = attempt_with_gate(1, true, "");
        a.review = Some(Verdict {
            approved: true,
            score: 60,
            summary: "ok".into(),
            blocking: vec![],
            suggestions: vec![],
            reviewer: "r".into(),
        });
        b.review = Some(Verdict {
            approved: true,
            score: 90,
            summary: "better".into(),
            blocking: vec![],
            suggestions: vec![],
            reviewer: "r".into(),
        });

        let mut list = [a, b];
        list.sort_by_key(|x| std::cmp::Reverse(x.rank()));
        assert_eq!(list[0].index, 1);
    }

    #[test]
    fn the_commit_message_links_back_to_the_issue_and_the_model() {
        let t = task();
        let mut winner = attempt_with_gate(0, true, "");
        winner.iterations = 3;
        let msg = commit_message(&t, t.spec.as_ref().unwrap(), &winner);

        let mut lines = msg.lines();
        assert_eq!(lines.next().unwrap(), "invalidate on write");
        assert!(msg.contains("Closes #7"), "{msg}");
        assert!(msg.contains("model `m0`"), "{msg}");
        assert!(msg.contains("3 iteration(s)"), "{msg}");
    }

    #[test]
    fn a_long_goal_is_trimmed_into_a_subject_line() {
        let mut t = task();
        t.spec = Some(Spec {
            goal: "x".repeat(200),
            ..Default::default()
        });
        let msg = commit_message(
            &t,
            t.spec.as_ref().unwrap(),
            &attempt_with_gate(0, true, ""),
        );
        assert_eq!(msg.lines().next().unwrap().len(), 68);
    }

    #[test]
    fn the_spend_line_reports_every_axis_of_the_budget() {
        let line = spend_line(&Spend {
            tokens_in: 1000,
            tokens_out: 500,
            cost_usd: 0.042,
            llm_calls: 7,
            wall_secs: 63,
        });
        assert!(line.contains("7 model calls"), "{line}");
        assert!(line.contains("1500 tokens"), "{line}");
        assert!(line.contains("$0.042"), "{line}");
        assert!(line.contains("63s"), "{line}");
    }

    #[tokio::test]
    async fn resolve_configured_image_matches_full_name_and_repo_name() {
        let cfg = Arc::new(Config {
            providers: std::collections::BTreeMap::from([(
                "mock".into(),
                gitai_core::config::ProviderConfig {
                    kind: gitai_core::config::ProviderKind::Mock,
                    ..Default::default()
                },
            )]),
            models: std::collections::BTreeMap::from([(
                "m".into(),
                gitai_core::config::ModelConfig {
                    provider: "mock".into(),
                    model: "mock".into(),
                    ..Default::default()
                },
            )]),
            roles: gitai_core::config::RoleConfig {
                planner: "m".into(),
                worker: vec!["m".into()],
                editor: "m".into(),
                reviewer: "m".into(),
                arbiter: "m".into(),
            },
            sandbox: gitai_core::config::SandboxConfig {
                images: std::collections::BTreeMap::from([
                    ("acme/widgets".into(), "acme-ci:v1".into()),
                    ("frontend".into(), "node:20-alpine".into()),
                ]),
                ..Default::default()
            },
            ..Default::default()
        });

        let store: Arc<dyn Store> = Arc::new(gitai_store::SqliteStore::in_memory().await.unwrap());
        let sandbox: Arc<dyn Sandbox> =
            Arc::new(crate::testing::MemSandbox::new(std::collections::BTreeMap::new()));
        let forges = Arc::new(gitai_forge::ForgeRegistry::build(&cfg).unwrap());
        let engine = Engine::new(cfg, store, sandbox, forges).unwrap();

        assert_eq!(
            engine.resolve_configured_image("acme/widgets"),
            Some("acme-ci:v1".into())
        );
        assert_eq!(
            engine.resolve_configured_image("my-org/frontend"),
            Some("node:20-alpine".into())
        );
        assert_eq!(engine.resolve_configured_image("other/project"), None);
    }
}
