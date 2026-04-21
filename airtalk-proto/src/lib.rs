//! Wire protocol between airtalk UI and airtalk-core.
//!
//! # Framing — newline-delimited JSON (NDJSON)
//!
//! Each message is compact JSON followed by a single `\n`:
//!
//! ```text
//! {"type":"begin","id":1,"vad":true}\n
//! {"type":"chunk","id":1,"pcm":"…base64…"}\n
//! ```
//!
//! One message per line. JSON string escaping guarantees `\n` never
//! appears inside a message body, so the line break is a reliable
//! record separator. Readers should bound line length to
//! [`MAX_FRAME_SIZE`] to protect against malformed peers.
//!
//! # Representation
//!
//! * Enums use an internal tag on `"type"`, lowercased — e.g.
//!   `{"type":"begin", …}`
//! * Structs / variants serialize as JSON objects with named fields
//! * `Option::None` fields are **omitted** entirely (never emitted as
//!   `null`), so minimal messages stay small
//! * `Chunk.pcm` is encoded as a **standard base64 string** (not a
//!   JSON array of numbers). ~33% size overhead over raw bytes,
//!   trivially decoded by any language's base64 library.
//!
//! # u64 note
//!
//! JSON numbers have no integer type. Session `id` is emitted as a
//! JSON number; consumers in languages that coerce to float64 (JS)
//! can safely represent values below 2^53 (~9 × 10^15). A fresh
//! `AtomicU64` counter won't approach that in any realistic session
//! lifetime, so this is a documentation note, not a correctness risk.
//!
//! # Lifecycle
//!
//! * `core` emits exactly one [`Response::Ready`] on startup
//! * UI then sends a stream of [`Request`] values over stdin
//! * core replies with [`Response::Result`] or [`Response::Error`],
//!   exactly one terminal response per [`Request::Begin`] session
//!
//! See `DESIGN.md` §Protocol for the full contract.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Protocol version. Bumped on any breaking wire-format change.
///
/// * v1 — postcard binary
/// * v2 — MessagePack binary
/// * v3 — NDJSON with base64-encoded PCM (current)
pub const PROTOCOL_VERSION: u32 = 3;

/// Maximum accepted frame payload size (16 MB).
///
/// Guards against a malformed peer — real chunks are tens of KB, and
/// the largest realistic payload (~5 min of PCM16 @ 16 kHz, base64-
/// encoded) is well under 15 MB.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

// ─── Messages ──────────────────────────────────────────────────────────

/// UI → core.
///
/// Wire representation uses internal tag `type` with lowercase variant
/// name (e.g. `"begin"`, `"chunk"`).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Request {
    /// Open a new session. Supersedes any in-flight session.
    ///
    /// * `vad = true`  — core uses Silero VAD to slice the audio and
    ///                   runs concurrent ASR per segment
    /// * `vad = false` — core buffers the whole stream and runs ASR
    ///                   once on `End`
    ///
    /// Optional per-session overrides for the Qwen3-ASR call. When
    /// `None`, the core's startup defaults apply.
    ///
    /// * `context`    — glossary / domain hints / recent conversation.
    ///                  Sent to Qwen3-ASR as the system message, and
    ///                  appended to the LLM cleanup system prompt.
    /// * `language`   — BCP-47-ish language tag (`"zh"`, `"en"`, …).
    ///                  The literal `"auto"` (or empty) forces
    ///                  language identification on the ASR side.
    /// * `enable_itn` — inverse text normalization
    ///                  (digits / punctuation normalization).
    /// * `enable_llm` — per-session LLM cleanup toggle. `Some(false)`
    ///                  skips cleanup and returns raw ASR text.
    ///                  `Some(true)` requests cleanup; if the core
    ///                  was started with `--no-llm`, core silently
    ///                  downgrades to raw (logs a warning) — the UI
    ///                  always gets a `Result`, never an error, for
    ///                  this mismatch. `None` = use core default.
    Begin {
        id: u64,
        vad: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enable_itn: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enable_llm: Option<bool>,
    },

    /// Audio data for an open session.
    ///
    /// Format is fixed: PCM16 little-endian, 16 kHz, mono.
    /// Recommended chunk duration: 20–50 ms (640–1600 bytes).
    /// Chunks whose `id` does not match the current session are
    /// silently dropped by core.
    ///
    /// On the wire, `pcm` is a standard base64 string.
    Chunk {
        id: u64,
        #[serde(with = "pcm_base64")]
        pcm: Vec<u8>,
    },

    /// Signal end of audio. Triggers ASR finalization and LLM cleanup.
    End { id: u64 },

    /// Cancel the open session. core responds with
    /// `Error { id, message: "cancelled" }`.
    Cancel { id: u64 },
}

/// core → UI.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Response {
    /// Emitted exactly once on core startup, before any Request is read.
    Ready { protocol_version: u32 },

    /// Successful completion.
    ///
    /// * `text`     — final output (LLM-cleaned unless `--no-llm` was
    ///                passed, in which case equals `raw`).
    /// * `raw`      — concatenated ASR text before LLM cleanup. Always
    ///                populated when ASR ran; may equal `text` when
    ///                LLM is disabled.
    /// * `language` — language detected by Qwen3-ASR. When
    ///                VAD-segmented, this is the first segment's
    ///                language — good enough as a UI hint, not a
    ///                per-segment tag.
    /// * `stats`    — per-session counters + latencies + provider
    ///                usage. Always populated; individual subfields
    ///                may still be `None` (e.g. `llm_usage` when
    ///                cleanup was skipped).
    Result {
        id: u64,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        stats: SessionStats,
    },

    /// Abnormal session termination.
    ///
    /// `message` is free-form but well-known values are exported as
    /// constants under [`error_code`] so callers can match without
    /// string literals spreading across the codebase.
    Error { id: u64, message: String },
}

// ─── Stats ─────────────────────────────────────────────────────────────

/// Per-session counters, latencies, and provider-reported usage.
///
/// Emitted on every successful `Result`. Fields are split into three
/// groups:
///
/// * **Self-computed timings/counters** — we measure these directly in
///   the pipeline, so they're always populated.
/// * **Provider-reported usage** — parsed from ASR/LLM responses. May
///   be `None` if the provider didn't include a `usage` block in that
///   particular call, or if the stage was skipped (`--no-llm`, etc.).
/// * **Mode-dependent counters** — e.g. `vad_segments` is only
///   meaningful when the session had `vad = true`.
///
/// Durations are in milliseconds. Byte counts reflect *encoded* (WAV
/// or Ogg/Opus) payload post-`audio_format.encode()` — before base64 —
/// so they match what actually crosses the wire to DashScope before
/// base64's 33% overhead.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    /// Total PCM audio received from the UI (before any VAD trimming).
    pub pcm_received_ms: u32,

    /// Total PCM audio actually forwarded to ASR. Equals
    /// `pcm_received_ms` in non-VAD mode; less when VAD trimmed silence.
    pub pcm_sent_to_asr_ms: u32,

    /// Number of VAD segments produced. `None` when `vad = false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vad_segments: Option<u32>,

    /// Number of ASR HTTP calls made (== `vad_segments` with VAD on,
    /// 1 with VAD off).
    pub asr_calls: u32,

    /// Total encoded audio bytes uploaded across all ASR calls (pre-
    /// base64). Useful for verifying actual compression ratios when
    /// testing different `--asr-audio-format` settings.
    pub asr_upload_bytes: u64,

    /// Slowest single ASR call's wall-clock time. With concurrent VAD
    /// segments each call is timed individually; this is the max
    /// across them (the bottleneck call). Not the sum, and not the
    /// "End → ASR done" wait — the latter is misleading because
    /// segments spawned during audio reception may already be finished
    /// by the time End arrives.
    pub asr_latency_ms: u32,

    /// Wall-clock time spent on LLM cleanup. `None` when `--no-llm` or
    /// per-session `enable_llm = false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_latency_ms: Option<u32>,

    /// Wall-clock time from `End` arriving at the actor to the terminal
    /// `Result` being emitted on stdout.
    pub total_latency_ms: u32,

    /// ASR provider-reported usage (summed across segments). `None`
    /// when the provider didn't include a usage block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_usage: Option<AsrUsage>,

    /// LLM provider-reported usage. `None` when cleanup was skipped or
    /// the provider didn't include a usage block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_usage: Option<LlmUsage>,
}

/// Qwen3-ASR usage. DashScope reports audio duration in seconds plus
/// token-equivalent counts for billing; we surface what we see.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct AsrUsage {
    /// Audio duration as reported by DashScope, in seconds. May differ
    /// from our `pcm_sent_to_asr_ms` (different rounding, or padding
    /// on the provider side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_seconds: Option<f64>,

    /// Token count reported by the provider (some DashScope responses
    /// include this alongside audio seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

/// OpenAI-compatible chat completion usage.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct LlmUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

// ─── PCM base64 codec (wire: base64 String ↔ Rust: Vec<u8>) ────────────

mod pcm_base64 {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::de::Error as DeError;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        STANDARD.decode(s.as_bytes()).map_err(DeError::custom)
    }
}

// ─── Error code constants ──────────────────────────────────────────────

/// Well-known `Response::Error::message` values.
///
/// Anything starting with `asr_failed:` / `llm_failed:` carries the
/// upstream provider message as a free-form suffix and should be
/// matched via `starts_with`, not equality.
pub mod error_code {
    pub const CANCELLED: &str = "cancelled";
    pub const SUPERSEDED: &str = "superseded";
    pub const NO_AUDIO: &str = "no_audio";
    pub const TIMEOUT: &str = "timeout";
    pub const ASR_FAILED_PREFIX: &str = "asr_failed:";
    pub const LLM_FAILED_PREFIX: &str = "llm_failed:";
}

// ─── Error type ────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("json codec error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("frame too large: {got} bytes exceeds max {max}")]
    FrameTooLarge { got: usize, max: usize },

    #[error("peer closed the stream")]
    Eof,
}

// ─── Sync framing ──────────────────────────────────────────────────────

/// Encode and write a framed message (NDJSON: one compact line + LF).
pub fn write_frame<W, M>(w: &mut W, msg: &M) -> Result<(), ProtocolError>
where
    W: Write,
    M: Serialize,
{
    let mut line = serde_json::to_vec(msg)?;
    if line.len() >= MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            got: line.len(),
            max: MAX_FRAME_SIZE,
        });
    }
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

/// Read one NDJSON frame from a blocking reader.
///
/// Returns [`ProtocolError::Eof`] cleanly when the peer closes before
/// any bytes of a new line arrive. A line cut short of a terminating
/// `\n` is reported as an I/O error.
///
/// Reads byte-by-byte to keep the input bound to `&mut R: Read`.
/// Production callers usually wrap a [`std::io::BufReader`] on the
/// outside, but tests (Cursor-over-Vec) are fine as-is.
pub fn read_frame<R, M>(r: &mut R) -> Result<M, ProtocolError>
where
    R: Read,
    M: for<'de> Deserialize<'de>,
{
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) => {
                return if buf.is_empty() {
                    Err(ProtocolError::Eof)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "partial line: no trailing newline",
                    )
                    .into())
                };
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if buf.len() >= MAX_FRAME_SIZE {
                    return Err(ProtocolError::FrameTooLarge {
                        got: buf.len() + 1,
                        max: MAX_FRAME_SIZE,
                    });
                }
                buf.push(byte[0]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(serde_json::from_slice(&buf)?)
}

// ─── Async framing (tokio) ─────────────────────────────────────────────

#[cfg(feature = "tokio")]
pub use tokio_io::*;

#[cfg(feature = "tokio")]
mod tokio_io {
    use super::*;
    use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn write_frame_async<W, M>(w: &mut W, msg: &M) -> Result<(), ProtocolError>
    where
        W: AsyncWrite + Unpin,
        M: Serialize,
    {
        let mut line = serde_json::to_vec(msg)?;
        if line.len() >= MAX_FRAME_SIZE {
            return Err(ProtocolError::FrameTooLarge {
                got: line.len(),
                max: MAX_FRAME_SIZE,
            });
        }
        line.push(b'\n');
        w.write_all(&line).await?;
        w.flush().await?;
        Ok(())
    }

    /// Async counterpart to [`read_frame`]. Caller must hand in an
    /// [`AsyncBufRead`] — wrap `stdin()` in `tokio::io::BufReader`.
    ///
    /// Enforces a post-read size check against [`MAX_FRAME_SIZE`]. A
    /// sufficiently hostile peer could still make us allocate up to
    /// that cap before we notice; this is acceptable for the stdio
    /// trust boundary where both endpoints are our own processes.
    pub async fn read_frame_async<R, M>(r: &mut R) -> Result<M, ProtocolError>
    where
        R: AsyncBufRead + Unpin,
        M: for<'de> Deserialize<'de>,
    {
        let mut buf = Vec::new();
        let n = r.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Err(ProtocolError::Eof);
        }
        if buf.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "partial line: no trailing newline",
            )
            .into());
        }
        buf.pop(); // drop the newline
        if buf.len() > MAX_FRAME_SIZE {
            return Err(ProtocolError::FrameTooLarge {
                got: buf.len(),
                max: MAX_FRAME_SIZE,
            });
        }
        Ok(serde_json::from_slice(&buf)?)
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_begin_full() {
        let msg = Request::Begin {
            id: 42,
            vad: true,
            context: Some("DashScope, Qwen-ASR, airtalk".into()),
            language: Some("zh".into()),
            enable_itn: Some(false),
            enable_llm: Some(true),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Request = read_frame(&mut cursor).unwrap();
        match decoded {
            Request::Begin {
                id,
                vad,
                context,
                language,
                enable_itn,
                enable_llm,
            } => {
                assert_eq!(id, 42);
                assert!(vad);
                assert_eq!(context.as_deref(), Some("DashScope, Qwen-ASR, airtalk"));
                assert_eq!(language.as_deref(), Some("zh"));
                assert_eq!(enable_itn, Some(false));
                assert_eq!(enable_llm, Some(true));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_begin_minimal() {
        // Minimal Begin: no overrides. Omitted fields decode as None
        // and do NOT appear on the wire.
        let msg = Request::Begin {
            id: 1,
            vad: false,
            context: None,
            language: None,
            enable_itn: None,
            enable_llm: None,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();

        let line = std::str::from_utf8(&buf).unwrap().trim_end();
        // Must not contain nulls or absent-field keys.
        for absent in ["context", "language", "enable_itn", "enable_llm", "null"] {
            assert!(
                !line.contains(absent),
                "minimal begin leaked `{absent}`: {line}"
            );
        }

        let mut cursor = Cursor::new(buf);
        let decoded: Request = read_frame(&mut cursor).unwrap();
        match decoded {
            Request::Begin {
                id,
                vad,
                context,
                language,
                enable_itn,
                enable_llm,
            } => {
                assert_eq!(id, 1);
                assert!(!vad);
                assert!(context.is_none());
                assert!(language.is_none());
                assert!(enable_itn.is_none());
                assert!(enable_llm.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_chunk_large() {
        let pcm = (0..32000u32).map(|i| i as u8).collect::<Vec<_>>();
        let msg = Request::Chunk {
            id: 1,
            pcm: pcm.clone(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Request = read_frame(&mut cursor).unwrap();
        match decoded {
            Request::Chunk { id, pcm: got } => {
                assert_eq!(id, 1);
                assert_eq!(got, pcm);
            }
            _ => panic!("wrong variant"),
        }
    }

    /// Regression guard: `pcm` MUST go on the wire as a standard
    /// base64 JSON string, not an array of numbers. An array encoding
    /// would bloat by ~5× and be awkward for non-Rust consumers.
    #[test]
    fn chunk_pcm_on_wire_is_base64_string() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;

        let pcm = b"hello world".to_vec();
        let expected_b64 = STANDARD.encode(&pcm);

        let msg = Request::Chunk {
            id: 7,
            pcm: pcm.clone(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let line = std::str::from_utf8(&buf).unwrap();

        assert!(
            line.contains(&format!(r#""pcm":"{expected_b64}""#)),
            "pcm not encoded as base64 string; wire = {line}"
        );
    }

    #[test]
    fn roundtrip_result_unicode_with_raw_and_lang() {
        let stats = SessionStats {
            pcm_received_ms: 3_200,
            pcm_sent_to_asr_ms: 2_800,
            vad_segments: Some(2),
            asr_calls: 2,
            asr_upload_bytes: 12_345,
            asr_latency_ms: 420,
            llm_latency_ms: Some(180),
            total_latency_ms: 610,
            asr_usage: Some(AsrUsage {
                audio_seconds: Some(2.8),
                input_tokens: None,
                output_tokens: None,
                total_tokens: Some(140),
            }),
            llm_usage: Some(LlmUsage {
                prompt_tokens: Some(42),
                completion_tokens: Some(8),
                total_tokens: Some(50),
            }),
        };
        let msg = Response::Result {
            id: 7,
            text: "你好，世界。🌏".to_string(),
            raw: Some("你好世界".to_string()),
            language: Some("zh".to_string()),
            stats: stats.clone(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Response = read_frame(&mut cursor).unwrap();
        match decoded {
            Response::Result {
                id,
                text,
                raw,
                language,
                stats: got_stats,
            } => {
                assert_eq!(id, 7);
                assert_eq!(text, "你好，世界。🌏");
                assert_eq!(raw.as_deref(), Some("你好世界"));
                assert_eq!(language.as_deref(), Some("zh"));
                assert_eq!(got_stats, stats);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_ready() {
        let msg = Response::Ready {
            protocol_version: PROTOCOL_VERSION,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Response = read_frame(&mut cursor).unwrap();
        assert!(matches!(
            decoded,
            Response::Ready { protocol_version: v } if v == PROTOCOL_VERSION
        ));
    }

    #[test]
    fn stream_multiple_frames() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Request::Begin {
                id: 1,
                vad: true,
                context: None,
                language: None,
                enable_itn: None,
                enable_llm: None,
            },
        )
        .unwrap();
        write_frame(
            &mut buf,
            &Request::Chunk {
                id: 1,
                pcm: vec![0u8; 100],
            },
        )
        .unwrap();
        write_frame(&mut buf, &Request::End { id: 1 }).unwrap();

        // Sanity: three lines, separated by \n.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 3);

        let mut cursor = Cursor::new(buf);
        let a: Request = read_frame(&mut cursor).unwrap();
        let b: Request = read_frame(&mut cursor).unwrap();
        let c: Request = read_frame(&mut cursor).unwrap();
        assert!(matches!(a, Request::Begin { id: 1, .. }));
        assert!(matches!(b, Request::Chunk { id: 1, .. }));
        assert!(matches!(c, Request::End { id: 1 }));
    }

    /// NDJSON guarantee: no raw `\n` inside a frame. JSON escapes
    /// newlines in strings as `\\n`.
    #[test]
    fn newlines_in_text_are_escaped_not_bare() {
        let msg = Response::Result {
            id: 1,
            text: "line1\nline2\nline3".into(),
            raw: None,
            language: None,
            stats: SessionStats::default(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        // Exactly one raw newline: the terminator.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(buf.last(), Some(&b'\n'));
    }

    #[test]
    fn clean_eof() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let err: Result<Request, _> = read_frame(&mut cursor);
        assert!(matches!(err, Err(ProtocolError::Eof)));
    }

    #[test]
    fn partial_line_is_unexpected_eof() {
        // Bytes without trailing newline.
        let partial = br#"{"type":"end","id":1}"#.to_vec();
        let mut cursor = Cursor::new(partial);
        let err: Result<Request, _> = read_frame(&mut cursor);
        assert!(matches!(err, Err(ProtocolError::Io(_))), "got {err:?}");
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn async_roundtrip() {
        use tokio::io::{duplex, BufReader};
        let (mut client, server) = duplex(64 * 1024);

        let send = Request::Chunk {
            id: 99,
            pcm: vec![1, 2, 3, 4],
        };
        write_frame_async(&mut client, &send).await.unwrap();

        let mut server = BufReader::new(server);
        let got: Request = read_frame_async(&mut server).await.unwrap();
        match got {
            Request::Chunk { id, pcm } => {
                assert_eq!(id, 99);
                assert_eq!(pcm, vec![1, 2, 3, 4]);
            }
            _ => panic!("wrong variant"),
        }
    }
}
