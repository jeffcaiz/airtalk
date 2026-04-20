"""Edge TTS wrapping — text → 16kHz mono PCM16 WAV file.

Edge TTS returns MP3; we decode + resample via pydub (which calls out
to ffmpeg). Users must have ffmpeg on PATH.
"""

from __future__ import annotations

import tempfile
from pathlib import Path

import edge_tts
from pydub import AudioSegment


async def synthesize(
    text: str,
    voice: str,
    output_wav: Path,
    *,
    rate: str = "+0%",
    volume: str = "+0%",
    pitch: str = "+0Hz",
) -> None:
    """Generate a 16kHz mono PCM16 WAV at `output_wav` speaking `text`.

    `rate` / `volume` / `pitch` are Edge TTS SSML prosody strings
    (e.g. `rate="+30%"`, `volume="-20%"`, `pitch="+10Hz"`).
    """
    # Edge TTS writes MP3; we convert to the format core expects.
    with tempfile.NamedTemporaryFile(suffix=".mp3", delete=False) as tmp:
        mp3_path = Path(tmp.name)
    try:
        communicate = edge_tts.Communicate(
            text, voice, rate=rate, volume=volume, pitch=pitch
        )
        await communicate.save(str(mp3_path))
        audio = AudioSegment.from_file(mp3_path, format="mp3")
        audio = (
            audio.set_frame_rate(16000)
            .set_channels(1)
            .set_sample_width(2)  # 16-bit
        )
        output_wav.parent.mkdir(parents=True, exist_ok=True)
        audio.export(str(output_wav), format="wav")
    finally:
        mp3_path.unlink(missing_ok=True)
