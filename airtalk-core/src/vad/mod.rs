//! VAD trait, factory, and implementations.

pub mod silero;

/// A speech segment detected by VAD, ready for ASR.
pub struct SpeechSegment {
    /// PCM16 LE 16 kHz mono, includes configured padding on both ends.
    pub pcm: Vec<u8>,
}

/// Stateful VAD instance. One per session.
pub trait VadEngine: Send {
    /// Feed PCM16 LE 16 kHz mono bytes. Returns zero or more completed
    /// segments (segments that ended inside this chunk).
    fn push_pcm(&mut self, pcm: &[u8]) -> Vec<SpeechSegment>;

    /// Called after the audio stream ends. Returns any trailing speech
    /// that hadn't yet been closed by a silence.
    fn finish(&mut self) -> Option<SpeechSegment>;
}

/// Creates a fresh [`VadEngine`] per session. Holds shared resources
/// (the loaded Silero ONNX session) so each session doesn't reload.
pub trait VadFactory: Send + Sync {
    fn create(&self) -> Box<dyn VadEngine>;
}
