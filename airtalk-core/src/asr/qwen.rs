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
//!       {"role": "user",   "content": [{"audio": "data:audio/wav;base64,…"}]}
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
//! * Audio is PCM16 LE 16 kHz mono wrapped in a 44-byte RIFF/WAVE
//!   header and base64-encoded as a data URI. DashScope also accepts
//!   uploaded file URLs, but inline is simpler for sub-3-min
//!   utterances (our case — one hotkey-held phrase).

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::{AsrOutput, AsrProvider, AsrRequest};

const MODEL: &str = "qwen3-asr-flash";

pub struct QwenAsr {
    client: Client,
    api_key: String,
    endpoint: String,
}

impl QwenAsr {
    pub fn new(endpoint: String, api_key: String, timeout: Duration) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .context("building reqwest Client")?;
        Ok(Self {
            client,
            api_key,
            endpoint,
        })
    }
}

// ─── Response shape ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DashScopeResponse {
    output: Option<DashScopeOutput>,
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

// ─── Provider impl ─────────────────────────────────────────────────────

#[async_trait]
impl AsrProvider for QwenAsr {
    async fn transcribe(&self, req: AsrRequest<'_>) -> anyhow::Result<AsrOutput> {
        // Wrap PCM16 LE 16 kHz mono in a WAV container + base64 data URI.
        let wav = pcm16_to_wav_16k_mono(req.pcm);
        let audio_uri = format!("data:audio/wav;base64,{}", STANDARD.encode(&wav));

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
            anyhow::bail!("HTTP {}: {}", status.as_u16(), truncate(&body_text, 500));
        }

        let parsed: DashScopeResponse = serde_json::from_str(&body_text).with_context(|| {
            format!("parsing ASR response: {}", truncate(&body_text, 500))
        })?;

        // DashScope sometimes returns 200 with an error `code` in the
        // body. Defensive: treat any non-empty `code` as failure.
        if let Some(code) = parsed.code.as_deref() {
            if !code.is_empty() {
                let msg = parsed.message.unwrap_or_default();
                anyhow::bail!("DashScope {code}: {msg}");
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

        Ok(AsrOutput { text, language })
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

/// Wrap PCM16 LE 16 kHz mono bytes in a 44-byte RIFF/WAVE header.
///
/// Layout:
///
/// ```text
/// 0  "RIFF"              4 bytes
/// 4  file_size - 8       u32 LE
/// 8  "WAVE"              4 bytes
/// 12 "fmt "              4 bytes
/// 16 subchunk1_size = 16 u32 LE
/// 20 audio_format   = 1  u16 LE   (PCM)
/// 22 num_channels   = 1  u16 LE
/// 24 sample_rate = 16000 u32 LE
/// 28 byte_rate   = 32000 u32 LE
/// 32 block_align     = 2 u16 LE
/// 34 bits_per_sample= 16 u16 LE
/// 36 "data"              4 bytes
/// 40 data_size           u32 LE
/// 44 PCM payload
/// ```
fn pcm16_to_wav_16k_mono(pcm: &[u8]) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    const CHANNELS: u16 = 1;
    const BITS_PER_SAMPLE: u16 = 16;
    const BYTE_RATE: u32 = SAMPLE_RATE * (CHANNELS as u32) * (BITS_PER_SAMPLE as u32) / 8;
    const BLOCK_ALIGN: u16 = CHANNELS * BITS_PER_SAMPLE / 8;

    let data_size = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32.saturating_add(data_size)).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&BYTE_RATE.to_le_bytes());
    out.extend_from_slice(&BLOCK_ALIGN.to_le_bytes());
    out.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…[truncated]")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_fields_are_correct() {
        let pcm: Vec<u8> = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let wav = pcm16_to_wav_16k_mono(&pcm);

        assert_eq!(wav.len(), 44 + pcm.len());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");

        let data_size = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(data_size as usize, pcm.len());

        let sr = u32::from_le_bytes(wav[24..28].try_into().unwrap());
        assert_eq!(sr, 16_000);

        let channels = u16::from_le_bytes(wav[22..24].try_into().unwrap());
        assert_eq!(channels, 1);

        let bits = u16::from_le_bytes(wav[34..36].try_into().unwrap());
        assert_eq!(bits, 16);

        assert_eq!(&wav[44..], &pcm[..]);
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
