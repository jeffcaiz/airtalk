"""Audio composition: concat TTS clips + silences, optionally mix noise."""

from __future__ import annotations

import tempfile
from pathlib import Path
from typing import Any

import numpy as np
from pydub import AudioSegment

from .tts import synthesize


async def build_audio(spec: dict, output_wav: Path) -> None:
    """Build the final test WAV from a case spec's `segments` + `noise`."""
    default_voice = spec.get("voice", "zh-CN-XiaoxiaoNeural")

    clips: list[AudioSegment] = []
    for i, seg in enumerate(spec["segments"]):
        if "say" in seg:
            text = seg["say"]
            voice = seg.get("voice", default_voice)
            clip = await _tts_clip(
                text,
                voice,
                i,
                rate=seg.get("rate", "+0%"),
                volume=seg.get("volume", "+0%"),
                pitch=seg.get("pitch", "+0Hz"),
            )
            clips.append(clip)
        elif "silence" in seg:
            clips.append(AudioSegment.silent(duration=int(seg["silence"]), frame_rate=16000))
        else:
            raise ValueError(f"segment {i} has neither `say` nor `silence`")

    if not clips:
        combined = AudioSegment.silent(duration=100, frame_rate=16000)
    else:
        combined = clips[0]
        for c in clips[1:]:
            combined += c

    combined = combined.set_frame_rate(16000).set_channels(1).set_sample_width(2)

    if "noise" in spec:
        combined = _mix_noise(combined, spec["noise"])

    output_wav.parent.mkdir(parents=True, exist_ok=True)
    combined.export(str(output_wav), format="wav")


async def _tts_clip(
    text: str,
    voice: str,
    index: int,
    *,
    rate: str = "+0%",
    volume: str = "+0%",
    pitch: str = "+0Hz",
) -> AudioSegment:
    with tempfile.TemporaryDirectory() as td:
        wav_path = Path(td) / f"seg{index}.wav"
        await synthesize(text, voice, wav_path, rate=rate, volume=volume, pitch=pitch)
        return AudioSegment.from_wav(wav_path)


def _mix_noise(audio: AudioSegment, noise_spec: dict[str, Any]) -> AudioSegment:
    """Overlay noise at a target post-normalized RMS."""
    kind = noise_spec["kind"]
    target_rms = float(noise_spec["rms"])

    n_samples = int(audio.frame_count())
    sr = audio.frame_rate

    if kind == "white":
        noise = np.random.default_rng(42).normal(0.0, 1.0, size=n_samples)
    elif kind == "pink":
        noise = _pink_noise(n_samples)
    elif kind.startswith("file:"):
        noise = _load_noise_file(kind[len("file:"):], n_samples, sr)
    else:
        raise ValueError(f"unknown noise kind: {kind!r}")

    # Normalize noise to target RMS (sample range [-1, 1]).
    current_rms = float(np.sqrt(np.mean(noise**2)))
    if current_rms > 0:
        noise = noise * (target_rms / current_rms)
    noise = np.clip(noise, -1.0, 1.0)

    noise_i16 = (noise * 32767.0).astype(np.int16)
    noise_segment = AudioSegment(
        noise_i16.tobytes(),
        frame_rate=sr,
        sample_width=2,
        channels=1,
    )
    return audio.overlay(noise_segment)


def _pink_noise(n: int) -> np.ndarray:
    """Approximate 1/f noise via FFT spectral shaping. Deterministic seed."""
    rng = np.random.default_rng(42)
    white = rng.normal(0.0, 1.0, size=n)
    spectrum = np.fft.rfft(white)
    freqs = np.arange(1, len(spectrum) + 1, dtype=float)
    spectrum /= np.sqrt(freqs)  # 1/sqrt(f) amplitude → 1/f power
    pink = np.fft.irfft(spectrum, n=n).real
    return pink


def _load_noise_file(path: str, n_samples: int, sr: int) -> np.ndarray:
    audio = AudioSegment.from_file(path)
    audio = audio.set_frame_rate(sr).set_channels(1).set_sample_width(2)
    samples = np.array(audio.get_array_of_samples(), dtype=np.float32) / 32768.0
    if len(samples) >= n_samples:
        return samples[:n_samples]
    reps = (n_samples // len(samples)) + 1
    return np.tile(samples, reps)[:n_samples]
