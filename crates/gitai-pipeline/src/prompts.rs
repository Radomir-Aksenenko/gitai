//! Prompt templates.
//!
//! Defaults are compiled in, so a fresh binary works with no files next to it.
//! A `{role}.md` found in `prompts.dir` replaces the built-in one, which is
//! how prompts get tuned without a rebuild: the tunable half of this system
//! lives in text files, the load-bearing half lives in Rust.

use std::path::Path;

use gitai_core::error::{Error, Result};
use gitai_core::model::Role;
use minijinja::{Environment, context};
use serde::Serialize;

const PLANNER: &str = include_str!("../../../prompts/planner.md");
const WORKER: &str = include_str!("../../../prompts/worker.md");
const EDITOR: &str = include_str!("../../../prompts/editor.md");
const REVIEWER: &str = include_str!("../../../prompts/reviewer.md");
const ARBITER: &str = include_str!("../../../prompts/arbiter.md");

/// The compiled-in templates, so `gitai init` can write them out for editing.
pub const BUILTIN: [(&str, &str); 5] = [
    ("planner", PLANNER),
    ("worker", WORKER),
    ("editor", EDITOR),
    ("reviewer", REVIEWER),
    ("arbiter", ARBITER),
];

pub struct Prompts {
    env: Environment<'static>,
    /// Which roles were taken from disk rather than the binary, for logging.
    overridden: Vec<String>,
}

/// Hand-written so the template bodies stay out of error messages and logs.
impl std::fmt::Debug for Prompts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prompts")
            .field("overridden", &self.overridden)
            .finish_non_exhaustive()
    }
}

impl Prompts {
    /// Loads the built-in templates, then lets files in `dir` override them.
    /// A missing directory is normal and not an error.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut env = Environment::new();
        let mut overridden = Vec::new();

        for (role, builtin) in [
            (Role::Planner, PLANNER),
            (Role::Worker, WORKER),
            (Role::Editor, EDITOR),
            (Role::Reviewer, REVIEWER),
            (Role::Arbiter, ARBITER),
        ] {
            let name = role.as_str();
            let path = dir.join(format!("{name}.md"));
            let source = match std::fs::read_to_string(&path) {
                Ok(s) => {
                    overridden.push(name.to_string());
                    s
                }
                Err(_) => builtin.to_string(),
            };
            env.add_template_owned(name.to_string(), source)
                .map_err(|e| {
                    Error::config(format!("prompt `{name}` is not a valid template: {e}"))
                })?;
        }

        if !overridden.is_empty() {
            tracing::info!(roles = ?overridden, dir = %dir.display(), "using prompt overrides from disk");
        }
        Ok(Self { env, overridden })
    }

    pub fn overridden(&self) -> &[String] {
        &self.overridden
    }

    /// Renders the user message for a role.
    pub fn render<S: Serialize>(&self, role: Role, data: S) -> Result<String> {
        let tmpl = self
            .env
            .get_template(role.as_str())
            .map_err(|e| Error::config(format!("no prompt for role `{role}`: {e}")))?;
        tmpl.render(context! { ..minijinja::Value::from_serialize(&data) })
            .map_err(|e| Error::config(format!("rendering prompt `{role}`: {e}")))
    }

    /// The system message. Short on purpose: the instructions live in the
    /// template, and this only pins the two things code depends on, the role
    /// marker and JSON-only output.
    pub fn system(role: Role) -> String {
        format!(
            "gitai-role: {role}\n\
             You are the {role} stage of an automated code pipeline. \
             Answer with a single JSON object and nothing else: no prose before it, \
             no code fence around it, no trailing commentary."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitai_core::model::{Issue, Spec};
    use serde::Serialize;

    #[derive(Serialize)]
    struct PlannerData {
        repo: String,
        issue: Issue,
        base_branch: String,
        file_tree: String,
        readme: String,
        readme_limit: usize,
    }

    fn issue() -> Issue {
        Issue {
            number: 7,
            title: "Cache is never invalidated".into(),
            body: "Stale reads after an update.".into(),
            url: "http://x/7".into(),
            labels: vec!["gitai".into(), "bug".into()],
            author: "radomir".into(),
        }
    }

    #[test]
    fn builtin_templates_all_load_and_render() {
        let p = Prompts::load(Path::new("does/not/exist")).unwrap();
        assert!(p.overridden().is_empty());

        let out = p
            .render(
                Role::Planner,
                PlannerData {
                    repo: "acme/widgets".into(),
                    issue: issue(),
                    base_branch: "main".into(),
                    file_tree: "src/cache.rs".into(),
                    readme: String::new(),
                    readme_limit: 2000,
                },
            )
            .unwrap();

        assert!(out.contains("Cache is never invalidated"), "{out}");
        assert!(out.contains("acme/widgets"));
        assert!(out.contains("gitai, bug"), "labels should be joined");
        assert!(out.contains("allowed_paths"));
    }

    #[test]
    fn the_system_message_always_carries_the_role_marker() {
        for role in [
            Role::Planner,
            Role::Worker,
            Role::Editor,
            Role::Reviewer,
            Role::Arbiter,
        ] {
            let s = Prompts::system(role);
            assert!(s.starts_with(&format!("gitai-role: {role}")), "{s}");
        }
    }

    #[test]
    fn a_file_on_disk_wins_over_the_builtin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("worker.md"), "custom prompt for {{ repo }}").unwrap();

        let p = Prompts::load(dir.path()).unwrap();
        assert_eq!(p.overridden(), ["worker"]);

        #[derive(Serialize)]
        struct D {
            repo: String,
        }
        let out = p
            .render(
                Role::Worker,
                D {
                    repo: "acme/widgets".into(),
                },
            )
            .unwrap();
        assert_eq!(out, "custom prompt for acme/widgets");
    }

    #[test]
    fn a_broken_override_fails_at_load_not_mid_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("editor.md"), "{% for x in %}").unwrap();
        let err = Prompts::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("editor"), "{err}");
    }

    #[test]
    fn optional_blocks_are_omitted_when_empty() {
        let p = Prompts::load(Path::new("nope")).unwrap();

        #[derive(Serialize)]
        struct D {
            repo: String,
            spec: Spec,
            file_tree: String,
            open_files: Vec<String>,
            feedback: String,
            iteration: u32,
        }
        let out = p
            .render(
                Role::Worker,
                D {
                    repo: "r".into(),
                    spec: Spec {
                        goal: "g".into(),
                        acceptance: vec!["a".into()],
                        ..Default::default()
                    },
                    file_tree: "src/a.rs".into(),
                    open_files: vec![],
                    feedback: String::new(),
                    iteration: 0,
                },
            )
            .unwrap();

        assert!(!out.contains("What happened last time"), "{out}");
        assert!(!out.contains("Paths you may touch"), "{out}");
        assert!(out.contains("## Goal"));
    }
}
