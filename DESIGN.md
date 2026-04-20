# airtalk — Design

Windows voice input tool. Hold a hotkey, speak, release — the cleaned-up
text is pasted into the focused application.

---

## 1. Architecture

Two processes, connected by stdio:

```
┌──────────────────────────┐       stdin        ┌──────────────────────────┐
│                          │ ─────────────────> │                          │
│     airtalk.exe (UI)     │                    │   airtalk-core.exe       │
│                          │ <───────────────── │                          │
│  tray / hotkey /         │       stdout       │  VAD (Silero) /          │
│  overlay / mic / paste   │                    │  ASR (Qwen3) /           │
│                          │       stderr       │  LLM cleanup             │
│                          │ <───────────────── │                          │
└──────────────────────────┘    (logs, opt.)    └──────────────────────────┘
```

Why split?

- **Clean boundary**. Core has zero Win32 dependencies; it is just an
  async pipeline over binary streams.
- **Reusability**. The core can be driven by anything that can spawn a
  child process — batch scripts, tests, future frontends.
- **Fault isolation**. If the core hangs on a network call or VAD
  inference, the UI stays responsive; if the UI crashes, the core
  dies with it (Job Object) and there's no orphan.

Lifetimes:

- UI spawns core on startup and holds it for the whole UI session
- Configuration changes (API keys, model, VAD params) cause the UI to
  kill + respawn core — **the core is stateless across restarts**
- UI attaches core to a Windows Job Object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so core dies when UI dies even
  on hard UI crashes

---

## 2. Protocol

### Wire format — NDJSON

Each message is **compact JSON followed by `\n`** (newline-delimited
JSON):

```
{"type":"begin","id":1,"vad":true}\n
{"type":"chunk","id":1,"pcm":"aGVsbG8..."}\n
{"type":"end","id":1}\n
```

One message per line. Reader does `read_until(b'\n')`, decoder does
`serde_json::from_slice(&line)`. No length prefix — JSON string
escaping guarantees `\n` never appears inside a payload, so the line
break is a reliable record separator.

`MAX_FRAME_SIZE` is 16 MB. Readers enforce this as a post-read cap to
protect against malformed peers; real frames are tens of KB.

Representation rules:

- Enums use an **internal tag** on `"type"`, lowercased — e.g.
  `{"type":"begin","id":1,"vad":true}`. No nested "kind" wrapper.
- Structs / variants serialize as **JSON objects with named fields**;
  cross-language clients (C#, TS, Python) read field-by-name without
  caring about declaration order.
- `Option::None` fields are **omitted** entirely (never emitted as
  `null`), so minimal messages stay small.
- `Chunk.pcm` is a **standard base64 string**, not an array of numbers.
  ~33% size overhead (32 KB/s PCM → ~43 KB/s wire) — trivially
  negligible over localhost stdio. Regression test in `airtalk-proto`
  guards the encoding.
- Session `id` is a JSON number. JS consumers should stay under 2^53;
  a fresh `AtomicU64` counter won't approach that in practice.

Why NDJSON (not MessagePack, not postcard)?

- **Zero-friction debugging**: `tail -f core.stdout`, `grep '"type":"error"'`,
  hand-craft a Begin with `echo`. No custom decoder to write for
  human eyeballs or shell scripts.
- **Universal language support**: every language ships JSON. Base64
  for the one binary field is a two-line helper everywhere.
- **Rust ergonomics unchanged**: still driven by `serde` derives; the
  codec swap is encapsulated in `airtalk-proto`.

Earlier iterations tried postcard (Rust-only) and MessagePack (better,
but a second-class citizen in C# / mobile SDKs). NDJSON is what stuck.

### Messages

Defined in `airtalk-proto/src/lib.rs`:

```rust
enum Request {
    Begin {
        id: u64,
        vad: bool,                     // true → Silero segmentation + concurrent ASR
        context:    Option<String>,    // per-session glossary / domain hints
        language:   Option<String>,    // override asr_lang; "auto" or empty → LID
        enable_itn: Option<bool>,      // override default ITN
        enable_llm: Option<bool>,      // override default LLM cleanup
    },
    Chunk  { id: u64, pcm: Vec<u8> },  // PCM16 LE 16 kHz mono (base64 on wire)
    End    { id: u64 },
    Cancel { id: u64 },
}

enum Response {
    Ready  { protocol_version: u32 },  // once on startup
    Result {
        id: u64,
        text: String,                  // final (LLM-cleaned unless --no-llm)
        raw:      Option<String>,      // ASR text before LLM cleanup
        language: Option<String>,      // language reported by Qwen3-ASR
    },
    Error  { id: u64, message: String },
}
```

Wire examples (one line per message, `\n`-terminated; shown wrapped
for legibility):

```
// UI → core
{"type":"begin","id":1,"vad":true,"context":"DashScope, Qwen-ASR, airtalk","language":"zh"}
{"type":"chunk","id":1,"pcm":"<base64 of PCM16 LE 16k mono>"}
{"type":"end","id":1}
{"type":"cancel","id":1}

// core → UI
{"type":"ready","protocol_version":3}
{"type":"result","id":1,"text":"你好，世界。","raw":"你好世界","language":"zh"}
{"type":"error","id":1,"message":"cancelled"}
```

### Per-session override semantics

`Begin` optional fields override core's startup CLI defaults. Omit a
field (wire: absent key) to fall back to the CLI default.

| Field          | CLI default flag            | `None` means…                        |
| -------------- | --------------------------- | ------------------------------------ |
| `context`      | `--hotwords-file`           | use startup hotwords alone           |
| `language`     | `--asr-lang`                | use CLI default; `"auto"` / `""` → LID |
| `enable_itn`   | `--asr-enable-itn`          | use CLI default                      |
| `enable_llm`   | (negation of) `--no-llm`    | use core default                     |

When both startup hotwords AND per-session `context` are present, core
joins them: `<hotwords_csv>\n<per_session_context>`. The joined string
is sent as Qwen3-ASR's system message **and** appended to the LLM
cleanup prompt as a `Context:` block (homophones and proper nouns
benefit from the same hints on both sides of the pipeline).

`enable_llm` interaction with core's `--no-llm` flag:

| core (`--no-llm`) | `Begin.enable_llm` | Effective     |
| ----------------- | ------------------ | ------------- |
| off (LLM on)      | `None`             | clean         |
| off (LLM on)      | `Some(true)`       | clean         |
| off (LLM on)      | `Some(false)`      | raw this call |
| on (LLM off)      | `None`             | raw           |
| on (LLM off)      | `Some(false)`      | raw           |
| on (LLM off)      | `Some(true)`       | raw (**silent downgrade**, warn-logged) |

Rationale for the silent downgrade in the last row: the UI treats LLM
cleanup as an optional optimization, not a hard requirement. Returning
a `Result` with `text == raw` is strictly more useful than an
`llm_disabled` error — the user still gets their transcription.

### Contract

1. **Startup handshake**. Core emits `Response::Ready` **exactly once**,
   only after all initialization (model load, HTTP clients, CLI parse)
   has succeeded. UI must receive it within **5 s** or treat core as
   failed (kill, surface error to user). `protocol_version` currently
   `3`; UI should refuse to talk to a core with a version it doesn't
   implement.

2. **Session id is monotonic**. UI assigns `id` from an `AtomicU64`,
   never reuses. Core echoes it back in every response.

3. **Exactly one terminal response per `Begin`**. Either `Result` or
   `Error`. No partial results, no progress events.

4. **Preemption**. A new `Begin` while a session is in flight causes
   core to send `Error { id: old, message: "superseded" }` and cancel
   the old pipeline before starting the new one.

5. **`Cancel` always replies**. `Error { id, message: "cancelled" }`.

6. **Stale messages are silently dropped**. Chunks/End/Cancel for an
   id that isn't current → ignored on the core side; Results/Errors
   for an id the UI has already finalized → ignored on the UI side.

7. **Audio format is fixed**: PCM16 LE, 16 kHz, mono. UI resamples.
   Recommended chunk size: 20–50 ms (640–1600 bytes).

### Known `Error` messages

| `message`              | Meaning                                         |
| ---------------------- | ----------------------------------------------- |
| `cancelled`            | UI sent Cancel                                  |
| `superseded`           | Newer Begin preempted this session              |
| `no_audio`             | Session ended without any detected speech       |
| `timeout`              | ASR or LLM HTTP request timed out               |
| `asr_failed: <detail>` | Upstream ASR error                              |
| `llm_failed: <detail>` | Upstream LLM error                              |

Well-known values are re-exported as constants in
`airtalk_proto::error_code` so UI code can match them without scattering
string literals. Anything beginning with `asr_failed:` / `llm_failed:`
carries a free-form upstream message — match with `starts_with`, not
equality.

---

## 3. Core internals

### Module layout

```
airtalk-core/src/
  main.rs           CLI parse, logging, top-level stdio loop, Ready
  session.rs        Actor + pipeline (the heart of the core)
  config.rs         CoreConfig struct
  prompt.rs         LLM system prompt loader
  default_prompt.txt
  asr/
    mod.rs          AsrProvider trait
    qwen.rs         Qwen3-ASR DashScope client (stub)
  llm/
    mod.rs          LlmProvider trait
    openai.rs       OpenAI-compatible chat client (stub)
  vad/
    mod.rs          VadEngine / VadFactory traits
    silero.rs       Silero v5 via onnxruntime
```

### Session actor

One `tokio::spawn`-ed task owns the "current session" state. It
receives an `ActorCmd` stream and spawns a per-session pipeline task.

```
         stdin reader
              │ Request
              ▼
         session actor
         (holds Option<ActiveSession>)
              │
              │ spawns pipeline on Begin
              │ cancels pipeline on Cancel/Supersede
              ▼
        pipeline task
         ┌──────────┐
         │ audio_rx │   (from actor via mpsc, closed on End)
         └────┬─────┘
              ▼
         Silero VAD (or passthrough if vad=false)
              │
              ▼
         per-segment ASR, concurrent (Semaphore + JoinSet)
              │
              ▼
         BTreeMap<seq, text> → join " "
              │
              ▼
         LLM cleanup (if enabled)
              │
              ▼
         Response::Result → stdout writer
```

Key decisions:

- **Single actor, single channel**. The actor receives both external
  `Request`s and internal `PipelineDone` signals through the same
  `ActorCmd` channel. This keeps the state machine serial and avoids
  locks.

- **Pipeline runs independently**. Once spawned, a pipeline holds its
  own cancellation token and clones of the providers. The actor does
  not await the pipeline — it moves on to accept more Requests even
  while an old session's LLM call is pending.

- **End via channel close**. `End` drops the actor's end of the audio
  mpsc; the pipeline sees `recv() → None` and proceeds to finalize.
  No separate "finish" message needed.

### Cancellation

Every session gets a fresh `CancellationToken`. The pipeline has
`tokio::select!` branches with `biased;` at every await point, each
including `_ = cancel.cancelled() => return Err(Cancelled)`:

```rust
tokio::select! {
    biased;
    _ = cancel.cancelled() => return Err(Cancelled),
    recv = audio_rx.recv() => { ... }
}
```

- **`biased;` is required.** Without it, tokio randomly chooses among
  ready branches; cancel can be delayed by a round while we process
  more audio / wait for ASR response.
- **Actor sends the terminal Error for cancel/supersede.** The
  pipeline observes the token, exits silently, and only sends
  `PipelineDone` so the actor can clear `current`. This avoids a race
  between "pipeline naturally finished with Result" and "actor wants
  to send cancelled Error" ending up with two terminal messages on
  the wire.

### Concurrency and ordering

`vad=true` spawns N concurrent ASR tasks per session (bounded by
`Semaphore::new(asr_concurrency)`). Each segment gets a monotonic
`seq`. Results are collected into `BTreeMap<u64, String>` — when
phase 2 ends, iterating the BTreeMap in key order reassembles them
in speaking order regardless of completion order.

`JoinSet` is used instead of `FuturesUnordered`. Reason: JoinSet
surfaces panicked tasks as `Err(JoinError)` out of `join_next`, while
FuturesUnordered lets panics propagate to the runtime. A single flaky
ASR response is not a reason to kill the whole core.

---

## 4. VAD (Silero)

### Model

v5 ONNX, input shape `[1, 512]` f32 (32 ms @ 16 kHz), LSTM state
`[2, 1, 128]`, plus a scalar sample-rate tensor. Output: single
speech probability in `[0, 1]`.

The file is **not committed**. See `airtalk-core/assets/README.md`
for the download URL. It is embedded at compile time via
`include_bytes!`, so the shipped binary is self-contained — but the
source tree must have the file present to build.

`build.rs` sets `cargo:rerun-if-changed=assets/silero_vad.onnx` so
swapping the model triggers a rebuild.

### Algorithm

1. Convert PCM16 bytes → f32 `[-1, 1]`, buffer until 512 samples.
2. Per frame, run ort inference → probability.
3. Hysteresis state machine (see `SileroEngine::process_frame`):
   - **Silence → Speech** when `prob ≥ speech_threshold` (default 0.5)
   - **Speech → Silence** when `prob < silence_threshold` (default
     0.35, strictly less than speech_threshold) for a continuous
     `end_silence_ms` (default 800)
4. Pre-padding: a `VecDeque<f32>` ring buffer of the last `padding_ms`
   of silence samples is prepended to a segment on speech onset, so
   the first phoneme isn't clipped.
5. Post-padding: up to `padding_ms` from the trailing-silence buffer
   is appended on segment close; the rest seeds the next pre-buf.
6. Segments shorter than `min_segment_ms` (default 250) are dropped.

### VAD pitfalls

- **Silero v5 needs 64 samples of prior-chunk context prepended to
  each inference.** The Python reference implementation does
  `x = torch.cat([self._context, x], dim=1)` before calling the
  model; `self._context = x[..., -64:]` afterwards. Without this the
  input-side convolution computes garbage on the leading window and
  probabilities stay near zero on real speech. The declared input
  shape is `[-1, -1]` (fully dynamic) so feeding plain 512-sample
  frames doesn't error — it just silently misbehaves. `SileroEngine`
  carries a 64-sample `context` buffer initialized to zero; each
  `infer` call prepends it to form a 576-sample input, then saves
  the last 64 samples of the new frame as next-call context.
- **`sr` must be a 0-d scalar tensor (shape `[]`), not rank-1 `[1]`.**
  ort 2.x accepts rank-1 silently but the model misinterprets the
  sample rate. Use `ndarray::arr0(SAMPLE_RATE)`.
- **Tensor names and shapes are version-specific.** Silero v5 uses
  `input` / `state` / `sr` / `output` / `stateN`. v3 and v4 differ.
  Inspect with `netron` and update `SileroEngine::infer` if you ship
  a different model.
- **LSTM state must persist across calls.** Resetting to zero each
  frame kills accuracy. `state` lives on the engine, re-read from
  every `outputs["stateN"]`.
- **The `ort` API changes between minor versions.** 2.0 and 2.1
  differ on `Tensor::from_array` vs `Value::from_array`, on the
  `inputs!` macro signature, and on `try_extract_tensor`'s return
  shape. Code targets ort 2.x as of writing; pin the version if
  you need stability.
- **Thresholds should stay separated.** `silence_threshold <
  speech_threshold` gives hysteresis. Setting them equal causes
  flapping at the boundary and produces fragment segments.
- **Aggressive `end_silence_ms` (<400) fragments sentences.** Users
  pause to think; 700–900 ms is the sweet spot empirically.

---

## 5. ASR (Qwen3-ASR)

Non-streaming DashScope REST API. `asr::qwen::QwenAsr` is currently a
stub (builds HTTP client, `transcribe` returns an error).

**Request format caveats:**

- DashScope's Qwen3-ASR schema changes between API versions. Verify
  against current docs at implementation time; the stub has a sketch
  of the fields.
- Audio encoding: typically base64-encoded WAV or raw PCM16, specified
  by a `format` parameter. Check docs.
- The `hotwords` parameter name and shape is not stable across the
  various Qwen ASR surfaces — check carefully.
- Response shape typically includes `output.sentence[].text`; return
  the concatenation.

Wrap errors with `anyhow::Context` so `PipelineError::AsrFailed`
surfaces the upstream message, not just "request failed".

---

## 6. LLM cleanup

OpenAI Chat Completions-compatible. Works against DashScope's
compatible mode, DeepSeek, Moonshot, OpenRouter, and OpenAI itself.

Provider quirks:

- Some providers accept `max_tokens`; OpenAI's newest models require
  `max_completion_tokens`. Configured via `--llm-max-token-param`.
- Temperature should be low (0.2 default). The cleanup task should be
  near-deterministic.
- `stream: false` — we emit exactly one Result anyway, no need for SSE.

Default system prompt is in `src/default_prompt.txt` (embedded via
`include_str!`). Override via `--llm-prompt-file`.

With `--no-llm`, the ASR text is emitted verbatim as `Result.text`.
Useful for batch pipelines that want raw transcription or that will
do their own post-processing.

---

## 7. UI (airtalk.exe)

This crate is **a stub**. Planned components:

- **Tray icon**: `Shell_NotifyIconW` + popup menu (Settings, Quit,
  Restart core).
- **Global hotkey**: `SetWindowsHookExW(WH_KEYBOARD_LL)` on a
  dedicated thread with its own message pump.
  - Supports modifier-only taps (Alt, Ctrl, Shift).
  - `RegisterHotKey` doesn't handle modifier-only; don't try.
  - **Alt specifically**: return non-zero from the hook to eat the
    event, or pressing Alt will activate the window menu (F10
    behavior).
- **Overlay window**: WPF-style layered window with
  `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOPMOST`.
  Shows recording/processing state and mic level.
- **Audio capture**: `cpal` in a dedicated thread, running
  continuously once initialized (avoids device warm-up latency per
  session). A gate flag controls whether frames are forwarded.
  Resample to 16 kHz mono PCM16 LE before sending to core.
- **Paste**: two strategies, configurable.
  - `clipboard` (default): `SetClipboardData(CF_UNICODETEXT)` +
    `SendInput(Ctrl+V)`. Back up/restore old clipboard around it.
  - `sendinput`: SendInput unicode keystrokes directly. Slower
    (20–50 ms/char) but works in UWP and elevated windows where
    Ctrl+V gets filtered.
- **Settings**: writes `%APPDATA%\airtalk\config.toml`; save triggers
  core respawn with new CLI args + env vars.
- **core_client**: `tokio::process::Command` with piped stdin/stdout.
  Use `airtalk_proto::{read_frame_async, write_frame_async}`. On
  Windows, attach the child PID to a Job Object; see
  `windows::Win32::System::JobObjects`.

Expected config layout:

```toml
[llm]
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
model = "qwen-plus"
api_key = ""                    # → AIRTALK_LLM_API_KEY env
max_token_param = "max_tokens"
disable = false
prompt_file = ""

[asr]
api_key = ""                    # → AIRTALK_ASR_API_KEY env
lang = "zh"
hotwords_file = ""

[vad]
silence_ms = 800
min_segment_ms = 250
padding_ms = 150
asr_concurrency = 2

[hotkey]
modifier = "Alt"
mode = "hold"                   # hold | tap

[audio]
device = "default"

[paste]
strategy = "clipboard"          # clipboard | sendinput

[vad]
enabled = true
```

---

## 8. Gotchas (read before editing)

Collected here because they're easy to get wrong and hard to debug:

1. **Never emit `Response::Ready` before all init succeeds.** If
   Silero fails to load, exit with a stderr message; don't announce
   Ready and then crash on first Begin. UI times out cleanly on no
   Ready; it does not handle "Ready then dead" gracefully.

2. **Every `tokio::select!` branch needs `biased;` + cancel check.**
   Grep for `tokio::select!` in `session.rs` — each one has
   `_ = cancel.cancelled()` as the first branch. Miss one and
   preempted sessions can wastefully complete their ASR/LLM calls
   (or worse, send a stale `Result` after actor already sent
   `superseded` Error).

3. **`superseded` / `cancelled` Errors come from the actor, not the
   pipeline.** If you add a code path that emits terminal responses
   directly from the pipeline, guard it with `if !cancel.is_cancelled()`
   or you'll double-post.

4. **Audio channel capacity 128 × 30 ms ≈ 3.8 s.** `try_send` failure
   indicates VAD/ASR stuck, not normal load. Log at `warn`, don't
   promote to an error.

5. **`JoinSet` over `FuturesUnordered`.** A panic in one ASR task
   becomes a `JoinError` we can surface as `asr_failed: join error`,
   rather than blowing up the entire core.

6. **Silero LSTM state MUST persist across frames.** `infer` reads
   `outputs["stateN"]` back into `self.state`. If you skip that step,
   the model behaves like it's seeing the first 32 ms forever and
   triggers on any energy blip.

7. **Silero tensor names differ by version.** v5 uses `input`,
   `state`, `sr`, `output`, `stateN`. Older versions or repackaged
   variants may use `state_in`/`state_out`, etc. If you hit a
   `KeyError: stateN` at runtime, inspect your `.onnx` with `netron`.

8. **Frame size is NOT flexible.** Silero v5 expects exactly 512
   samples @ 16 kHz. Passing 480 or 1024 produces garbage probabilities
   (no shape error — just wrong output).

9. **PCM conversion is asymmetric.** Reading: divide by 32768.
   Writing: multiply by 32767 and clamp. This matches the i16 range
   `[-32768, 32767]` and avoids overflow on the positive side.

10. **Core-level config changes require a core restart.** Per-session
    knobs (`language`, `context`, `enable_itn`, `enable_llm`) can be
    overridden via `Begin` fields without restart. But anything set
    once at startup (ASR / LLM endpoints, VAD thresholds, hotwords
    file) still needs a respawn. Core is stateless across restarts.

11. **ONNX Runtime is bundled via `ort/download-binaries`.** Cargo
    feature set in `airtalk-core/Cargo.toml`. Produces a standalone
    ~25 MB binary. Don't switch back to `load-dynamic` without
    shipping a version-matched `onnxruntime.dll` alongside — ort
    2.0-rc.10 panics at startup if it finds an older DLL on PATH
    (wants ONNX Runtime 1.22.x; old 1.17.x DLLs from Python installs
    WILL get picked up and crash the process).

12. **Hotkey must be in UI, not core.** The low-level keyboard hook
    needs a message pump, and it must run in the process that owns
    the user-interactive session. Don't try to push this to core via
    RPC — the keyboard hook callback is synchronous and can't tolerate
    a round-trip to another process.

13. **Windows Alt key has menu-activation side effects.** Even with
    WH_KEYBOARD_LL, single-tapping Alt makes Windows flash the window
    menu (F10 behavior). The hook callback should return non-zero to
    eat the key event.

14. **Job Object attach must happen BEFORE resume.** When spawning
    core on Windows, create the process suspended (`CREATE_SUSPENDED`),
    assign it to the job object with `AssignProcessToJobObject`, then
    call `ResumeThread`. If you let it run first, there's a small
    window where `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` doesn't apply
    and a quick UI crash can orphan the core.

15. **Silero v5 needs 64-sample context prepended to every inference.**
    `SileroEngine` carries a 64-sample buffer (`self.context`) of the
    tail of the previous frame; `infer` builds a 576-sample input
    (64 + 512). **Without this, probabilities stay near 0.0005 on
    any input, including loud speech** — the input-side conv window
    reads uninitialized/zero context and produces garbage. The
    declared input shape is `[-1, -1]` so ort accepts bare 512-sample
    input silently. Symptom: "VAD never triggers on real audio". If
    you see all-flat probs, check this first.

16. **`sr` must be a 0-d scalar tensor (shape `[]`), not rank-1 `[1]`.**
    Use `ndarray::arr0(SAMPLE_RATE)`. ort 2.x silently accepts rank-1
    but the model misinterprets sample rate and produces constant
    ~0.0005 output. Same "silent garbage" failure mode as #15.

17. **Session actor needs an explicit `Shutdown` command on EOF.** The
    actor holds its own `cmd_tx` clone (pipelines route `PipelineDone`
    back via the shared channel), so `cmd_rx` never returns `None`
    when the external handle drops. `main.rs` MUST call
    `session_handle.shutdown()` after stdin EOF — without it the
    process hangs at `stdout_task.await` with a Result already
    emitted. Current flow: `shutdown()` flips a flag, current
    pipeline finishes naturally, actor exits when `shutting_down &&
    current.is_none()`.

---

## 9. Implementation status

| Component                         | Status                                             |
| --------------------------------- | -------------------------------------------------- |
| `airtalk-proto` (NDJSON v3)       | ✅ Complete, 11 unit tests                         |
| `airtalk-core` main / CLI / stdio | ✅ Complete, graceful shutdown                     |
| Session actor + pipeline          | ✅ Complete, per-session overrides + timeout code  |
| Silero VAD                        | ✅ Complete, verified end-to-end on real audio     |
| Qwen3-ASR client                  | ✅ Complete (WAV base64 data URI, 4 unit tests)    |
| OpenAI-compatible LLM client      | ✅ Complete (dynamic max-token param)              |
| Integration tests                 | ✅ 9 tests, wiremock-based, in `tests/integration` |
| testkit (Python dev tool)         | ✅ 16 TTS cases + streaming miccheck               |
| UI (tray / hotkey / overlay / …)  | 🟥 Not started                                     |

Next steps:

1. **UI (`airtalk.exe`)**, build incrementally in this order:
   - `core_client` — spawn core, NDJSON framing, Job Object attach,
     lifecycle. Simplest first deliverable; proves stdio roundtrip.
   - `audio_capture` — cpal input stream, 16 kHz mono PCM16 resample,
     always-on + gate flag (avoids per-session warm-up latency).
   - `hotkey` — WH_KEYBOARD_LL, hold/tap modes, eat Alt to dodge F10
     menu activation.
   - `overlay` — layered + transparent + noactivate + topmost window
     showing recording state + mic level.
   - `paste` — `SetClipboardData` + `SendInput(Ctrl+V)` default;
     SendInput unicode keystroke fallback for elevated / UWP apps.
   - `tray` + `settings` — `Shell_NotifyIconW`, config TOML in
     `%APPDATA%\airtalk\`.
2. **(Optional)** Once UI captures real recordings, use the testkit
   case-diff pattern to calibrate VAD thresholds / `end_silence_ms`
   against actual user speech rather than TTS output.

---

## 10. Build & run

```bash
# `silero_vad.onnx` is committed under airtalk-core/assets/; just:
cargo build --workspace

# Minimal run (ASR only, no LLM — skips the three --llm-* flags):
AIRTALK_ASR_API_KEY=sk-... \
  ./target/debug/airtalk-core --no-llm

# Full pipeline (ASR + LLM cleanup):
AIRTALK_ASR_API_KEY=sk-... AIRTALK_LLM_API_KEY=sk-... \
  ./target/debug/airtalk-core \
    --llm-base-url https://dashscope.aliyuncs.com/compatible-mode/v1 \
    --llm-model qwen-plus

# Drive it by hand — NDJSON makes this trivial:
printf '%s\n' '{"type":"begin","id":1,"vad":false}'
# ... send chunks; for base64 PCM use `base64 -w0 sample.pcm` into
# the "pcm" field. Stdout is also line-oriented.

# Interactive mic test (streams audio while recording, measures
# perceived latency from End→Result):
cd tools/testkit
AIRTALK_ASR_API_KEY=sk-... AIRTALK_LLM_API_KEY=sk-... \
  uv run testkit miccheck
```

Run the tests:

```bash
cargo test --workspace               # all unit + integration tests
cargo test -p airtalk-proto          # proto roundtrip (11 tests)
cargo test -p airtalk-core --test integration   # end-to-end (9 tests)

# Python synthesis-based regression suite:
cd tools/testkit && uv run testkit run-all cases/
```
