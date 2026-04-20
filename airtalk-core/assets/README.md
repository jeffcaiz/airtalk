# airtalk-core assets

## silero_vad.onnx (not committed)

Download from the official Silero VAD release and place here:

  https://github.com/snakers4/silero-vad/raw/master/src/silero_vad/data/silero_vad.onnx

The file is ~2 MB. `airtalk-core/src/vad/silero.rs` includes it at
compile time via `include_bytes!`, so the binary is self-contained —
but the source tree needs the file present to build.

Tested against Silero VAD v5 (512-sample frames @ 16 kHz, LSTM state
shape [2, 1, 128]). If you use a different version, verify the tensor
shapes and input/output names in `silero.rs::infer`.
