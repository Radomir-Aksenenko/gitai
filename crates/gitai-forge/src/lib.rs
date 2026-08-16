//! Forge adapters: Gitea and Forgejo through one implementation, GitHub
//! (including Enterprise Server) through another.

use std::collections::HashMap;
use std::sync::Arc;

use gitai_core::config::{Config, ForgeKind};
use gitai_core::error::{Error, Result};
use gitai_core::forge::Forge;

pub mod client;
pub mod gitea;
pub mod github;
pub mod gitlab;
pub mod map;
pub mod payload;
pub mod sig;
pub mod url;

pub use gitea::GiteaForge;
pub use github::GithubForge;
pub use gitlab::GitlabForge;

/// Every configured forge, keyed by its `[forges.*]` name.
pub struct ForgeRegistry {
    forges: HashMap<String, Arc<dyn Forge>>,
}

impl ForgeRegistry {
    pub fn build(cfg: &Config) -> Result<Self> {
        let mut forges: HashMap<String, Arc<dyn Forge>> = HashMap::new();
        for (name, fc) in &cfg.forges {
            let forge: Arc<dyn Forge> = match fc.kind {
                ForgeKind::Gitea => Arc::new(GiteaForge::new(name, fc.clone())?),
                ForgeKind::Github => Arc::new(GithubForge::new(name, fc.clone())?),
                ForgeKind::Gitlab => Arc::new(GitlabForge::new(name, fc.clone())?),
            };
            forges.insert(name.clone(), forge);
        }
        Ok(Self { forges })
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn Forge>> {
        self.forges
            .get(name)
            .cloned()
            .ok_or_else(|| Error::config(format!("no forge named `{name}` is configured")))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.forges.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.forges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_builds_every_kind_from_one_config() {
        let cfg = Config::from_toml(
            r#"
[forges.local]
kind = "gitea"
base_url = "https://git.example.com/api/v1"

[forges.hub]
kind = "github"
base_url = "https://api.github.com"

[forges.lab]
kind = "gitlab"
base_url = "https://gitlab.com/api/v4"
"#,
        )
        .unwrap();
        let reg = ForgeRegistry::build(&cfg).unwrap();
        assert_eq!(reg.get("local").unwrap().kind(), ForgeKind::Gitea);
        assert_eq!(reg.get("hub").unwrap().kind(), ForgeKind::Github);
        assert_eq!(reg.get("lab").unwrap().kind(), ForgeKind::Gitlab);
        assert!(reg.get("missing").is_err());
    }

    #[test]
    fn branch_pruning_follows_the_forge_setting() {
        let cfg = Config::from_toml(
            r#"
[forges.keeps]
kind = "gitea"
base_url = "https://git.example.com/api/v1"
delete_rejected_branches = false

[forges.prunes]
kind = "github"
base_url = "https://api.github.com"
"#,
        )
        .unwrap();
        let reg = ForgeRegistry::build(&cfg).unwrap();
        assert!(!reg.get("keeps").unwrap().prunes_branches());
        assert!(
            reg.get("prunes").unwrap().prunes_branches(),
            "default is on"
        );
    }
}
