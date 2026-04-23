# AirTalk - 空·谈

Windows voice input tool. Hold a hotkey, speak, release — your cleaned-up
text is pasted into the focused app.

Two processes, connected by stdio:

- `airtalk.exe` — native UI: tray, global hotkey, overlay, mic capture, paste
- `airtalk-core.exe` — computation: VAD (Silero), ASR (Qwen3-ASR), LLM cleanup

See [`DESIGN.md`](./DESIGN.md) for architecture, protocol, and
implementation notes.

## Install

Download the latest installer from the
[Releases page](https://github.com/jeffcaiz/airtalk/releases):

- **`airtalk-vX.Y.Z-x86_64-windows-setup.exe`** — standard installer.
  Optional "Launch at Startup" task is checked by default; creates
  Start Menu entries; uninstaller removes `%APPDATA%\airtalk\` and saved
  API keys from Credential Manager.
- **`airtalk-vX.Y.Z-x86_64-pc-windows-msvc.zip`** — portable build if you
  prefer no installer. Extract anywhere and run `airtalk.exe`.

Both builds are **not code-signed** (no certificate yet). Windows
SmartScreen will show "Windows protected your PC" on first launch —
click **More info → Run anyway**. The warning goes away after Windows
learns the binary.

On first launch AirTalk opens Settings and asks for your DashScope API
key. Get one at [dashscope.console.aliyun.com](https://dashscope.console.aliyun.com/).

## Status

Core is complete and verified end-to-end against a real DashScope
account (Qwen3-ASR + qwen-flash LLM cleanup). Windows UI ships with
tray, global hotkey, overlay, paste, Settings window, and "Launch at
Startup". See DESIGN.md §Implementation status for the component
breakdown.

## Layout

```
airtalk-proto/   shared wire types (Request / Response + framing helpers)
airtalk-core/    background computation process
airtalk/         native UI process (tray, hotkey, overlay, Settings)
installer/       Inno Setup script for Windows installer
```

## Build

```
cargo build --workspace
```

The Silero ONNX model is not committed. Place
`silero_vad.onnx` in `airtalk-core/assets/` before building
`airtalk-core` — see DESIGN.md §Silero.

`onnxruntime` is bundled at build time via the
`ort/download-binaries` cargo feature (see `airtalk-core/Cargo.toml`).
The resulting `airtalk-core.exe` is standalone — no `onnxruntime.dll`
to ship separately. First build downloads the runtime (~15 MB); later
builds cache.

## Testing tools

- **`cargo test --workspace`** — 11 proto tests, 4 core unit tests,
  9 integration tests (black-box subprocess + wiremock).
- **`tools/testkit/`** — Python-based synthesis regression suite.
  Generates test audio via Edge TTS from YAML specs, drives core,
  asserts on CER / segment count / keywords. Includes `miccheck`
  subcommand for interactive mic → transcript loop. See
  [`tools/testkit/README.md`](./tools/testkit/README.md).
