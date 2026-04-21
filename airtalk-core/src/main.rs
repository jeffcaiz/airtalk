//! airtalk-core: background computation process.
//!
//! Reads Requests from stdin, emits Responses to stdout. See DESIGN.md
//! for protocol details.

mod asr;
mod config;
mod llm;
mod prompt;
mod session;
mod vad;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use airtalk_proto::{
    read_frame_async, write_frame_async, ProtocolError, Request, Response, PROTOCOL_VERSION,
};
use anyhow::Context;
use clap::Parser;
use tokio::io::{stdin, stdout, BufReader};
use tokio::sync::mpsc;

use crate::asr::audio::AudioFormat;
use crate::config::CoreConfig;

#[derive(Parser, Debug)]
#[command(name = "airtalk-core", version, about = "airtalk core process")]
struct Cli {
    // ─── ASR ──────────────────────────────────────────────────────────
    /// Default recognition language code passed to Qwen3-ASR (e.g. `zh`,
    /// `en`). Default is `auto` — language identification is on unless
    /// the UI/session explicitly pins a language. Per-session
    /// `Begin.language` overrides this.
    #[arg(long, default_value = "auto")]
    asr_lang: String,

    /// Single ASR HTTP request timeout, in milliseconds.
    #[arg(long, default_value_t = 30_000)]
    asr_timeout_ms: u64,

    /// DashScope endpoint URL for Qwen3-ASR. Default is the mainland
    /// multimodal-generation endpoint; for the international region
    /// use the `dashscope-intl.aliyuncs.com` host variant.
    #[arg(
        long,
        default_value = "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
    )]
    asr_base_url: String,

    /// DashScope API key for Qwen3-ASR.
    #[arg(long, env = "AIRTALK_ASR_API_KEY")]
    asr_api_key: String,

    /// Default for Qwen3-ASR's inverse text normalization
    /// (digits / punctuation normalization). Off by default; per-session
    /// `Begin.enable_itn` overrides this.
    #[arg(long)]
    asr_enable_itn: bool,

    /// Audio encoding used when uploading to Qwen3-ASR. One of:
    /// `wav` (PCM16/16k/mono in RIFF, lossless, ~32 KB/s), `opus`
    /// (Opus @ 24 kbps in Ogg, default, ~3 KB/s), or `opus:<bitrate>`
    /// where bitrate is e.g. `16k`, `24k`, `32k`, or a literal bps
    /// number like `24000`.
    #[arg(long, default_value = "opus:24k")]
    asr_audio_format: String,

    /// Optional hotwords file, one per line. Lines starting with `#`
    /// are treated as comments. Prepended to every session's context
    /// (on top of per-session `Begin.context`).
    #[arg(long)]
    hotwords_file: Option<PathBuf>,

    // ─── LLM ──────────────────────────────────────────────────────────
    /// OpenAI-compatible base URL. Default points at DashScope Bailian
    /// (mainland region); for other providers (DeepSeek, Moonshot,
    /// OpenAI itself, etc.) override this.
    #[arg(
        long,
        default_value = "https://dashscope.aliyuncs.com/compatible-mode/v1"
    )]
    llm_base_url: String,

    /// Model ID to use for cleanup. Default `qwen-flash` (cheap/fast
    /// on DashScope); swap if you change `--llm-base-url`.
    #[arg(long, default_value = "qwen-flash")]
    llm_model: String,

    /// **JSON field name** for the max-tokens cap in the chat request
    /// body — NOT the value. The value itself is hardcoded to 4096
    /// (cleanup output is always tiny, so the cap is just a safety
    /// bound). Most providers accept `max_tokens`; newer OpenAI models
    /// (o1 family) require `max_completion_tokens` instead.
    #[arg(long, default_value = "max_tokens")]
    llm_max_token_param: String,

    /// Single LLM HTTP request timeout, in milliseconds.
    #[arg(long, default_value_t = 30_000)]
    llm_timeout_ms: u64,

    /// LLM provider API key. Required unless `--no-llm` is set. Can
    /// also come from `AIRTALK_LLM_API_KEY` in the environment.
    #[arg(long, env = "AIRTALK_LLM_API_KEY")]
    llm_api_key: Option<String>,

    /// Optional file whose contents replace the built-in system prompt.
    #[arg(long)]
    llm_prompt_file: Option<PathBuf>,

    /// Skip LLM cleanup entirely — emit raw ASR text as the Result.
    #[arg(long)]
    no_llm: bool,

    // ─── VAD ──────────────────────────────────────────────────────────
    /// Silence duration (ms) that closes a speech segment.
    #[arg(long, default_value_t = 800)]
    vad_silence_ms: u32,

    /// Minimum segment duration (ms); shorter segments are dropped.
    #[arg(long, default_value_t = 250)]
    vad_min_segment_ms: u32,

    /// Pre- and post-padding (ms) attached to each emitted segment.
    #[arg(long, default_value_t = 150)]
    vad_padding_ms: u32,

    /// Concurrent ASR workers.
    #[arg(long, default_value_t = 2)]
    asr_concurrency: usize,

    // ─── Logging ──────────────────────────────────────────────────────
    /// Log level filter: error, warn, info, debug, trace.
    #[arg(long, default_value = "info")]
    log_level: String,

    /// If set, logs are appended here. Otherwise logs go to stderr.
    #[arg(long)]
    log_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    init_logging(&args)?;

    log::info!("airtalk-core starting (pid={})", std::process::id());

    // ─── Phase 1: synchronous init ────────────────────────────────────
    // Anything that can fail at startup happens here, BEFORE emitting
    // Ready. The UI treats "no Ready within 5s" as a boot failure, so
    // we must not emit Ready until we're fully ready to accept work.

    let hotwords = load_hotwords(args.hotwords_file.as_deref())
        .context("loading hotwords")?;

    let prompt = prompt::load(args.llm_prompt_file.as_deref())
        .context("loading LLM prompt")?;

    let asr_audio_format = AudioFormat::from_str(&args.asr_audio_format)
        .with_context(|| format!("parsing --asr-audio-format {:?}", args.asr_audio_format))?;
    log::info!("asr audio format: {asr_audio_format:?}");

    let config = Arc::new(CoreConfig {
        asr_default_language: args.asr_lang.clone(),
        asr_concurrency: args.asr_concurrency,
        hotwords,
        asr_default_enable_itn: args.asr_enable_itn,
        llm_enabled: !args.no_llm,
        llm_prompt: prompt,
    });

    let asr: Arc<dyn asr::AsrProvider> = Arc::new(
        asr::qwen::QwenAsr::new(
            args.asr_base_url.clone(),
            args.asr_api_key.clone(),
            Duration::from_millis(args.asr_timeout_ms),
            asr_audio_format,
        )
        .context("building Qwen ASR client")?,
    );

    let llm: Arc<dyn llm::LlmProvider> = if args.no_llm {
        Arc::new(llm::DisabledLlm)
    } else {
        // URL and model always have defaults (DashScope + qwen-flash);
        // only the API key is strictly required. The env-var fallback
        // on `llm_api_key` already covers the common case.
        let api_key = args
            .llm_api_key
            .clone()
            .context("--llm-api-key (or AIRTALK_LLM_API_KEY) is required unless --no-llm is set")?;
        Arc::new(
            llm::openai::OpenAiLlm::new(llm::openai::OpenAiConfig {
                base_url: args.llm_base_url.clone(),
                api_key,
                model: args.llm_model.clone(),
                max_token_param: args.llm_max_token_param.clone(),
                timeout: Duration::from_millis(args.llm_timeout_ms),
            })
            .context("building OpenAI-compatible LLM client")?,
        )
    };

    let vad_factory: Arc<dyn vad::VadFactory> = Arc::new(
        vad::silero::SileroFactory::load(vad::silero::SileroConfig {
            speech_threshold: 0.5,
            silence_threshold: 0.35,
            min_segment_ms: args.vad_min_segment_ms,
            end_silence_ms: args.vad_silence_ms,
            padding_ms: args.vad_padding_ms,
        })
        .context("loading Silero VAD model")?,
    );

    // ─── Phase 2: actor + stdio tasks ─────────────────────────────────

    let (response_tx, mut response_rx) = mpsc::unbounded_channel::<Response>();

    let session_handle = session::spawn(
        response_tx.clone(),
        config,
        asr,
        llm,
        vad_factory,
    );

    // Announce readiness. This MUST go to the stdout writer so it gets
    // properly framed; don't write to stdout directly.
    let _ = response_tx.send(Response::Ready {
        protocol_version: PROTOCOL_VERSION,
    });

    // stdout writer task: drains response_rx and writes framed Responses.
    let stdout_task = tokio::spawn(async move {
        let mut out = stdout();
        while let Some(resp) = response_rx.recv().await {
            if let Err(e) = write_frame_async(&mut out, &resp).await {
                log::error!("stdout write failed: {e}");
                break;
            }
        }
    });

    // Main task: read stdin, forward to session actor.
    // BufReader is required because read_frame_async expects AsyncBufRead
    // (NDJSON line reader).
    let mut input = BufReader::new(stdin());
    loop {
        match read_frame_async::<_, Request>(&mut input).await {
            Ok(req) => session_handle.submit(req),
            Err(ProtocolError::Eof) => {
                log::info!("stdin EOF — shutting down");
                break;
            }
            Err(e) => {
                log::error!("stdin protocol error: {e}");
                break;
            }
        }
    }

    // Graceful shutdown:
    //   1. Tell the actor no more Requests are coming. The actor
    //      stops accepting new Begins but lets any in-flight pipeline
    //      finish and emit its terminal response.
    //   2. Drop our external sender handles. The stdout writer only
    //      closes once the last response_tx clone (held by a running
    //      pipeline, or by the actor) is dropped.
    //   3. Wait for the stdout writer to drain.
    session_handle.shutdown();
    drop(session_handle);
    drop(response_tx);
    let _ = stdout_task.await;
    log::info!("airtalk-core exiting");
    Ok(())
}

fn init_logging(args: &Cli) -> anyhow::Result<()> {
    let level: log::LevelFilter = args
        .log_level
        .parse()
        .with_context(|| format!("parsing log level {:?}", args.log_level))?;

    let mut builder = env_logger::Builder::new();
    builder.filter_level(level);

    if let Some(path) = &args.log_file {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening log file {}", path.display()))?;
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    } else {
        builder.target(env_logger::Target::Stderr);
    }

    builder.init();
    Ok(())
}

fn load_hotwords(path: Option<&std::path::Path>) -> anyhow::Result<Vec<String>> {
    match path {
        None => Ok(Vec::new()),
        Some(p) => {
            let s = std::fs::read_to_string(p)
                .with_context(|| format!("reading {}", p.display()))?;
            Ok(s.lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
                .collect())
        }
    }
}
