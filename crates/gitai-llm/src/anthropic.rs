//! Anthropic Messages API adapter.
//!
//! Two shape differences from the OpenAI adapter matter: system prompts are a
//! top-level field rather than a message, and `max_tokens` is mandatory.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use gitai_core::config::ProviderConfig;
use gitai_core::error::{Error, Result};
use gitai_core::llm::{ChatRequest, ChatResponse, LlmProvider, MessageRole, Usage};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::retry::{RetryDecision, classify, sleep_backoff};

/// Used when a model config leaves `max_tokens` unset, since the API demands one.
const DEFAULT_MAX_TOKENS: u32 = 8192;
const API_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    name: String,
    cfg: ProviderConfig,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(name: impl Into<String>, mut cfg: ProviderConfig) -> Result<Self> {
        let name = name.into();
        if cfg.base_url.is_empty() || cfg.base_url.contains("openai") {
            cfg.base_url = "https://api.anthropic.com".into();
        }
        let client = reqwest::Client::builder()
            .user_agent(gitai_core::USER_AGENT)
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|e| Error::config(format!("provider `{name}`: {e}")))?;
        Ok(Self { name, cfg, client })
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'))
    }

    fn body(&self, req: &ChatRequest) -> Value {
        // System turns are collected out of the conversation and joined.
        let system: Vec<&str> = req
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .map(|m| m.content.as_str())
            .collect();

        let messages: Vec<Value> = req
            .messages
            .iter()
            .filter(|m| m.role != MessageRole::System)
            .map(|m| {
                json!({
                    "role": if m.role == MessageRole::Assistant { "assistant" } else { "user" },
                    "content": m.content,
                })
            })
            .collect();

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        });
        let map = body.as_object_mut().expect("object literal");
        if !system.is_empty() {
            map.insert("system".into(), json!(system.join("\n\n")));
        }
        if let Some(t) = req.temperature {
            map.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.top_p {
            map.insert("top_p".into(), json!(p));
        }
        if !req.stop.is_empty() {
            map.insert("stop_sequences".into(), json!(req.stop));
        }
        body
    }
}

#[derive(Debug, Deserialize)]
struct MessageResponse {
    #[serde(default)]
    model: String,
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let url = self.endpoint();
        let body = self.body(&req);
        let started = Instant::now();
        let mut last_err = String::new();

        for attempt in 0..=self.cfg.max_retries {
            let mut builder = self
                .client
                .post(&url)
                .header("anthropic-version", API_VERSION)
                .json(&body);
            if !self.cfg.api_key.is_empty() {
                builder = builder.header("x-api-key", &self.cfg.api_key);
            }
            for (k, v) in &self.cfg.headers {
                builder = builder.header(k, v);
            }

            let resp = match builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.to_string();
                    tracing::warn!(provider = %self.name, attempt, error = %last_err, "request failed");
                    sleep_backoff(attempt, None).await;
                    continue;
                }
            };

            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let text = resp.text().await.unwrap_or_default();

            match classify(status.as_u16()) {
                RetryDecision::Ok => {
                    let parsed: MessageResponse = serde_json::from_str(&text).map_err(|e| {
                        Error::llm(&self.name, format!("bad response: {e}; body: {text}"))
                    })?;
                    let content = parsed
                        .content
                        .iter()
                        .filter(|b| b.kind == "text")
                        .map(|b| b.text.as_str())
                        .collect::<Vec<_>>()
                        .join("");
                    let usage = parsed.usage.unwrap_or_default();
                    return Ok(ChatResponse {
                        content,
                        usage: Usage {
                            prompt_tokens: usage.input_tokens,
                            completion_tokens: usage.output_tokens,
                        },
                        // Normalised so ChatResponse::truncated works uniformly.
                        finish_reason: parsed.stop_reason.map(|r| {
                            if r == "max_tokens" {
                                "length".into()
                            } else {
                                r
                            }
                        }),
                        model: if parsed.model.is_empty() {
                            req.model.clone()
                        } else {
                            parsed.model
                        },
                        latency_ms: started.elapsed().as_millis() as u64,
                    });
                }
                RetryDecision::Retry => {
                    last_err = format!("http {status}: {text}");
                    tracing::warn!(provider = %self.name, attempt, %status, "retrying");
                    sleep_backoff(attempt, retry_after).await;
                }
                RetryDecision::Fatal => {
                    return Err(Error::llm(&self.name, format!("http {status}: {text}")));
                }
            }
        }

        Err(Error::llm(
            &self.name,
            format!(
                "giving up after {} retries: {last_err}",
                self.cfg.max_retries
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitai_core::llm::ChatMessage;

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new("anthropic", ProviderConfig::default()).unwrap()
    }

    #[test]
    fn system_turns_are_hoisted_out_of_the_message_list() {
        let req = ChatRequest::new(
            "claude-opus-5",
            vec![
                ChatMessage::system("rule one"),
                ChatMessage::system("rule two"),
                ChatMessage::user("do it"),
            ],
        );
        let body = provider().body(&req);
        assert_eq!(body["system"], "rule one\n\nrule two");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn max_tokens_is_always_present() {
        let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        let body = provider().body(&req);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn default_base_url_is_repaired() {
        assert_eq!(
            provider().endpoint(),
            "https://api.anthropic.com/v1/messages"
        );
    }
}
