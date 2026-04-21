//! Always-on microphone capture with a gated forwarder to core.
//!
//! `cpal::Stream` is not `Send` on Windows (it owns thread-local WASAPI
//! state), so the stream lives in a dedicated std thread spawned by
//! [`AudioCapture::start`]. The thread:
//!
//!   1. Opens the requested input device (default when
//!      [`DeviceChoice::Auto`], by name otherwise) and keeps the stream
//!      running forever (no start/stop per session — that path has a
//!      ~1 s warm-up penalty on USB mics).
//!   2. On each cpal callback: downmix to mono, compute RMS (→ atomic
//!      for overlay waveform viz), and — if the atomic gate is open —
//!      resample to 16 kHz PCM16 LE and forward 30 ms chunks into the
//!      mpsc consumed by the UI loop.
//!   3. When the gate is closed, the resampler's state is reset and the
//!      pending-output buffer is cleared so the next open starts at a
//!      clean zero-phase.
//!   4. Blocks on a control channel between streams. [`switch_to`] posts
//!      a message; the thread drops the current stream, rebuilds on the
//!      new device, and resumes. The same `AudioCapture` handle survives
//!      the switch — the pcm_rx is shared across stream lifetimes.
//!
//! Resampling is linear interpolation — adequate for 16 kHz speech ASR.
//! A one-sample history (`prev_sample`) carries across cpal callbacks so
//! the interpolated boundary is continuous; `next_out_pos` preserves the
//! fractional output phase.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use tokio::sync::mpsc;

/// Target PCM spec sent to core. See DESIGN.md §Protocol.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;
/// 30 ms @ 16 kHz = 480 samples.
pub const CHUNK_SAMPLES: usize = 480;
pub const CHUNK_BYTES: usize = CHUNK_SAMPLES * 2;

/// Input device selection. `Auto` follows the OS default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceChoice {
    Auto,
    Named(String),
}


enum ControlMsg {
    SwitchDevice(DeviceChoice),
}

/// Handle to a running capture stream. Drop to stop capture.
#[allow(dead_code)] // most methods exposed for hotkey / overlay / tray wiring
pub struct AudioCapture {
    gate: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    pcm_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    control_tx: std::sync::mpsc::Sender<ControlMsg>,
    device_name: Arc<Mutex<String>>,
    _thread: JoinHandle<()>,
}

#[allow(dead_code)]
impl AudioCapture {
    /// Start capture on the requested device. Blocks briefly (≤ 3 s) while
    /// the capture thread builds its first stream so failures surface here.
    pub fn start(choice: DeviceChoice) -> Result<Self> {
        let gate = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let device_name = Arc::new(Mutex::new(String::new()));
        let (pcm_tx, pcm_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = std::sync::mpsc::channel::<ControlMsg>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<()>>();

        let gate_c = gate.clone();
        let level_c = level.clone();
        let device_name_c = device_name.clone();
        let thread = std::thread::Builder::new()
            .name("airtalk-audio".into())
            .spawn(move || {
                run_capture_thread(
                    choice,
                    gate_c,
                    level_c,
                    pcm_tx,
                    control_rx,
                    init_tx,
                    device_name_c,
                );
            })
            .context("spawn audio capture thread")?;

        init_rx
            .recv_timeout(Duration::from_secs(3))
            .context("audio thread init timeout (3 s)")?
            .context("audio thread init error")?;

        Ok(Self {
            gate,
            level,
            pcm_rx,
            control_tx,
            device_name,
            _thread: thread,
        })
    }

    /// Human-readable name of the currently-open input device. Returns
    /// `"<unavailable: …>"` if the last rebuild couldn't open a device.
    pub fn device_name(&self) -> String {
        self.device_name.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Shared handle to the current RMS level atomic. Clone for the
    /// overlay thread so it can read the waveform without going through
    /// a channel.
    pub fn level_source(&self) -> Arc<AtomicU32> {
        self.level.clone()
    }

    pub fn open_gate(&self) {
        self.gate.store(true, Ordering::Release);
    }

    pub fn close_gate(&self) {
        self.gate.store(false, Ordering::Release);
    }

    pub fn is_open(&self) -> bool {
        self.gate.load(Ordering::Acquire)
    }

    /// Current RMS level in [0.0, 1.0].
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Acquire))
    }

    /// Next 30 ms PCM16 LE chunk. Returns `None` when the capture thread
    /// has exited. Only produces data while the gate is open.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.pcm_rx.recv().await
    }

    /// Drain any chunks already queued from a previous gate-open cycle.
    /// Useful after `close_gate`: the audio thread may have pushed one or
    /// two chunks that raced the gate observation, and those would otherwise
    /// leak into the next session. Returns the count dropped.
    pub fn drain_pending(&mut self) -> usize {
        let mut n = 0;
        while self.pcm_rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    /// Request the capture thread to tear down its current cpal stream
    /// and open `choice` instead. Returns immediately; rebuild happens
    /// asynchronously on the audio thread (a new stream typically comes
    /// up within 50–200 ms). Observe `device_name()` to see when the
    /// switch lands.
    pub fn switch_to(&self, choice: DeviceChoice) {
        let _ = self.control_tx.send(ControlMsg::SwitchDevice(choice));
    }
}

/// Enumerate all input devices cpal's default host reports. Called fresh
/// every time the tray menu opens so hot-plugged mics appear without any
/// polling loop on our side.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(iter) => iter.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Placeholder name used when no device is currently open. Callers
/// (tray / settings) can spot it with `device_name().starts_with("<")`.
const NO_DEVICE_NAME: &str = "<no input device>";

fn run_capture_thread(
    initial: DeviceChoice,
    gate: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    control_rx: std::sync::mpsc::Receiver<ControlMsg>,
    init_tx: std::sync::mpsc::Sender<Result<()>>,
    device_name: Arc<Mutex<String>>,
) {
    let mut choice = initial;
    let mut announced_init = false;

    loop {
        // Try the requested choice first; if it's a `Named` device that
        // isn't plugged in, fall back to `Auto` before giving up. This is
        // what lets a user's preferred mic vanish without taking the app
        // down with it — they get their default back on startup.
        let mut attempts: Vec<DeviceChoice> = vec![choice.clone()];
        if matches!(choice, DeviceChoice::Named(_)) {
            attempts.push(DeviceChoice::Auto);
        }

        let mut opened: Option<(cpal::Stream, String)> = None;
        for attempt in &attempts {
            match build_stream(attempt, gate.clone(), level.clone(), tx.clone()) {
                Ok(result) => {
                    if attempt != &choice {
                        log::warn!(
                            "preferred device unavailable ({:?}); using {:?}",
                            choice, attempt
                        );
                    }
                    opened = Some(result);
                    break;
                }
                Err(e) => log::warn!("audio build failed for {:?}: {e}", attempt),
            }
        }

        match opened {
            Some((stream, resolved_name)) => {
                if let Ok(mut guard) = device_name.lock() {
                    *guard = resolved_name.clone();
                }
                log::info!("audio: opened \"{resolved_name}\"");
                if !announced_init {
                    let _ = init_tx.send(Ok(()));
                    announced_init = true;
                }
                // Block until the UI asks us to switch, then drop the
                // stream (stop capture) and loop to rebuild.
                match control_rx.recv() {
                    Ok(ControlMsg::SwitchDevice(new)) => {
                        drop(stream);
                        choice = new;
                    }
                    Err(_) => return,
                }
            }
            None => {
                // All attempts failed — no cpal device available right now.
                // Stay alive so the user can plug something in and pick it
                // from the tray; `recv` will be noise-less while idle.
                if let Ok(mut guard) = device_name.lock() {
                    *guard = NO_DEVICE_NAME.into();
                }
                log::error!("no audio device available; capture idle until user picks one");
                if !announced_init {
                    // Still allow the UI to come up — recording just
                    // won't produce chunks until a device is picked.
                    let _ = init_tx.send(Ok(()));
                    announced_init = true;
                }
                match control_rx.recv() {
                    Ok(ControlMsg::SwitchDevice(new)) => choice = new,
                    Err(_) => return,
                }
            }
        }
    }
}

fn build_stream(
    choice: &DeviceChoice,
    gate: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
) -> Result<(cpal::Stream, String)> {
    let host = cpal::default_host();
    let device = match choice {
        DeviceChoice::Auto => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?,
        DeviceChoice::Named(name) => host
            .input_devices()
            .context("host.input_devices")?
            .find(|d| d.name().as_deref().ok() == Some(name.as_str()))
            .ok_or_else(|| anyhow!("input device not found: {name}"))?,
    };
    let name = device.name().unwrap_or_else(|_| "<unknown>".into());

    let supported = device
        .default_input_config()
        .context("default_input_config")?;
    let sample_format = supported.sample_format();
    let stream_config: StreamConfig = supported.clone().into();
    let device_sr = stream_config.sample_rate.0;
    let device_channels = stream_config.channels as usize;
    log::info!("audio: {name} @ {device_sr} Hz, {device_channels} ch, {sample_format:?}");

    let mut state = CaptureState::new(device_sr, device_channels);
    let err_fn = |e| log::error!("audio stream error: {e}");

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _| state.process_f32(data, &gate, &level, &tx),
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _| state.process_i16(data, &gate, &level, &tx),
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _| state.process_u16(data, &gate, &level, &tx),
            err_fn,
            None,
        ),
        other => anyhow::bail!("unsupported sample format: {other:?}"),
    }
    .context("build_input_stream")?;

    stream.play().context("stream.play")?;
    Ok((stream, name))
}

struct CaptureState {
    device_channels: usize,
    ratio: f64,
    /// Last mono sample of the previous callback — prepended to the next
    /// callback's virtual source buffer so linear interpolation stays
    /// continuous across the callback boundary.
    prev_sample: f32,
    /// Fractional output position, relative to the current callback's
    /// source buffer. Starts at 0, advances by `ratio` per emitted
    /// output sample, and wraps by subtracting callback length at the
    /// end of each callback.
    next_out_pos: f64,
    /// PCM16 LE bytes not yet emitted as a 30ms chunk.
    out_buf: Vec<u8>,
}

impl CaptureState {
    fn new(device_sr: u32, device_channels: usize) -> Self {
        Self {
            device_channels,
            ratio: device_sr as f64 / TARGET_SAMPLE_RATE as f64,
            prev_sample: 0.0,
            next_out_pos: 0.0,
            out_buf: Vec::with_capacity(CHUNK_BYTES * 2),
        }
    }

    fn process_f32(
        &mut self,
        data: &[f32],
        gate: &AtomicBool,
        level: &AtomicU32,
        tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) {
        self.process(data.iter().copied(), data.len(), gate, level, tx);
    }

    fn process_i16(
        &mut self,
        data: &[i16],
        gate: &AtomicBool,
        level: &AtomicU32,
        tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let iter = data.iter().map(|&s| s as f32 / 32768.0);
        self.process(iter, data.len(), gate, level, tx);
    }

    fn process_u16(
        &mut self,
        data: &[u16],
        gate: &AtomicBool,
        level: &AtomicU32,
        tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let iter = data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0);
        self.process(iter, data.len(), gate, level, tx);
    }

    fn process<I: Iterator<Item = f32>>(
        &mut self,
        samples: I,
        total_len: usize,
        gate: &AtomicBool,
        level: &AtomicU32,
        tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) {
        if total_len == 0 {
            return;
        }
        let channels = self.device_channels.max(1);
        let frames = total_len / channels;
        if frames == 0 {
            return;
        }

        let mut mono: Vec<f32> = Vec::with_capacity(frames);
        let mut buf: Vec<f32> = Vec::with_capacity(channels);
        for s in samples {
            buf.push(s);
            if buf.len() == channels {
                let avg = buf.iter().sum::<f32>() / channels as f32;
                mono.push(avg);
                buf.clear();
            }
        }

        let sum_sq: f32 = mono.iter().map(|&s| s * s).sum();
        let rms = (sum_sq / mono.len() as f32).sqrt();
        level.store(rms.to_bits(), Ordering::Release);

        if !gate.load(Ordering::Acquire) {
            if let Some(&last) = mono.last() {
                self.prev_sample = last;
            }
            self.next_out_pos = 0.0;
            self.out_buf.clear();
            return;
        }

        let n = mono.len();
        while self.next_out_pos < n as f64 {
            let p = self.next_out_pos;
            let i = p.floor() as usize;
            let f = (p - i as f64) as f32;
            let a = if i == 0 { self.prev_sample } else { mono[i - 1] };
            let b = mono[i];
            let s = a * (1.0 - f) + b * f;

            let pcm = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.out_buf.extend_from_slice(&pcm.to_le_bytes());

            self.next_out_pos += self.ratio;
        }
        self.prev_sample = mono[n - 1];
        self.next_out_pos -= n as f64;

        while self.out_buf.len() >= CHUNK_BYTES {
            let chunk: Vec<u8> = self.out_buf.drain(..CHUNK_BYTES).collect();
            if tx.send(chunk).is_err() {
                return;
            }
        }
    }
}

#[allow(dead_code)]
fn _assert_send()
where
    AudioCapture: Send,
{
}
