# airtalk-core assets

## silero_vad.onnx

Committed to the repo (~2 MB). Source:

  https://github.com/snakers4/silero-vad/raw/master/src/silero_vad/data/silero_vad.onnx

`airtalk-core/src/vad/silero.rs` includes it at compile time via
`include_bytes!`, so the binary is self-contained and CI needs no
extra download step.

Tested against Silero VAD v5 (512-sample frames @ 16 kHz, LSTM state
shape [2, 1, 128]). If you use a different version, verify the tensor
shapes and input/output names in `silero.rs::infer`.
