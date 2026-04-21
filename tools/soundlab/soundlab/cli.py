"""Generate 9 candidate UI tones for airtalk: 3 styles x {start, stop, error}.

Run via `uv run soundlab`. Output lands in `tools/soundlab/out/`; double-click
the .wav files to audition. Pick the variant you like and tell Claude the
style number — the render parameters will be ported verbatim into
`airtalk/src/feedback.rs` (runtime-synthesised, no WAV shipped).
"""

from __future__ import annotations

import argparse
import wave
from pathlib import Path

import numpy as np

SR = 48000  # 48 kHz — matches Windows default mix format

# Music notes (Hz). Rounded to 2dp.
NOTES = {
    "C5": 523.25,
    "E5": 659.25,
    "G5": 783.99,
    "C6": 1046.50,
    "G4": 392.00,
    "E4": 329.63,
    "D4": 293.66,
    "A3": 220.00,
    "F3": 174.61,
    "D3": 146.83,
}


def render_tone(
    freq: float,
    duration_ms: int,
    attack_ms: float = 5.0,
    decay_tau_ms: float | None = None,
    harmonics: list[tuple[float, float]] | None = None,
    gain: float = 0.45,
) -> np.ndarray:
    """One tone with linear attack + exponential decay.

    `harmonics`: list of (multiplier, amplitude) pairs added to the
    fundamental, e.g. [(2.0, 0.15), (3.0, 0.08)] for a slightly woody feel.
    `decay_tau_ms`: exponential time constant; default = duration/3 (≈ 95%
    decayed by the end of the buffer).
    """
    n = int(SR * duration_ms / 1000)
    t = np.arange(n) / SR
    sig = np.sin(2 * np.pi * freq * t)
    if harmonics:
        amp_sum = 1.0
        for mult, amp in harmonics:
            sig = sig + amp * np.sin(2 * np.pi * freq * mult * t)
            amp_sum += amp
        sig = sig / amp_sum

    env = np.ones(n)
    attack_n = max(1, int(SR * attack_ms / 1000))
    env[:attack_n] = np.linspace(0.0, 1.0, attack_n)
    tau = (decay_tau_ms or (duration_ms / 3.0)) / 1000.0
    decay_t = t[attack_n:] - t[attack_n]
    env[attack_n:] = np.exp(-decay_t / tau)

    return (sig * env * gain).astype(np.float32)


def concat(*segments: np.ndarray, gap_ms: int = 0) -> np.ndarray:
    gap = np.zeros(int(SR * gap_ms / 1000), dtype=np.float32)
    out = []
    for i, seg in enumerate(segments):
        if i > 0 and gap_ms > 0:
            out.append(gap)
        out.append(seg)
    return np.concatenate(out)


def save_wav(path: Path, sig: np.ndarray) -> None:
    pcm = np.clip(sig, -1.0, 1.0)
    pcm16 = (pcm * 32767.0).astype(np.int16)
    with wave.open(str(path), "wb") as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(SR)
        f.writeframes(pcm16.tobytes())


# ─── Three style families ──────────────────────────────────────────────

def v1_hard_sine(kind: str) -> np.ndarray:
    """Pure sine, fast attack, short decay. Crisp, business-y."""
    if kind == "start":
        return render_tone(NOTES["C5"], 120, attack_ms=4)
    if kind == "stop":
        return render_tone(NOTES["G4"], 120, attack_ms=4)
    if kind == "error":
        # Two low hits, lower 2nd, short gap — subtle "uh-oh"
        return concat(
            render_tone(NOTES["A3"], 100, attack_ms=4),
            render_tone(NOTES["F3"], 160, attack_ms=4),
            gap_ms=15,
        )
    raise ValueError(kind)


def v2_soft_harmonics(kind: str) -> np.ndarray:
    """Sine + 2nd/3rd harmonics, longer attack/decay. Warmer, softer."""
    h = [(2.0, 0.18), (3.0, 0.08)]
    if kind == "start":
        return render_tone(NOTES["C5"], 180, attack_ms=10, harmonics=h, decay_tau_ms=90)
    if kind == "stop":
        return render_tone(NOTES["G4"], 180, attack_ms=10, harmonics=h, decay_tau_ms=90)
    if kind == "error":
        return concat(
            render_tone(NOTES["A3"], 140, attack_ms=10, harmonics=h, decay_tau_ms=70),
            render_tone(NOTES["F3"], 200, attack_ms=10, harmonics=h, decay_tau_ms=90),
            gap_ms=20,
        )
    raise ValueError(kind)


def v3_mallet(kind: str) -> np.ndarray:
    """Two quick notes per event, like a small wood mallet. Musical, bright."""
    if kind == "start":
        # C5 → C6 (octave up, rising)
        return concat(
            render_tone(NOTES["C5"], 70, attack_ms=2, decay_tau_ms=40),
            render_tone(NOTES["C6"], 100, attack_ms=2, decay_tau_ms=50),
            gap_ms=10,
        )
    if kind == "stop":
        # G5 → G4 (octave down, falling)
        return concat(
            render_tone(NOTES["G5"], 70, attack_ms=2, decay_tau_ms=40),
            render_tone(NOTES["G4"], 100, attack_ms=2, decay_tau_ms=50),
            gap_ms=10,
        )
    if kind == "error":
        # D4 → D3 (octave down, low-register)
        return concat(
            render_tone(NOTES["D4"], 90, attack_ms=2, decay_tau_ms=50),
            render_tone(NOTES["D3"], 150, attack_ms=2, decay_tau_ms=80),
            gap_ms=15,
        )
    raise ValueError(kind)


VARIANTS = {
    "v1": ("hard_sine", v1_hard_sine),
    "v2": ("soft_harmonics", v2_soft_harmonics),
    "v3": ("mallet", v3_mallet),
}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "out",
        help="Output directory (default: tools/soundlab/out/)",
    )
    args = parser.parse_args()

    out_dir: Path = args.out
    out_dir.mkdir(parents=True, exist_ok=True)

    for v, (label, fn) in VARIANTS.items():
        for kind in ("start", "stop", "error"):
            path = out_dir / f"{v}_{kind}.wav"
            save_wav(path, fn(kind))
            print(f"  wrote {path.name}")

    # Drop a legend so user remembers which variant is which later.
    (out_dir / "LEGEND.txt").write_text(
        "v1 = hard_sine       (pure sine, fast attack, crisp)\n"
        "v2 = soft_harmonics  (sine + 2nd/3rd harmonic, warmer)\n"
        "v3 = mallet          (two quick notes, wood-mallet feel)\n"
        "\n"
        "Each variant has start/stop/error.\n"
        "Double-click to audition. Tell Claude which variant to port into\n"
        "airtalk/src/feedback.rs (runtime-synthesised, no WAV shipped).\n",
        encoding="utf-8",
    )

    print(f"\nDone. 9 wavs in {out_dir}")
    print("Double-click to audition; pick v1 / v2 / v3.")


if __name__ == "__main__":
    main()
