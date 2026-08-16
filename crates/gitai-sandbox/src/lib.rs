//! Where model-written code is allowed to run, and the gate that decides
//! whether what it produced is worth a reviewer's attention.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gitai_core::config::{SandboxConfig, SandboxKind};
use gitai_core::error::{Error, Result};
use gitai_core::sandbox::Sandbox;

pub mod docker;
pub mod gate;
pub mod git;
pub mod glob;
pub mod local;
pub mod proc;
pub mod tree;

pub use docker::DockerSandbox;
pub use gate::{run_gate, run_setup};
pub use local::LocalSandbox;

pub fn build_sandbox(cfg: &SandboxConfig) -> Arc<dyn Sandbox> {
    match cfg.kind {
        SandboxKind::Docker => Arc::new(DockerSandbox::new(cfg.clone())),
        SandboxKind::Local => Arc::new(LocalSandbox::new(cfg.clone())),
    }
}

/// git is not optional for any backend.
pub async fn require_git() -> Result<()> {
    let out = proc::run(
        "git",
        &["--version".into()],
        None,
        &BTreeMap::new(),
        Duration::from_secs(30),
    )
    .await
    .map_err(|_| Error::sandbox("git was not found on PATH"))?;

    if !out.ok() {
        return Err(Error::sandbox(format!(
            "git is present but did not run: {}",
            out.tail(300)
        )));
    }
    Ok(())
}

/// Strips userinfo out of any URL in `text`.
///
/// Applied to anything derived from a git invocation before it is logged or
/// stored, because git happily echoes the remote it was given, credentials and
/// all, in its error messages.
pub fn redact_url(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(scheme_at) = rest.find("://") {
        let after_scheme = scheme_at + 3;
        // The authority ends at the first '/', '?' or whitespace.
        let authority_len = rest[after_scheme..]
            .find(['/', '?', ' ', '\n', '\t', '"', '\''])
            .unwrap_or(rest.len() - after_scheme);
        let authority = &rest[after_scheme..after_scheme + authority_len];

        out.push_str(&rest[..after_scheme]);
        match authority.rsplit_once('@') {
            Some((_creds, host)) => {
                out.push_str("***@");
                out.push_str(host);
            }
            None => out.push_str(authority),
        }
        rest = &rest[after_scheme + authority_len..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_stripped_from_a_url() {
        assert_eq!(
            redact_url("https://gitai:t0ken@git.example.com/acme/widgets.git"),
            "https://***@git.example.com/acme/widgets.git"
        );
    }

    #[test]
    fn urls_without_credentials_are_untouched() {
        let clean = "fatal: repository 'https://git.example.com/a/b.git' not found";
        assert_eq!(redact_url(clean), clean);
    }

    #[test]
    fn every_url_in_a_multiline_message_is_scrubbed() {
        let msg =
            "remote https://u:p@a.com/x.git failed\nfalling back to https://u2:p2@b.com/y.git";
        let out = redact_url(msg);
        assert!(!out.contains("p@"), "{out}");
        assert!(!out.contains("p2@"), "{out}");
        assert_eq!(out.matches("***@").count(), 2, "{out}");
        assert!(out.contains("a.com/x.git"), "{out}");
        assert!(out.contains("b.com/y.git"), "{out}");
    }

    #[test]
    fn text_with_no_url_survives_unchanged() {
        assert_eq!(redact_url("nothing to see"), "nothing to see");
        assert_eq!(redact_url(""), "");
    }

    #[test]
    fn a_token_containing_an_at_sign_is_still_removed() {
        let out = redact_url("https://x-access-token:gh%40p@github.com/a/b.git");
        assert_eq!(out, "https://***@github.com/a/b.git");
    }
}
