//! `gitai init` and `gitai doctor`.
//!
//! `doctor` exists because every failure it catches would otherwise surface
//! twenty minutes and several dollars into a run.

use std::path::Path;

use gitai_core::config::{Config, SandboxKind};
use gitai_core::error::Result;
use gitai_core::llm::ChatMessage;
use gitai_llm::registry::{Call, ModelRegistry};

const EXAMPLE_CONFIG: &str = include_str!("../../../gitai.example.toml");

/// Writes a starting config and the prompt templates. Never overwrites.
pub fn init(dir: &Path, force: bool) -> Result<Vec<String>> {
    let mut written = Vec::new();

    let config_path = dir.join("gitai.toml");
    if config_path.exists() && !force {
        println!("  gitai.toml already exists, left alone");
    } else {
        std::fs::create_dir_all(dir)?;
        std::fs::write(&config_path, EXAMPLE_CONFIG)?;
        written.push("gitai.toml".to_string());
    }

    let prompts_dir = dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)?;
    for (role, body) in gitai_pipeline::prompts::BUILTIN {
        let path = prompts_dir.join(format!("{role}.md"));
        if path.exists() && !force {
            continue;
        }
        std::fs::write(&path, body)?;
        written.push(format!("prompts/{role}.md"));
    }

    Ok(written)
}

/// Checks everything that can be checked without starting a run.
///
/// Returns the number of problems found. Warnings do not count: a warning is
/// something that will work and probably should not.
pub async fn doctor(cfg: &Config) -> Result<usize> {
    let mut problems = 0;
    let mut warnings = 0;

    println!("config");
    match cfg.validate() {
        Ok(()) => println!("  ok: models, roles and forges all resolve"),
        Err(e) => {
            println!("  PROBLEM: {e}");
            problems += 1;
        }
    }

    // -- the gate -----------------------------------------------------------
    println!("\ngate");
    if cfg.gate.is_empty() {
        println!(
            "  PROBLEM: no build, test or lint commands are configured.\n\
             \x20          With an empty gate nothing can objectively reject a patch, and the\n\
             \x20          pipeline degrades into models grading each other. Set [gate] before\n\
             \x20          running this against anything you care about."
        );
        problems += 1;
    } else {
        for (name, cmds) in [
            ("setup", &cfg.gate.setup),
            ("build", &cfg.gate.build),
            ("test", &cfg.gate.test),
            ("lint", &cfg.gate.lint),
        ] {
            if cmds.is_empty() {
                println!("  {name}: (none)");
            } else {
                println!("  {name}: {}", cmds.join(" && "));
            }
        }
        if cfg.gate.test.is_empty() {
            println!("  warning: no test command, so only compilation is being proven");
            warnings += 1;
        }
    }

    // -- sandbox ------------------------------------------------------------
    println!("\nsandbox ({:?})", cfg.sandbox.kind);
    let sandbox = gitai_sandbox::build_sandbox(&cfg.sandbox);
    match sandbox.preflight().await {
        Ok(()) => println!("  ok: {} backend is usable", sandbox.kind()),
        Err(e) => {
            println!("  PROBLEM: {e}");
            problems += 1;
        }
    }
    if cfg.sandbox.kind == SandboxKind::Local {
        println!("  warning: `local` runs model-written code on this host with no isolation");
        warnings += 1;
    }
    if cfg.sandbox.kind == SandboxKind::Docker && cfg.sandbox.network != "none" {
        println!(
            "  warning: sandbox.network is `{}`, so generated code can reach the network",
            cfg.sandbox.network
        );
        warnings += 1;
    }

    // -- prompts ------------------------------------------------------------
    println!("\nprompts");
    match gitai_pipeline::Prompts::load(&cfg.prompts.dir) {
        Ok(p) if p.overridden().is_empty() => {
            println!("  ok: using the built-in templates");
        }
        Ok(p) => println!("  ok: overridden from disk: {}", p.overridden().join(", ")),
        Err(e) => {
            println!("  PROBLEM: {e}");
            problems += 1;
        }
    }

    // -- models -------------------------------------------------------------
    println!("\nmodels");
    match ModelRegistry::build(cfg) {
        Ok(registry) => {
            for role in [
                ("planner", vec![cfg.roles.planner.clone()]),
                ("worker", cfg.roles.worker.clone()),
                ("editor", vec![cfg.roles.editor.clone()]),
                ("reviewer", vec![cfg.roles.reviewer.clone()]),
                ("arbiter", vec![cfg.roles.arbiter.clone()]),
            ] {
                for name in role.1 {
                    let call = Call::new(vec![
                        ChatMessage::system("gitai-role: doctor"),
                        ChatMessage::user("Reply with the single word: ok"),
                    ]);
                    match registry.complete(&name, call).await {
                        Ok(out) => println!(
                            "  ok: {} -> {} ({} tokens, {:.0}ms)",
                            role.0,
                            name,
                            out.spend.tokens(),
                            out.spend.wall_secs as f64 * 1000.0
                        ),
                        Err(e) => {
                            println!("  PROBLEM: {} -> {}: {e}", role.0, name);
                            problems += 1;
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("  PROBLEM: {e}");
            problems += 1;
        }
    }

    // -- forges -------------------------------------------------------------
    println!("\nforges");
    if cfg.forges.is_empty() {
        println!("  none configured; only `gitai run` against a local checkout will work");
    }
    for (name, fc) in &cfg.forges {
        if fc.token.is_empty() {
            println!("  PROBLEM: `{name}` has no token");
            problems += 1;
        }
        if fc.webhook_secret.is_empty() {
            println!(
                "  PROBLEM: `{name}` has no webhook_secret, so its endpoint would accept \
                 unsigned deliveries and gitai refuses to serve it"
            );
            problems += 1;
        }
        if fc.trigger_label.is_empty() {
            println!("  warning: `{name}` has no trigger_label, so every issue starts a run");
            warnings += 1;
        }
        println!("  {name}: {:?} at {}", fc.kind, fc.base_url);
    }

    println!("\n{} problem(s), {} warning(s)", problems, warnings);
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_example_config_is_valid() {
        let cfg = Config::from_toml(EXAMPLE_CONFIG).expect("example config must parse");
        cfg.validate().expect("example config must validate");
    }

    #[test]
    fn init_writes_a_config_and_every_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let written = init(dir.path(), false).unwrap();

        assert!(written.contains(&"gitai.toml".to_string()));
        assert_eq!(written.len(), 6, "config plus five prompts: {written:?}");
        for role in ["planner", "worker", "editor", "reviewer", "arbiter"] {
            assert!(dir.path().join(format!("prompts/{role}.md")).exists());
        }
    }

    #[test]
    fn init_does_not_clobber_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), false).unwrap();
        std::fs::write(dir.path().join("prompts/worker.md"), "mine").unwrap();

        let second = init(dir.path(), false).unwrap();
        assert!(second.is_empty(), "{second:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("prompts/worker.md")).unwrap(),
            "mine"
        );
    }

    #[tokio::test]
    async fn doctor_flags_an_empty_gate_as_a_problem() {
        let cfg = Config::from_toml(
            r#"
[providers.mock]
kind = "mock"

[models.mock]
provider = "mock"
model = "mock"

[roles]
planner = "mock"
worker = ["mock"]
editor = "mock"
reviewer = "mock"
arbiter = "mock"

[sandbox]
kind = "local"
"#,
        )
        .unwrap();

        let problems = doctor(&cfg).await.unwrap();
        assert!(problems >= 1, "an empty gate must be reported");
    }
}
