//! Qwen3-ASR client — DashScope multimodal-generation REST.
//!
//! Posts a single non-streaming request to DashScope's
//! `multimodal-generation/generation` endpoint with `model =
//! qwen3-asr-flash`, and parses the recognized text out of
//! `output.choices[0].message.content`. See the API reference at
//! <https://help.aliyun.com/zh/model-studio/qwen-speech-recognition>.
//!
//! # Request shape
//!
//! ```json
//! {
//!   "model": "qwen3-asr-flash",
//!   "input": {
//!     "messages": [
//!       {"role": "system", "content": [{"text": "<context>"}]},
//!       {"role": "user",   "content": [{"audio": "data:audio/ogg;base64,…"}]}
//!     ]
//!   },
//!   "parameters": {
//!     "asr_options": {
//!       "enable_lid": true,
//!       "enable_itn": false,
//!       "language": "zh"
//!     },
//!     "result_format": "message"
//!   }
//! }
//! ```
//!
//! * The system message carries per-session `context` (glossary,
//!   domain hints). Omitted entirely when there's no context.
//! * `enable_lid` is automatically `true` when `AsrRequest.language`
//!   is `None` (i.e. caller wants auto-detection), and `false` when
//!   an explicit language is pinned.
//! * Audio is encoded per [`super::audio::AudioFormat`] (default:
//!   Opus @ 24 kbps in Ogg) and base64-embedded as a data URI.
//!   DashScope also accepts uploaded file URLs, but inline is simpler
//!   for sub-3-min utterances (our case — one hotkey-held phrase).

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::audio::AudioFormat;
use super::{AsrOutput, AsrProvider, AsrRequest};
use crate::util::{redact_api_key, truncate};

const MODEL: &str = "qwen3-asr-flash";

pub struct QwenAsr {
    client: Client,
    api_key: String,
    endpoint: String,
    audio_format: AudioFormat,
}

impl QwenAsr {
    pub fn new(
        endpoint: String,
        api_key: String,
        timeout: Duration,
        audio_format: AudioFormat,
    ) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .context("building reqwest Client")?;
        Ok(Self {
            client,
            api_key,
            endpoint,
            audio_format,
        })
    }
}

// ─── Response shape ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DashScopeResponse {
    output: Option<DashScopeOutput>,
    #[serde(default)]
    usage: Option<DashScopeUsage>,
    /// Populated by DashScope on error (non-empty `code` means failure).
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct DashScopeOutput {
    choices: Vec<DashScopeChoice>,
}

#[derive(Deserialize)]
struct DashScopeChoice {
    message: DashScopeMessage,
}

#[derive(Deserialize)]
struct DashScopeMessage {
    /// Content may be either a plain string or an array of typed
    /// objects like `[{"text": "…"}]`. We handle both.
    content: Value,
    #[serde(default)]
    annotations: Option<Vec<DashScopeAnnotation>>,
}

#[derive(Deserialize)]
struct DashScopeAnnotation {
    #[serde(default)]
    language: Option<String>,
}

/// DashScope's `usage` block.
///
/// Field names differ between the DashScope sync endpoint (what we
/// call) and the OpenAI-compatible endpoint. We accept any subset of
/// both shapes:
///
/// * Sync API (`/multimodal-generation/generation`) — documented shape:
///   ```json
///   "usage": {
///     "input_tokens_details": {"text_tokens": 0},
///     "output_tokens_details": {"text_tokens": 6},
///     "seconds": 1
///   }
///   ```
///   `seconds` is the billing unit for Qwen3-ASR. The nested details
///   expose text output length; `input_tokens_details.text_tokens` is
///   documented as "无需关注" (ignore) but we surface it if present.
///
/// * OpenAI-compat API (`/chat/completions`) adds top-level
///   `prompt_tokens` / `completion_tokens` / `total_tokens` alongside
///   `seconds`, with `prompt_tokens_details.audio_tokens = 25 × seconds`.
#[derive(Deserialize)]
struct DashScopeUsage {
    /// Audio duration in seconds — DashScope's billing unit.
    #[serde(default)]
    seconds: Option<u64>,

    // OpenAI-compat top-level aggregates.
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,

    // Sync-API nested details.
    #[serde(default)]
    input_tokens_details: Option<DashScopeTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<DashScopeTokenDetails>,

    // Legacy / observed variants — kept as fallbacks so we don't
    // lose signal if DashScope renames fields.
    #[serde(default)]
    audio_seconds: Option<f64>,
    #[serde(default)]
    audio_duration: Option<f64>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct DashScopeTokenDetails {
    #[serde(default)]
    text_tokens: Option<u64>,
    #[serde(default)]
    audio_tokens: Option<u64>,
}

impl DashScopeUsage {
    fn into_proto(self) -> airtalk_proto::AsrUsage {
        // Billing unit: `seconds` (documented) → legacy float fallbacks.
        let audio_seconds = self
            .seconds
            .map(|s| s as f64)
            .or(self.audio_seconds)
            .or(self.audio_duration)
            .or(self.duration);

        // Input tokens: prefer top-level aggregate (OpenAI-compat); else
        // nested audio_tokens (also OpenAI-compat) or text_tokens.
        let input_tokens = self.prompt_tokens.or(self.input_tokens).or_else(|| {
            self.input_tokens_details
                .as_ref()
                .and_then(|d| d.audio_tokens.or(d.text_tokens))
        });

        // Output tokens: aggregate first; else nested text_tokens
        // (sync API — actual transcription length).
        let output_tokens = self.completion_tokens.or(self.output_tokens).or_else(|| {
            self.output_tokens_details
                .as_ref()
                .and_then(|d| d.text_tokens)
        });

        airtalk_proto::AsrUsage {
            audio_seconds,
            input_tokens,
            output_tokens,
            total_tokens: self.total_tokens,
        }
    }
}

// ─── Provider impl ─────────────────────────────────────────────────────

#[async_trait]
impl AsrProvider for QwenAsr {
    async fn transcribe(&self, req: AsrRequest<'_>) -> anyhow::Result<AsrOutput> {
        // Encode PCM16 LE 16 kHz mono per the configured format
        // (WAV passthrough or Opus/Ogg). Wrap as a base64 data URI.
        let encoded = self
            .audio_format
            .encode(req.pcm)
            .context("encoding audio for ASR upload")?;
        let upload_bytes = encoded.len() as u64;
        let audio_uri = format!(
            "data:{};base64,{}",
            self.audio_format.mime(),
            STANDARD.encode(&encoded),
        );

        // Messages: optional system (context) + mandatory user (audio).
        let mut messages: Vec<Value> = Vec::with_capacity(2);
        if let Some(ctx) = req.context.filter(|c| !c.is_empty()) {
            messages.push(json!({
                "role": "system",
                "content": [{"text": ctx}],
            }));
        }
        messages.push(json!({
            "role": "user",
            "content": [{"audio": audio_uri}],
        }));

        // asr_options: enable_lid iff caller left language unset.
        let mut asr_options: Map<String, Value> = Map::new();
        asr_options.insert("enable_lid".into(), json!(req.language.is_none()));
        asr_options.insert("enable_itn".into(), json!(req.enable_itn));
        if let Some(lang) = req.language {
            asr_options.insert("language".into(), json!(lang));
        }

        let body = json!({
            "model": MODEL,
            "input": {"messages": messages},
            "parameters": {
                "asr_options": Value::Object(asr_options),
                "result_format": "message",
            },
        });

        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("ASR request failed")?;

        let status = resp.status();
        let body_text = resp.text().await.context("reading ASR response body")?;
        if !status.is_success() {
            anyhow::bail!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate(&redact_api_key(&body_text, &self.api_key), 500)
            );
        }

        let parsed: DashScopeResponse = serde_json::from_str(&body_text).with_context(|| {
            format!(
                "parsing ASR response: {}",
                truncate(&redact_api_key(&body_text, &self.api_key), 500)
            )
        })?;

        // DashScope sometimes returns 200 with an error `code` in the
        // body. Defensive: treat any non-empty `code` as failure.
        if let Some(code) = parsed.code.as_deref() {
            if !code.is_empty() {
                let msg = parsed.message.unwrap_or_default();
                anyhow::bail!("DashScope {code}: {}", redact_api_key(&msg, &self.api_key));
            }
        }

        let output = parsed.output.context("response has no output")?;
        let first = output
            .choices
            .into_iter()
            .next()
            .context("response has no choices")?;

        let text = extract_text(&first.message.content)
            .context("could not extract text from response content")?;

        let language = first
            .message
            .annotations
            .as_ref()
            .and_then(|a| a.first())
            .and_then(|a| a.language.clone());

        let usage = parsed.usage.map(DashScopeUsage::into_proto);

        Ok(AsrOutput {
            text,
            language,
            upload_bytes,
            usage,
        })
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────

/// Extract recognized text from either response shape DashScope may emit.
///
/// Most of the time it's `[{"text": "…"}]`; some variants send a plain
/// string. A few also interleave non-text items we ignore.
fn extract_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        let trimmed = s.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut combined = String::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                combined.push_str(t);
            }
        }
        let trimmed = combined.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_parses_sync_api_shape() {
        // Real DashScope `/multimodal-generation/generation` shape.
        let raw = json!({
            "input_tokens_details": {"text_tokens": 0},
            "output_tokens_details": {"text_tokens": 6},
            "seconds": 14
        });
        let parsed: DashScopeUsage = serde_json::from_value(raw).unwrap();
        let proto = parsed.into_proto();
        assert_eq!(proto.audio_seconds, Some(14.0));
        assert_eq!(proto.output_tokens, Some(6));
        assert_eq!(proto.total_tokens, None); // not in sync API
    }

    #[test]
    fn usage_parses_openai_compat_shape() {
        // Real `/chat/completions` shape for qwen3-asr-flash.
        let raw = json!({
            "completion_tokens": 12,
            "completion_tokens_details": {"text_tokens": 12},
            "prompt_tokens": 42,
            "prompt_tokens_details": {"audio_tokens": 42, "text_tokens": 0},
            "seconds": 1,
            "total_tokens": 54
        });
        let parsed: DashScopeUsage = serde_json::from_value(raw).unwrap();
        let proto = parsed.into_proto();
        assert_eq!(proto.audio_seconds, Some(1.0));
        assert_eq!(proto.input_tokens, Some(42));
        assert_eq!(proto.output_tokens, Some(12));
        assert_eq!(proto.total_tokens, Some(54));
    }

    #[test]
    fn extract_text_handles_array_form() {
        let v = json!([{"text": "hello "}, {"text": "world"}]);
        assert_eq!(extract_text(&v).as_deref(), Some("hello world"));
    }

    #[test]
    fn extract_text_handles_string_form() {
        let v = json!("你好，世界");
        assert_eq!(extract_text(&v).as_deref(), Some("你好，世界"));
    }

    #[test]
    fn extract_text_empty_returns_none() {
        assert!(extract_text(&json!("")).is_none());
        assert!(extract_text(&json!([{"text": ""}])).is_none());
        assert!(extract_text(&json!(null)).is_none());
    }
}
