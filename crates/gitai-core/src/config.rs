//! Configuration. One TOML file drives every replaceable part of the system:
//! which model sits in which role, where code is executed, what counts as a
//! passing gate, and which forges to talk to.
//!
//! Secrets are never written here directly. `${VAR}` and `${VAR:-fallback}`
//! are expanded from the environment when the file is loaded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::Budget;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub sandbox: SandboxConfig,
    pub budget: Budget,
    pub gate: GateConfig,
    pub prompts: PromptConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub models: BTreeMap<String, ModelConfig>,
    pub roles: RoleConfig,
    pub forges: BTreeMap<String, ForgeConfig>,
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
    /// Reachable base URL, used when building links back into gitai.
    pub public_url: String,
    /// How many tasks the worker pool runs at once.
    pub concurrency: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            public_url: "http://127.0.0.1:8080".into(),
            concurrency: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// sqlite://path?mode=rwc for now. The Store trait leaves room for postgres.
    pub url: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            url: "sqlite://gitai.db?mode=rwc".into(),
        }
    }
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// Every attempt runs in its own throwaway container with no network.
    Docker,
    /// Commands run directly on the host. Convenient, and a real security
    /// boundary you no longer have. Development only.
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub kind: SandboxKind,
    pub image: String,
    /// Per-repository or per-stack image mapping, e.g. "acme/widgets" -> "acme/ci:v1".
    pub images: BTreeMap<String, String>,
    /// Mount point of the checkout inside the container.
    pub workdir: String,
    /// Docker `--network`. `none` is the right default: model-written code has
    /// no business reaching the internet.
    pub network: String,
    pub cpus: f32,
    pub memory: String,
    /// Ceiling for a single command, not for the whole attempt.
    pub timeout_secs: u64,
    /// Docker `--user`. Left empty on Unix, gitai derives the host uid:gid so
    /// files written in the container do not come back root-owned. Ignored on
    /// Docker Desktop, where the mount is already translated.
    pub user: String,
    /// Docker `--pids-limit`. A fork bomb in generated code is a Tuesday.
    pub pids_limit: u32,
    /// Where bare clones and worktrees are kept between runs.
    pub work_root: PathBuf,
    /// Extra `-v host:container` mounts, for language toolchain caches.
    pub mounts: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            kind: SandboxKind::Docker,
            image: "docker.io/library/debian:bookworm-slim".into(),
            images: BTreeMap::new(),
            workdir: "/work".into(),
            network: "none".into(),
            cpus: 2.0,
            memory: "4g".into(),
            timeout_secs: 900,
            user: String::new(),
            pids_limit: 512,
            work_root: PathBuf::from(".gitai/work"),
            mounts: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------

/// The objective gate. Model opinions come after this, never instead of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GateConfig {
    /// Runs once per workspace before anything else. Dependency install.
    pub setup: Vec<String>,
    pub build: Vec<String>,
    pub test: Vec<String>,
    pub lint: Vec<String>,
    /// Refuse a patch that changes no test file. Blunt, and it works.
    pub require_tests: bool,
    /// What counts as a test file for `require_tests`.
    pub test_path_patterns: Vec<String>,
    /// Refuse a patch touching files outside `Spec::allowed_paths`.
    pub enforce_scope: bool,
    /// Reject oversized diffs outright rather than paying a reviewer to read them.
    pub max_changed_files: usize,
    /// Bytes of tail output kept per check.
    pub output_tail_bytes: usize,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            setup: Vec::new(),
            build: Vec::new(),
            test: Vec::new(),
            lint: Vec::new(),
            require_tests: false,
            test_path_patterns: [
                "**/test/**",
                "**/tests/**",
                "**/spec/**",
                "**/*_test.*",
                "**/test_*",
                "**/*.test.*",
                "**/*.spec.*",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            enforce_scope: true,
            max_changed_files: 60,
            output_tail_bytes: 8_000,
        }
    }
}

impl GateConfig {
    /// A gate with nothing to run cannot reject anything, which turns the whole
    /// loop into models grading models.
    pub fn is_empty(&self) -> bool {
        self.build.is_empty() && self.test.is_empty() && self.lint.is_empty()
    }
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    /// Directory of `{role}.md` templates. Editing them needs no rebuild.
    pub dir: PathBuf,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("prompts"),
        }
    }
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Anything speaking the OpenAI chat-completions API: vLLM, Ollama,
    /// LM Studio, llama.cpp server, OpenRouter, DeepSeek, Together, and
    /// OpenAI itself.
    Openai,
    Anthropic,
    /// Canned answers. Lets the whole pipeline run in tests with no network.
    Mock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    pub timeout_secs: u64,
    pub max_retries: u32,
    /// Cap on in-flight requests to this provider. Local runtimes need it most.
    pub concurrency: usize,
    pub headers: BTreeMap<String, String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Openai,
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            timeout_secs: 300,
            max_retries: 3,
            concurrency: 8,
            headers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    /// Key in `[providers.*]`.
    pub provider: String,
    /// Model id as the provider knows it.
    pub model: String,
    /// f64 rather than f32: these go straight into a JSON body, and f32 widens
    /// into artefacts like 0.20000000298023224.
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    /// USD per million tokens. Used for the cost budget; leave at 0 for local models.
    pub price_in: f64,
    pub price_out: f64,
    /// Rough input limit, used to trim context before sending.
    pub context_tokens: u32,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            price_in: 0.0,
            price_out: 0.0,
            context_tokens: 128_000,
        }
    }
}

/// Which model plays which part. `worker` is a list so a round can fan out
/// across several different small models instead of the same one N times.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoleConfig {
    pub planner: String,
    pub worker: Vec<String>,
    pub editor: String,
    pub reviewer: String,
    pub arbiter: String,
}

impl Default for RoleConfig {
    fn default() -> Self {
        Self {
            planner: "big".into(),
            worker: vec!["small".into()],
            editor: "mid".into(),
            reviewer: "mid".into(),
            arbiter: "big".into(),
        }
    }
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    /// Gitea and Forgejo share an API, one adapter covers both.
    Gitea,
    Github,
    Gitlab,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForgeConfig {
    pub kind: ForgeKind,
    /// API root. `https://git.example.com/api/v1` for Gitea,
    /// `https://api.github.com` for GitHub.
    pub base_url: String,
    pub token: String,
    pub webhook_secret: String,
    /// Only issues carrying this label are picked up. Empty means every issue,
    /// which you almost never want on a busy repo.
    pub trigger_label: String,
    /// Login of the account gitai pushes as, used to skip its own webhooks.
    pub bot_login: String,
    /// Open pull requests as drafts so CI does not fire on half-finished work.
    pub draft_prs: bool,
    /// Delete the branches of attempts that were not selected, once the task
    /// ends either way. The diffs stay in the event log, so nothing is lost
    /// that you would want during a post-mortem. Turn it off while debugging.
    pub delete_rejected_branches: bool,
    pub timeout_secs: u64,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            kind: ForgeKind::Gitea,
            base_url: String::new(),
            token: String::new(),
            webhook_secret: String::new(),
            trigger_label: "gitai".into(),
            bot_login: "gitai".into(),
            draft_prs: true,
            delete_rejected_branches: true,
            timeout_secs: 30,
        }
    }
}

// ---------------------------------------------------------------------------

/// Optional repository-level configuration (.gitai.toml in repo root).
/// Gives repository authors control over the custom sandbox image and exact gate verification commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    /// Custom Docker image for this repo (e.g. "registry.example.com/ci:v1").
    pub image: Option<String>,
    /// Shell commands to run during setup (dependency install).
    pub setup: Vec<String>,
    /// Shell commands to build the project.
    pub build: Vec<String>,
    /// Shell commands to test the project.
    pub test: Vec<String>,
    /// Shell commands to lint the project.
    pub lint: Vec<String>,
}

impl RepoConfig {
    pub fn from_toml(raw: &str) -> Result<Self> {
        let expanded = expand_env(raw);
        toml::from_str(&expanded).map_err(|e| Error::config(format!("invalid .gitai.toml: {e}")))
    }
}

// ---------------------------------------------------------------------------

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::config(format!("cannot read {}: {e}", path.display())))?;
        let mut cfg = Self::from_toml(&raw)?;
        cfg.resolve_paths(path.parent().unwrap_or(Path::new(".")));
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_toml(raw: &str) -> Result<Self> {
        let expanded = expand_env(raw);
        toml::from_str(&expanded).map_err(|e| Error::config(format!("invalid config: {e}")))
    }

    /// Relative paths in the file are read as relative to the file, not to the
    /// working directory the daemon happened to start in.
    fn resolve_paths(&mut self, base: &Path) {
        if self.prompts.dir.is_relative() {
            self.prompts.dir = base.join(&self.prompts.dir);
        }
        if self.sandbox.work_root.is_relative() {
            self.sandbox.work_root = base.join(&self.sandbox.work_root);
        }
    }

    /// Catch the wiring mistakes that would otherwise surface an hour into a run.
    pub fn validate(&self) -> Result<()> {
        if self.models.is_empty() {
            return Err(Error::config("no [models.*] defined"));
        }
        for (name, m) in &self.models {
            if !self.providers.contains_key(&m.provider) {
                return Err(Error::config(format!(
                    "model `{name}` points at unknown provider `{}`",
                    m.provider
                )));
            }
            if m.model.is_empty() {
                return Err(Error::config(format!("model `{name}` has no `model` id")));
            }
        }

        let check_role = |role: &str, model: &str| -> Result<()> {
            if !self.models.contains_key(model) {
                return Err(Error::config(format!(
                    "role `{role}` points at unknown model `{model}`"
                )));
            }
            Ok(())
        };
        check_role("planner", &self.roles.planner)?;
        check_role("editor", &self.roles.editor)?;
        check_role("reviewer", &self.roles.reviewer)?;
        check_role("arbiter", &self.roles.arbiter)?;
        if self.roles.worker.is_empty() {
            return Err(Error::config("roles.worker must list at least one model"));
        }
        for m in &self.roles.worker {
            check_role("worker", m)?;
        }

        for (name, f) in &self.forges {
            if f.base_url.is_empty() {
                return Err(Error::config(format!("forge `{name}` has no base_url")));
            }
        }
        Ok(())
    }

    pub fn model(&self, name: &str) -> Result<&ModelConfig> {
        self.models
            .get(name)
            .ok_or_else(|| Error::config(format!("unknown model `{name}`")))
    }

    pub fn provider(&self, name: &str) -> Result<&ProviderConfig> {
        self.providers
            .get(name)
            .ok_or_else(|| Error::config(format!("unknown provider `{name}`")))
    }

    pub fn forge(&self, name: &str) -> Result<&ForgeConfig> {
        self.forges
            .get(name)
            .ok_or_else(|| Error::config(format!("unknown forge `{name}`")))
    }
}

/// Replaces `${VAR}` and `${VAR:-fallback}` with values from the environment.
/// An unset variable with no fallback expands to an empty string, and
/// `validate` is what turns that into a readable error later.
pub fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = input[i + 2..].find('}') {
                let expr = &input[i + 2..i + 2 + end];
                let (name, fallback) = match expr.split_once(":-") {
                    Some((n, f)) => (n, Some(f)),
                    None => (expr, None),
                };
                let value = std::env::var(name.trim())
                    .ok()
                    .or_else(|| fallback.map(str::to_string))
                    .unwrap_or_default();
                out.push_str(&value);
                i += 2 + end + 1;
                continue;
            }
        }
        // Push a whole char, not a byte, so multibyte content survives.
        let ch = input[i..]
            .chars()
            .next()
            .expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_expansion_handles_fallbacks_and_unicode() {
        unsafe { std::env::set_var("GITAI_TEST_TOKEN", "secret") };
        assert_eq!(expand_env("t = \"${GITAI_TEST_TOKEN}\""), "t = \"secret\"");
        assert_eq!(expand_env("${GITAI_NOT_SET:-fallback}"), "fallback");
        assert_eq!(expand_env("${GITAI_NOT_SET}"), "");
        assert_eq!(
            expand_env("описание ${GITAI_TEST_TOKEN} хвост"),
            "описание secret хвост"
        );
        assert_eq!(expand_env("no ${unclosed"), "no ${unclosed");
    }

    #[test]
    fn validate_rejects_dangling_role() {
        let raw = r#"
[providers.local]
kind = "openai"
base_url = "http://localhost:11434/v1"

[models.small]
provider = "local"
model = "qwen2.5-coder:7b"

[roles]
planner = "nonexistent"
worker = ["small"]
editor = "small"
reviewer = "small"
arbiter = "small"
"#;
        let err = Config::from_toml(raw)
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonexistent"), "{err}");
    }

    #[test]
    fn validate_rejects_model_on_unknown_provider() {
        let raw = r#"
[models.small]
provider = "ghost"
model = "x"
"#;
        let err = Config::from_toml(raw)
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn empty_gate_is_detected() {
        assert!(GateConfig::default().is_empty());
    }

    #[test]
    fn repo_config_parses_cleanly_with_env() {
        unsafe { std::env::set_var("GITAI_TEST_IMAGE", "registry.local/custom-cpp:v1") };
        let raw = r#"
image = "${GITAI_TEST_IMAGE}"
setup = ["conan install ."]
build = ["cmake --build build"]
test = ["ctest --test-dir build"]
lint = ["clang-tidy src/*.cpp"]
"#;
        let cfg = RepoConfig::from_toml(raw).unwrap();
        assert_eq!(cfg.image.as_deref(), Some("registry.local/custom-cpp:v1"));
        assert_eq!(cfg.setup, vec!["conan install ."]);
        assert_eq!(cfg.build, vec!["cmake --build build"]);
        assert_eq!(cfg.test, vec!["ctest --test-dir build"]);
        assert_eq!(cfg.lint, vec!["clang-tidy src/*.cpp"]);
    }

    #[test]
    fn sandbox_config_supports_per_repo_images() {
        let raw = r#"
[providers.local]
kind = "openai"
base_url = "http://localhost:11434/v1"

[models.small]
provider = "local"
model = "qwen2.5-coder:7b"

[roles]
planner = "small"
worker = ["small"]
editor = "small"
reviewer = "small"
arbiter = "small"

[sandbox]
image = "gitai-sandbox:latest"

[sandbox.images]
"org/backend" = "registry.company.com/backend-ci:latest"
"org/frontend" = "node:20-bookworm"
"#;
        let cfg = Config::from_toml(raw).unwrap();
        assert_eq!(cfg.sandbox.image, "gitai-sandbox:latest");
        assert_eq!(
            cfg.sandbox.images.get("org/backend").map(String::as_str),
            Some("registry.company.com/backend-ci:latest")
        );
        assert_eq!(
            cfg.sandbox.images.get("org/frontend").map(String::as_str),
            Some("node:20-bookworm")
        );
    }
}
