//! The model boundary. One trait, so a hosted API and a local runtime are
//! interchangeable everywhere in the pipeline.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::Spend;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Text,
    /// Ask the provider for JSON. Support is uneven across local runtimes, so
    /// callers must still parse defensively.
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Provider-side model id, already resolved from the `[models.*]` name.
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Vec<String>,
    #[serde(default)]
    pub response_format: ResponseFormat,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop: Vec::new(),
            response_format: ResponseFormat::Text,
        }
    }

    /// Characters in the prompt. Used for rough context budgeting without
    /// pulling a tokenizer into the core.
    pub fn prompt_chars(&self) -> usize {
        self.messages.iter().map(|m| m.content.len()).sum()
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub latency_ms: u64,
}

impl ChatResponse {
    /// Spend for this single call, priced with the model's USD-per-million rates.
    pub fn spend(&self, price_in: f64, price_out: f64) -> Spend {
        Spend {
            tokens_in: self.usage.prompt_tokens,
            tokens_out: self.usage.completion_tokens,
            cost_usd: (self.usage.prompt_tokens as f64 * price_in
                + self.usage.completion_tokens as f64 * price_out)
                / 1_000_000.0,
            llm_calls: 1,
            wall_secs: self.latency_ms / 1000,
        }
    }

    /// True when the provider stopped for length. A truncated patch is worse
    /// than no patch, so callers treat this as a failure rather than parse it.
    pub fn truncated(&self) -> bool {
        matches!(
            self.finish_reason.as_deref(),
            Some("length") | Some("max_tokens")
        )
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Key in `[providers.*]`.
    fn name(&self) -> &str;

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse>;
}
