//! Session state machine.
//!
//! A single actor task owns the "current session" state. It accepts
//! Requests forwarded from the stdin reader and spawns per-session
//! pipeline tasks that do VAD, ASR, and LLM cleanup.
//!
//! See DESIGN.md §Session actor for architecture, §Cancellation for
//! the cancel-token invariants, and §Gotchas for easy mistakes.

use std::collections::BTreeMap;
use std::sync::Arc;

use airtalk_proto::{Request, Response};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::asr::{AsrOutput, AsrProvider, AsrRequest};
use crate::config::CoreConfig;
use crate::llm::LlmProvider;
use crate::vad::VadFactory;

// ─── Public surface ────────────────────────────────────────────────────

/// Cheaply cloneable handle for submitting Requests to the actor.
#[derive(Clone)]
pub struct SessionHandle {
    cmd_tx: mpsc::UnboundedSender<ActorCmd>,
}

impl SessionHandle {
    /// Forward a Request from the stdin reader. Never blocks.
    pub fn submit(&self, req: Request) {
        let _ = self.cmd_tx.send(ActorCmd::Req(req));
    }

    /// Tell the actor to stop accepting new work and exit once any
    /// in-flight pipeline finishes naturally. Without this, the
    /// actor's self-held `cmd_tx` clone (kept so pipelines can route
    /// `PipelineDone` back) would keep the command channel open
    /// indefinitely and hang process exit. Callers (main.rs on
    /// stdin EOF) must invoke this before dropping the handle.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(ActorCmd::Shutdown);
    }
}

/// Spawn the session actor.
pub fn spawn(
    response_tx: mpsc::UnboundedSender<Response>,
    config: Arc<CoreConfig>,
    asr: Arc<dyn AsrProvider>,
    llm: Arc<dyn LlmProvider>,
    vad_factory: Arc<dyn VadFactory>,
) -> SessionHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let inner_cmd_tx = cmd_tx.clone();
    tokio::spawn(run_actor(
        cmd_rx,
        inner_cmd_tx,
        response_tx,
        config,
        asr,
        llm,
        vad_factory,
    ));
    SessionHandle { cmd_tx }
}

// ─── Per-session resolved parameters ───────────────────────────────────

/// Parameters for one pipeline run, resolved from the Begin request
/// plus CoreConfig defaults. Immutable for the lifetime of the session.
///
/// `language = None` means "auto-detect" (Qwen3-ASR `enable_lid = true`).
/// `context = None` means no glossary (neither per-session nor startup
/// hotwords had anything to contribute).
/// `llm_enabled` is the resolved effective value — if core started
/// with `--no-llm`, a session's `enable_llm = Some(true)` is silently
/// downgraded to `false` (policy: UI always gets a Result, not an
/// error, when the cleanup stage is unavailable).
struct SessionParams {
    language: Option<String>,
    context: Option<String>,
    enable_itn: bool,
    llm_enabled: bool,
}

impl SessionParams {
    fn resolve(
        id: u64,
        config: &CoreConfig,
        per_session_context: Option<String>,
        per_session_language: Option<String>,
        per_session_enable_itn: Option<bool>,
        per_session_enable_llm: Option<bool>,
    ) -> Self {
        // Language: per-session wins, else default. Literal "auto" or
        // empty string collapse to None (== enable LID).
        let resolved_lang = per_session_language
            .filter(|s| !s.is_empty() && s != "auto")
            .or_else(|| {
                let d = &config.asr_default_language;
                if d.is_empty() || d == "auto" {
                    None
                } else {
                    Some(d.clone())
                }
            });

        // Context: merge startup hotwords + per-session context.
        let hotwords_joined = if config.hotwords.is_empty() {
            None
        } else {
            Some(config.hotwords.join(", "))
        };
        let context = match (hotwords_joined, per_session_context) {
            (None, None) => None,
            (Some(h), None) => Some(h),
            (None, Some(c)) if c.is_empty() => None,
            (None, Some(c)) => Some(c),
            (Some(h), Some(c)) if c.is_empty() => Some(h),
            (Some(h), Some(c)) => Some(format!("{h}\n{c}")),
        };

        let enable_itn = per_session_enable_itn.unwrap_or(config.asr_default_enable_itn);

        // LLM: per-session override clamped by core availability.
        // Requesting LLM when core started with --no-llm silently
        // downgrades (logs a warning) rather than erroring — callers
        // treat LLM cleanup as an optional optimization.
        let llm_enabled = match per_session_enable_llm {
            Some(true) if !config.llm_enabled => {
                log::warn!(
                    "session {id}: enable_llm=true requested but core started with --no-llm; returning raw ASR text"
                );
                false
            }
            Some(want) => want,
            None => config.llm_enabled,
        };

        Self {
            language: resolved_lang,
            context,
            enable_itn,
            llm_enabled,
        }
    }
}

// ─── Actor internals ───────────────────────────────────────────────────

enum ActorCmd {
    /// Incoming Request from stdin.
    Req(Request),
    /// A pipeline task has finished (sent its terminal response or was
    /// cancelled). Lets the actor clear `current` if the id matches.
    PipelineDone(u64),
    /// Graceful-shutdown signal from the stdin reader on EOF. The
    /// actor stops accepting new Begins but lets the current pipeline
    /// (if any) finish and emit its terminal response, then exits.
    Shutdown,
}

struct ActiveSession {
    id: u64,
    cancel: CancellationToken,
    /// `None` after `End` has been processed — further Chunks are dropped.
    audio_tx: Option<mpsc::Sender<Vec<u8>>>,
}

#[allow(clippy::too_many_arguments)]
async fn run_actor(
    mut cmd_rx: mpsc::UnboundedReceiver<ActorCmd>,
    cmd_tx: mpsc::UnboundedSender<ActorCmd>,
    response_tx: mpsc::UnboundedSender<Response>,
    config: Arc<CoreConfig>,
    asr: Arc<dyn AsrProvider>,
    llm: Arc<dyn LlmProvider>,
    vad_factory: Arc<dyn VadFactory>,
) {
    let mut current: Option<ActiveSession> = None;
    let mut shutting_down = false;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            ActorCmd::Req(Request::Begin { id, .. }) if shutting_down => {
                // New session rejected during shutdown. In practice
                // this path is unreachable: main.rs sends Shutdown as
                // its last command after stdin EOF, so no further
                // Requests should arrive. Defensive.
                let _ = response_tx.send(Response::Error {
                    id,
                    message: "shutting_down".into(),
                });
            }

            ActorCmd::Req(Request::Begin {
                id,
                vad,
                context,
                language,
                enable_itn,
                enable_llm,
            }) => {
                // Supersede any in-flight session.
                if let Some(old) = current.take() {
                    old.cancel.cancel();
                    let _ = response_tx.send(Response::Error {
                        id: old.id,
                        message: "superseded".into(),
                    });
                }

                let params = Arc::new(SessionParams::resolve(
                    id,
                    config.as_ref(),
                    context,
                    language,
                    enable_itn,
                    enable_llm,
                ));

                let cancel = CancellationToken::new();
                let (audio_tx, audio_rx) = mpsc::channel(128);

                tokio::spawn(run_pipeline(
                    id,
                    vad,
                    audio_rx,
                    cancel.clone(),
                    response_tx.clone(),
                    cmd_tx.clone(),
                    asr.clone(),
                    llm.clone(),
                    vad_factory.clone(),
                    config.clone(),
                    params,
                ));

                current = Some(ActiveSession {
                    id,
                    cancel,
                    audio_tx: Some(audio_tx),
                });
            }

            ActorCmd::Req(Request::Chunk { id, pcm }) => {
                if let Some(s) = current.as_ref() {
                    if s.id == id {
                        if let Some(tx) = &s.audio_tx {
                            // Non-blocking: actor never stalls on a slow
                            // pipeline. Loss here means VAD/ASR can't
                            // keep up — warn, don't fail.
                            if tx.try_send(pcm).is_err() {
                                log::warn!("audio queue full, dropping chunk (id={id})");
                            }
                        }
                    }
                }
            }

            ActorCmd::Req(Request::End { id }) => {
                if let Some(s) = current.as_mut() {
                    if s.id == id {
                        // Drop the sender → pipeline sees channel close
                        // and proceeds to finalize.
                        s.audio_tx = None;
                    }
                }
            }

            ActorCmd::Req(Request::Cancel { id }) => {
                if let Some(s) = current.as_ref() {
                    if s.id == id {
                        let s = current.take().unwrap();
                        s.cancel.cancel();
                        let _ = response_tx.send(Response::Error {
                            id,
                            message: "cancelled".into(),
                        });
                    }
                }
            }

            ActorCmd::PipelineDone(id) => {
                if let Some(s) = current.as_ref() {
                    if s.id == id {
                        current = None;
                    }
                }
                // Stale PipelineDone (for a superseded or cancelled
                // session that still ran to conclusion) — no-op.
            }

            ActorCmd::Shutdown => {
                shutting_down = true;
                log::debug!("shutdown requested; draining in-flight session");
            }
        }

        // Graceful exit: after Shutdown has been observed AND the
        // current pipeline (if any) has sent its PipelineDone, we can
        // break out. The pipeline's Response::Result/Error was already
        // written to `response_tx` before PipelineDone, so the stdout
        // task has seen it.
        if shutting_down && current.is_none() {
            break;
        }
    }

    log::debug!("session actor exiting");
}

// ─── Pipeline ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
enum PipelineError {
    #[error("cancelled")]
    Cancelled,
    #[error("no_audio")]
    NoAudio,
    #[error("timeout")]
    Timeout,
    #[error("asr_failed: {0}")]
    AsrFailed(String),
    #[error("llm_failed: {0}")]
    LlmFailed(String),
}

/// Classify a provider error into the right `PipelineError` kind.
///
/// Walks the anyhow chain looking for a `reqwest::Error` whose
/// `.is_timeout()` is true. If found, returns `Timeout` so the
/// response uses the protocol's dedicated `timeout` message instead
/// of the generic `asr_failed:` / `llm_failed:` prefix.
fn classify_provider_err(err: anyhow::Error, stage: ProviderStage) -> PipelineError {
    for cause in err.chain() {
        if let Some(re) = cause.downcast_ref::<reqwest::Error>() {
            if re.is_timeout() {
                return PipelineError::Timeout;
            }
        }
    }
    match stage {
        ProviderStage::Asr => PipelineError::AsrFailed(err.to_string()),
        ProviderStage::Llm => PipelineError::LlmFailed(err.to_string()),
    }
}

#[derive(Clone, Copy)]
enum ProviderStage {
    Asr,
    Llm,
}

/// Successful terminal output of the pipeline, before wrapping in
/// `Response::Result`.
struct PipelineOutput {
    /// Final (LLM-cleaned, or passthrough) text.
    text: String,
    /// Raw ASR text before LLM cleanup. Equals `text` when LLM is off.
    raw: String,
    /// Language reported by Qwen3-ASR. First segment's language when
    /// VAD-segmented; whatever the single call returned otherwise.
    language: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    id: u64,
    vad: bool,
    audio_rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
    response_tx: mpsc::UnboundedSender<Response>,
    cmd_tx: mpsc::UnboundedSender<ActorCmd>,
    asr: Arc<dyn AsrProvider>,
    llm: Arc<dyn LlmProvider>,
    vad_factory: Arc<dyn VadFactory>,
    config: Arc<CoreConfig>,
    params: Arc<SessionParams>,
) {
    let outcome = if vad {
        run_segmented(
            audio_rx,
            cancel.clone(),
            asr.clone(),
            llm.clone(),
            vad_factory.as_ref(),
            config.clone(),
            params.clone(),
        )
        .await
    } else {
        run_single(
            audio_rx,
            cancel.clone(),
            asr.clone(),
            llm.clone(),
            config.clone(),
            params.clone(),
        )
        .await
    };

    // If the actor cancelled us, it already sent the terminal Error.
    // Otherwise translate the outcome into a Response.
    if !cancel.is_cancelled() {
        let response = match outcome {
            Ok(out) => Response::Result {
                id,
                text: out.text,
                raw: Some(out.raw),
                language: out.language,
            },
            Err(PipelineError::Cancelled) => {
                // Defensive: only reachable if cancel got set between
                // the check above and select branches below (possible
                // under pathological scheduling). Do not emit a
                // terminal response here — the actor will.
                let _ = cmd_tx.send(ActorCmd::PipelineDone(id));
                return;
            }
            Err(e) => Response::Error {
                id,
                message: e.to_string(),
            },
        };
        let _ = response_tx.send(response);
    }

    let _ = cmd_tx.send(ActorCmd::PipelineDone(id));
}

async fn run_segmented(
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
    asr: Arc<dyn AsrProvider>,
    llm: Arc<dyn LlmProvider>,
    vad_factory: &dyn VadFactory,
    config: Arc<CoreConfig>,
    params: Arc<SessionParams>,
) -> Result<PipelineOutput, PipelineError> {
    let mut vad = vad_factory.create();
    let sem = Arc::new(Semaphore::new(config.asr_concurrency.max(1)));
    let mut asr_tasks: JoinSet<(u64, anyhow::Result<AsrOutput>)> = JoinSet::new();
    let mut next_seq: u64 = 0;

    // Phase 1: read audio, push to VAD, spawn ASR per segment.
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(PipelineError::Cancelled),
            recv = audio_rx.recv() => match recv {
                Some(pcm) => {
                    for seg in vad.push_pcm(&pcm) {
                        spawn_asr_task(
                            &mut asr_tasks,
                            sem.clone(),
                            asr.clone(),
                            params.clone(),
                            next_seq,
                            seg.pcm,
                        );
                        next_seq += 1;
                    }
                }
                None => break, // End received, channel closed
            }
        }
    }

    // Trailing speech without a closing silence becomes the final segment.
    // No increment needed on `next_seq` after this — no further segments
    // will be spawned.
    if let Some(tail) = vad.finish() {
        spawn_asr_task(
            &mut asr_tasks,
            sem.clone(),
            asr.clone(),
            params.clone(),
            next_seq,
            tail.pcm,
        );
    }

    // Phase 2: collect ASR results, reassemble by seq.
    let mut results: BTreeMap<u64, AsrOutput> = BTreeMap::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(PipelineError::Cancelled),
            joined = asr_tasks.join_next() => match joined {
                Some(Ok((seq, Ok(out)))) => { results.insert(seq, out); }
                Some(Ok((seq, Err(e)))) => {
                    return Err(classify_provider_err(
                        e.context(format!("seq {seq}")),
                        ProviderStage::Asr,
                    ));
                }
                Some(Err(e)) => {
                    return Err(PipelineError::AsrFailed(format!("join error: {e}")));
                }
                None => break,
            }
        }
    }

    if results.is_empty() {
        return Err(PipelineError::NoAudio);
    }

    // First segment by seq drives the reported language.
    let language = results
        .values()
        .find_map(|o| o.language.clone());

    let raw = results
        .into_values()
        .map(|o| o.text)
        .collect::<Vec<_>>()
        .join(" ");

    // Phase 3: LLM cleanup (or pass-through).
    let text = if params.llm_enabled {
        let prompt = build_llm_prompt(&config.llm_prompt, params.context.as_deref());
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(PipelineError::Cancelled),
            res = llm.cleanup(&raw, &prompt) => {
                res.map_err(|e| classify_provider_err(e, ProviderStage::Llm))?
            }
        }
    } else {
        raw.clone()
    };

    Ok(PipelineOutput { text, raw, language })
}

async fn run_single(
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
    asr: Arc<dyn AsrProvider>,
    llm: Arc<dyn LlmProvider>,
    config: Arc<CoreConfig>,
    params: Arc<SessionParams>,
) -> Result<PipelineOutput, PipelineError> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(PipelineError::Cancelled),
            recv = audio_rx.recv() => match recv {
                Some(pcm) => buf.extend_from_slice(&pcm),
                None => break,
            }
        }
    }

    if buf.is_empty() {
        return Err(PipelineError::NoAudio);
    }

    let asr_out = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(PipelineError::Cancelled),
        res = asr.transcribe(AsrRequest {
            pcm: &buf,
            language: params.language.as_deref(),
            context: params.context.as_deref(),
            enable_itn: params.enable_itn,
        }) => {
            res.map_err(|e| classify_provider_err(e, ProviderStage::Asr))?
        }
    };

    let raw = asr_out.text;
    let language = asr_out.language;

    let text = if params.llm_enabled {
        let prompt = build_llm_prompt(&config.llm_prompt, params.context.as_deref());
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(PipelineError::Cancelled),
            res = llm.cleanup(&raw, &prompt) => {
                res.map_err(|e| classify_provider_err(e, ProviderStage::Llm))?
            }
        }
    } else {
        raw.clone()
    };

    Ok(PipelineOutput { text, raw, language })
}

fn spawn_asr_task(
    set: &mut JoinSet<(u64, anyhow::Result<AsrOutput>)>,
    sem: Arc<Semaphore>,
    asr: Arc<dyn AsrProvider>,
    params: Arc<SessionParams>,
    seq: u64,
    pcm: Vec<u8>,
) {
    set.spawn(async move {
        let _permit = match sem.acquire_owned().await {
            Ok(p) => p,
            Err(e) => return (seq, Err(anyhow::anyhow!("semaphore closed: {e}"))),
        };
        let res = asr
            .transcribe(AsrRequest {
                pcm: &pcm,
                language: params.language.as_deref(),
                context: params.context.as_deref(),
                enable_itn: params.enable_itn,
            })
            .await;
        (seq, res)
    });
}

/// Append the per-session context onto the LLM system prompt.
///
/// Empty / None context → return prompt unchanged. Otherwise append
/// a neutral labelled block so the model treats it as glossary hints
/// rather than instructions.
fn build_llm_prompt(base: &str, context: Option<&str>) -> String {
    match context {
        Some(ctx) if !ctx.is_empty() => format!("{base}\n\nContext:\n{ctx}"),
        _ => base.to_string(),
    }
}
