# airtalk testkit

Black-box audio test harness for `airtalk-core`. Generates synthetic
speech via Edge TTS, optionally overlays noise, feeds the result
through core's NDJSON protocol, and asserts on the returned text.

## Quick start

```bash
# 1. Install Python deps into a local .venv (once, and after pyproject
#    changes). Uses uv — https://github.com/astral-sh/uv
cd tools/testkit
uv sync

# 2. Make sure ffmpeg is on PATH (pydub shells out to it for MP3 decode).
#    Windows: `winget install Gyan.FFmpeg.Essentials`
#    macOS:   `brew install ffmpeg`

# 3. Export your DashScope key — core calls real ASR end-to-end. LLM
#    stage is skipped in the default cases (`no_llm: true`) because
#    LLM output is non-deterministic; testkit asserts on raw ASR.
export AIRTALK_ASR_API_KEY=sk-your-key

# 4. Build core (debug is the default binary testkit looks for)
cd ../..
cargo build -p airtalk-core

# 5. Run one case or the whole directory
cd tools/testkit
uv run python -m testkit run cases/smoke_zh.yaml
uv run python -m testkit run-all cases/
```

You can also use the console script entry point:

```bash
uv run testkit run cases/smoke_zh.yaml
```

## Interactive mic loop (`miccheck`)

For ad-hoc "does it actually work with my voice?" tests, `miccheck`
records from the default input device and drives core in real time:

```bash
# Defaults: vad=true, LLM cleanup via qwen-plus
uv run testkit miccheck

# ASR only (skip LLM)
uv run testkit miccheck --no-llm

# Save each round's recording for later replay
uv run testkit miccheck --save recordings/take.wav
# → writes recordings/take_001.wav, take_002.wav, ...
```

Design:

- **One persistent core subprocess** is spawned on launch. All rounds
  share it — no per-round ONNX load cost.
- **Audio streams into core while you're still speaking** (every
  ~100ms the captured buffer is base64'd and sent as a `chunk`
  frame). VAD can split segments and ASR can start transcribing
  before you finish.
- When you press Enter, the tool sends `end` and measures the time
  until the terminal `result` arrives — reported as `wait` (the
  perceived latency from user POV).
- Ctrl+C quits cleanly at any time. Stdin is closed on exit so core
  drains gracefully.

Output:

```
🎙️  Recording round 1... press Enter to stop.
[you talk, press Enter]
────────────────────────────────────────────────────────────
text:     你好，这是一个简短的测试。
raw:      你好这是一个简短的测试
language: zh
rec:  4.20s   wait:   720ms (end→result)   total:  4920ms
────────────────────────────────────────────────────────────

Press Enter to record again (Ctrl+C to quit):
```

## Case spec (YAML)

```yaml
name: demo                     # free-form identifier

voice: zh-CN-XiaoxiaoNeural    # default voice for segments without one

segments:
  - say: "我们来测试一下分段"   # synthesize via Edge TTS
  - silence: 1200              # milliseconds
  - say: "第二段"
    voice: zh-CN-YunxiNeural   # per-segment voice override

noise:                         # optional background noise
  kind: white                  # white | pink | file:path/to/noise.wav
  rms: 0.01                    # post-normalized RMS in [-1, 1] range

core:
  vad: true                    # vad=true uses Silero segmentation
  no_llm: true                 # skip LLM cleanup, assert on raw ASR

expect:
  segments_emitted: 2          # must match VAD summary line in stderr
  raw_cer_max: 0.10            # character error rate ≤ 10%
  raw_must_contain: ["分段", "第二"]
  language: zh
```

## Output

```
[PASS] smoke_zh           segments=1/1  CER=0.03  lang=zh  took=4.1s
[FAIL] with_white_noise   segments=1/2  CER=0.28  (expected ≤0.15)
         └─ raw: "在白噪声背景下测试..."
         └─ missing keyword: "嘈杂"
Summary: 1/2 passed
```

Generated audio lands in `outputs/<case_name>.wav` for manual
inspection. Directory is gitignored.

## Design notes

- Calls **real** DashScope ASR. The whole point is to measure the
  full VAD + ASR pipeline on generated audio; mocking ASR would
  defeat it.
- LLM is skipped by default (`no_llm: true`) because LLM output
  is non-deterministic and hard to assert on. Set `no_llm: false`
  and add the LLM CLI args if you want to cover the full chain.
- CER is computed after light normalization (strip whitespace and
  standard punctuation) — ASR won't emit commas the same way the
  TTS script spells them.
- Noise is mixed post-TTS at a target RMS. Use small values
  (0.002–0.02); Silero's been trained with background noise but
  isn't infinitely robust.
