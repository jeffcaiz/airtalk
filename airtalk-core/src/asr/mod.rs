//! ASR provider trait and implementations.

use async_trait::async_trait;

pub mod qwen;

/// Per-call ASR request.
///
/// `context` is the merged glossary / domain hint string that goes into
/// Qwen3-ASR's system message (startup hotwords joined with the
/// per-session `Begin.context`). `None` means no context.
///
/// `language = None` requests automatic language identification (LID);
/// `Some(lang)` pins the language and disables LID upstream.
pub struct AsrRequest<'a> {
    pub pcm: &'a [u8],
    pub language: Option<&'a str>,
    pub context: Option<&'a str>,
    pub enable_itn: bool,
}

/// Per-call ASR response.
pub struct AsrOutput {
    pub text: String,
    /// Language reported by Qwen3-ASR. `None` if the provider didn't
    /// surface one (or stub).
    pub language: Option<String>,
}

#[async_trait]
pub trait AsrProvider: Send + Sync {
    /// Transcribe a single chunk of PCM16 LE 16 kHz mono audio.
    async fn transcribe(&self, req: AsrRequest<'_>) -> anyhow::Result<AsrOutput>;
}
