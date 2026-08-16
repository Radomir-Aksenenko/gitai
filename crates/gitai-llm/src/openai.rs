//! OpenAI chat-completions adapter.
//!
//! This one adapter is the whole local-model story: vLLM, Ollama, LM Studio and
//! llama.cpp all serve this API, as do OpenRouter, DeepSeek, Together and
//! OpenAI itself. Point `base_url` at whichever and it works.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use gitai_core::config::ProviderConfig;
use gitai_core::error::{Error, Result};
use gitai_core::llm::{ChatRequest, ChatResponse, LlmProvider, MessageRole, ResponseFormat, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::retry::{RetryDecision, classify, sleep_backoff};

pub struct OpenAiProvider {
    name: String,
    cfg: ProviderConfig,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(name: impl Into<String>, cfg: ProviderConfig) -> Result<Self> {
        let name = name.into();
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in &cfg.headers {
            let key = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::config(format!("provider `{name}` header `{k}`: {e}")))?;
            let val = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| Error::config(format!("provider `{name}` header `{k}`: {e}")))?;
            headers.insert(key, val);
        }
        let client = reqwest::Client::builder()
            .user_agent(gitai_core::USER_AGENT)
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .default_headers(headers)
            .build()
            .map_err(|e| Error::config(format!("provider `{name}`: {e}")))?;
        Ok(Self { name, cfg, client })
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    fn body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| {
                json!({
                    "role": match m.role {
                        MessageRole::System => "system",
                        MessageRole::User => "user",
                        MessageRole::Assistant => "assistant",
                    },
                    "content": m.content,
                })
            })
            .collect();

        let mut body = json!({ "model": req.model, "messages": messages });
        let map = body.as_object_mut().expect("object literal");
        if let Some(t) = req.temperature {
            map.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.top_p {
            map.insert("top_p".into(), json!(p));
        }
        if let Some(m) = req.max_tokens {
            map.insert("max_tokens".into(), json!(m));
        }
        if !req.stop.is_empty() {
            map.insert("stop".into(), json!(req.stop));
        }
        if req.response_format == ResponseFormat::Json {
            map.insert("response_format".into(), json!({ "type": "json_object" }));
        }
        body
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    model: String,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<ChoiceMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    /// Some reasoning-model servers put the answer here and leave `content` null.
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let url = self.endpoint();
        let body = self.body(&req);
        let started = Instant::now();
        let mut last_err = String::new();

        for attempt in 0..=self.cfg.max_retries {
            let mut builder = self.client.post(&url).json(&body);
            if !self.cfg.api_key.is_empty() {
                builder = builder.bearer_auth(&self.cfg.api_key);
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
                    let parsed: ChatCompletion = serde_json::from_str(&text).map_err(|e| {
                        Error::llm(
                            &self.name,
                            format!("bad response: {e}; body: {}", trim(&text)),
                        )
                    })?;
                    let choice = parsed.choices.into_iter().next().ok_or_else(|| {
                        Error::llm(
                            &self.name,
                            format!("no choices in response: {}", trim(&text)),
                        )
                    })?;
                    let msg = choice.message.unwrap_or(ChoiceMessage {
                        content: None,
                        reasoning_content: None,
                    });
                    let content = msg
                        .content
                        .filter(|c| !c.trim().is_empty())
                        .or(msg.reasoning_content)
                        .unwrap_or_default();
                    let usage = parsed.usage.unwrap_or_default();
                    return Ok(ChatResponse {
                        content,
                        usage: Usage {
                            prompt_tokens: usage.prompt_tokens,
                            completion_tokens: usage.completion_tokens,
                        },
                        finish_reason: choice.finish_reason,
                        model: if parsed.model.is_empty() {
                            req.model.clone()
                        } else {
                            parsed.model
                        },
                        latency_ms: started.elapsed().as_millis() as u64,
                    });
                }
                RetryDecision::Retry => {
                    last_err = format!("http {status}: {}", trim(&text));
                    tracing::warn!(provider = %self.name, attempt, %status, "retrying");
                    sleep_backoff(attempt, retry_after).await;
                }
                RetryDecision::Fatal => {
                    return Err(Error::llm(
                        &self.name,
                        format!("http {status}: {}", trim(&text)),
                    ));
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

fn trim(s: &str) -> String {
    const LIMIT: usize = 600;
    if s.len() <= LIMIT {
        return s.to_string();
    }
    let mut end = LIMIT;
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitai_core::llm::ChatMessage;

    fn provider() -> OpenAiProvider {
        OpenAiProvider::new(
            "test",
            ProviderConfig {
                base_url: "http://localhost:11434/v1/".into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn endpoint_tolerates_a_trailing_slash() {
        assert_eq!(
            provider().endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn optional_params_are_omitted_not_nulled() {
        let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        let body = provider().body(&req);
        let map = body.as_object().unwrap();
        assert!(!map.contains_key("temperature"));
        assert!(!map.contains_key("max_tokens"));
        assert!(!map.contains_key("response_format"));
    }

    #[test]
    fn json_mode_sets_response_format() {
        let mut req = ChatRequest::new("m", vec![ChatMessage::system("s"), ChatMessage::user("u")]);
        req.response_format = ResponseFormat::Json;
        req.temperature = Some(0.2);
        let body = provider().body(&req);
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["temperature"], 0.2);
    }
}
