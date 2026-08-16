//! The five roles, as calls.
//!
//! Each one renders its template, asks its configured model for JSON, and
//! returns a typed answer plus what the call cost. Nothing here knows about
//! loops, budgets or git: that is the engine's job.

use std::sync::Arc;

use gitai_core::config::Config;
use gitai_core::error::Result;
use gitai_core::llm::ChatMessage;
use gitai_core::model::{GateReport, Issue, Role, Spec, Spend, Verdict};
use gitai_llm::registry::{Call, ModelRegistry};
use serde::{Deserialize, Serialize};

use crate::context::{ContextLimits, OpenFile};
use crate::edits::Edit;
use crate::prompts::Prompts;

/// How many times a model gets to correct malformed JSON before the call fails.
const JSON_REPAIRS: u32 = 1;

// ---------------------------------------------------------------------------
// Role outputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkerOutput {
    #[serde(default)]
    pub reasoning: String,
    /// Files the worker wants to see before it commits to an edit.
    #[serde(default)]
    pub read: Vec<String>,
    /// Web search queries the worker wants to execute before it commits to an edit.
    #[serde(default)]
    pub search: Vec<String>,
    #[serde(default)]
    pub edits: Vec<Edit>,
}

impl WorkerOutput {
    /// The worker asked to look around at files.
    pub fn is_read_request(&self) -> bool {
        self.edits.is_empty() && !self.read.is_empty()
    }

    /// The worker asked to perform a web search.
    pub fn is_search_request(&self) -> bool {
        self.edits.is_empty() && !self.search.is_empty()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EditorOutput {
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub next_steps: Vec<String>,
}

impl EditorOutput {
    /// Instruction text handed to the next worker turn.
    pub fn feedback(&self) -> String {
        let mut out = self.notes.clone();
        if !self.next_steps.is_empty() {
            out.push_str("\n\nDo these, in order:\n");
            for (i, step) in self.next_steps.iter().enumerate() {
                out.push_str(&format!("{}. {step}\n", i + 1));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Template inputs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PlannerCtx {
    pub repo: String,
    pub issue: Issue,
    pub base_branch: String,
    pub file_tree: String,
    pub readme: String,
    pub readme_limit: usize,
}

#[derive(Debug, Serialize)]
pub struct WorkerCtx {
    pub repo: String,
    pub spec: Spec,
    pub file_tree: String,
    pub open_files: Vec<OpenFile>,
    pub feedback: String,
    pub iteration: u32,
}

#[derive(Debug, Serialize)]
pub struct EditorCtx {
    pub repo: String,
    pub spec: Spec,
    pub gate: GateReport,
    pub gate_summary: String,
    pub diff: String,
}

#[derive(Debug, Serialize)]
pub struct ReviewerCtx {
    pub repo: String,
    pub spec: Spec,
    pub gate: GateReport,
    pub gate_summary: String,
    pub diff: String,
}

#[derive(Debug, Serialize)]
pub struct ReviewSummary {
    pub score: u8,
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct ArbiterCtx {
    pub repo: String,
    pub issue: Issue,
    pub spec: Spec,
    pub round: u32,
    pub max_rounds: u32,
    pub attempt_count: usize,
    pub review: Verdict,
    pub other_reviews: Vec<ReviewSummary>,
    pub gate: GateReport,
    pub gate_summary: String,
    pub diff: String,
}

// ---------------------------------------------------------------------------

pub struct Roles {
    cfg: Arc<Config>,
    models: Arc<ModelRegistry>,
    prompts: Arc<Prompts>,
}

impl Roles {
    pub fn new(cfg: Arc<Config>, models: Arc<ModelRegistry>, prompts: Arc<Prompts>) -> Self {
        Self {
            cfg,
            models,
            prompts,
        }
    }

    /// Models named for the worker role, cycled so a fan-out of five across
    /// three models is 2/2/1 rather than five of the first.
    pub fn worker_model(&self, index: u32) -> &str {
        let list = &self.cfg.roles.worker;
        &list[index as usize % list.len()]
    }

    /// Context budget for one model. A 7B worker with a 32k window and a
    /// 200k-window arbiter must not be handed the same prompt, so every stage
    /// sizes its context from the model that will actually read it.
    pub fn limits(&self, model: &str) -> ContextLimits {
        ContextLimits::for_context(self.models.context_tokens(model))
    }

    pub fn limits_for_role(&self, role: Role) -> ContextLimits {
        let model = match role {
            Role::Planner => &self.cfg.roles.planner,
            Role::Editor => &self.cfg.roles.editor,
            Role::Reviewer => &self.cfg.roles.reviewer,
            Role::Arbiter => &self.cfg.roles.arbiter,
            // Workers differ per attempt; callers use `limits` with the model.
            Role::Worker => &self.cfg.roles.worker[0],
        };
        self.limits(model)
    }

    async fn ask<C: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        role: Role,
        model: &str,
        ctx: C,
    ) -> Result<(T, Spend)> {
        let user = self.prompts.render(role, ctx)?;
        let call = Call::new(vec![
            ChatMessage::system(Prompts::system(role)),
            ChatMessage::user(user),
        ]);
        self.models.complete_json(model, call, JSON_REPAIRS).await
    }

    pub async fn plan(&self, ctx: PlannerCtx) -> Result<(Spec, Spend)> {
        let model = self.cfg.roles.planner.clone();
        let file_tree = ctx.file_tree.clone();
        let (mut spec, spend): (Spec, Spend) = self.ask(Role::Planner, &model, ctx).await?;
        normalise_spec(&mut spec, &file_tree);
        Ok((spec, spend))
    }

    pub async fn work(&self, model: &str, ctx: WorkerCtx) -> Result<(WorkerOutput, Spend)> {
        self.ask(Role::Worker, model, ctx).await
    }

    pub async fn edit(&self, ctx: EditorCtx) -> Result<(EditorOutput, Spend)> {
        let model = self.cfg.roles.editor.clone();
        self.ask(Role::Editor, &model, ctx).await
    }

    pub async fn review(&self, ctx: ReviewerCtx) -> Result<(Verdict, Spend)> {
        let model = self.cfg.roles.reviewer.clone();
        let (verdict, spend): (Verdict, Spend) = self.ask(Role::Reviewer, &model, ctx).await?;
        Ok((normalise(verdict, &model), spend))
    }

    pub async fn arbitrate(&self, ctx: ArbiterCtx) -> Result<(Verdict, Spend)> {
        let model = self.cfg.roles.arbiter.clone();
        let (verdict, spend): (Verdict, Spend) = self.ask(Role::Arbiter, &model, ctx).await?;
        Ok((normalise(verdict, &model), spend))
    }
}

/// Repairs the contradictions models produce: approving while listing blockers,
/// or rejecting with a high score. The safe reading wins in both directions.
fn normalise(mut v: Verdict, reviewer: &str) -> Verdict {
    if v.reviewer.is_empty() {
        v.reviewer = reviewer.to_string();
    }
    if !v.blocking.is_empty() && v.approved {
        tracing::warn!(
            reviewer,
            blocking = v.blocking.len(),
            "verdict approved with blocking items; treating it as a rejection"
        );
        v.approved = false;
    }
    if !v.approved && v.score > 60 {
        v.score = 60;
    }
    v.score = v.score.min(100);
    v
}

/// Infers the project language/stack from repository files if the planner model left it blank.
fn normalise_spec(spec: &mut Spec, file_tree: &str) {
    if spec.language.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true) {
        if file_tree.contains("Cargo.toml") {
            spec.language = Some("Rust".into());
        } else if file_tree.contains("package.json") {
            spec.language = Some("Node.js / TypeScript".into());
        } else if file_tree.contains("pyproject.toml")
            || file_tree.contains("requirements.txt")
            || file_tree.contains("setup.py")
        {
            spec.language = Some("Python".into());
        } else if file_tree.contains("go.mod") {
            spec.language = Some("Go".into());
        } else {
            spec.language = Some("Plain / Multi-purpose".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitai_core::config::Config;

    fn config() -> Arc<Config> {
        Arc::new(
            Config::from_toml(
                r#"
[providers.mock]
kind = "mock"
base_url = ""

[models.tiny]
provider = "mock"
model = "tiny"

[models.small-a]
provider = "mock"
model = "a"

[models.small-b]
provider = "mock"
model = "b"

[roles]
planner = "tiny"
worker = ["small-a", "small-b"]
editor = "tiny"
reviewer = "tiny"
arbiter = "tiny"
"#,
            )
            .unwrap(),
        )
    }

    fn roles() -> Roles {
        let cfg = config();
        let models = Arc::new(ModelRegistry::build(&cfg).unwrap());
        let prompts = Arc::new(Prompts::load(std::path::Path::new("../../prompts")).unwrap());
        Roles::new(cfg, models, prompts)
    }

    #[test]
    fn worker_models_are_cycled_across_the_fan_out() {
        let r = roles();
        assert_eq!(r.worker_model(0), "small-a");
        assert_eq!(r.worker_model(1), "small-b");
        assert_eq!(r.worker_model(2), "small-a");
    }

    #[test]
    fn each_role_gets_a_budget_sized_for_its_own_model() {
        let cfg = Arc::new(
            Config::from_toml(
                r#"
[providers.mock]
kind = "mock"

[models.tiny]
provider = "mock"
model = "tiny"
context_tokens = 8192

[models.huge]
provider = "mock"
model = "huge"
context_tokens = 200000

[roles]
planner = "huge"
worker = ["tiny"]
editor = "tiny"
reviewer = "tiny"
arbiter = "huge"
"#,
            )
            .unwrap(),
        );
        let models = Arc::new(ModelRegistry::build(&cfg).unwrap());
        let prompts = Arc::new(Prompts::load(std::path::Path::new("../../prompts")).unwrap());
        let r = Roles::new(cfg, models, prompts);

        let worker = r.limits("tiny");
        let arbiter = r.limits_for_role(Role::Arbiter);
        assert!(
            arbiter.max_diff_bytes > worker.max_diff_bytes,
            "the 200k arbiter should get more room than the 8k worker"
        );
        assert!(worker.estimated_ceiling_tokens() <= 8192);
    }

    #[test]
    fn an_approval_with_blockers_is_downgraded_to_a_rejection() {
        let v = normalise(
            Verdict {
                approved: true,
                score: 90,
                summary: "looks good".into(),
                blocking: vec!["but it deletes the database".into()],
                suggestions: vec![],
                reviewer: String::new(),
            },
            "mid",
        );
        assert!(!v.approved);
        assert_eq!(v.score, 60, "a rejection cannot keep a top score");
        assert_eq!(v.reviewer, "mid");
    }

    #[test]
    fn a_clean_approval_is_left_alone() {
        let v = normalise(
            Verdict {
                approved: true,
                score: 88,
                summary: "s".into(),
                blocking: vec![],
                suggestions: vec!["tidy later".into()],
                reviewer: "big".into(),
            },
            "ignored",
        );
        assert!(v.approved);
        assert_eq!(v.score, 88);
        assert_eq!(v.reviewer, "big");
    }

    #[test]
    fn a_read_request_is_distinguished_from_an_edit() {
        let read = WorkerOutput {
            read: vec!["a.rs".into()],
            ..Default::default()
        };
        assert!(read.is_read_request());

        let both = WorkerOutput {
            read: vec!["a.rs".into()],
            edits: vec![Edit::Delete {
                path: "b.rs".into(),
            }],
            ..Default::default()
        };
        assert!(!both.is_read_request(), "edits win over a read request");
        assert!(!WorkerOutput::default().is_read_request());
    }

    #[test]
    fn editor_feedback_numbers_its_steps() {
        let out = EditorOutput {
            done: false,
            notes: "the build is broken".into(),
            next_steps: vec![
                "fix the import in src/a.rs".into(),
                "rerun the tests".into(),
            ],
        };
        let fb = out.feedback();
        assert!(fb.starts_with("the build is broken"));
        assert!(fb.contains("1. fix the import"), "{fb}");
        assert!(fb.contains("2. rerun the tests"), "{fb}");
    }

    #[tokio::test]
    async fn the_planner_call_produces_a_spec_through_the_mock() {
        let r = roles();
        let (spec, spend) = r
            .plan(PlannerCtx {
                repo: "acme/widgets".into(),
                issue: Issue {
                    number: 1,
                    title: "t".into(),
                    body: "b".into(),
                    url: "u".into(),
                    labels: vec![],
                    author: "a".into(),
                },
                base_branch: "main".into(),
                file_tree: "src/a.rs".into(),
                readme: String::new(),
                readme_limit: 2000,
            })
            .await
            .unwrap();

        assert!(!spec.goal.is_empty());
        assert_eq!(spend.llm_calls, 1);
    }
}
