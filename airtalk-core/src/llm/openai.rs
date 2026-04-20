//! OpenAI Chat Completions–compatible LLM client.
//!
//! Works against any provider that speaks the OpenAI chat endpoint
//! (DashScope's compatible mode, DeepSeek, Moonshot, OpenRouter,
//! OpenAI itself, etc.). Provider-specific quirks are parameterized:
//!
//! * [`OpenAiConfig::max_token_param`] — some providers accept
//!   `max_tokens` and others want `max_completion_tokens`.
//!
//! The request body is:
//!
//! ```json
//! {
//!   "model": "<model>",
//!   "messages": [
//!     {"role": "system", "content": "<prompt>"},
//!     {"role": "user",   "content": "<raw ASR text>"}
//!   ],
//!   "stream": false,
//!   "temperature": 0.2,
//!   "<max_token_param>": 4096
//! }
//! ```
//!
//! Temperature is kept low (0.2): cleanup should be near-deterministic
//! — we want the same raw input to produce roughly the same cleaned
//! output. `stream: false` because we emit exactly one terminal
//! Result per session; SSE adds complexity with no benefit here.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::LlmProvider;

pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_token_param: String,
    pub timeout: Duration,
}

pub struct OpenAiLlm {
    client: Client,
    config: OpenAiConfig,
}

impl OpenAiLlm {
    pub fn new(config: OpenAiConfig) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(config.timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .context("building reqwest Client")?;
        Ok(Self { client, config })
    }
}

// ─── Response shape ────────────────────────────────────────────────────
// Only the fields we actually use. Unknown fields (usage, finish_reason,
// etc.) are ignored by serde.

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

#[async_trait]
impl LlmProvider for OpenAiLlm {
    async fn cleanup(&self, text: &str, system_prompt: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        // Build body with a dynamic field name for the max-token param
        // (some providers want `max_tokens`, newer OpenAI models want
        // `max_completion_tokens`). Using a Map so the dynamic key
        // doesn't need a compile-time name.
        let mut obj: Map<String, Value> = Map::new();
        obj.insert("model".into(), Value::String(self.config.model.clone()));
        obj.insert(
            "messages".into(),
            json!([
                {"role": "system", "content": system_prompt},
                {"role": "user",   "content": text},
            ]),
        );
        obj.insert("stream".into(), Value::Bool(false));
        obj.insert("temperature".into(), json!(0.2));
        obj.insert(self.config.max_token_param.clone(), json!(4096));
        // Disable Qwen3's reasoning mode — cleanup is near-deterministic
        // and thinking tokens just add latency. Non-Qwen OpenAI-compatible
        // providers ignore unknown fields.
        obj.insert("enable_thinking".into(), Value::Bool(false));
        let body = Value::Object(obj);

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await
            .context("LLM request failed")?;

        let status = resp.status();
        let body_text = resp.text().await.context("reading LLM response body")?;
        if !status.is_success() {
            anyhow::bail!("HTTP {}: {}", status.as_u16(), truncate(&body_text, 500));
        }

        let parsed: ChatResponse = serde_json::from_str(&body_text).with_context(|| {
            format!("parsing LLM response: {}", truncate(&body_text, 500))
        })?;
        let cleaned = parsed
            .choices
            .into_iter()
            .next()
            .context("response has no choices")?
            .message
            .content
            .trim()
            .to_string();
        if cleaned.is_empty() {
            anyhow::bail!("LLM returned empty content");
        }
        Ok(cleaned)
    }
}

/// Truncate to at most `max_chars` characters, appending an ellipsis
/// marker when cut. Used to bound error-context strings so a huge
/// HTML error page or runaway response doesn't blow up logs.
fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…[truncated]")
    } else {
        head
    }
}
