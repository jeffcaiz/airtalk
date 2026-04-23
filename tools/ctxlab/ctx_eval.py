"""Ad-hoc evaluator: record human audio once per sentence, run ASR with
multiple context variants against the same recording, print raw
recognition side-by-side.

Per sentence group:
  1. Prompt you to read the sentence.
  2. Press Enter → record → Enter to stop.
  3. Recording saved to outputs/ctxeval_human_<group>.wav.
  4. Same wav is sent to core once per context variant.
  5. All raw results printed at the end for easy eyeballing.

If a recording already exists on disk, you'll be asked whether to
reuse, re-record, or skip. Pass --rerecord to always re-record.

Usage (PowerShell, from repo root):
    $env:AIRTALK_ASR_API_KEY = "sk-..."
    cd D:\\vibe\\airtalk\\tools\\testkit
    uv run python ..\\ctxlab\\ctx_eval.py
    uv run python ..\\ctxlab\\ctx_eval.py --only kubectl
    uv run python ..\\ctxlab\\ctx_eval.py --rerecord
"""

from __future__ import annotations

import argparse
import asyncio
import sys
import threading
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import numpy as np
import sounddevice as sd

ROOT = Path(__file__).resolve().parents[2]
# Make `testkit` importable even if this script runs from a different cwd.
sys.path.insert(0, str(ROOT / "tools" / "testkit"))

from testkit.driver import run_core  # noqa: E402

CORE_BIN = ROOT / "target" / "debug" / "airtalk-core.exe"
OUT_DIR = ROOT / "tools" / "testkit" / "outputs"
SAMPLE_RATE = 16_000


@dataclass
class Trial:
    label: str
    context: Optional[str]


@dataclass
class Group:
    key: str
    sentence: str
    hint: str  # shown next to the sentence prompt
    trials: list[Trial]


GROUPS: list[Group] = [
    Group(
        key="pip",
        sentence="pip install 到 site-packages",
        hint="zh voice, English terms embedded",
        trials=[
            Trial(label="pip_noctx", context=None),
            # "Cheating": target words appear verbatim in context.
            Trial(
                label="pip_titlelike",
                context="Python 包管理：用 pip 把依赖装到 site-packages 目录",
            ),
            # "Bare title": domain-adjacent words only — no pip/install/
            # site-packages. Mimics a realistic VSCode window title.
            Trial(
                label="pip_baretitle",
                context="VSCode — pyproject.toml — editing Python dependencies",
            ),
        ],
    ),
    Group(
        key="kubectl",
        sentence="kubectl get pods in kube-system",
        hint="English; say 'kubectl' however feels natural to you",
        trials=[
            Trial(label="kubectl_noctx", context=None),
            # "Cheating": target words appear verbatim.
            Trial(
                label="kubectl_titlelike",
                context="Kubernetes debugging: kubectl inspect pods in the kube-system namespace",
            ),
            # "Bare title": k8s-domain words only — no kubectl/pods/
            # kube-system/get/in. Mimics a terminal window title.
            Trial(
                label="kubectl_baretitle",
                context="Terminal — k8s-prod cluster — deployment rollout",
            ),
        ],
    ),
]


def _write_pcm16_wav(samples_f32: np.ndarray, path: Path) -> None:
    pcm16 = np.clip(samples_f32 * 32767.0, -32768, 32767).astype(np.int16)
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(pcm16.tobytes())


def _record_to_wav(wav_path: Path) -> bool:
    """Blocking record flow: Enter to start, Enter to stop.
    Returns True if a recording was saved, False on cancel/empty."""
    try:
        input("   >> press Enter to START recording (Ctrl+C to cancel)... ")
    except (EOFError, KeyboardInterrupt):
        print("\n   cancelled.")
        return False

    frames: list[np.ndarray] = []
    lock = threading.Lock()

    def cb(indata, _count, _ti, status):
        if status:
            print(f"   [audio status] {status}", file=sys.stderr)
        with lock:
            frames.append(indata.copy())

    try:
        with sd.InputStream(
            samplerate=SAMPLE_RATE,
            channels=1,
            dtype="float32",
            blocksize=1600,
            callback=cb,
        ):
            try:
                input("   🎙️  recording... press Enter to STOP. ")
            except (EOFError, KeyboardInterrupt):
                print("\n   recording interrupted — discarding.")
                return False
    except Exception as exc:
        print(f"   error opening mic: {exc}", file=sys.stderr)
        return False

    with lock:
        if not frames:
            print("   no audio captured.")
            return False
        audio = np.concatenate(frames).flatten()

    _write_pcm16_wav(audio, wav_path)
    duration_s = len(audio) / SAMPLE_RATE
    peak = float(np.max(np.abs(audio))) if audio.size else 0.0
    print(f"   saved: {wav_path.name}  ({duration_s:.1f}s, peak={peak:.2f})")
    if peak < 0.02:
        print("   warning: audio is very quiet — check your mic.")
    return True


def _ensure_recording(group: Group, force_rerecord: bool) -> Optional[Path]:
    wav_path = OUT_DIR / f"ctxeval_human_{group.key}.wav"

    if wav_path.exists() and not force_rerecord:
        print(f"   existing recording: {wav_path.name}")
        try:
            choice = input("   [Enter=reuse / r=re-record / s=skip]: ").strip().lower()
        except (EOFError, KeyboardInterrupt):
            print()
            return None
        if choice == "r":
            if not _record_to_wav(wav_path):
                return None
        elif choice == "s":
            print("   skipped.")
            return None
        else:
            print("   reusing existing recording.")
    else:
        if not _record_to_wav(wav_path):
            return None

    return wav_path


async def _run_trials(wav_path: Path, group: Group) -> list[dict]:
    results: list[dict] = []
    for trial in group.trials:
        print(f"   [asr] {trial.label}...", end="", flush=True)
        r = await run_core(
            wav_path,
            core_bin=CORE_BIN,
            vad=True,
            no_llm=True,
            context=trial.context,
        )
        raw = ""
        if r.response and r.response.get("type") == "result":
            raw = r.response.get("raw") or r.response.get("text", "")
        elif r.response and r.response.get("type") == "error":
            raw = f"<error: {r.response.get('message')!r}>"
        else:
            raw = f"<no response (exit={r.exit_code})>"
        print(f" done ({r.elapsed_s:.1f}s)")
        results.append(
            {
                "label": trial.label,
                "context": trial.context,
                "raw": raw,
                "lang": (r.response or {}).get("language"),
                "elapsed_s": r.elapsed_s,
            }
        )
    return results


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument(
        "--only",
        help=f"run only this group key (choices: {', '.join(g.key for g in GROUPS)})",
    )
    parser.add_argument(
        "--rerecord",
        action="store_true",
        help="always re-record, skip the reuse prompt",
    )
    args = parser.parse_args()

    if not CORE_BIN.is_file():
        print(f"error: core binary missing at {CORE_BIN}", file=sys.stderr)
        print("hint: cargo build -p airtalk-core", file=sys.stderr)
        return 2

    groups = GROUPS
    if args.only:
        groups = [g for g in GROUPS if g.key == args.only]
        if not groups:
            print(f"error: no such group '{args.only}'", file=sys.stderr)
            print(
                f"available: {', '.join(g.key for g in GROUPS)}", file=sys.stderr
            )
            return 2

    all_results: list[tuple[Group, list[dict]]] = []
    for group in groups:
        print()
        print("=" * 78)
        print(f"Group: {group.key}   ({group.hint})")
        print()
        print(f"    Please read this sentence:")
        print(f"      「{group.sentence}」")
        print()
        wav_path = _ensure_recording(group, args.rerecord)
        if wav_path is None:
            continue
        print(f"   running {len(group.trials)} context variant(s)...")
        results = await _run_trials(wav_path, group)
        all_results.append((group, results))

    print()
    print("=" * 78)
    print("Results")
    print("=" * 78)
    if not all_results:
        print("(nothing to report)")
        return 0
    for group, results in all_results:
        print(f"\n--- {group.key}: 「{group.sentence}」 ---")
        for r in results:
            ctx_show = r["context"] if r["context"] else "(no context)"
            print(f"  [{r['label']:<22}] ctx={ctx_show!r}")
            print(f"    raw : {r['raw']!r}")
            print(f"    lang={r['lang']}  took={r['elapsed_s']:.1f}s")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print("\n[aborted]")
        sys.exit(130)
