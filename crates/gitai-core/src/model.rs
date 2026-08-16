//! The domain. Everything the pipeline moves around is described here, and
//! nothing in this module knows about HTTP, git, containers or SQL.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Uuid::parse_str(s)
                    .map($name)
                    .map_err(|e| Error::store(format!("bad {}: {e}", stringify!($name))))
            }
        }
    };
}

id_type!(TaskId);
id_type!(AttemptId);
id_type!(EventId);

/// Which forge a repository lives on, plus enough to address it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    /// Key of the forge in `[forges.*]` config, not the forge kind.
    pub forge: String,
    pub owner: String,
    pub name: String,
    #[serde(default)]
    pub default_branch: Option<String>,
}

impl RepoRef {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Accepts `owner/name`, optionally prefixed with `forge:`.
    ///
    /// Splits from the right, so a GitLab path with subgroups
    /// (`group/subgroup/project`) keeps the whole namespace in `owner` and the
    /// project in `name`.
    pub fn parse(s: &str, fallback_forge: &str) -> Result<Self> {
        let (forge, rest) = match s.split_once(':') {
            Some((f, r)) => (f.to_string(), r),
            None => (fallback_forge.to_string(), s),
        };
        let (owner, name) = rest
            .trim_matches('/')
            .rsplit_once('/')
            .ok_or_else(|| Error::config(format!("repo must look like owner/name, got `{s}`")))?;
        if owner.is_empty() || name.is_empty() {
            return Err(Error::config(format!(
                "repo must look like owner/name, got `{s}`"
            )));
        }
        Ok(Self {
            forge,
            owner: owner.to_string(),
            name: name.trim_end_matches(".git").to_string(),
            default_branch: None,
        })
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/{}", self.forge, self.owner, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub author: String,
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Planning,
    Working,
    Reviewing,
    /// A pull request is open and the decision is a human's.
    AwaitingHuman,
    Done,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Planning => "planning",
            Self::Working => "working",
            Self::Reviewing => "reviewing",
            Self::AwaitingHuman => "awaiting_human",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::AwaitingHuman | Self::Done | Self::Failed | Self::Cancelled
        )
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskState {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "queued" => Self::Queued,
            "planning" => Self::Planning,
            "working" => Self::Working,
            "reviewing" => Self::Reviewing,
            "awaiting_human" => Self::AwaitingHuman,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            other => return Err(Error::store(format!("unknown task state `{other}`"))),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub repo: RepoRef,
    pub issue: Issue,
    pub state: TaskState,
    /// Branch the work is based on. Filled in once the repo is inspected.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Set for runs that did not come from a forge, such as `gitai run`
    /// against a checkout on disk. When present the clone source is this path,
    /// the branch is left in the local repository, and no pull request opens.
    #[serde(default)]
    pub local_repo: Option<String>,
    #[serde(default)]
    pub spec: Option<Spec>,
    /// Outer loop counter: how many times the arbiter has sent work back.
    #[serde(default)]
    pub round: u32,
    pub budget: Budget,
    #[serde(default)]
    pub spend: Spend,
    #[serde(default)]
    pub pull_request: Option<PullRequest>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(repo: RepoRef, issue: Issue, budget: Budget) -> Self {
        let now = Utc::now();
        Self {
            id: TaskId::new(),
            repo,
            issue,
            state: TaskState::Queued,
            base_branch: None,
            local_repo: None,
            spec: None,
            round: 0,
            budget,
            spend: Spend::default(),
            pull_request: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Branch name for one attempt. Stable and greppable in the forge UI.
    pub fn branch_name(&self, round: u32, attempt_index: u32) -> String {
        format!(
            "gitai/issue-{}/r{}-a{}",
            self.issue.number, round, attempt_index
        )
    }
}

// ---------------------------------------------------------------------------
// Spec: the planner's output, and the contract every later role is judged against
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Spec {
    /// Detected language/stack of the project, e.g. "Rust", "Python (pytest)", "Node.js (TypeScript)", "Go".
    #[serde(default)]
    pub language: Option<String>,
    /// One paragraph: what "done" means, in the repo's own vocabulary.
    pub goal: String,
    /// Checkable statements. The reviewer scores against exactly these.
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Glob patterns the patch is allowed to touch. Empty means no restriction,
    /// which the scope check treats as "warn, do not block".
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Files worth reading before writing anything.
    #[serde(default)]
    pub relevant_files: Vec<String>,
    #[serde(default)]
    pub test_plan: Vec<String>,
    /// Shell commands to install dependencies, if required.
    #[serde(default)]
    pub setup_commands: Vec<String>,
    /// Shell commands to build the project, if required.
    #[serde(default)]
    pub build_commands: Vec<String>,
    /// Shell commands to test the project, if required.
    #[serde(default)]
    pub test_commands: Vec<String>,
    /// Shell commands to lint the project, if required.
    #[serde(default)]
    pub lint_commands: Vec<String>,
    #[serde(default)]
    pub notes: String,
    /// Planner's own read of how risky this is, 1 (trivial) to 5 (architectural).
    #[serde(default = "default_difficulty")]
    pub difficulty: u8,
}

fn default_difficulty() -> u8 {
    3
}

// ---------------------------------------------------------------------------
// Attempt
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Running,
    /// Produced a patch that cleared the objective gate.
    GatePassed,
    /// Ran out of editor iterations without clearing the gate.
    GateFailed,
    /// Cleared the gate but a reviewer or the arbiter turned it down.
    Rejected,
    /// The arbiter picked this one.
    Selected,
    Errored,
}

impl AttemptState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::GatePassed => "gate_passed",
            Self::GateFailed => "gate_failed",
            Self::Rejected => "rejected",
            Self::Selected => "selected",
            Self::Errored => "errored",
        }
    }
}

impl fmt::Display for AttemptState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AttemptState {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "running" => Self::Running,
            "gate_passed" => Self::GatePassed,
            "gate_failed" => Self::GateFailed,
            "rejected" => Self::Rejected,
            "selected" => Self::Selected,
            "errored" => Self::Errored,
            other => return Err(Error::store(format!("unknown attempt state `{other}`"))),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: AttemptId,
    pub task_id: TaskId,
    pub round: u32,
    pub index: u32,
    /// Name of the model in `[models.*]` that did the writing.
    pub model: String,
    pub branch: String,
    pub state: AttemptState,
    /// Unified diff against the base branch.
    #[serde(default)]
    pub patch: Option<String>,
    #[serde(default)]
    pub gate: Option<GateReport>,
    /// One entry per inner-loop pass by the editor.
    #[serde(default)]
    pub editor_notes: Vec<String>,
    #[serde(default)]
    pub review: Option<Verdict>,
    #[serde(default)]
    pub iterations: u32,
    #[serde(default)]
    pub spend: Spend,
    #[serde(default)]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Attempt {
    pub fn new(task_id: TaskId, round: u32, index: u32, model: String, branch: String) -> Self {
        let now = Utc::now();
        Self {
            id: AttemptId::new(),
            task_id,
            round,
            index,
            model,
            branch,
            state: AttemptState::Running,
            patch: None,
            gate: None,
            editor_notes: Vec::new(),
            review: None,
            iterations: 0,
            spend: Spend::default(),
            error: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn gate_passed(&self) -> bool {
        self.gate.as_ref().is_some_and(|g| g.passed)
    }

    /// Combined score used to rank surviving attempts. Review score dominates,
    /// a smaller diff breaks ties.
    pub fn rank(&self) -> (u8, i64) {
        let score = self.review.as_ref().map_or(0, |v| v.score);
        let size = self.patch.as_ref().map_or(i64::MAX, |p| p.len() as i64);
        (score, -size)
    }
}

// ---------------------------------------------------------------------------
// The objective gate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub duration_ms: u64,
    /// Tail of the output. Full logs live in the event log, not here.
    #[serde(default)]
    pub output: String,
}

impl CheckResult {
    pub fn skipped(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            skipped: true,
            exit_code: 0,
            duration_ms: 0,
            output: String::new(),
        }
    }
}

/// The part of the loop that models cannot argue with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    pub checks: Vec<CheckResult>,
    pub passed: bool,
    /// Files the patch touched, for the scope check and for reviewer context.
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub insertions: u64,
    #[serde(default)]
    pub deletions: u64,
}

impl GateReport {
    pub fn from_checks(checks: Vec<CheckResult>) -> Self {
        let passed = checks.iter().all(|c| c.ok);
        Self {
            checks,
            passed,
            changed_files: Vec::new(),
            insertions: 0,
            deletions: 0,
        }
    }

    pub fn failures(&self) -> impl Iterator<Item = &CheckResult> {
        self.checks.iter().filter(|c| !c.ok)
    }

    /// Compact rendering handed back to the worker as feedback.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            let status = if c.skipped {
                "skip"
            } else if c.ok {
                "pass"
            } else {
                "FAIL"
            };
            out.push_str(&format!("[{status}] {}\n", c.name));
            if !c.ok && !c.output.is_empty() {
                out.push_str(&c.output);
                out.push('\n');
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Reviews
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub approved: bool,
    /// 0-100. Only meaningful for comparing attempts inside one round.
    #[serde(default)]
    pub score: u8,
    #[serde(default)]
    pub summary: String,
    /// Must-fix items. Non-empty implies `approved == false`.
    #[serde(default)]
    pub blocking: Vec<String>,
    #[serde(default)]
    pub suggestions: Vec<String>,
    #[serde(default)]
    pub reviewer: String,
}

impl Verdict {
    pub fn rejected(reviewer: impl Into<String>, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            approved: false,
            score: 0,
            summary: reason.clone(),
            blocking: vec![reason],
            suggestions: Vec::new(),
            reviewer: reviewer.into(),
        }
    }

    /// Feedback text fed back into the next round.
    pub fn feedback(&self) -> String {
        let mut out = self.summary.clone();
        if !self.blocking.is_empty() {
            out.push_str("\n\nMust fix:\n");
            for b in &self.blocking {
                out.push_str(&format!("- {b}\n"));
            }
        }
        if !self.suggestions.is_empty() {
            out.push_str("\nWorth considering:\n");
            for s in &self.suggestions {
                out.push_str(&format!("- {s}\n"));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Budget and spend
// ---------------------------------------------------------------------------

/// Hard stops. Without these a self-correcting loop happily burns a month of
/// tokens on a typo fix.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    /// Outer loop: arbiter rejection sends work back this many times.
    pub max_rounds: u32,
    /// How many models attack the task in parallel each round.
    pub attempts_per_round: u32,
    /// Inner loop: worker to editor passes before an attempt is abandoned.
    pub max_iterations: u32,
    pub max_tokens: u64,
    pub max_wall_secs: u64,
    pub max_cost_usd: f64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_rounds: 3,
            attempts_per_round: 3,
            max_iterations: 8,
            max_tokens: 2_000_000,
            max_wall_secs: 3_600,
            max_cost_usd: 5.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Spend {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub llm_calls: u64,
    pub wall_secs: u64,
}

impl Spend {
    pub fn add(&mut self, other: &Spend) {
        self.tokens_in += other.tokens_in;
        self.tokens_out += other.tokens_out;
        self.cost_usd += other.cost_usd;
        self.llm_calls += other.llm_calls;
        self.wall_secs = self.wall_secs.max(other.wall_secs);
    }

    pub fn tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }

    /// `Err` naming the limit that was hit, so the message is actionable.
    pub fn check(&self, budget: &Budget) -> Result<()> {
        if self.tokens() > budget.max_tokens {
            return Err(Error::BudgetExhausted(format!(
                "tokens {} > {}",
                self.tokens(),
                budget.max_tokens
            )));
        }
        if self.cost_usd > budget.max_cost_usd {
            return Err(Error::BudgetExhausted(format!(
                "cost ${:.2} > ${:.2}",
                self.cost_usd, budget.max_cost_usd
            )));
        }
        if self.wall_secs > budget.max_wall_secs {
            return Err(Error::BudgetExhausted(format!(
                "wall clock {}s > {}s",
                self.wall_secs, budget.max_wall_secs
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pull requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestReq {
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub base: String,
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Planner,
    Worker,
    Editor,
    Reviewer,
    Arbiter,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Worker => "worker",
            Self::Editor => "editor",
            Self::Reviewer => "reviewer",
            Self::Arbiter => "arbiter",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ref_parses_both_shapes() {
        let r = RepoRef::parse("acme/widgets", "gitea").unwrap();
        assert_eq!(r.forge, "gitea");
        assert_eq!(r.full_name(), "acme/widgets");

        let r = RepoRef::parse("github:acme/widgets.git", "gitea").unwrap();
        assert_eq!(r.forge, "github");
        assert_eq!(r.name, "widgets");

        assert!(RepoRef::parse("nope", "gitea").is_err());
    }

    #[test]
    fn gitlab_subgroups_keep_the_whole_namespace() {
        let r = RepoRef::parse("acme/platform/widgets", "gitlab").unwrap();
        assert_eq!(r.owner, "acme/platform");
        assert_eq!(r.name, "widgets");
        assert_eq!(r.full_name(), "acme/platform/widgets");
    }

    #[test]
    fn gate_fails_if_any_check_fails() {
        let checks = vec![
            CheckResult {
                name: "build".into(),
                ok: true,
                skipped: false,
                exit_code: 0,
                duration_ms: 10,
                output: String::new(),
            },
            CheckResult {
                name: "test".into(),
                ok: false,
                skipped: false,
                exit_code: 1,
                duration_ms: 10,
                output: "2 failed".into(),
            },
        ];
        let report = GateReport::from_checks(checks);
        assert!(!report.passed);
        assert_eq!(report.failures().count(), 1);
        assert!(report.summary().contains("FAIL"));
    }

    #[test]
    fn budget_names_the_limit_it_hit() {
        let budget = Budget::default();
        let spend = Spend {
            tokens_in: budget.max_tokens + 1,
            ..Default::default()
        };
        let err = spend.check(&budget).unwrap_err().to_string();
        assert!(err.contains("tokens"), "{err}");
    }

    #[test]
    fn attempts_rank_by_score_then_smaller_diff() {
        let mut a = Attempt::new(TaskId::new(), 0, 0, "m".into(), "b".into());
        let mut b = Attempt::new(TaskId::new(), 0, 1, "m".into(), "b".into());
        a.review = Some(Verdict {
            approved: true,
            score: 80,
            summary: String::new(),
            blocking: vec![],
            suggestions: vec![],
            reviewer: "r".into(),
        });
        b.review = a.review.clone();
        a.patch = Some("x".repeat(100));
        b.patch = Some("x".repeat(10));
        assert!(b.rank() > a.rank());
    }
}
