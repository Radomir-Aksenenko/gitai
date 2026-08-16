//! Model router. The pipeline asks for a role's model by its config name and
//! gets back text plus what the call cost, without knowing or caring which
//! provider served it.

use std::collections::HashMap;
use std::sync::Arc;

use gitai_core::config::{Config, ModelConfig, ProviderKind};
use gitai_core::error::{Error, Result};
use gitai_core::llm::{ChatMessage, ChatRequest, LlmProvider, ResponseFormat};
use gitai_core::model::Spend;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use crate::anthropic::AnthropicProvider;
use crate::json;
use crate::mock::MockProvider;
use crate::openai::OpenAiProvider;

/// One request to a named model.
#[derive(Debug, Clone)]
pub struct Call {
    pub messages: Vec<ChatMessage>,
    /// Ask the provider for JSON where it supports the flag.
    pub json: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
}

impl Call {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            json: false,
            temperature: None,
            max_tokens: None,
        }
    }

    pub fn json(mut self) -> Self {
        self.json = true;
        self
    }

    pub fn temperature(mut self, t: f64) -> Self {
        self.temperature = Some(t);
        self
    }
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    pub spend: Spend,
    /// Model id the provider reported, which can differ from what was asked for.
    pub model: String,
}

pub struct ModelRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    models: HashMap<String, ModelConfig>,
    /// One permit pool per provider, so a local runtime is not stampeded by a
    /// fan-out of eight workers.
    limits: HashMap<String, Arc<Semaphore>>,
}

impl ModelRegistry {
    pub fn build(cfg: &Config) -> Result<Self> {
        let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        let mut limits = HashMap::new();

        for (name, pc) in &cfg.providers {
            let provider: Arc<dyn LlmProvider> = match pc.kind {
                ProviderKind::Openai => Arc::new(OpenAiProvider::new(name, pc.clone())?),
                ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(name, pc.clone())?),
                ProviderKind::Mock => Arc::new(MockProvider::new(name, pc)),
            };
            providers.insert(name.clone(), provider);
            limits.insert(
                name.clone(),
                Arc::new(Semaphore::new(pc.concurrency.max(1))),
            );
        }

        Ok(Self {
            providers,
            models: cfg.models.clone().into_iter().collect(),
            limits,
        })
    }

    pub fn has_model(&self, name: &str) -> bool {
        self.models.contains_key(name)
    }

    /// Input window of a model, for planning how much context to build.
    /// Unknown models fall back to a small window rather than a generous one,
    /// so a typo in a role name truncates instead of overflowing.
    pub fn context_tokens(&self, name: &str) -> u32 {
        self.models
            .get(name)
            .map(|m| m.context_tokens)
            .unwrap_or(8_192)
    }

    fn resolve(
        &self,
        model_name: &str,
    ) -> Result<(&ModelConfig, Arc<dyn LlmProvider>, Arc<Semaphore>)> {
        let mc = self
            .models
            .get(model_name)
            .ok_or_else(|| Error::config(format!("unknown model `{model_name}`")))?;
        let provider = self
            .providers
            .get(&mc.provider)
            .ok_or_else(|| Error::config(format!("unknown provider `{}`", mc.provider)))?
            .clone();
        let limit = self
            .limits
            .get(&mc.provider)
            .cloned()
            .unwrap_or_else(|| Arc::new(Semaphore::new(1)));
        Ok((mc, provider, limit))
    }

    pub async fn complete(&self, model_name: &str, call: Call) -> Result<Completion> {
        let (mc, provider, limit) = self.resolve(model_name)?;

        let mut req = ChatRequest::new(&mc.model, call.messages);
        req.temperature = call.temperature.or(mc.temperature);
        req.top_p = mc.top_p;
        req.max_tokens = call.max_tokens.or(mc.max_tokens);
        req.response_format = if call.json {
            ResponseFormat::Json
        } else {
            ResponseFormat::Text
        };

        // The context builder sizes itself from the same estimate, so this
        // firing means something upstream did not go through it.
        let estimated =
            crate::tokens::estimate_all(req.messages.iter().map(|m| m.content.as_str()));
        if estimated as u32 > mc.context_tokens {
            tracing::warn!(
                model = model_name,
                estimated_tokens = estimated,
                context_tokens = mc.context_tokens,
                "prompt is estimated to exceed the model's context window"
            );
        }

        let _permit = limit
            .acquire()
            .await
            .map_err(|e| Error::llm(provider.name(), format!("semaphore closed: {e}")))?;

        let resp = provider.complete(req).await?;
        let spend = resp.spend(mc.price_in, mc.price_out);

        if resp.truncated() {
            return Err(Error::llm(
                provider.name(),
                format!(
                    "model `{model_name}` hit its output limit; raise models.{model_name}.max_tokens"
                ),
            ));
        }

        Ok(Completion {
            content: resp.content,
            spend,
            model: resp.model,
        })
    }

    /// Calls a model and parses JSON out of the answer, re-asking on a parse
    /// failure. Small models get this wrong often enough that one repair pass
    /// is cheaper than losing the attempt.
    pub async fn complete_json<T: DeserializeOwned>(
        &self,
        model_name: &str,
        call: Call,
        repairs: u32,
    ) -> Result<(T, Spend)> {
        let mut messages = call.messages.clone();
        let mut total = Spend::default();
        let mut last_err = None;

        for _ in 0..=repairs {
            let attempt = Call {
                messages: messages.clone(),
                json: true,
                temperature: call.temperature,
                max_tokens: call.max_tokens,
            };
            let completion = self.complete(model_name, attempt).await?;
            total.add(&completion.spend);

            match json::parse_json::<T>(&completion.content) {
                Ok(v) => return Ok((v, total)),
                Err(e) => {
                    tracing::warn!(model = model_name, error = %e, "unparseable JSON, asking again");
                    messages.push(ChatMessage::assistant(&completion.content));
                    messages.push(ChatMessage::user(
                        "That was not valid JSON matching the requested schema. \
                         Reply again with the JSON object only. No prose, no code fence.",
                    ));
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| Error::bad_output("no JSON after repair attempts")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn mock_config() -> Config {
        Config::from_toml(
            r#"
[providers.mock]
kind = "mock"
base_url = ""

[models.tiny]
provider = "mock"
model = "tiny"
price_in = 1.0
price_out = 2.0

[roles]
planner = "tiny"
worker = ["tiny"]
editor = "tiny"
reviewer = "tiny"
arbiter = "tiny"
"#,
        )
        .unwrap()
    }

    #[derive(Debug, Deserialize)]
    struct Plan {
        goal: String,
    }

    #[tokio::test]
    async fn routes_to_the_provider_and_prices_the_call() {
        let reg = ModelRegistry::build(&mock_config()).unwrap();
        let call = Call::new(vec![ChatMessage::system("gitai-role: planner")]);
        let out = reg.complete("tiny", call).await.unwrap();
        assert!(out.content.contains("goal"));
        assert_eq!(out.spend.llm_calls, 1);
        assert!(out.spend.cost_usd > 0.0);
    }

    #[tokio::test]
    async fn json_helper_deserialises() {
        let reg = ModelRegistry::build(&mock_config()).unwrap();
        let call = Call::new(vec![ChatMessage::system("gitai-role: planner")]);
        let (plan, spend) = reg.complete_json::<Plan>("tiny", call, 1).await.unwrap();
        assert!(plan.goal.contains("Mock plan"));
        assert_eq!(spend.llm_calls, 1);
    }

    #[tokio::test]
    async fn unknown_model_is_a_config_error() {
        let reg = ModelRegistry::build(&mock_config()).unwrap();
        let err = reg
            .complete("ghost", Call::new(vec![]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost"), "{err}");
    }
}
