//! Immutable core configuration, captured from CLI args at startup.

/// Shared immutable config passed to the session actor and its pipelines.
///
/// The language / ITN fields here are **defaults** — each session's
/// `Begin` may override them via its `language` / `enable_itn` fields.
/// See DESIGN.md §Protocol.
///
/// Audio encoding (`--asr-audio-format`) is also a startup-level
/// choice but lives entirely inside `QwenAsr` — the session actor
/// never sees it, so it's not threaded through here.
///
/// Any change requires restarting the core process (UI side restarts
/// `airtalk-core.exe` whenever the user saves settings — see DESIGN.md
/// §Config lifecycle).
pub struct CoreConfig {
    /// Default language tag for ASR (e.g. `"zh"`). Literal `"auto"` or
    /// empty = let Qwen3-ASR detect (enables LID).
    pub asr_default_language: String,
    pub asr_concurrency: usize,
    /// Global glossary / hotwords loaded from `--hotwords-file`.
    /// Prepended to any per-session context when calling ASR / LLM.
    pub hotwords: Vec<String>,
    /// Default for `enable_itn` when the session's Begin doesn't set it.
    pub asr_default_enable_itn: bool,
    pub llm_enabled: bool,
    pub llm_prompt: String,
}
