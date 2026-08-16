//! The objective gate.
//!
//! This is the part of the loop no model gets a vote on. An attempt clears the
//! gate or it does not, and only attempts that cleared it are ever shown to a
//! reviewer. Without this, a hundred iterations of small models grading each
//! other converge on something that reads well and does not work.
//!
//! Cheap checks run first: there is no point compiling a patch that already
//! reached outside its allowed paths.

use gitai_core::config::GateConfig;
use gitai_core::error::Result;
use gitai_core::model::{CheckResult, GateReport};
use gitai_core::sandbox::{ExecRequest, Workspace};

/// Runs the `setup` commands once, before any gate pass.
///
/// Kept separate because dependency installation is slow, needs the network
/// the gate itself does not get, and only has to happen once per workspace.
pub async fn run_setup(ws: &dyn Workspace, cfg: &GateConfig) -> Result<CheckResult> {
    run_step(ws, "setup", &cfg.setup, cfg.output_tail_bytes).await
}

/// Full gate pass over the current working tree.
pub async fn run_gate(
    ws: &dyn Workspace,
    cfg: &GateConfig,
    allowed_paths: &[String],
) -> Result<GateReport> {
    let changed = ws.changed_files().await?;
    let (insertions, deletions) = ws.line_stats().await?;
    let mut checks = Vec::new();

    checks.push(check_produced_changes(&changed));

    if cfg.enforce_scope {
        checks.push(check_scope(&changed, allowed_paths));
    }
    checks.push(check_size(&changed, cfg.max_changed_files));
    if cfg.require_tests {
        checks.push(check_tests_touched(&changed, &cfg.test_path_patterns));
    }

    // Only pay for the expensive steps once the cheap ones agree.
    if checks.iter().all(|c| c.ok) {
        for (name, cmds) in [
            ("build", &cfg.build),
            ("test", &cfg.test),
            ("lint", &cfg.lint),
        ] {
            let result = run_step(ws, name, cmds, cfg.output_tail_bytes).await?;
            let failed = !result.ok;
            checks.push(result);
            // A patch that does not compile has nothing to say about its tests.
            if failed {
                break;
            }
        }
    }

    let mut report = GateReport::from_checks(checks);
    report.changed_files = changed;
    report.insertions = insertions;
    report.deletions = deletions;
    Ok(report)
}

/// Runs a list of shell commands, stopping at the first failure.
async fn run_step(
    ws: &dyn Workspace,
    name: &str,
    cmds: &[String],
    tail_bytes: usize,
) -> Result<CheckResult> {
    if cmds.is_empty() {
        return Ok(CheckResult::skipped(name));
    }

    let mut total_ms = 0;
    for cmd in cmds {
        let out = ws.exec(&ExecRequest::new(cmd)).await?;
        total_ms += out.duration_ms;
        if !out.ok() {
            return Ok(CheckResult {
                name: name.to_string(),
                ok: false,
                skipped: false,
                exit_code: out.exit_code,
                duration_ms: total_ms,
                output: format!("$ {cmd}\n{}", out.tail(tail_bytes)),
            });
        }
    }

    Ok(CheckResult {
        name: name.to_string(),
        ok: true,
        skipped: false,
        exit_code: 0,
        duration_ms: total_ms,
        output: String::new(),
    })
}

fn check_produced_changes(changed: &[String]) -> CheckResult {
    let ok = !changed.is_empty();
    CheckResult {
        name: "changes".into(),
        ok,
        skipped: false,
        exit_code: if ok { 0 } else { 1 },
        duration_ms: 0,
        output: if ok {
            String::new()
        } else {
            "The attempt produced no file changes at all.".into()
        },
    }
}

/// Keeps a patch inside the area the planner marked as in scope. An empty
/// allow-list means the planner did not restrict anything, so nothing to check.
fn check_scope(changed: &[String], allowed: &[String]) -> CheckResult {
    if allowed.is_empty() {
        return CheckResult::skipped("scope");
    }
    let strays: Vec<&String> = changed
        .iter()
        .filter(|f| !crate::glob::matches_any(allowed, f))
        .collect();

    CheckResult {
        name: "scope".into(),
        ok: strays.is_empty(),
        skipped: false,
        exit_code: if strays.is_empty() { 0 } else { 1 },
        duration_ms: 0,
        output: if strays.is_empty() {
            String::new()
        } else {
            format!(
                "These files are outside the allowed paths {allowed:?}:\n{}",
                strays
                    .iter()
                    .map(|f| format!("- {f}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        },
    }
}

/// A patch nobody can review is not a finished patch.
fn check_size(changed: &[String], max: usize) -> CheckResult {
    if max == 0 {
        return CheckResult::skipped("size");
    }
    let ok = changed.len() <= max;
    CheckResult {
        name: "size".into(),
        ok,
        skipped: false,
        exit_code: if ok { 0 } else { 1 },
        duration_ms: 0,
        output: if ok {
            String::new()
        } else {
            format!(
                "The patch touches {} files, over the limit of {max}. \
                 Narrow the change or split the issue.",
                changed.len()
            )
        },
    }
}

fn check_tests_touched(changed: &[String], patterns: &[String]) -> CheckResult {
    let ok = changed
        .iter()
        .any(|f| crate::glob::matches_any(patterns, f));
    CheckResult {
        name: "tests_touched".into(),
        ok,
        skipped: false,
        exit_code: if ok { 0 } else { 1 },
        duration_ms: 0,
        output: if ok {
            String::new()
        } else {
            "No test file was added or changed. Cover the new behaviour with a test.".into()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_empty_patch_fails_the_gate() {
        let c = check_produced_changes(&[]);
        assert!(!c.ok);
        assert!(c.output.contains("no file changes"));
    }

    #[test]
    fn scope_check_flags_files_outside_the_allowed_paths() {
        let changed = v(&["src/a.rs", "deploy/prod.yaml"]);
        let allowed = v(&["src/**"]);
        let c = check_scope(&changed, &allowed);
        assert!(!c.ok);
        assert!(c.output.contains("deploy/prod.yaml"), "{}", c.output);
        assert!(!c.output.contains("src/a.rs"), "{}", c.output);
    }

    #[test]
    fn scope_check_is_skipped_when_the_planner_set_no_boundary() {
        let c = check_scope(&v(&["anything"]), &[]);
        assert!(c.skipped);
        assert!(c.ok);
    }

    #[test]
    fn scope_check_passes_when_everything_is_inside() {
        let c = check_scope(&v(&["src/a.rs", "src/deep/b.rs"]), &v(&["src/**"]));
        assert!(c.ok, "{}", c.output);
    }

    #[test]
    fn size_check_has_an_upper_bound_and_an_off_switch() {
        let changed = v(&["a", "b", "c"]);
        assert!(check_size(&changed, 3).ok);
        assert!(!check_size(&changed, 2).ok);
        assert!(check_size(&changed, 0).skipped);
    }

    #[test]
    fn tests_touched_recognises_common_layouts() {
        let patterns = GateConfig::default().test_path_patterns;
        assert!(check_tests_touched(&v(&["src/thing_test.rs"]), &patterns).ok);
        assert!(check_tests_touched(&v(&["tests/integration.rs"]), &patterns).ok);
        assert!(check_tests_touched(&v(&["web/Button.spec.tsx"]), &patterns).ok);
        let miss = check_tests_touched(&v(&["src/thing.rs"]), &patterns);
        assert!(!miss.ok);
        assert!(miss.output.contains("test"));
    }

    #[test]
    fn a_report_summary_names_the_failing_check() {
        let report = GateReport::from_checks(vec![
            check_produced_changes(&v(&["src/a.rs"])),
            check_scope(&v(&["src/a.rs"]), &v(&["docs/**"])),
        ]);
        assert!(!report.passed);
        let summary = report.summary();
        assert!(summary.contains("[FAIL] scope"), "{summary}");
        assert!(summary.contains("[pass] changes"), "{summary}");
    }
}
