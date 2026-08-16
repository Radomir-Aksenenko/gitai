use std::fmt;

/// Every fallible path in gitai funnels into this. The variants exist so the
/// pipeline can decide whether a failure is worth retrying: `Llm`, `Sandbox`
/// and `Forge` are usually transient, `Config` and `BadModelOutput` are not.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),

    #[error("forge {forge}: {msg}")]
    Forge { forge: String, msg: String },

    #[error("llm {provider}: {msg}")]
    Llm { provider: String, msg: String },

    #[error("sandbox: {0}")]
    Sandbox(String),

    #[error("store: {0}")]
    Store(String),

    /// The model answered, but not in a shape we can use.
    #[error("model output: {0}")]
    BadModelOutput(String),

    #[error("budget exhausted: {0}")]
    BudgetExhausted(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("cancelled")]
    Cancelled,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn config(msg: impl fmt::Display) -> Self {
        Self::Config(msg.to_string())
    }

    pub fn forge(forge: impl Into<String>, msg: impl fmt::Display) -> Self {
        Self::Forge {
            forge: forge.into(),
            msg: msg.to_string(),
        }
    }

    pub fn llm(provider: impl Into<String>, msg: impl fmt::Display) -> Self {
        Self::Llm {
            provider: provider.into(),
            msg: msg.to_string(),
        }
    }

    pub fn sandbox(msg: impl fmt::Display) -> Self {
        Self::Sandbox(msg.to_string())
    }

    pub fn store(msg: impl fmt::Display) -> Self {
        Self::Store(msg.to_string())
    }

    pub fn bad_output(msg: impl fmt::Display) -> Self {
        Self::BadModelOutput(msg.to_string())
    }

    /// Whether a retry with the same inputs has any chance of a different result.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Error::Llm { .. } | Error::Forge { .. } | Error::Sandbox(_) | Error::Io(_)
        )
    }
}

pub type Result<T> = std::result::Result<T, Error>;
