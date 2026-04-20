fn main() {
    // Rebuild when the Silero model file changes, since it is embedded
    // via include_bytes! in src/vad/silero.rs.
    println!("cargo:rerun-if-changed=assets/silero_vad.onnx");
}
