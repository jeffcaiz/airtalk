//! LLM provider trait and OpenAI-compatible implementation.

use async_trait::async_trait;

pub mod openai;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Clean up the raw ASR text using the given system prompt.
    async fn cleanup(&self, text: &str, system_prompt: &str) -> anyhow::Result<String>;
}

/// No-op provider used when core is started with `--no-llm`.
///
/// The session pipeline honors `SessionParams.llm_enabled` and will
/// not call `cleanup()` in that mode, so this stub only exists to
/// satisfy the `Arc<dyn LlmProvider>` type. If it *is* called
/// (mis-wired caller, bug in resolution logic), it errors loudly
/// instead of silently returning empty text.
pub struct DisabledLlm;

#[async_trait]
impl LlmProvider for DisabledLlm {
    async fn cleanup(&self, _text: &str, _system_prompt: &str) -> anyhow::Result<String> {
        anyhow::bail!("LLM is disabled (--no-llm); cleanup() should never be called")
    }
}
