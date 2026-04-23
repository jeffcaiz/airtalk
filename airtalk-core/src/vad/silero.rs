//! Silero VAD via onnxruntime (`ort` crate).
//!
//! # Model
//!
//! Tested against Silero VAD v5:
//!   * Input  `input`: f32 tensor [1, 512]   (32 ms @ 16 kHz)
//!   * Input  `state`: f32 tensor [2, 1, 128] (LSTM state)
//!   * Input  `sr`:    i64 tensor [1]        (16000)
//!   * Output `output`: f32 tensor [1, 1]    (speech probability)
//!   * Output `stateN`: f32 tensor [2, 1, 128] (updated state)
//!
//! If you use a different Silero version, inspect the model with
//! `netron` and adjust the tensor names + frame size below. v4 uses
//! 256 samples @ 8 kHz or 512 @ 16 kHz; v3 used 1536 samples.
//!
//! # Algorithm
//!
//! * Convert incoming PCM16 → f32 [-1, 1]
//! * Buffer into 512-sample frames
//! * Per frame: run inference → probability, apply hysteresis:
//!     - Silence → Speech when `prob ≥ speech_threshold`
//!     - Speech → Silence when `prob < silence_threshold` for
//!       `end_silence_ms` continuously
//! * Emit segment with `padding_ms` of pre-padding (from a ring buffer
//!   of recent silence samples) and up to `padding_ms` of post-padding
//!   drawn from the closing silence.
//! * Segments shorter than `min_segment_ms` are dropped (filters
//!   single "uh", mic clicks, etc.).
//!
//! See DESIGN.md §VAD pitfalls for tuning notes and known gotchas.

use std::collections::VecDeque;

use ndarray::{Array, Array0, Array2, Array3};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

use super::{SpeechSegment, VadEngine, VadFactory};

// The ONNX bytes are embedded at compile time. See assets/README.md
// for how to obtain the file.
const SILERO_ONNX: &[u8] = include_bytes!("../../assets/silero_vad.onnx");

const SAMPLE_RATE: i64 = 16_000;
const FRAME_SAMPLES: usize = 512; // 32 ms @ 16 kHz (Silero v5)
const FRAME_MS: u32 = 32;
const MAX_SPEECH_SAMPLES: usize = 5 * 60 * SAMPLE_RATE as usize;
/// Silero v5 expects each inference to see 64 samples of context from
/// the previous chunk, prepended before the 512 new samples. Without
/// it the model's input-side convolution computes garbage on the
/// leading window and probabilities stay near zero even for loud
/// speech. (For 8 kHz this would be 32 samples; we only run 16 kHz.)
const CONTEXT_SAMPLES: usize = 64;

#[derive(Clone)]
pub struct SileroConfig {
    /// Probability at or above which a frame is considered speech
    /// (used for entering Speech state). Default 0.5.
    pub speech_threshold: f32,

    /// Probability below which a frame is considered silence while
    /// already in Speech state. Must be <= speech_threshold to get
    /// hysteresis (avoid flapping). Default 0.35.
    pub silence_threshold: f32,

    /// Minimum duration of a segment before it can be emitted.
    /// Shorter candidate segments are dropped silently. Default 250.
    pub min_segment_ms: u32,

    /// Silence duration (continuous below speech_threshold) required
    /// to close a segment. Default 800.
    pub end_silence_ms: u32,

    /// Pre- and post-padding (ms) attached to each emitted segment.
    /// Prevents clipping of the first/last phoneme. Default 150.
    pub padding_ms: u32,
}

impl Default for SileroConfig {
    fn default() -> Self {
        Self {
            speech_threshold: 0.5,
            silence_threshold: 0.35,
            min_segment_ms: 250,
            end_silence_ms: 800,
            padding_ms: 150,
        }
    }
}

pub struct SileroFactory {
    config: SileroConfig,
}

impl SileroFactory {
    /// Load the embedded Silero ONNX model.
    ///
    /// This requires `onnxruntime` to be available at runtime (either
    /// a `onnxruntime.dll` alongside the exe, or bundled via the `ort`
    /// crate's `download-binaries` feature).
    ///
    /// We don't store the Session here — the ort 2.x `Session::run`
    /// takes `&mut self`, which `Arc<Session>` (the natural "share
    /// across engines" pattern) can't satisfy. Instead, each call to
    /// `create()` builds a fresh owned Session from the embedded
    /// bytes. That costs ~50 ms per new recording session and uses a
    /// few MB of extra memory while active — negligible for a
    /// hotkey-driven voice-input tool. See DESIGN.md §VAD pitfalls.
    ///
    /// This function does a startup **probe** build so the core fails
    /// loudly (no Ready emitted) if the ONNX bytes are unparseable.
    pub fn load(config: SileroConfig) -> anyhow::Result<Self> {
        let _probe = build_session()?;
        Ok(Self { config })
    }
}

impl VadFactory for SileroFactory {
    fn create(&self) -> Box<dyn VadEngine> {
        match build_session() {
            Ok(session) => Box::new(SileroEngine::new(session, self.config.clone())),
            Err(e) => {
                log::error!("Silero ONNX re-parse failed after startup probe: {e}");
                Box::new(DisabledVadEngine)
            }
        }
    }
}

struct DisabledVadEngine;

impl VadEngine for DisabledVadEngine {
    fn push_pcm(&mut self, _pcm: &[u8]) -> Vec<SpeechSegment> {
        Vec::new()
    }

    fn finish(&mut self) -> Option<SpeechSegment> {
        None
    }
}

/// Build one Silero Session from the embedded ONNX bytes.
///
/// Shared between the startup probe in `SileroFactory::load` and the
/// per-session construction in `SileroFactory::create`.
///
/// Logs the model's declared input/output signatures on first call so
/// we can diagnose tensor-name mismatches (v3/v4/v5 builds differ on
/// names like `state` vs `stateN`, `sr` rank, etc.).
fn build_session() -> anyhow::Result<Session> {
    let session = Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .commit_from_memory(SILERO_ONNX)?;

    // Use a one-shot log so each session-build doesn't spam — we only
    // need to see this once per process. Debug level: quiet by
    // default; easy to enable when diagnosing tensor-name mismatches.
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        log::debug!(
            "Silero model signature: {} input(s), {} output(s)",
            session.inputs.len(),
            session.outputs.len()
        );
        for (i, input) in session.inputs.iter().enumerate() {
            log::debug!(
                "  input[{i}]: name={:?} type={:?}",
                input.name,
                input.input_type
            );
        }
        for (i, output) in session.outputs.iter().enumerate() {
            log::debug!(
                "  output[{i}]: name={:?} type={:?}",
                output.name,
                output.output_type
            );
        }
    });

    Ok(session)
}

enum State {
    Silence,
    Speech {
        /// Samples accumulated for the current speech segment, including
        /// pre-padding that was in the ring when speech started.
        samples: Vec<f32>,
        /// Running silence counter since the last speech-positive frame.
        silence_ms: u32,
    },
}

struct SileroEngine {
    session: Session,
    config: SileroConfig,

    /// LSTM state carried between inference calls.
    state: Array3<f32>,

    /// PCM16-derived f32 samples buffered until we have FRAME_SAMPLES.
    sample_buf: Vec<f32>,

    /// Ring buffer of the last `padding_ms` of silence samples. Used
    /// to prepend audio to a new segment so the first phoneme isn't
    /// clipped.
    pre_buf: VecDeque<f32>,
    pre_buf_capacity: usize,

    state_machine: State,

    /// While in Speech and the current frame is below-threshold, we
    /// accumulate here. If speech resumes, the buffer is flushed back
    /// into `samples`. On segment close, the first `padding_ms` of it
    /// becomes the post-padding and the rest seeds a new pre-buf.
    trailing: Vec<f32>,

    /// Last `CONTEXT_SAMPLES` samples from the previous inference,
    /// prepended before each new 512-sample frame. Required by Silero
    /// v5's input-side conv window.
    context: Vec<f32>,

    // ─── Diagnostics (cheap to track, useful when tuning) ──────────
    frames_processed: u64,
    max_prob: f32,
    sum_prob: f64,
    segments_emitted: u64,
}

impl SileroEngine {
    fn new(session: Session, config: SileroConfig) -> Self {
        let pre_buf_capacity = ms_to_samples(config.padding_ms);
        Self {
            session,
            config,
            state: Array3::zeros((2, 1, 128)),
            sample_buf: Vec::with_capacity(FRAME_SAMPLES * 2),
            pre_buf: VecDeque::with_capacity(pre_buf_capacity),
            pre_buf_capacity,
            state_machine: State::Silence,
            trailing: Vec::new(),
            context: vec![0.0; CONTEXT_SAMPLES],
            frames_processed: 0,
            max_prob: 0.0,
            sum_prob: 0.0,
            segments_emitted: 0,
        }
    }

    fn process_frame(&mut self, frame: &[f32]) -> Option<SpeechSegment> {
        let prob = self.infer(frame).unwrap_or_else(|e| {
            log::warn!("Silero inference failed: {e}; treating frame as silence");
            0.0
        });

        self.frames_processed += 1;
        if prob > self.max_prob {
            self.max_prob = prob;
        }
        self.sum_prob += prob as f64;

        let frame_idx = self.frames_processed;

        // `std::mem::replace` lets us pattern-match on the owned state
        // and produce a new state without tripping the borrow checker.
        match std::mem::replace(&mut self.state_machine, State::Silence) {
            State::Silence => {
                if prob >= self.config.speech_threshold {
                    log::debug!("VAD: silence→speech at frame {frame_idx} (prob={prob:.3})");
                    // Speech onset: seed new segment with pre-padding + frame.
                    let mut samples: Vec<f32> =
                        Vec::with_capacity(self.pre_buf.len() + frame.len());
                    samples.extend(self.pre_buf.drain(..));
                    samples.extend_from_slice(frame);
                    self.state_machine = State::Speech {
                        samples,
                        silence_ms: 0,
                    };
                    None
                } else {
                    extend_ring(&mut self.pre_buf, frame, self.pre_buf_capacity);
                    self.state_machine = State::Silence;
                    None
                }
            }
            State::Speech {
                mut samples,
                mut silence_ms,
            } => {
                if prob >= self.config.silence_threshold {
                    // Still speaking — flush any trailing silence back in.
                    if !self.trailing.is_empty() {
                        samples.append(&mut self.trailing);
                    }
                    samples.extend_from_slice(frame);
                    if samples.len() >= MAX_SPEECH_SAMPLES {
                        log::warn!(
                            "VAD: forcing segment close after {} ms of continuous speech",
                            samples_to_ms(samples.len())
                        );
                        self.state_machine = State::Silence;
                        self.trailing.clear();
                        self.pre_buf.clear();
                        self.segments_emitted += 1;
                        return Some(SpeechSegment {
                            pcm: f32_to_pcm16_bytes(&samples),
                        });
                    }
                    silence_ms = 0;
                    self.state_machine = State::Speech {
                        samples,
                        silence_ms,
                    };
                    None
                } else {
                    // Below speech threshold — accumulate trailing silence.
                    self.trailing.extend_from_slice(frame);
                    silence_ms += FRAME_MS;

                    if silence_ms >= self.config.end_silence_ms {
                        log::debug!(
                            "VAD: speech→silence at frame {frame_idx} (prob={prob:.3}, silence_ms={silence_ms})"
                        );
                        let seg = self.close_segment(samples);
                        if seg.is_some() {
                            self.segments_emitted += 1;
                        }
                        seg
                    } else {
                        self.state_machine = State::Speech {
                            samples,
                            silence_ms,
                        };
                        None
                    }
                }
            }
        }
    }

    /// Close the current speech segment.
    ///
    /// Takes ownership of the accumulated `samples`, appends up to
    /// `padding_ms` of post-pad from `self.trailing`, leaves the rest
    /// of `trailing` as the new pre-buf, and returns the segment (if
    /// it's long enough to pass `min_segment_ms`).
    fn close_segment(&mut self, mut samples: Vec<f32>) -> Option<SpeechSegment> {
        let post_pad = ms_to_samples(self.config.padding_ms).min(self.trailing.len());
        samples.extend_from_slice(&self.trailing[..post_pad]);

        // Whatever's left of trailing seeds the pre-buf. Keep only the
        // last `pre_buf_capacity` samples so we don't overfill.
        let remaining = &self.trailing[post_pad..];
        self.pre_buf.clear();
        let start = remaining.len().saturating_sub(self.pre_buf_capacity);
        for &s in &remaining[start..] {
            self.pre_buf.push_back(s);
        }
        self.trailing.clear();
        self.state_machine = State::Silence;

        if samples_to_ms(samples.len()) >= self.config.min_segment_ms {
            Some(SpeechSegment {
                pcm: f32_to_pcm16_bytes(&samples),
            })
        } else {
            None
        }
    }

    fn infer(&mut self, frame: &[f32]) -> anyhow::Result<f32> {
        // Silero v5: prepend previous-chunk context (64 samples) to
        // the current frame (512 samples) → 576-sample input. The
        // first call sees zero-padded context, which is the Python
        // reference's "initial" condition.
        let full_len = CONTEXT_SAMPLES + frame.len();
        let mut full: Vec<f32> = Vec::with_capacity(full_len);
        full.extend_from_slice(&self.context);
        full.extend_from_slice(frame);
        let input: Array2<f32> = Array::from_shape_vec((1, full_len), full)?;
        let sr: Array0<i64> = ndarray::arr0(SAMPLE_RATE);

        let outputs = self.session.run(ort::inputs! {
            "input" => Tensor::from_array(input)?,
            "state" => Tensor::from_array(self.state.clone())?,
            "sr"    => Tensor::from_array(sr)?,
        })?;

        // Extract probability.
        let (_out_shape, prob_data) = outputs["output"].try_extract_tensor::<f32>()?;
        let prob = prob_data.first().copied().unwrap_or(0.0);

        // Extract updated state. Output name is `stateN` on v5; some
        // repackaged variants call it `state` — adjust if you hit a
        // KeyError at runtime.
        let (state_shape, state_data) = outputs["stateN"].try_extract_tensor::<f32>()?;

        let dims: Vec<usize> = state_shape.iter().map(|&d| d as usize).collect();
        if dims.len() == 3 {
            self.state = Array::from_shape_vec((dims[0], dims[1], dims[2]), state_data.to_vec())?;
        } else {
            anyhow::bail!("unexpected state shape {:?}", dims);
        }

        // Save last 64 samples of this frame as context for next call.
        // Matches the Python reference: `self._context = x[-ctx:]`.
        let tail_start = frame.len().saturating_sub(CONTEXT_SAMPLES);
        self.context.clear();
        self.context.extend_from_slice(&frame[tail_start..]);

        Ok(prob)
    }
}

impl VadEngine for SileroEngine {
    fn push_pcm(&mut self, pcm_bytes: &[u8]) -> Vec<SpeechSegment> {
        let mut out = Vec::new();

        // PCM16 LE → f32 [-1, 1].
        // chunks_exact silently drops an odd trailing byte — not ideal
        // but also shouldn't happen: every source chunk should be an
        // even number of bytes (one sample = 2 bytes).
        for pair in pcm_bytes.chunks_exact(2) {
            let s = i16::from_le_bytes([pair[0], pair[1]]);
            self.sample_buf.push(s as f32 / 32768.0);
        }

        while self.sample_buf.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = self.sample_buf.drain(..FRAME_SAMPLES).collect();
            if let Some(seg) = self.process_frame(&frame) {
                out.push(seg);
            }
        }

        out
    }

    fn finish(&mut self) -> Option<SpeechSegment> {
        // A partial trailing frame (<512 samples) is discarded — not
        // worth running inference on it, and it's at most 31 ms.
        self.sample_buf.clear();

        let tail = match std::mem::replace(&mut self.state_machine, State::Silence) {
            State::Speech { samples, .. } => {
                let seg = self.close_segment(samples);
                if seg.is_some() {
                    self.segments_emitted += 1;
                }
                seg
            }
            State::Silence => None,
        };

        let avg_prob = if self.frames_processed > 0 {
            self.sum_prob / self.frames_processed as f64
        } else {
            0.0
        };
        // Demoted from info → debug once `SessionStats.vad_segments`
        // became the authoritative segment count on the response. The
        // max_prob / avg_prob / threshold bits are still useful for
        // tuning — run with `--log-level debug` to see them.
        log::debug!(
            "VAD summary: frames={} max_prob={:.3} avg_prob={:.3} segments_emitted={} \
             thresholds(speech={:.2}, silence={:.2}, end_ms={}, min_ms={})",
            self.frames_processed,
            self.max_prob,
            avg_prob,
            self.segments_emitted,
            self.config.speech_threshold,
            self.config.silence_threshold,
            self.config.end_silence_ms,
            self.config.min_segment_ms,
        );
        tail
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn ms_to_samples(ms: u32) -> usize {
    (ms as usize * SAMPLE_RATE as usize) / 1000
}

fn samples_to_ms(samples: usize) -> u32 {
    (samples as u64 * 1000 / SAMPLE_RATE as u64) as u32
}

fn extend_ring(ring: &mut VecDeque<f32>, frame: &[f32], cap: usize) {
    if cap == 0 {
        return;
    }
    for &s in frame {
        if ring.len() == cap {
            ring.pop_front();
        }
        ring.push_back(s);
    }
}

fn f32_to_pcm16_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let i = (clamped * 32767.0) as i16;
        out.extend_from_slice(&i.to_le_bytes());
    }
    out
}
